/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! GitHub APIのrepo/ref確認と限定認証での固定commit取得
//!

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use octocrab::service::middleware::retry::RetryConfig;
use serde::Deserialize;
use url::Url;

use crate::cmd_args::resolve::source::GitSelector;
use crate::secret::Secret;

use super::{AcquisitionLimits, SourceError, SourceSnapshot, StopContext};
use super::{remote, runtime};

///
/// URLのrepo一致とcommit実体を確認済みの所有されたAPI情報。
///
pub(super) struct Metadata {
    url: Url,
    commit: String,
    private: bool,
}

impl Metadata {
    ///
    /// 確定した完全commit IDを参照する。
    ///
    /// # 戻り値
    /// APIで実体とhex形式を確認したID
    ///
    pub(super) fn commit(&self) -> &str { &self.commit }

    ///
    /// APIで確認したrepoのprivate状態を取得する。
    ///
    /// # 戻り値
    /// API応答のprivate値
    ///
    pub(super) fn is_private(&self) -> bool { self.private }
}

impl fmt::Debug for Metadata {
    ///
    /// repo名・URL・commit値を表示せず状態だけを出す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な状態表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GitHubMetadata")
            .field("private", &self.private).finish()
    }
}

///
/// repo確認に必要なJSON項目だけを受け取る。
///
#[derive(Deserialize)]
struct RepositoryInfo { full_name: String, clone_url: String, private: bool }

///
/// GitHubのGit object参照。
///
#[derive(Deserialize)]
struct ObjectInfo {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

///
/// 完全ref名と指すobjectを検査するための応答。
///
#[derive(Deserialize)]
struct ReferenceInfo {
    #[serde(rename = "ref")]
    name: String,
    object: ObjectInfo,
}

///
/// 注釈tagをcommitまで辿るための応答。
///
#[derive(Deserialize)]
struct TagInfo { sha: String, object: ObjectInfo }

///
/// commit用endpointで実体を確認したID。
///
#[derive(Deserialize)]
struct CommitInfo { sha: String }

///
/// GitHub APIで確定したcommitを既存のblocking取得へ渡す。
///
/// # 概要
/// トークンは環境から解決済みのSecretだけを受け、表示型へ保持しない。
/// HTTPは直列に実行し、workerへは所有された情報と限定資格情報を渡す。
///
/// # 引数
/// * `owner` - GitHub owner
/// * `repo` - GitHub repo
/// * `selector` - 参照方針
/// * `token` - GITHUB_TOKEN由来の任意のSecret
/// * `limits` - 取得上限
/// * `stop` - 全体と共有する期限・停止境界
///
/// # 戻り値
/// GitHub種別の所有されたsnapshot
///
pub(crate) async fn acquire(owner: String, repo: String,
    selector: GitSelector, token: Option<&Secret>, limits: AcquisitionLimits,
    stop: StopContext) -> Result<SourceSnapshot, SourceError>
{
    /*
     * API境界でだけ秘密値を複製し、非同期情報確認を完結する
     */
    stop.checkpoint()?;
    let token = token.map(|value| Arc::new(Secret::new(
        value.expose().to_owned(),
    )));
    let metadata = inspect(&owner, &repo, selector, token.as_deref(), &stop)
        .await?;
    /*
     * gix値は同一worker内に留め、成功snapshotだけを公開する
     */
    runtime::run(stop, move |context| {
        remote::acquire_github(&metadata.url, metadata.commit, token,
            limits, &context)
    }).await
}

///
/// 固定GitHub APIでrepo/refの検証済み情報を取得する。
///
/// # 引数
/// * `owner` - owner
/// * `repo` - repo
/// * `selector` - 参照方針
/// * `token` - GITHUB_TOKEN由来の任意の秘密値
/// * `stop` - 停止境界
///
/// # 戻り値
/// 取得元と完全commitの確認済み情報
///
pub(super) async fn inspect(owner: &str, repo: &str, selector: GitSelector,
    token: Option<&Secret>, stop: &StopContext) -> Result<Metadata, SourceError>
{
    stop.checkpoint()?;
    resolve(&client("https://api.github.com", token)?, owner, repo,
        selector, stop).await
}

///
/// 自動再試行なしのoctocrab clientを限定したAPI originへ構成する。
///
/// # 概要
/// 不正な認証headerを依存側のpanicより先に拒否する。
///
/// # 引数
/// * `base` - 製品は固定GitHub API、試験はloopback模擬API
/// * `token` - 製品は環境由来、試験はダミーのみ
///
/// # 戻り値
/// redirect/trace featureを除外したclient
///
fn client(base: &str, token: Option<&Secret>)
    -> Result<octocrab::Octocrab, SourceError>
{
    /*
     * originと再試行を固定し、認証headerを検証してから構成する
     */
    let mut builder = octocrab::OctocrabBuilder::default()
        .add_retry_config(RetryConfig::None)
        .add_header(reqwest::header::CACHE_CONTROL, "no-cache".to_owned())
        .base_uri(base).map_err(|_| SourceError::GitHubApi)?;
    if let Some(token) = token {
        // octocrab内部のheader parse unwrapへ不正な値を渡さない。
        if token.expose().is_empty() || reqwest::header::HeaderValue::from_str(
            &format!("Bearer {}", token.expose()),
        ).is_err() {
            return Err(SourceError::GitHubAuth);
        }
        builder = builder.personal_token(token.expose().to_owned());
    }
    builder.build().map_err(|_| SourceError::GitHubApi)
}

///
/// repo情報を確認し、指定参照を完全commit IDへ解決する。
///
/// # 概要
/// default branchを使用せずmainの404時だけmasterを探索する。
/// API内のURLを次の要求先へ流用せず、各endpointを自分で構築する。
///
/// # 引数
/// * `client` - originを限定したAPI client
/// * `owner` - 入力owner
/// * `repo` - 入力repo
/// * `selector` - 参照方針
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 取得可能なrepoの検証済み情報
///
async fn resolve(client: &octocrab::Octocrab, owner: &str, repo: &str,
    selector: GitSelector, stop: &StopContext) -> Result<Metadata, SourceError>
{
    /*
     * repo実在とclone先の同一性を最初に確認する
     */
    let info: RepositoryInfo = get(client, &route(owner, repo, &[])?,
        SourceError::GitHubMissing, stop).await?;
    let url = clone_url(owner, repo, &info)?;
    let object = match selector {
        GitSelector::MainThenMaster => {
            match reference(client, owner, repo, "heads", "main", stop).await {
                Err(SourceError::RefNotFound) =>
                    reference(client, owner, repo, "heads", "master", stop)
                        .await?,
                result => result?,
            }
        }
        GitSelector::Branch(name) =>
            reference(client, owner, repo, "heads", &name, stop).await?,
        GitSelector::Tag(name) =>
            reference(client, owner, repo, "tags", &name, stop).await?,
        GitSelector::Commit(id) => {
            if id.is_empty() || id.len() > 64
                || !id.bytes().all(|v| v.is_ascii_hexdigit())
            {
                return Err(SourceError::RefNotFound);
            }
            let commit: CommitInfo = get(client,
                &route(owner, repo, &["commits", &id])?,
                SourceError::RefNotFound, stop).await?;
            check_id(&commit.sha)?;
            if !commit.sha.to_ascii_lowercase()
                .starts_with(&id.to_ascii_lowercase())
            {
                return Err(SourceError::RefNotFound);
            }
            return Ok(Metadata { url, commit: commit.sha.to_ascii_lowercase(),
                private: info.private });
        }
    };
    /*
     * 注釈tagの循環を拒否し、commit endpointで実体を確認する
     */
    let commit = peel(client, owner, repo, object, stop).await?;
    Ok(Metadata { url, commit, private: info.private })
}

///
/// GitHub repo/refのURL pathをsegment単位でescapeする。
///
/// # 概要
/// owner/repoの危険segmentを拒否し、refのURL構文を値としてescapeする。
///
/// # 引数
/// * `owner` - owner segment
/// * `repo` - repo segment
/// * `tail` - endpointとref名のsegment群
///
/// # 戻り値
/// API originを含まない絶対path
///
fn route(owner: &str, repo: &str, tail: &[&str])
    -> Result<String, SourceError>
{
    for name in [owner, repo] {
        if matches!(name, "" | "." | "..") || name.contains('/')
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(SourceError::GitHubIdentity);
        }
    }
    /*
     * #やslashをURL構文として解釈せず、値としてescapeする
     */
    let mut url = Url::parse("https://api.github.com")
        .map_err(|_| SourceError::GitHubIdentity)?;
    {
        let mut parts = url.path_segments_mut()
            .map_err(|_| SourceError::GitHubIdentity)?;
        parts.clear().push("repos").push(owner).push(repo);
        for part in tail { parts.push(part); }
    }
    Ok(url.path().to_owned())
}

///
/// 要求repoとAPI応答・clone URLの同一性を確認する。
///
/// # 概要
/// tokenを渡すauthorityとrepoを限定し、別host/port/userinfo等を拒否する。
///
/// # 引数
/// * `owner` - 要求owner
/// * `repo` - 要求repo
/// * `info` - repo APIの応答
///
/// # 戻り値
/// 資格情報を含まない同一repoのGitHub HTTPS URL
///
fn clone_url(owner: &str, repo: &str, info: &RepositoryInfo)
    -> Result<Url, SourceError>
{
    let expected = format!("{owner}/{repo}");
    if !info.full_name.eq_ignore_ascii_case(&expected) {
        return Err(SourceError::GitHubIdentity);
    }
    /*
     * APIが返したURLを無条件に信頼せず、escape済みの期待pathと照合する
     */
    let url = Url::parse(&info.clone_url)
        .map_err(|_| SourceError::GitHubIdentity)?;
    let (owner, repo) = info.full_name.split_once('/')
        .ok_or(SourceError::GitHubIdentity)?;
    let mut expected = Url::parse("https://github.com")
        .map_err(|_| SourceError::GitHubIdentity)?;
    expected.path_segments_mut().map_err(|_| SourceError::GitHubIdentity)?
        .clear().push(owner).push(&format!("{repo}.git"));
    let authority = info.clone_url.split("://").nth(1).unwrap_or("")
        .split('/').next().unwrap_or("");
    if url.scheme() != "https" || url.host_str() != Some("github.com")
        || url.port().is_some() || !url.username().is_empty()
        || url.password().is_some() || authority.contains('@')
        || url.query().is_some() || url.fragment().is_some()
        || !url.path().eq_ignore_ascii_case(expected.path())
    {
        return Err(SourceError::GitHubIdentity);
    }
    Ok(url)
}

///
/// 完全ref名を照合し、指すobjectを取得する。
///
/// # 引数
/// * `client` - API client
/// * `owner` - owner
/// * `repo` - repo
/// * `namespace` - headsまたはtags
/// * `name` - ref名
/// * `stop` - 停止境界
///
/// # 戻り値
/// 期待したrefのobject
///
async fn reference(client: &octocrab::Octocrab, owner: &str, repo: &str,
    namespace: &str, name: &str, stop: &StopContext)
    -> Result<ObjectInfo, SourceError>
{
    let info: ReferenceInfo = get(client,
        &route(owner, repo, &["git", "ref", namespace, name])?,
        SourceError::RefNotFound, stop).await?;
    if info.name != format!("refs/{namespace}/{name}")
        || (namespace == "heads" && info.object.kind != "commit")
    {
        return Err(SourceError::GitHubIdentity);
    }
    check_id(&info.object.sha)?;
    Ok(info.object)
}

///
/// 注釈tagを循環なくpeelし、commit実体を確認する。
///
/// # 概要
/// APIに含まれるobject URLを使用せず、shaから限定endpointを構築する。
///
/// # 引数
/// * `client` - API client
/// * `owner` - owner
/// * `repo` - repo
/// * `object` - refのobject
/// * `stop` - 停止境界
///
/// # 戻り値
/// 完全commit ID
///
async fn peel(client: &octocrab::Octocrab, owner: &str, repo: &str,
    mut object: ObjectInfo, stop: &StopContext) -> Result<String, SourceError>
{
    let mut seen = HashSet::new();
    /*
     * 各tag要求で期限を共有し、同じtagの再訪を拒否する
     */
    while object.kind == "tag" {
        check_id(&object.sha)?;
        object.sha.make_ascii_lowercase();
        if !seen.insert(object.sha.clone()) {
            return Err(SourceError::GitHubIdentity);
        }
        let tag: TagInfo = get(client,
            &route(owner, repo, &["git", "tags", &object.sha])?,
            SourceError::RefNotFound, stop).await?;
        if !tag.sha.eq_ignore_ascii_case(&object.sha) {
            return Err(SourceError::GitHubIdentity);
        }
        object = tag.object;
    }
    /*
     * blob/tree tagを拒否し、commit endpointのshaを照合する
     */
    if object.kind != "commit" { return Err(SourceError::RefNotFound); }
    check_id(&object.sha)?;
    let commit: CommitInfo = get(client,
        &route(owner, repo, &["git", "commits", &object.sha])?,
        SourceError::RefNotFound, stop).await?;
    check_id(&commit.sha)?;
    if !commit.sha.eq_ignore_ascii_case(&object.sha) {
        return Err(SourceError::GitHubIdentity);
    }
    Ok(commit.sha.to_ascii_lowercase())
}

///
/// 一件のAPI要求を共有期限・協調停止のもとで実行する。
///
/// # 概要
/// 401は認証失敗、403/429は拒否、404は呼出し側の不存在分類へ写す。
/// 応答本文や外部エラーは保持・表示せず、要求を自動再送しない。
///
/// # 引数
/// * `client` - 限定originのclient
/// * `route` - 自分で構築したAPI path
/// * `missing` - 404時の分類
/// * `stop` - 停止境界
///
/// # 戻り値
/// 必要なJSON情報または安全な失敗分類
///
async fn get<T: serde::de::DeserializeOwned>(client: &octocrab::Octocrab,
    route: &str, missing: SourceError, stop: &StopContext)
    -> Result<T, SourceError>
{
    stop.checkpoint()?;
    /*
     * HTTP完了と停止通知を競合させ、取消時はHTTP futureを破棄する
     */
    let result = tokio::select! {
        result = client.get::<T, _, ()>(route, None) => result,
        error = wait_stop(stop) => return Err(error),
    };
    stop.checkpoint()?;
    result.map_err(|error| match error {
        octocrab::Error::GitHub { source, .. } => {
            match source.status_code.as_u16() {
                401 => SourceError::GitHubAuth,
                403 | 429 => SourceError::GitHubDenied,
                404 => missing,
                _ => SourceError::GitHubApi,
            }
        }
        _ => SourceError::GitHubApi,
    })
}

///
/// HTTP要求中の共有期限と取消を監視する。
///
/// # 引数
/// * `stop` - 停止境界
///
/// # 戻り値
/// 到達した期限または取消の分類
///
async fn wait_stop(stop: &StopContext) -> SourceError {
    loop {
        if let Err(error) = stop.checkpoint() { return error; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

///
/// API由来のIDが完全なhex形式か検査する。
///
/// # 引数
/// * `id` - API応答のID
///
/// # 戻り値
/// 完全IDなら成功
///
fn check_id(id: &str) -> Result<(), SourceError> {
    if !matches!(id.len(), 40 | 64)
        || !id.bytes().all(|v| v.is_ascii_hexdigit())
    {
        return Err(SourceError::GitHubIdentity);
    }
    Ok(())
}

///
/// loopback模擬APIで製品と同じ情報確認処理を試す。
///
/// # 引数
/// * `base` - 試験専用API origin
/// * `owner` - fixture owner
/// * `repo` - fixture repo
/// * `selector` - 参照方針
/// * `token` - ダミーだけの資格情報
/// * `stop` - 停止境界
///
/// # 戻り値
/// 模擬APIの検証済み情報
///
#[cfg(test)]
pub(super) async fn metadata_fixture(base: &str, owner: &str, repo: &str,
    selector: GitSelector, token: Option<&Secret>, stop: &StopContext)
    -> Result<Metadata, SourceError>
{
    stop.checkpoint()?;
    resolve(&client(base, token)?, owner, repo, selector, stop).await
}
