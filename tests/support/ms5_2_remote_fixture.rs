/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS52専用sourceと参照条件を独立期待値へ結合する追加実入口
//!

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;

use super::{Case, DATA_LIMIT, Environment, ExpectedSet, Mode, PreparedSource,
    RemoteFixtureSpec, RemoteGrant, SPEC_LIMIT, Selector, SourceId,
    full_commit, git_bytes, github_token, local_complete, offline_command,
    verify_identities};
use super::super::super::real_cli_http::Auth;

const MAIN_REPO: &str = match option_env!("LUWIKI_MS52_FIXTURE_MAIN_REPO") {
    Some(value) => value,
    None => "kwgt/test-repository-ms52-main",
};
const MASTER_REPO: &str = match option_env!("LUWIKI_MS52_FIXTURE_MASTER_REPO") {
    Some(value) => value,
    None => "kwgt/test-repository-ms52-master",
};
const TAG: &str = "ms52-annotated";

///
/// 実入口が選ぶ固定役割。任意refや取得先へ拡張しない。
///
#[derive(Clone, Copy)]
pub enum Role {
    /// mainをdefaultより優先する。
    Main,
    /// main不在時だけmasterを選ぶ。
    Master,
    /// public注釈tagをpeelする。
    Tag,
    /// private資格と注釈tagの取得を結合する。
    PrivateTag,
    /// 広告tipではない到達可能な完全commitを取得する。
    Ancestor,
}

impl Role {
    ///
    /// ログとcase名へ使う固定ラベルを返す。
    ///
    /// # 戻り値
    /// 秘密や実ref値を含まないラベル
    ///
    fn label(self) -> &'static str {
        match self {
            Self::Main => "main", Self::Master => "master",
            Self::Tag => "tag", Self::PrivateTag => "private-tag",
            Self::Ancestor => "ancestor",
        }
    }

    ///
    /// 役割専用の保護指定環境名を返す。
    ///
    /// # 戻り値
    /// 新schemaだけを読む固定環境名
    ///
    fn variable(self) -> &'static str {
        match self {
            Self::Master => "LUWIKI_MS52_MASTER_SPEC",
            Self::PrivateTag => "LUWIKI_MS52_PRIVATE_REF_SPEC",
            _ => "LUWIKI_MS52_MAIN_SPEC",
        }
    }
}

///
/// 追加の参照選択試験を独立ファイルで指定する閉じた入力schema。
///
// 指定fileを型付きで復号し、未知fieldを拒否するために導出する。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    /// この入力形式専用のschema番号。
    schema: u64,
    /// PUB52-A/PUB52-B/PRIV-01の安全ID。
    source_id: String,
    /// 値を診断へ出さない固定repository。
    repository: String,
    /// 完全objectを持つ独立local clone。
    local_repository: PathBuf,
    /// A0または既存private V0の完全commit。
    a0: String,
    /// Aだけが持つ非広告祖先commit。
    a1: Option<String>,
    /// Aだけが持つmain tip。
    a2: Option<String>,
}

impl Spec {
    ///
    /// 固定roleとsource ID/完全commitを照合する。
    ///
    /// # 概要
    /// 通常境界試験でも使える純粋検査でenv/Git/HTTPを読まない。
    ///
    /// # 引数
    /// * `role` - コード内の固定役割
    ///
    /// # 戻り値
    /// 固定source/版/構造の一致
    ///
    fn validate(&self, role: Role) -> Result<()> {
        ensure!(self.schema == 1 && full_commit(&self.a0), "MS52 schema");
        PreparedSource::github(&self.repository, Selector::Default)?;
        /*
         * privateは後段で旧保護指定へ結合しpublicは二repoへ閉じる
         */
        match role {
            Role::PrivateTag => ensure!(self.source_id == "PRIV-01"
                && self.a1.is_none() && self.a2.is_none(), "MS52 private ID"),
            Role::Master => ensure!(self.source_id == "PUB52-B"
                && self.repository == MASTER_REPO && self.a1.is_none()
                && self.a2.is_none(), "MS52 master scope"),
            _ => {
                let a1 = self.a1.as_deref().unwrap_or("");
                let a2 = self.a2.as_deref().unwrap_or("");
                ensure!(self.source_id == "PUB52-A"
                    && self.repository == MAIN_REPO && full_commit(a1)
                    && full_commit(a2) && self.a0 != a1 && a1 != a2
                    && self.a0 != a2, "MS52 main scope");
            }
        }
        Ok(())
    }

    ///
    /// 実入口だけでgateと保護指定を読み取る。
    ///
    /// # 概要
    /// 未準備は失敗とし、private指定は既存PRIV-01へ追加拘束する。
    ///
    /// # 引数
    /// * `role` - 読取許可を決める固定役割
    ///
    /// # 戻り値
    /// 検査済み指定とprivate時だけの非表示資格情報
    ///
    fn load(role: Role) -> Result<(Self, Option<String>)> {
        ensure!(std::env::var("LUWIKI_MS52_REMOTE_APPROVAL").ok().as_deref()
            == Some("MS52-R1"), "MS52 approval gate closed");
        let path = std::env::var_os(role.variable())
            .ok_or_else(|| anyhow!("MS52 specification missing"))?;
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| anyhow!("MS52 specification metadata"))?;
        ensure!(meta.is_file() && meta.permissions().mode() & 0o077 == 0
            && meta.len() <= SPEC_LIMIT as u64, "MS52 specification guard");
        /*
         * schemaの原文やparse失敗の値を診断へ渡さない
         */
        let mut file = File::open(path)
            .map_err(|_| anyhow!("MS52 specification open"))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(SPEC_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("MS52 specification read"))?;
        ensure!(bytes.len() <= SPEC_LIMIT, "MS52 specification capacity");
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow!("MS52 specification UTF8"))?;
        let spec: Self = toml::from_str(text)
            .map_err(|_| anyhow!("MS52 specification schema"))?;
        spec.validate(role)?;
        let credential = if matches!(role, Role::PrivateTag) {
            ensure!(std::env::var("LUWIKI_MS52_PRIVATE_REF_IMPORT")
                .ok().as_deref() == Some("PRIV-01-ANNOTATED"),
                "MS52 private approval gate closed");
            let old = RemoteFixtureSpec::load(SourceId::Private, true)?;
            ensure!(spec.repository == old.repository
                && spec.local_repository == old.local_repository
                && spec.a0 == old.v0, "MS52 private source binding");
            Some(github_token()?)
        } else { None };
        Ok((spec, credential))
    }

    ///
    /// 役割が要求する完全commitとselectorを返す。
    ///
    /// # 引数
    /// * `role` - 固定役割
    ///
    /// # 戻り値
    /// 値を表示しない完全commitと実CLIへ渡す選択指定
    ///
    fn selected(&self, role: Role) -> Result<(&str, Selector)> {
        let (commit, selector) = match role {
            Role::Main => (self.a2.as_deref(), Selector::Default),
            Role::Master => (Some(self.a0.as_str()), Selector::Default),
            Role::Tag | Role::PrivateTag =>
                (Some(self.a0.as_str()), Selector::Tag(TAG.to_owned())),
            Role::Ancestor => {
                let commit = self.a1.as_deref()
                    .ok_or_else(|| anyhow!("MS52 ancestor missing"))?;
                (Some(commit), Selector::Commit(commit.to_owned()))
            }
        };
        Ok((commit.ok_or_else(|| anyhow!("MS52 selected version"))?,
            selector))
    }
}

///
/// symbolic HEADとobject広告を別々に保持する非表示値。
///
#[derive(PartialEq, Eq)]
struct Advertisement {
    /// HEADの参照先。
    head: String,
    /// HEAD/heads/tags/peeledの完全OID。
    refs: BTreeMap<String, String>,
}

impl Advertisement {
    ///
    /// 有限ls-remote出力を重複/未知行を許さず解析する。
    ///
    /// # 概要
    /// 純粋parserなので通常境界試験で実networkを使わない。
    ///
    /// # 引数
    /// * `bytes` - 完全な有限広告出力
    ///
    /// # 戻り値
    /// 原文を診断へ出さない広告構造
    ///
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= DATA_LIMIT, "MS52 advertisement capacity");
        let text = std::str::from_utf8(bytes)
            .map_err(|_| anyhow!("MS52 advertisement UTF8"))?;
        let mut head = None;
        let mut refs = BTreeMap::new();
        /*
         * symref一行と一意な完全OIDだけを有限mapへ写す
         */
        for line in text.lines() {
            /*
             * 行を二fieldへ分け、symbolic HEADと一意な完全OIDを別々に検査する
             */
            let (left, name) = line.split_once('\t')
                .ok_or_else(|| anyhow!("MS52 advertisement framing"))?;
            if let Some(value) = left.strip_prefix("ref: ") {
                ensure!(name == "HEAD" && head.is_none()
                    && matches!(value, "refs/heads/old-default"
                        | "refs/heads/master"), "MS52 symbolic HEAD");
                head = Some(value.to_owned());
            } else {
                ensure!(full_commit(left) && (name == "HEAD"
                    || name.starts_with("refs/heads/")
                    || name.starts_with("refs/tags/"))
                    && refs.insert(name.to_owned(), left.to_owned()).is_none(),
                    "MS52 advertisement record");
            }
        }
        Ok(Self { head: head.ok_or_else(|| anyhow!("MS52 HEAD missing"))?,
            refs })
    }

    ///
    /// A/Bの広告配置と祖先がtipでない条件を照合する。
    ///
    /// # 概要
    /// 未指定refや異なるpeeled先も追加せず失敗にする。
    ///
    /// # 引数
    /// * `spec` - 固定source
    /// * `tag_oid` - Aだけが持つ注釈tag object
    ///
    /// # 戻り値
    /// 承認した広告集合との完全一致
    ///
    fn verify(&self, spec: &Spec, tag_oid: Option<&str>) -> Result<()> {
        let mut expected = BTreeMap::new();
        expected.insert("HEAD".to_owned(), spec.a0.clone());
        /*
         * Bはmasterだけ、Aはmainとold-defaultと注釈tagだけに閉じる
         */
        if spec.source_id == "PUB52-B" {
            expected.insert("refs/heads/master".to_owned(), spec.a0.clone());
            ensure!(self.head == "refs/heads/master", "MS52 master HEAD");
        } else {
            let a1 = spec.a1.as_ref()
                .ok_or_else(|| anyhow!("MS52 ancestor missing"))?;
            let a2 = spec.a2.as_ref()
                .ok_or_else(|| anyhow!("MS52 main missing"))?;
            ensure!(self.head == "refs/heads/old-default"
                && !self.refs.values().any(|value| value == a1),
                "MS52 ancestor advertised or HEAD changed");
            expected.insert("refs/heads/main".to_owned(), a2.clone());
            expected.insert("refs/heads/old-default".to_owned(),
                spec.a0.clone());
            expected.insert(format!("refs/tags/{TAG}"),
                tag_oid.ok_or_else(|| anyhow!("MS52 tag missing"))?.to_owned());
            expected.insert(format!("refs/tags/{TAG}^{{}}"), spec.a0.clone());
        }
        ensure!(self.refs == expected, "MS52 advertisement binding");
        Ok(())
    }
}

///
/// 独立GitHub観測のうち必要な有限fieldだけを受ける構造。
///
// 独立API応答から必要な有限fieldだけを復号するために導出する。
#[derive(Deserialize)]
struct ApiRecord {
    /// repositoryの既定branch。
    default_branch: Option<String>,
    /// ref/tagが示すobject。
    object: Option<ApiObject>,
    /// 完全commit/tagのID。
    sha: Option<String>,
}

///
/// GitHub ref/tag objectの識別。生値は診断へ出さない。
///
// API応答内のobject種別と完全IDを復号するために導出する。
#[derive(Deserialize)]
struct ApiObject {
    /// commitまたはtag。
    #[serde(rename = "type")]
    kind: String,
    /// 完全object ID。
    sha: String,
}

///
/// 製品clientを使わない固定GitHub観測者。
///
struct Api<'a> {
    /// retry/redirectなしの観測専用client。
    client: reqwest::Client,
    /// 固定sourceの非表示repo。
    repository: &'a str,
    /// private時だけ使うRAM資格情報。
    credential: Option<&'a str>,
    /// 一連のAPI観測で再起算しない期限。
    deadline: Instant,
}

impl<'a> Api<'a> {
    ///
    /// 固定sourceへ閉じる観測clientを作る。
    ///
    /// # 引数
    /// * `spec` - 検査済みsource
    /// * `credential` - private時だけの非表示token
    ///
    /// # 戻り値
    /// 製品処理と別期限の独立観測者
    ///
    fn new(spec: &'a Spec, credential: Option<&'a str>) -> Result<Self> {
        let client = reqwest::Client::builder().https_only(true).no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never()).user_agent("ms52-observer")
            .connect_timeout(Duration::from_secs(30)).build()
            .map_err(|_| anyhow!("MS52 API client"))?;
        Ok(Self { client, repository: &spec.repository, credential,
            deadline: Instant::now() + Duration::from_secs(300) })
    }

    ///
    /// 呼出元の固定suffixへ一回だけGETする。
    ///
    /// # 概要
    /// raw error/URL/資格情報をanyhowへ伝播させない。
    ///
    /// # 引数
    /// * `suffix` - コード内で構成したAPI操作
    ///
    /// # 戻り値
    /// 404はNone、200は選択field。ほかは固定失敗。
    ///
    async fn get(&self, suffix: &str) -> Result<Option<ApiRecord>> {
        let remaining = self.deadline.checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("MS52 API deadline"))?;
        let url = format!("https://api.github.com/repos/{}{suffix}",
            self.repository);
        let mut request = self.client.get(url).timeout(remaining);
        if let Some(token) = self.credential {
            request = request.bearer_auth(token);
        }
        /*
         * 想定外statusを再試行せず安全な分類だけで停止する
         */
        let response = request.send().await
            .map_err(|_| anyhow!("MS52 API transport"))?;
        match response.status().as_u16() {
            404 => Ok(None),
            200 => {
                let record = response.json::<ApiRecord>().await
                    .map_err(|_| anyhow!("MS52 API response"))?;
                ensure!(Instant::now() < self.deadline, "MS52 API deadline");
                Ok(Some(record))
            }
            _ => Err(anyhow!("MS52 API status")),
        }
    }

    ///
    /// 固定操作の200を必須とする。
    ///
    /// # 引数
    /// * `suffix` - 固定API操作
    ///
    /// # 戻り値
    /// 必須の選択field構造
    ///
    async fn required(&self, suffix: &str) -> Result<ApiRecord> {
        self.get(suffix).await?
            .ok_or_else(|| anyhow!("MS52 API object missing"))
    }

    ///
    /// 注釈tag objectを一段peelして完全commitへ結合する。
    ///
    /// # 引数
    /// * `commit` - local期待の完全commit
    /// * `tag_oid` - localのtag object ID
    ///
    /// # 戻り値
    /// 軽量tagではないref/object/peel先の一致
    ///
    async fn tag(&self, commit: &str, tag_oid: &str) -> Result<()> {
        let reference = self.required(&format!("/git/ref/tags/{TAG}")).await?;
        let object = reference.object
            .ok_or_else(|| anyhow!("MS52 API ref object"))?;
        ensure!(object.kind == "tag" && object.sha == tag_oid,
            "MS52 API annotated tag");
        let tag = self.required(&format!("/git/tags/{tag_oid}")).await?;
        let target = tag.object.ok_or_else(|| anyhow!("MS52 API tag target"))?;
        ensure!(tag.sha.as_deref() == Some(tag_oid)
            && target.kind == "commit" && target.sha == commit,
            "MS52 API peeled commit");
        Ok(())
    }
}

///
/// 完全OIDだけをlocal plumbingから取得する。
///
/// # 引数
/// * `spec` - 独立local指定
/// * `object` - 固定refまたは検査済み完全commit式
///
/// # 戻り値
/// 診断へ出さない完全OID
///
fn local_oid(spec: &Spec, object: &str) -> Result<String> {
    let bytes = git_bytes(&spec.local_repository,
        &["rev-parse", "--verify", object])?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("MS52 local OID UTF8"))?.trim();
    ensure!(full_commit(text), "MS52 local OID");
    Ok(text.to_owned())
}

///
/// public fixtureの全file集合と固定ASCII本文を独立に確認する。
///
/// # 概要
/// 期待値の取り違えやmainとdefaultの同内容化を拒否する。
///
/// # 引数
/// * `spec` - 固定public source
/// * `commit` - 対象完全commit
/// * `version` - A0～A2の有限版番号
/// * `expected` - 完全treeから作った期待所有者
///
/// # 戻り値
/// 二通常fileと全期待bytesの一致
///
fn public_body(spec: &Spec, commit: &str, version: usize,
    expected: &ExpectedSet) -> Result<()>
{
    ensure!(git_bytes(&spec.local_repository,
        &["ls-tree", "-r", "--name-only", commit])?
        == b"root/alpha.md\nroot/beta.md\n", "MS52 public tree");
    let pages = expected.views();
    ensure!(pages.len() == 2, "MS52 public page count");
    /*
     * 二本文とも既知の小さいASCIIだけに限定する
     */
    for (page, name) in pages.iter().zip(["alpha", "beta"]) {
        ensure!(page.name == format!("root/{name}.md"), "MS52 public name");
        let mut body = Vec::new();
        page.open()?.take(65).read_to_end(&mut body)
            .map_err(|_| anyhow!("MS52 expected body read"))?;
        ensure!(body == format!("ms52-a{version}-{name}\n").as_bytes(),
            "MS52 public bytes");
    }
    Ok(())
}

///
/// 対象版の全期待を作り履歴・tag型もlocalで確認する。
///
/// # 概要
/// 全準備はWiki HTTPより前。private値をログへ出さない。
///
/// # 引数
/// * `spec` - 検査済みsource
/// * `role` - 選択役割
///
/// # 戻り値
/// 選択期待とA/private時だけの注釈tag object ID
///
fn prepare(spec: &Spec, role: Role) -> Result<(ExpectedSet, Option<String>)> {
    local_complete(&spec.local_repository)?;
    let mut total = 0;
    let (selected, _) = spec.selected(role)?;
    let expected = ExpectedSet::prepare(&spec.local_repository, selected,
        &mut total)?;
    ensure!(expected.pages.len() == 2, "MS52 fixture page count");
    /*
     * public全版のbytesと直列履歴を独立localで拘束する
     */
    if !matches!(role, Role::PrivateTag) {
        /*
         * 公開各版の本文を固定fixtureと照合する
         */
        for (version, commit) in [Some(spec.a0.as_str()),
            spec.a1.as_deref(), spec.a2.as_deref()].into_iter().enumerate()
        {
            if let Some(commit) = commit {
                let set = ExpectedSet::prepare(&spec.local_repository,
                    commit, &mut total)?;
                public_body(spec, commit, version, &set)?;
            }
        }
        /*
         * 各commitの親と祖先関係を独立Gitで照合する
         */
        if let (Some(a1), Some(a2)) = (&spec.a1, &spec.a2) {
            ensure!(local_oid(spec, &format!("{a1}^"))? == spec.a0
                && local_oid(spec, &format!("{a2}^"))? == *a1,
                "MS52 linear history");
            let command = offline_command(&spec.local_repository,
                &["merge-base", "--is-ancestor", a1, a2]);
            let output = super::bounded_git(command)?;
            ensure!(output.is_empty(), "MS52 ancestor output");
        }
    }
    /*
     * B以外は注釈tag型とA0/V0への一段peelを要求する
     */
    let tag = if matches!(role, Role::Master) { None } else {
        let reference = format!("refs/tags/{TAG}");
        let oid = local_oid(spec, &reference)?;
        ensure!(git_bytes(&spec.local_repository,
            &["cat-file", "-t", &oid])? == b"tag\n"
            && local_oid(spec, &format!("{reference}^{{}}"))? == spec.a0,
            "MS52 local annotated tag");
        Some(oid)
    };
    Ok((expected, tag))
}

///
/// publicのsymbolic HEADと広告refを独立networkで観測する。
///
/// # 引数
/// * `spec` - 固定public source
///
/// # 戻り値
/// 有限の広告構造。privateには使用しない。
///
fn advertise(spec: &Spec) -> Result<Advertisement> {
    ensure!(matches!(spec.repository.as_str(), MAIN_REPO | MASTER_REPO),
        "MS52 public advertisement scope");
    let area = tempfile::tempdir()
        .map_err(|_| anyhow!("MS52 advertisement area"))?;
    let url = format!("https://github.com/{}.git", spec.repository);
    let mut command = offline_command(area.path(), &[]);
    command.args(["-c", "protocol.https.allow=always", "-c",
        "http.followRedirects=false", "ls-remote", "--symref", &url,
        "HEAD", "refs/heads/*", "refs/tags/*"])
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_CEILING_DIRECTORIES", area.path());
    Advertisement::parse(&super::bounded_git(command)?)
}

///
/// GitHub経路のdefault/ref/tag/完全commitを別clientで確認する。
///
/// # 概要
/// productのref解決コードを使わず選択条件を独立に照合する。
///
/// # 引数
/// * `spec` - 固定source
/// * `role` - 選択役割
/// * `token` - private時だけの資格情報
/// * `tag` - 注釈tag object ID
///
/// # 戻り値
/// APIの独立観測と固定local期待の一致
///
async fn github(spec: &Spec, role: Role, token: Option<&str>,
    tag: Option<&str>) -> Result<()>
{
    let api = Api::new(spec, token)?;
    let (commit, _) = spec.selected(role)?;
    /*
     * public defaultは製品のmain/master方針とは別に確認する
     */
    if !matches!(role, Role::PrivateTag) {
        let repository = api.required("").await?;
        let default = if matches!(role, Role::Master) { "master" }
            else { "old-default" };
        ensure!(repository.default_branch.as_deref() == Some(default),
            "MS52 API default branch");
    }
    match role {
        Role::Tag | Role::PrivateTag => api.tag(commit,
            tag.ok_or_else(|| anyhow!("MS52 tag missing"))?).await?,
        Role::Ancestor => {
            let record = api.required(&format!("/commits/{commit}")).await?;
            ensure!(record.sha.as_deref() == Some(commit),
                "MS52 API complete commit");
        }
        Role::Main | Role::Master => {
            /*
             * main404だけをmaster fallbackの前提として受ける
             */
            let main = api.get("/git/ref/heads/main").await?;
            let record = if matches!(role, Role::Master) {
                ensure!(main.is_none(), "MS52 API main unexpectedly exists");
                api.required("/git/ref/heads/master").await?
            } else { main.ok_or_else(|| anyhow!("MS52 API main missing"))? };
            let object = record.object
                .ok_or_else(|| anyhow!("MS52 API branch object"))?;
            ensure!(object.kind == "commit" && object.sha == commit,
                "MS52 API branch binding");
        }
    }
    Ok(())
}

///
/// 一つの固定役割を一製品CLIと独立状態へ結合する。
///
/// # 概要
/// 事前照合完了後だけWikiへ進み、実行後もref配置を照合する。
///
/// # 引数
/// * `use_github` - GitHub API経路か匿名HTTPS Gitか
/// * `role` - コード内の固定役割
///
/// # 戻り値
/// 全期待bytes/Report/独立状態と前後source条件の一致
///
pub async fn run(use_github: bool, role: Role) -> Result<()> {
    ensure!(use_github || !matches!(role, Role::PrivateTag),
        "MS52 anonymous private prohibited");
    let (spec, token) = Spec::load(role)?;
    let (expected, tag) = prepare(&spec, role)?;
    let before = if matches!(role, Role::PrivateTag) { None }
        else { Some(advertise(&spec)?) };
    if let Some(advertisement) = &before {
        advertisement.verify(&spec, tag.as_deref())?;
    }
    if use_github { github(&spec, role, token.as_deref(),
        tag.as_deref()).await?; }
    /*
     * 固定commitの期待所有者と選択指定を一つの取得許可へ封印する
     */
    let (_, selector) = spec.selected(role)?;
    let source = if use_github {
        PreparedSource::github(&spec.repository, selector)?
    } else { PreparedSource::https(&format!("https://github.com/{}.git",
        spec.repository), selector)? };
    let source_id = match role {
        Role::Master => "PUB52-B", Role::PrivateTag => "PRIV-01",
        _ => "PUB52-A",
    };
    let grant = RemoteGrant { source, credential: token.clone(), source_id,
        version: role.label(), expected: expected.pages.iter().map(|page|
            (page.name.clone(), page.path.clone(), page.size)).collect() };
    let environment = Environment::load_ms51()?;
    let auth = if use_github { Auth::Bearer } else { Auth::Basic };
    let case = Case::new(&format!("ms52-{}", role.label()))?;
    let mut observer = case.observer(&environment, auth)?;
    let pages = expected.views();
    case.absent_expected(&mut observer, &pages).await?;
    let result = case.run_remote(&environment, auth, Mode::Error, &grant,
        &pages, None)?;
    result.expect_ms51(0)?;
    result.expect_counts([pages.len() as u64, 0, 0, 0, 0, 0])?;
    /*
     * 別主体/別期限の独立観測を製品の完了事実へ混入しない
     */
    let states = case.observe_expected(&mut observer, &pages).await?;
    verify_identities(&case, &result, &pages, &states, "Created", "draft_id")?;
    if let Some(before) = before {
        let after = advertise(&spec)?;
        after.verify(&spec, tag.as_deref())?;
        ensure!(before == after, "MS52 remote refs changed");
    }
    if use_github { github(&spec, role, token.as_deref(),
        tag.as_deref()).await?; }
    eprintln!("MS52 role={} github={use_github} source_bound=true \
        independent_state=true", role.label());
    Ok(())
}

///
/// 合成spec/広告の拒否境界を実env/Git/HTTPなしで検査する。
///
/// # 概要
/// 旧specや実資格情報を一切読まず純粋parser/validatorだけを呼ぶ。
///
/// # 戻り値
/// 未知キー/誤source/軽量tag/祖先広告/重複の拒否
///
pub fn verify_boundary() -> Result<()> {
    ensure!(MAIN_REPO != MASTER_REPO, "MS52 public roles must differ");
    let a0 = "0".repeat(40);
    let a1 = "1".repeat(40);
    let a2 = "2".repeat(40);
    let tag = "3".repeat(40);
    let mut spec = Spec { schema: 1, source_id: "PUB52-A".to_owned(),
        repository: MAIN_REPO.to_owned(), local_repository: PathBuf::new(),
        a0: a0.clone(), a1: Some(a1.clone()), a2: Some(a2.clone()) };
    spec.validate(Role::Main)?;
    /*
     * 正常広告から個々の不正条件を独立して作る
     */
    let text = format!("ref: refs/heads/old-default\tHEAD\n\
        {a0}\tHEAD\n{a0}\trefs/heads/old-default\n\
        {a2}\trefs/heads/main\n{tag}\trefs/tags/{TAG}\n\
        {a0}\trefs/tags/{TAG}^{{}}\n");
    let advertisement = Advertisement::parse(text.as_bytes())?;
    advertisement.verify(&spec, Some(&tag))?;
    let extra = format!("{text}{a1}\trefs/heads/extra\n");
    ensure!(Advertisement::parse(extra.as_bytes())?.verify(&spec,
        Some(&tag)).is_err(), "MS52 advertised ancestor accepted");
    let duplicate = format!("{text}{a0}\tHEAD\n");
    ensure!(Advertisement::parse(duplicate.as_bytes()).is_err(),
        "MS52 duplicate accepted");
    let lightweight = text.replace(&format!("{tag}\trefs/tags/{TAG}\n"), "");
    ensure!(Advertisement::parse(lightweight.as_bytes())?.verify(&spec,
        Some(&tag)).is_err(), "MS52 incomplete tag accepted");
    /*
     * 未知schemaと任意repoを旧許可から受け入れない
     */
    spec.repository = MASTER_REPO.to_owned();
    ensure!(spec.validate(Role::Main).is_err(), "MS52 source escaped");
    let valid = format!("schema=1\nsource_id='PUB52-B'\n\
        repository='{MASTER_REPO}'\nlocal_repository='/synthetic'\n\
        a0='{a0}'\n");
    toml::from_str::<Spec>(&valid)
        .map_err(|_| anyhow!("MS52 synthetic schema rejected"))?
        .validate(Role::Master)?;
    ensure!(toml::from_str::<Spec>(&format!("{valid}unknown=true\n")).is_err(),
        "MS52 unknown spec accepted");
    let mut master = Spec { schema: 1, source_id: "PUB52-B".to_owned(),
        repository: MASTER_REPO.to_owned(), local_repository: PathBuf::new(),
        a0, a1: None, a2: None };
    master.validate(Role::Master)?;
    let master_text = format!("ref: refs/heads/master\tHEAD\n\
        {}\tHEAD\n{}\trefs/heads/master\n", master.a0, master.a0);
    Advertisement::parse(master_text.as_bytes())?.verify(&master, None)?;
    let with_main = format!("{master_text}{a2}\trefs/heads/main\n");
    ensure!(Advertisement::parse(with_main.as_bytes())?
        .verify(&master, None).is_err(), "MS52 main accepted in fallback");
    master.a1 = Some(a1);
    ensure!(master.validate(Role::Master).is_err(), "MS52 extra version");
    Ok(())
}
