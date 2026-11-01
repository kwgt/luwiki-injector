/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! HTTPS Gitの隔離取得・選択ツリー検査・全体計量と協調停止
//!

pub(super) mod watch;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use url::Url;

use crate::cmd_args::resolve::source::GitSelector;
use crate::report::types::SourceKind;
use crate::secret::Secret;

use super::store::Builder;
use super::{AcquisitionLimits, SourceError, SourceLease};
use super::{SourceSnapshot, StopContext, git};

///
/// 所有された取得領域内での安全検査境界。
///
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase { Prepared, Fetched, Validated, CheckedOut }

///
/// GitHubの同一repoに限定したメモリ内資格情報。
///
#[derive(Clone)]
pub(super) struct GitAuthentication {
    scope: String,
    token: Arc<Secret>,
}

impl GitAuthentication {
    ///
    /// 同一HTTPS host・port・repoのGetにだけ資格情報を返す。
    ///
    /// # 概要
    /// gixのURLだけの要求を解析し、保存要求と異なる取得先を拒否する。
    ///
    /// # 引数
    /// * `action` - gixの資格情報要求
    ///
    /// # 戻り値
    /// 限定したGetだけの資格情報。Store/Eraseは保存操作をしない。
    ///
    pub(super) fn credential(&self, action: gix::credentials::helper::Action)
        -> gix::credentials::protocol::Result
    {
        use gix::credentials::helper::{Action, NextAction};
        use gix::credentials::protocol::Outcome;
        use gix::sec::identity::Account;

        /*
         * protocol・authority・repo pathの全てが一致するときだけ供給する
         */
        let Action::Get(mut context) = action else { return Ok(None); };
        if context.destructure_url_in_place(true).is_err() {
            return Ok(None);
        }
        if context.protocol.as_deref() != Some("https")
            || context.host.as_deref() != Some("github.com")
            || context.username.is_some() || context.password.is_some()
            || context.path.as_ref().map(|v| &v[..])
                != Some(self.scope.as_bytes())
        {
            return Ok(None);
        }
        Ok(Some(Outcome {
            identity: Account {
                username: "x-access-token".to_owned(),
                password: self.token.expose().to_owned(),
                oauth_refresh_token: None,
            },
            next: NextAction::from(context),
        }))
    }
}

///
/// 入力方式と任意のGitHub限定認証を取得全体へ渡す。
///
struct FetchPolicy {
    kind: SourceKind,
    auth: Option<GitAuthentication>,
}

///
/// URLを再検証し、匿名HTTPS取得を実行する。
///
/// # 引数
/// * `url` - HTTPSの入力URL
/// * `selector` - 取得参照方針
/// * `limits` - 設定上限
/// * `stop` - 協調停止境界
///
/// # 戻り値
/// 入力元を再読込しない所有されたsnapshot
///
pub(super) fn acquire(url: &Url, selector: GitSelector,
    limits: AcquisitionLimits, stop: &StopContext)
    -> Result<SourceSnapshot, SourceError>
{
    stop.checkpoint()?;
    validate_url(url)?;
    fetch(url.as_str(), selector, limits, stop,
        FetchPolicy { kind: SourceKind::HttpsGit, auth: None },
        &mut |_, _| Ok(()))
}

///
/// APIで確定したGitHub commitを限定した認証で取得する。
///
/// # 引数
/// * `url` - 要求repoとの一致を検査済みのclone URL
/// * `commit` - APIで実体を確認した完全commit ID
/// * `token` - 環境由来の任意の秘密値
/// * `limits` - 取得上限
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// GitHub種別の所有されたsnapshot
///
pub(super) fn acquire_github(url: &Url, commit: String,
    token: Option<Arc<Secret>>, limits: AcquisitionLimits, stop: &StopContext)
    -> Result<SourceSnapshot, SourceError>
{
    /*
     * 取得直前にもGitHub HTTPSのauthorityを限定する
     */
    validate_url(url)?;
    if url.host_str() != Some("github.com") || url.port().is_some() {
        return Err(SourceError::GitHubIdentity);
    }
    let auth = token.map(|token| GitAuthentication {
        scope: url.path().trim_matches('/').to_owned(), token,
    });
    fetch(url.as_str(), GitSelector::Commit(commit), limits, stop,
        FetchPolicy { kind: SourceKind::GitHub, auth },
        &mut |_, _| Ok(()))
}

///
/// 資格情報・曖昧なauthority・非HTTPS入力を接続前に拒否する。
///
/// # 引数
/// * `url` - 解析済みURL
///
/// # 戻り値
/// 許容するHTTPS入力なら成功
///
fn validate_url(url: &Url) -> Result<(), SourceError> {
    let authority = url.as_str().split("://").nth(1).unwrap_or("")
        .split('/').next().unwrap_or("");
    if url.scheme() != "https" || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || authority.contains('@') || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SourceError::InvalidGit);
    }
    Ok(())
}

///
/// fixtureと製品で同じ取得・停止・回収経路を実行する。
///
/// # 概要
/// 一時領域はworkerが所有し、成功時だけsnapshotへ寿命を移す。
/// file transportを渡せるのはテストからのみである。
///
/// # 引数
/// * `url` - 製品は検証済みHTTPS、試験は専用file URL
/// * `selector` - コミット参照方針
/// * `limits` - 取得上限
/// * `stop` - 停止境界
/// * `policy` - 入力種別と限定した資格情報
/// * `observe` - phase境界のfixture注入。製品は何もしない。
///
/// # 戻り値
/// 全成功時だけ取得領域と本文を所有するsnapshot
///
fn fetch(url: &str, selector: GitSelector, limits: AcquisitionLimits,
    stop: &StopContext, policy: FetchPolicy,
    observe: &mut impl FnMut(Phase, &Path) -> Result<(), SourceError>)
    -> Result<SourceSnapshot, SourceError>
{
    /*
     * 同期取得を始める前に停止とcommit構文を検査する
     */
    stop.checkpoint()?;
    if let GitSelector::Commit(id) = &selector {
        if id.is_empty() || id.len() > 64
            || !id.bytes().all(|v| v.is_ascii_hexdigit())
        {
            return Err(SourceError::RefNotFound);
        }
    }
    let area = crate::measurement::area(crate::measurement::Role::Source)
        .map_err(|_| SourceError::Io)?;
    /*
     * 通信中も容量・期限を監視し、worker終了後にだけ所有権を移す
     */
    let result = watch::run(area.path(), limits.git_bytes, stop, || {
        clone_into(url, selector, limits, stop, area.path(), policy, observe)
    });
    let mut snapshot = result?;
    snapshot.acquisition = Some(SourceLease(area));
    Ok(snapshot)
}

///
/// clone準備、fetch、検査、checkoutを同じ同期作業内で完結する。
///
/// # 概要
/// 全branch/tagを取得して参照を解決する。完全commit IDは直接要求する。
/// HEADの自動設定は入力選択に使わず、選択commitへ明示的に付け替える。
///
/// # 引数
/// * `url` - 取得URL
/// * `selector` - 参照方針
/// * `limits` - 取得上限
/// * `stop` - 停止境界
/// * `area` - worker所有の専用領域
/// * `policy` - 入力種別と限定した資格情報
/// * `observe` - テスト用phase境界
///
/// # 戻り値
/// 選択commitの検査済みentryと未変換本文
///
fn clone_into(url: &str, selector: GitSelector, limits: AcquisitionLimits,
    stop: &StopContext, area: &Path, policy: FetchPolicy,
    observe: &mut impl FnMut(Phase, &Path) -> Result<(), SourceError>)
    -> Result<SourceSnapshot, SourceError>
{
    let kind = policy.kind;
    let auth = policy.auth;
    /*
     * 外部設定・helper・対話認証・redirectに依存しない接続を準備する
     */
    let options = gix::open::Options::isolated()
        .config_overrides([
            "http.sslVerify=true", "http.followRedirects=false",
            "credential.useHttpPath=true",
        ]);
    let mut prepare = gix::clone::PrepareFetch::new(url, area.join("repo"),
        gix::create::Kind::WithWorktree, gix::create::Options::default(),
        options).map_err(|_| SourceError::InvalidGit)?
        .configure_remote(|mut remote| {
            remote.replace_refspecs([
                "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*",
            ], gix::remote::Direction::Fetch).expect("static fetch specs");
            Ok(remote.with_fetch_tags(gix::remote::fetch::Tags::None))
        }).configure_connection(move |connection| {
            let auth = auth.clone();
            connection.set_credentials(move |action| match &auth {
                Some(auth) => auth.credential(action),
                None => Ok(None),
            });
            Ok(())
        });
    /*
     * 完全IDだけはserverへ直接要求し、短縮IDは取得後に実体解決する
     */
    if let GitSelector::Commit(id) = &selector {
        if matches!(id.len(), 40 | 64)
            && id.bytes().all(|v| v.is_ascii_hexdigit())
        {
            prepare = prepare.with_revision(Some(id.as_str()))
                .map_err(|_| SourceError::RefNotFound)?;
        }
    }
    observe(Phase::Prepared, area)?;
    stop.checkpoint()?;
    let (mut checkout, _) = prepare.fetch_then_checkout(
        gix::progress::Discard, stop.cancelled.as_ref(),
    ).map_err(|_error| {
        #[cfg(test)]
        eprintln!("{}", fetch_diagnostic(&_error));
        SourceError::GitFetch
    })?;
    observe(Phase::Fetched, area)?;
    watch::check_size(area, limits.git_bytes, stop)?;

    /*
     * checkout前に選択ツリーを全件検査し、不変本文を全体計量へ含める
     */
    let repo = checkout.repo();
    let id = git::commit(repo, selector.clone())?.id;
    let mut builder = Builder::in_acquisition(area, limits.git_bytes,
        limits.page_bytes, kind)?;
    git::committed(repo, GitSelector::Commit(id.to_string()),
        &mut builder, stop)?;
    observe(Phase::Validated, area)?;
    watch::check_size(area, limits.git_bytes, stop)?;
    repo.reference("HEAD", id, gix::refs::transaction::PreviousValue::Any,
        "select input commit").map_err(|_| SourceError::InvalidGit)?;
    stop.checkpoint()?;

    /*
     * 指定commitだけをcheckoutし、部分失敗・超過では公開しない
     */
    let (repo, outcome) = checkout.main_worktree(
        gix::progress::Discard, stop.cancelled.as_ref(),
    ).map_err(|_| SourceError::GitCheckout)?;
    if !outcome.errors.is_empty() || !outcome.collisions.is_empty()
        || !outcome.delayed_paths_unknown.is_empty()
        || !outcome.delayed_paths_unprocessed.is_empty()
    {
        return Err(SourceError::GitCheckout);
    }
    observe(Phase::CheckedOut, area)?;
    watch::check_size(area, limits.git_bytes, stop)?;
    if repo.head_commit().map_err(|_| SourceError::InvalidGit)?.id != id {
        return Err(SourceError::Changed);
    }
    builder.finish(stop)
}

///
/// 試験時のfetch失敗を生の外部messageを使わない診断へ写す。
///
/// # 概要
/// 型で取得できる分類と固定されたphase名だけを返す。
/// URL・header・資格情報・本文・依存ソースの実パスは出力しない。
///
/// # 引数
/// * `error` - その場で破棄するgix取得失敗
///
/// # 戻り値
/// 秘密情報を含まない試験専用診断
///
#[cfg(test)]
pub(super) fn fetch_diagnostic(error: &gix::Error) -> String {
    /*
     * 表示messageを読まず、具体型から通信とI/Oの分類だけを取得する
     */
    let http = error.downcast_any_ref::<reqwest::Error>();
    let status = http.and_then(|e| e.status()).map(|v| v.as_u16());
    let timeout = http.is_some_and(|e| e.is_timeout());
    let connect = http.is_some_and(|e| e.is_connect());
    let io = error.downcast_any_ref::<std::io::Error>().map(|e| e.kind());
    let phase = error.iter_errors_with_locations().find_map(|source| {
        let file = source.location()?.file();
        if file.contains("/client/blocking_io/http/") {
            Some("http_transport")
        } else if file.contains("/fetch/response/") {
            Some("git_response")
        } else if file.contains("/handshake/") {
            Some("git_handshake")
        } else { None }
    }).unwrap_or("clone_fetch");
    /*
     * 静的ラベルと数値・真偽・enum値だけを出力する
     */
    format!("fetch diagnostic: phase={phase} http={status:?} \
        connect={connect} timeout={timeout} io={io:?} \
        not_found={} validation={} corrupted={} retryable={}",
        error.is_not_found(), error.is_validation(), error.is_corrupted(),
        error.is_retryable())
}

///
/// ローカルfixtureから製品と同じclone処理を試す。
///
/// # 引数
/// * `origin` - テスト所有のGit worktree
/// * `selector` - 参照方針
/// * `limits` - テスト上限
/// * `stop` - 停止境界
/// * `observe` - 注入・寿命観測用のphase境界
///
/// # 戻り値
/// fixtureから生成したsnapshot。製品の入力条件は広げない。
///
#[cfg(test)]
pub(super) fn fixture(origin: &Path, selector: GitSelector,
    limits: AcquisitionLimits, stop: &StopContext,
    observe: &mut impl FnMut(Phase, &Path) -> Result<(), SourceError>)
    -> Result<SourceSnapshot, SourceError>
{
    let url = Url::from_directory_path(origin)
        .map_err(|_| SourceError::InvalidGit)?;
    fetch(url.as_str(), selector, limits, stop,
        FetchPolicy { kind: SourceKind::HttpsGit, auth: None }, observe)
}

///
/// 試験だけでダミートークンの適用先を構成する。
///
/// # 引数
/// * `scope` - GitHub repoの相対clone path
/// * `token` - ダミー資格情報
///
/// # 戻り値
/// 表示経路を持たない限定認証
///
#[cfg(test)]
pub(super) fn authentication_fixture(scope: &str, token: Secret)
    -> GitAuthentication
{
    GitAuthentication { scope: scope.to_owned(), token: Arc::new(token) }
}

///
/// 模擬APIで確定したcommitを試験専用file transportへ渡す。
///
/// # 引数
/// * `origin` - テスト所有のGit repo
/// * `commit` - 模擬APIで確認した完全ID
/// * `limits` - テスト上限
/// * `stop` - 停止境界
/// * `observe` - 取得領域観測
///
/// # 戻り値
/// GitHub種別のsnapshot。製品はfile入力を受け付けない。
///
#[cfg(test)]
pub(super) fn github_fixture(origin: &Path, commit: &str,
    limits: AcquisitionLimits, stop: &StopContext,
    observe: &mut impl FnMut(Phase, &Path) -> Result<(), SourceError>)
    -> Result<SourceSnapshot, SourceError>
{
    let url = Url::from_directory_path(origin)
        .map_err(|_| SourceError::InvalidGit)?;
    fetch(url.as_str(), GitSelector::Commit(commit.to_owned()), limits, stop,
        FetchPolicy { kind: SourceKind::GitHub,
            auth: Some(authentication_fixture("fixture/repo.git",
                Secret::new("dummy-github-secret".to_owned()))) }, observe)
}

///
/// 非通常fileを辿らず、専用領域の論理バイト数を合計する。
///
/// # 概要
/// .git・checkout・本文保管を全て含め、整数overflowも超過として拒否する。
/// 書込と同時のscanは非原子的で、監視側は失敗を次のscanへ持ち越す。
///
/// # 引数
/// * `root` - 専用領域root
/// * `maximum` - 全体サイズ上限（bytes）
/// * `stop` - 停止境界
///
/// # 戻り値
/// 上限内なら合計サイズ（bytes）
///
pub(super) fn size(root: &Path, maximum: u64, stop: &StopContext)
    -> Result<u64, SourceError>
{
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    /*
     * directory単位と各fileで停止を確認し、symlinkは辿らない
     */
    while let Some(path) = pending.pop() {
        /*
         * symlinkを辿らず、directoryを列挙して通常fileだけ計量する
         */
        stop.checkpoint()?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| SourceError::Io)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|_| SourceError::Io)? {
                stop.checkpoint()?;
                pending.push(entry.map_err(|_| SourceError::Io)?.path());
            }
        } else if metadata.is_file() {
            total = total.checked_add(metadata.len())
                .filter(|v| *v <= maximum)
                .ok_or(SourceError::Limit("max_git_bytes"))?;
        } else {
            return Err(SourceError::InvalidPath);
        }
    }
    Ok(total)
}
