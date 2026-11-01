/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 製品取得を再利用しないoffline Git期待と限定remote実行
//!

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use tempfile::TempDir;

use super::{Case, ExpectedBody, ExpectedPage, Mode, PreparedSource, RemoteGrant,
    Selector, SourceKind, State, alias, capture, metadata_record,
    metadata_started};
use super::super::real_cli_http::{Auth, Environment, Lookup, Observer,
    RejectedCredential, isolate_child};
use super::super::real_cli_report::Captured;
use super::super::source_fixture::safe_entry;

#[path = "ms5_2_remote_fixture.rs"]
pub mod additional;

/// 保護指定は本文を含めず64KiBへ限定する。
pub const SPEC_LIMIT: usize = 65_536;
/// 期待tree/各blobの保持量。製品上限の検証ではない。
pub const DATA_LIMIT: usize = 1_048_576;
/// 二版の期待store合計。実用入力の測定とは別のtiny fixture条件。
pub const STORE_LIMIT: u64 = 8_388_608;

///
/// 許可済みの二つのsource安全ID。
///
#[derive(Clone, Copy)]
pub enum SourceId {
    /// 固定public repo。
    Public,
    /// 保護指定にだけ実名を保存するprivate repo。
    Private,
}

impl SourceId {
    ///
    /// 記録可能な安全ラベルを返す。
    ///
    /// # 戻り値
    /// 元repositoryを含まない固定ラベル
    ///
    pub fn label(self) -> &'static str {
        match self { Self::Public => "PUB-01", Self::Private => "PRIV-01" }
    }
}

///
/// tokenを含まない保護source指定。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFixtureSpec {
    /// 固定schema番号。
    schema: u64,
    /// PUB-01またはPRIV-01。
    source_id: String,
    /// private実名は保護指定内だけに保持する。
    repository: String,
    /// 完全objectが存在する独立local clone。
    local_repository: PathBuf,
    /// 初回版の完全commit。
    v0: String,
    /// 相違版の完全commit。
    v1: String,
    /// 追加refの役割名に対応する指定。
    #[serde(default)]
    refs: BTreeMap<String, RefSpec>,
}

impl RemoteFixtureSpec {
    ///
    /// 実入口だけで保護指定を読み、入力値を診断へ出さず検査する。
    ///
    /// # 概要
    /// private本文の読取/投入は別gateが有効なときだけ許す。
    ///
    /// # 引数
    /// * `id` - コード内で選ぶ安全ID
    /// * `bodies` - private期待本文も読むか
    ///
    /// # 戻り値
    /// 検査済み指定。未準備はpassへ写さない。
    ///
    pub fn load(id: SourceId, bodies: bool) -> Result<Self> {
        if matches!(id, SourceId::Private) && bodies {
            ensure!(std::env::var("LUWIKI_MS51_PRIVATE_IMPORT").ok().as_deref()
                == Some("PRIV-01"), "private import approval gate closed");
        }
        let name = match id {
            SourceId::Public => "LUWIKI_MS51_PUBLIC_SPEC",
            SourceId::Private => "LUWIKI_MS51_PRIVATE_SPEC",
        };
        let path = std::env::var_os(name)
            .ok_or_else(|| anyhow!("remote fixture specification missing"))?;
        /*
         * symlink/広い権限を拒否し、読み込む指定の量を有限化する
         */
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| anyhow!("remote specification metadata"))?;
        ensure!(meta.is_file() && meta.permissions().mode() & 0o077 == 0
            && meta.len() <= SPEC_LIMIT as u64, "remote specification guard");
        let mut file = File::open(path)
            .map_err(|_| anyhow!("remote spec open"))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(SPEC_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("remote spec read"))?;
        ensure!(bytes.len() <= SPEC_LIMIT, "remote specification size");
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow!("remote specification UTF8"))?;
        let spec: Self = toml::from_str(text)
            .map_err(|_| anyhow!("remote specification schema"))?;
        spec.validate(id)?;
        Ok(spec)
    }

    ///
    /// source同一性と完全commit構文を確認する。
    ///
    /// # 概要
    /// 追加refがある場合は未使用の役割もclosed schemaで検査する。
    ///
    /// # 引数
    /// * `id` - 入口の安全ID
    ///
    /// # 戻り値
    /// 固定sourceに一致する指定
    ///
    pub fn validate(&self, id: SourceId) -> Result<()> {
        /*
         * 固定sourceと完全二版commitを検査する
         */
        ensure!(self.schema == 1 && self.source_id == id.label(), "source ID");
        PreparedSource::github(&self.repository, Selector::Default)?;
        if matches!(id, SourceId::Public) {
            ensure!(self.repository == "kwgt/test-repository", "public source");
        }
        ensure!(full_commit(&self.v0) && full_commit(&self.v1),
            "version commit");
        /*
         * 記載された役割のselector/version/valueをすべて検査する
         */
        for (role, item) in &self.refs {
            ensure!(matches!(item.version.as_str(), "v0" | "v1"),
                "reference version");
            let selector = match (role.as_str(), item.selector.as_str()) {
                ("main" | "master", "default") => {
                    ensure!(item.value.is_none(), "default reference value");
                    Selector::Default
                },
                ("branch", "branch") => Selector::Branch(item.value.clone()
                    .ok_or_else(|| anyhow!("branch reference value"))?),
                ("tag", "tag") => Selector::Tag(item.value.clone()
                    .ok_or_else(|| anyhow!("tag reference value"))?),
                ("commit", "commit") => {
                    ensure!(item.value.is_none(), "commit reference value");
                    Selector::Commit(self.v0.clone())
                },
                _ => return Err(anyhow!("reference role/selector")),
            };
            selector.validate()?;
        }
        Ok(())
    }

    ///
    /// 本文を読まずprivate匿名負例の許可だけを作る。
    ///
    /// # 戻り値
    /// Wiki sink専用の匿名取得指定
    ///
    pub fn anonymous(&self) -> Result<RemoteGrant> {
        ensure!(self.source_id == "PRIV-01", "anonymous private source scope");
        Ok(RemoteGrant { source: PreparedSource::github(&self.repository,
            Selector::Commit(self.v0.clone()))?, credential: None,
            source_id: "PRIV-01", version: "V0", expected: Vec::new() })
    }
}

///
/// 追加refの内部役割と期待版。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefSpec {
    /// default/branch/tag/commitのいずれか。
    selector: String,
    /// branch/tagだけが持つ値。
    value: Option<String>,
    /// v0またはv1。
    version: String,
}

///
/// 全treeから抽出した一つの通常Markdown blob。
///
struct Blob {
    /// 元名。外部記録へ出さない。
    name: String,
    /// commit treeが参照する完全blob ID。
    oid: String,
}

///
/// 保護storeの一ページ。本文は全量常駐しない。
///
struct StoredPage {
    /// 内部の入力対応名。
    name: String,
    /// private名を含まない連番file。
    path: PathBuf,
    /// 封印時の実bytes。
    size: u64,
}

///
/// 完全commitに結合した一版の期待本文所有者。
///
pub struct ExpectedSet {
    /// 連番fileを保持する0700領域。
    area: TempDir,
    /// 計画順に並ぶ全Markdown。
    pages: Vec<StoredPage>,
}

impl ExpectedSet {
    ///
    /// 保護fileの寿命を借用する全期待viewを返す。
    ///
    /// # 戻り値
    /// 元本文を複製しないview集合
    ///
    pub fn views(&self) -> Vec<ExpectedPage<'_>> {
        let _owner = self.area.path();
        self.pages.iter().map(|page| ExpectedPage { name: &page.name,
            body: ExpectedBody::File { path: &page.path, size: page.size } })
            .collect()
    }

    ///
    /// offline全tree/固定blobを検査して期待fileへ封印する。
    ///
    /// # 概要
    /// 一ページだけ有限保持し、全本文をVec集合に保持しない。
    ///
    /// # 引数
    /// * `root` - 完全な独立local Git
    /// * `commit` - 検査済み完全commit
    /// * `total` - 二版合計の期待store使用量
    ///
    /// # 戻り値
    /// 子/観測完了まで生存させる保護期待
    ///
    fn prepare(root: &Path, commit: &str, total: &mut u64) -> Result<Self> {
        ensure!(git_bytes(root, &["cat-file", "-t", commit])? == b"commit\n",
            "expected commit object");
        let tree = git_bytes(root,
            &["ls-tree", "-r", "-t", "-z", "--full-tree", commit])?;
        let blobs = parse_tree(&tree)?;
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir()
            .map_err(|_| anyhow!("expected area"))?;
        let mut pages = Vec::new();
        /*
         * 各blobを独立読取し、UTF8/量/EOF/子statusを確定して保存する
         */
        for (index, blob) in blobs.into_iter().enumerate() {
            let size = git_bytes(root, &["cat-file", "-s", &blob.oid])?;
            let declared = std::str::from_utf8(&size).ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .ok_or_else(|| anyhow!("expected blob size"))?;
            ensure!(declared <= DATA_LIMIT as u64, "expected page capacity");
            let bytes = git_bytes(root, &["cat-file", "blob", &blob.oid])?;
            ensure!(bytes.len() as u64 == declared, "expected blob EOF");
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow!("expected body UTF8"))?;
            ensure!(!text.contains('\u{fffd}'),
                "expected replacement character");
            *total = total.checked_add(declared)
                .ok_or_else(|| anyhow!("expected store overflow"))?;
            ensure!(*total <= STORE_LIMIT, "expected store capacity");
            /*
             * 元名をfile名へ使わず、書込完了後にreadonlyへ封印する
             */
            let path = area.path().join(format!("body-{index}"));
            let mut file = OpenOptions::new().write(true).create_new(true)
                .mode(0o600).open(&path).map_err(|_| anyhow!("expected file"))?;
            file.write_all(&bytes)
                .map_err(|_| anyhow!("expected store write"))?;
            file.set_permissions(fs::Permissions::from_mode(0o400))
                .map_err(|_| anyhow!("expected seal"))?;
            pages.push(StoredPage { name: blob.name, path, size: declared });
        }
        Ok(Self { area, pages })
    }
}

///
/// 二版と許可repoを封印したremote fixture。
///
pub struct RemoteFixture {
    /// 外部へ出さない検査済み指定。
    spec: RemoteFixtureSpec,
    /// 固定安全ID。
    id: SourceId,
    /// private load時だけ一度読んだ非表示token。
    credential: Option<String>,
    /// 初回版の期待所有者。
    pub v0: ExpectedSet,
    /// 相違版の期待所有者。
    pub v1: ExpectedSet,
    /// 同位置の本文が相違するか。独立file比較から作る。
    pub different: Vec<bool>,
}

impl RemoteFixture {
    ///
    /// 実入口だけで二版をoffline準備する。
    ///
    /// # 概要
    /// private gate/全tree/二版集合と相違をWiki HTTP前に確認する。
    ///
    /// # 引数
    /// * `id` - 入口が選んだ固定安全ID
    ///
    /// # 戻り値
    /// 所有二版。未準備を成功へ補完しない。
    ///
    pub fn load(id: SourceId) -> Result<Self> {
        let spec = RemoteFixtureSpec::load(id, true)?;
        local_complete(&spec.local_repository)?;
        let mut total = 0;
        let v0 = ExpectedSet::prepare(&spec.local_repository, &spec.v0,
            &mut total)?;
        let v1 = ExpectedSet::prepare(&spec.local_repository, &spec.v1,
            &mut total)?;
        /*
         * 全投影集合を一致させ、少なくとも一本文の相違を要求する
         */
        let left = v0.views();
        let right = v1.views();
        ensure!(!left.is_empty() && left.len() == right.len(), "version set");
        let mut different = Vec::new();
        for (a, b) in left.iter().zip(&right) {
            ensure!(a.name == b.name, "version projected set differs");
            different.push(!same_body(a, b)?);
        }
        ensure!(different.iter().any(|v| *v),
            "versions have no body difference");
        let credential = if matches!(id, SourceId::Private) {
            Some(github_token()?)
        } else { None };
        Ok(Self { spec, id, credential, v0, v1, different })
    }

    ///
    /// 固定版とsource方式に閉じた許可を作る。
    ///
    /// # 概要
    /// 全期待の名前/所有file/sizeを取得版へ封印する。
    ///
    /// # 引数
    /// * `github` - GitHub指定か匿名HTTPSか
    /// * `changed` - V1かV0か
    ///
    /// # 戻り値
    /// 該当private GitHub子だけがtokenを所有する許可
    ///
    pub fn grant(&self, github: bool, changed: bool) -> Result<RemoteGrant> {
        /*
         * 固定repo/版と資格情報を一つのsourceへ結合する
         */
        ensure!(github || matches!(self.id, SourceId::Public),
            "anonymous HTTPS private source prohibited");
        let commit = if changed { &self.spec.v1 } else { &self.spec.v0 };
        let selector = Selector::Commit(commit.clone());
        let source = if github {
            PreparedSource::github(&self.spec.repository, selector)?
        } else {
            PreparedSource::https(&format!("https://github.com/{}.git",
                self.spec.repository), selector)?
        };
        let credential = self.credential.clone();
        let pages = if changed { &self.v1.pages } else { &self.v0.pages };
        let expected = pages.iter().map(|page|
            (page.name.clone(), page.path.clone(), page.size)).collect();
        Ok(RemoteGrant { source, credential, source_id: self.id.label(),
            version: if changed { "V1" } else { "V0" }, expected })
    }

    ///
    /// 一つの明示役割を指定refと期待版へ結合する。
    ///
    /// # 概要
    /// 未設定の役割はHTTP前に拒否し、既定参照をcommitへ置換しない。
    ///
    /// # 引数
    /// * `github` - 取得方式
    /// * `role` - main/master/branch/tag/commitの固定役割
    ///
    /// # 戻り値
    /// 実指定するrefと期待版の許可
    ///
    pub async fn reference(&self, github: bool, role: &str)
        -> Result<(RemoteGrant, &ExpectedSet)>
    {
        /*
         * 役割と指定selectorを検証し、local完全commitへ結合する
         */
        ensure!(matches!(role, "main" | "master" | "branch" | "tag" | "commit"),
            "reference role");
        let item = self.spec.refs.get(role)
            .ok_or_else(|| anyhow!("reference role not prepared"))?;
        ensure!(matches!(item.version.as_str(), "v0" | "v1"), "ref version");
        let changed = item.version == "v1";
        let commit = if changed { &self.spec.v1 } else { &self.spec.v0 };
        let selector = match (role, item.selector.as_str()) {
            ("main" | "master", "default") => {
                ensure!(item.value.is_none(), "default reference value");
                Selector::Default
            },
            ("branch", "branch") => Selector::Branch(item.value.clone()
                .ok_or_else(|| anyhow!("branch not prepared"))?),
            ("tag", "tag") => Selector::Tag(item.value.clone()
                .ok_or_else(|| anyhow!("tag not prepared"))?),
            ("commit", "commit") => Selector::Commit(commit.clone()),
            _ => return Err(anyhow!("reference selector role mismatch")),
        };
        selector.validate()?;
        let resolved = local_reference(&self.spec.local_repository,
            &selector, role)?;
        ensure!(resolved.eq_ignore_ascii_case(commit),
            "local reference version mismatch");
        /*
         * Wiki変更より前に実remote参照も独立照合する
         */
        verify_remote_reference(&self.spec.repository, github, &selector,
            role, commit).await?;
        let mut grant = self.grant(github, changed)?;
        grant.source = if github {
            PreparedSource::github(&self.spec.repository, selector)?
        } else {
            PreparedSource::https(&format!("https://github.com/{}.git",
                self.spec.repository), selector)?
        };
        Ok((grant, if changed { &self.v1 } else { &self.v0 }))
    }
}

///
/// 完全commit/blobのhex構文を確認する。
///
/// # 引数
/// * `value` - 表示しないobject ID
///
/// # 戻り値
/// SHA1完全IDならtrue
///
pub fn full_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|v| v.is_ascii_hexdigit())
}

///
/// localの明示refを完全commitへ解決する。
///
/// # 概要
/// mainが存在する場合はmasterへfallbackせず、役割との一致も要求する。
///
/// # 引数
/// * `root` - 独立local repository
/// * `selector` - 検査済み参照
/// * `role` - defaultの期待役割
///
/// # 戻り値
/// tagをpeelした完全commit
///
fn local_reference(root: &Path, selector: &Selector, role: &str)
    -> Result<String>
{
    /*
     * mainの存在は全head一覧から確認し、欠落以外をfallbackにしない
     */
    let name = match selector {
        Selector::Default => {
            let refs = git_bytes(root, &["for-each-ref", "--format=%(refname)",
                "refs/heads/main", "refs/heads/master"])?;
            let refs = std::str::from_utf8(&refs)
                .map_err(|_| anyhow!("expected branch UTF8"))?;
            let main = refs.lines().any(|line| line == "refs/heads/main");
            let selected = if main { "main" } else { "master" };
            ensure!(selected == role, "default reference role unavailable");
            format!("refs/heads/{selected}")
        },
        Selector::Branch(value) => format!("refs/heads/{value}"),
        Selector::Tag(value) => format!("refs/tags/{value}"),
        Selector::Commit(value) => value.clone(),
        Selector::Worktree => return Err(anyhow!("remote worktree rejected")),
    };
    let expression = format!("{name}^{{commit}}");
    let output = git_bytes(root, &["rev-parse", "--verify", &expression])?;
    let value = std::str::from_utf8(&output)
        .map_err(|_| anyhow!("expected reference UTF8"))?.trim();
    ensure!(full_commit(value), "expected reference commit");
    Ok(value.to_owned())
}

///
/// GitHub上の参照を独立HTTPS plumbingで事前照合する。
///
/// # 概要
/// GitHub指定も同一public repoを匿名HTTPSで確認する。
/// 照合後のref移動は防げず、CLI後にも全文照合する。
///
/// # 引数
/// * `repository` - 固定public repo
/// * `github` - 製品の取得方式
/// * `selector` - 製品へ渡す検査済み参照
/// * `role` - defaultのmain/master役割
/// * `commit` - localで確定した完全commit
///
/// # 戻り値
/// 観測したremote refと期待commitの一致
///
async fn verify_remote_reference(repository: &str, github: bool,
    selector: &Selector, role: &str, commit: &str) -> Result<()>
{
    ensure!(repository == "kwgt/test-repository", "public reference scope");
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700)).tempdir()
        .map_err(|_| anyhow!("reference observation area"))?;
    let url = format!("https://github.com/{repository}.git");
    let mut command = offline_command(area.path(), &[]);
    /*
     * 空cwdと隔離configを使い、HTTPSだけを読取専用で許可する
     */
    command.args(["-c", "protocol.https.allow=always", "-c",
        "http.followRedirects=false", "ls-remote", &url,
        "refs/heads/*", "refs/tags/*"])
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_CEILING_DIRECTORIES", area.path());
    let bytes = bounded_git(command)?;
    let refs = parse_remote_refs(&bytes)?;
    let name = match selector {
        Selector::Default => {
            let selected = if refs.contains_key("refs/heads/main") {
                "main"
            } else { "master" };
            ensure!(selected == role, "remote default role mismatch");
            format!("refs/heads/{selected}")
        },
        Selector::Branch(value) => format!("refs/heads/{value}"),
        Selector::Tag(value) => {
            let peeled = format!("refs/tags/{value}^{{}}");
            if refs.contains_key(&peeled) { peeled }
            else { format!("refs/tags/{value}") }
        },
        Selector::Commit(value) => {
            ensure!(refs.values().any(|oid| oid.eq_ignore_ascii_case(value)),
                "remote commit not advertised");
            value.clone()
        },
        Selector::Worktree => return Err(anyhow!("remote worktree")),
    };
    let actual = if matches!(selector, Selector::Commit(_)) { &name }
        else { refs.get(&name)
            .ok_or_else(|| anyhow!("remote reference missing"))? };
    ensure!(actual.eq_ignore_ascii_case(commit), "remote reference mismatch");
    eprintln!("MS51 source=PUB-01 role={role} \
        remote_reference_bound=true github={github}");
    Ok(())
}

///
/// 有限ls-remote出力を表示せず完全OIDのmapへ変換する。
///
/// # 引数
/// * `bytes` - 有限stdout
///
/// # 戻り値
/// 重複/不正記録を含まない参照一覧
///
fn parse_remote_refs(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow!("remote reference UTF8"))?;
    let mut refs = BTreeMap::new();
    /*
     * 完全OIDと参照名の一意な組だけを保持する
     */
    for line in text.lines() {
        let (oid, name) = line.split_once('\t')
            .ok_or_else(|| anyhow!("remote reference framing"))?;
        ensure!(full_commit(oid) && name.starts_with("refs/")
            && refs.insert(name.to_owned(), oid.to_owned()).is_none(),
            "remote reference record");
    }
    Ok(refs)
}

///
/// 全treeの生名/modeを検査してMarkdown blobを返す。
///
/// # 概要
/// directoryと非mdも検査し、非md本文にUTF8制約は加えない。
///
/// # 引数
/// * `bytes` - 有限のls-tree -r -t -z出力
///
/// # 戻り値
/// 投影順の全Markdown。99件を超えた部分集合を返さない。
///
fn parse_tree(bytes: &[u8]) -> Result<Vec<Blob>> {
    ensure!(bytes.len() <= DATA_LIMIT && (bytes.is_empty()
        || bytes.last() == Some(&0)), "expected tree framing");
    let mut pages = BTreeMap::new();
    let mut names = std::collections::BTreeSet::new();
    let mut root_present = false;
    /*
     * 全entryを検査し、通常Markdownだけを全件期待へ投影する
     */
    for row in bytes.split(|v| *v == 0).filter(|v| !v.is_empty()) {
        let split = row.iter().position(|v| *v == b'\t')
            .ok_or_else(|| anyhow!("tree entry framing"))?;
        let header = std::str::from_utf8(&row[..split])
            .map_err(|_| anyhow!("tree header"))?;
        let fields: Vec<_> = header.split_ascii_whitespace().collect();
        let name = std::str::from_utf8(&row[split + 1..])
            .map_err(|_| anyhow!("tree path UTF8"))?;
        ensure!(fields.len() == 3 && full_commit(fields[2])
            && safe_entry(name) && names.insert(name.to_owned()), "tree path");
        ensure!(matches!((fields[0], fields[1]),
            ("040000", "tree") | ("100644" | "100755", "blob")),
            "tree mode/symlink/gitlink rejected");
        /*
         * 元tree全体の安全検査を保ち、期待ページはroot内だけへ限定する
         */
        if name == "root" {
            ensure!(fields[1] == "tree", "expected root directory");
            root_present = true;
        }
        let Some(relative) = name.strip_prefix("root/") else { continue; };
        root_present = true;
        if fields[1] == "blob" && relative.ends_with(".md") {
            let projected = relative.strip_suffix(".md").unwrap_or("");
            ensure!(safe_entry(projected), "empty Markdown basename");
            ensure!(pages.insert(projected.to_owned(), Blob {
                name: name.to_owned(), oid: fields[2].to_owned() }).is_none(),
                "duplicate expected projection");
            ensure!(pages.len() <= 99, "expected alias capacity");
        }
    }
    ensure!(root_present, "expected root missing");
    let mut values: Vec<_> = pages.into_values().collect();
    values.sort_by(|a, b| a.name.matches('/').count()
        .cmp(&b.name.matches('/').count()).then(
            a.name.strip_suffix(".md").cmp(&b.name.strip_suffix(".md"))));
    Ok(values)
}

///
/// 期待側Gitのtransport/資格情報/外部設定を遮断する。
///
/// # 概要
/// 固定PATHと空環境から、内部plumbing用commandを構成する。
///
/// # 引数
/// * `root` - 指定済み独立local Git
/// * `args` - ハーネス内部だけが作るplumbing引数
///
/// # 戻り値
/// 実行前のコマンド。Debug表示しない。
///
pub fn offline_command(root: &Path, args: &[&str]) -> Command {
    /*
     * 外部資格情報と設定を除き、内部plumbingだけを渡す
     */
    let mut command = Command::new("git");
    command.env_clear().env("PATH", "/usr/bin:/bin").env("LANG", "C.UTF-8");
    isolate_child(&mut command);
    command.current_dir(root).args(["--no-replace-objects", "-c",
        "core.hooksPath=/dev/null", "-c", "credential.helper=", "-c",
        "protocol.allow=never"]).args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "1").env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null()).stderr(Stdio::null());
    command
}

///
/// 有限stdoutとchild statusを両方確認してplumbingを返す。
///
/// # 引数
/// * `root` - 独立local Git
/// * `args` - 固定plumbing操作
///
/// # 戻り値
/// 完全な有限bytes。断片/生診断を表示しない。
///
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    bounded_git(offline_command(root, args))
}

///
/// 隔離Git子を有限file/30秒監視で回収する。
///
/// # 概要
/// stdoutはpipeにせず、容量または期限超過では停止回収する。
///
/// # 引数
/// * `command` - 隔離設定済みGit command
///
/// # 戻り値
/// 完全stdoutまたは固定失敗
///
fn bounded_git(mut command: Command) -> Result<Vec<u8>> {
    let kill = fs::metadata("/bin/kill")
        .map_err(|_| anyhow!("Git recovery utility missing"))?;
    ensure!(kill.is_file() && kill.permissions().mode() & 0o111 != 0,
        "Git recovery utility unavailable");
    /*
     * pipeのEOF待ちを避け、所有fileと子の期限を別々に監視する
     */
    let mut file = tempfile::tempfile()
        .map_err(|_| anyhow!("expected Git capture file"))?;
    let output = file.try_clone()
        .map_err(|_| anyhow!("expected Git capture handle"))?;
    command.process_group(0);
    let mut child = command.stdout(output).spawn()
        .map_err(|_| anyhow!("expected Git spawn"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {},
            Err(_) => break Err(anyhow!("expected Git wait")),
        }
        if Instant::now() >= deadline || file.metadata()
            .map(|meta| meta.len() > DATA_LIMIT as u64).unwrap_or(true)
        { break Err(anyhow!("expected Git capacity or deadline")); }
        std::thread::sleep(Duration::from_millis(10));
    };
    /*
     * 期限/読取失敗も子を停止回収してから固定診断へ移す
     */
    if status.is_err() {
        let mut stop = Command::new("/bin/kill");
        stop.env_clear().env("PATH", "/usr/bin:/bin");
        isolate_child(&mut stop);
        let stopped = stop.args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().map(|value| value.success()).unwrap_or(false);
        let direct = child.kill();
        let reaped = child.wait();
        ensure!(stopped && reaped.is_ok(), "expected Git recovery failed");
        let _direct_stop = direct;
    }
    ensure!(status?.success(), "expected Git failed");
    file.seek(SeekFrom::Start(0))
        .map_err(|_| anyhow!("expected Git capture offset"))?;
    let mut bytes = Vec::new();
    file.take(DATA_LIMIT as u64 + 1).read_to_end(&mut bytes)
        .map_err(|_| anyhow!("expected Git capture read"))?;
    ensure!(bytes.len() <= DATA_LIMIT, "expected Git capture capacity");
    Ok(bytes)
}

///
/// linked/alternate/promisor依存のないlocal object storeを要求する。
///
/// # 概要
/// 設定とobject経路を有限検査し、対象二版の自動fetchを防ぐ。
///
/// # 引数
/// * `root` - ユーザーが指定した独立clone
///
/// # 戻り値
/// local完結条件。欠落objectのfetchは許可しない。
///
fn local_complete(root: &Path) -> Result<()> {
    /*
     * 独立worktreeと外部object依存の不存在を確認する
     */
    let meta = fs::symlink_metadata(root)
        .map_err(|_| anyhow!("expected root metadata"))?;
    ensure!(meta.is_dir(), "expected root rejected");
    let git = root.join(".git");
    ensure!(fs::symlink_metadata(&git)
        .map_err(|_| anyhow!("expected Git metadata"))?.is_dir(),
        "linked expected repository rejected");
    for name in ["commondir", "objects/info/alternates",
        "objects/info/http-alternates"]
    {
        match fs::symlink_metadata(git.join(name)) {
            Ok(meta) => ensure!(meta.is_file() && meta.len() == 0,
                "expected repository external dependency"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(_) => return Err(anyhow!("expected repository metadata")),
        }
    }
    /*
     * configも通常file/事前size検査と有限読取の両方を要求する
     */
    let path = git.join("config");
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| anyhow!("expected config metadata"))?;
    ensure!(meta.is_file() && meta.len() <= SPEC_LIMIT as u64,
        "expected config capacity");
    let mut file = File::open(path)
        .map_err(|_| anyhow!("expected local config"))?;
    let mut config = Vec::new();
    Read::by_ref(&mut file).take(SPEC_LIMIT as u64 + 1)
        .read_to_end(&mut config)
        .map_err(|_| anyhow!("expected config read"))?;
    ensure!(config.len() <= SPEC_LIMIT, "expected config capacity");
    let config = String::from_utf8(config)
        .map_err(|_| anyhow!("expected config UTF8"))?.to_ascii_lowercase();
    ensure!(!config.contains("promisor") && !config.contains("partialclone"),
        "expected promisor repository rejected");
    let normalized: String = config.chars()
        .filter(|value| !value.is_ascii_whitespace()).collect();
    ensure!(!normalized.contains("[include")
        && !normalized.contains("worktreeconfig"),
        "expected external configuration rejected");
    /*
     * object store内の外部symlinkを件数上限つきで拒否する
     */
    let mut pending = vec![git.join("objects")];
    let mut entries = 0_u64;
    while let Some(path) = pending.pop() {
        let meta = fs::symlink_metadata(&path)
            .map_err(|_| anyhow!("expected object metadata"))?;
        ensure!(meta.is_dir() || meta.is_file(), "expected object type");
        entries += 1;
        ensure!(entries <= 65_536, "expected object capacity");
        if meta.is_dir() {
            for item in fs::read_dir(path)
                .map_err(|_| anyhow!("expected object directory"))?
            {
                pending.push(item.map_err(|_| anyhow!("expected object entry"))?
                    .path());
                ensure!(pending.len() <= 65_536, "expected object capacity");
            }
        }
    }
    Ok(())
}

///
/// 二つの保護本文を小bufferで比較する。
///
/// # 概要
/// 異なるreaderの読取幅に依存せず、両側EOFまで完全比較する。
///
/// # 引数
/// * `a` - 一方の期待view
/// * `b` - もう一方の期待view
///
/// # 戻り値
/// bytes同一。本文は記録しない。
///
pub fn same_body(a: &ExpectedPage<'_>, b: &ExpectedPage<'_>) -> Result<bool> {
    let mut left = a.open()?;
    let mut right = b.open()?;
    let mut x = [0_u8; 65_536];
    let mut y = [0_u8; 65_536];
    /*
     * 読取幅の違いを吸収してbytesと両側EOFを照合する
     */
    loop {
        let n = left.read(&mut x).map_err(|_| anyhow!("expected left read"))?;
        let mut offset = 0;
        while offset < n {
            let size = right.read(&mut y[offset..n])
                .map_err(|_| anyhow!("expected right read"))?;
            if size == 0 { return Ok(false); }
            offset += size;
        }
        if x[..n] != y[..n] { return Ok(false); }
        if n == 0 {
            return right.read(&mut y[..1]).map(|size| size == 0)
                .map_err(|_| anyhow!("expected right EOF"));
        }
    }
}

///
/// private実入口だけで取得tokenを読む。
///
/// # 戻り値
/// 表示しない非空/正当header値
///
fn github_token() -> Result<String> {
    let value = std::env::var("GITHUB_TOKEN")
        .map_err(|_| anyhow!("GitHub credential missing"))?;
    ensure!(!value.is_empty()
        && reqwest::header::HeaderValue::from_str(&format!("Bearer {value}"))
            .is_ok(), "GitHub credential format");
    Ok(value)
}

///
/// 実tokenを読まないpublic無効tokenの負例許可を作る。
///
/// # 戻り値
/// 固定合成tokenだけを該当子へ渡す指定
///
pub fn invalid_public_grant() -> Result<RemoteGrant> {
    Ok(RemoteGrant { source: PreparedSource::github("kwgt/test-repository",
        Selector::Default)?,
        credential: Some("ms51-invalid-dummy-token".into()),
        source_id: "PUB-01", version: "AUTH-NEGATIVE", expected: Vec::new() })
}

// 期待/取得許可と同じ文脈に閉じるため、Caseのremote専用implを分離する。
impl Case {
    ///
    /// 検査済みMarkdown名をcase内pathへ結合する。
    ///
    /// # 引数
    /// * `name` - 内部だけで使う相対entry
    ///
    /// # 戻り値
    /// 許可case内の投影先
    ///
    pub fn expected_path(&self, name: &str) -> Result<String> {
        self.path(name.strip_prefix("root/")
            .and_then(|name| name.strip_suffix(".md"))
            .ok_or_else(|| anyhow!("expected extension"))?)
    }

    ///
    /// 全期待pageの有効path不存在を一回ずつ確認する。
    ///
    /// # 引数
    /// * `observer` - case専用の独立観測者
    /// * `pages` - 所有者が生存する全期待
    ///
    /// # 戻り値
    /// 有効path不存在。不明draftを推測しない。
    ///
    pub async fn absent_expected(&self, observer: &mut Observer,
        pages: &[ExpectedPage<'_>]) -> Result<()>
    {
        for (index, page) in pages.iter().enumerate() {
            super::absent_path(observer, &self.expected_path(page.name)?,
                &alias(index)?).await?;
        }
        Ok(())
    }

    ///
    /// reader期待とlookup/meta/sourceを逐次照合する。
    ///
    /// # 概要
    /// metadataの想定外値もassert前にaliasだけで有限記録する。
    ///
    /// # 引数
    /// * `observer` - case独立観測
    /// * `pages` - 版に結合した全期待view
    ///
    /// # 戻り値
    /// 計画順の観測ID/revision
    ///
    pub async fn observe_expected(&self, observer: &mut Observer,
        pages: &[ExpectedPage<'_>]) -> Result<Vec<State>>
    {
        let mut states = Vec::new();
        /*
         * 元名を外へ出さず、lookup/metaの事実を先に残す
         */
        for (index, page) in pages.iter().enumerate() {
            let label = alias(index)?;
            let path = self.expected_path(page.name)?;
            let id = match observer.lookup(&path).await? {
                Lookup::Found(id) => id,
                Lookup::Missing(status) => {
                    eprintln!("MS51 page={label} observation=lookup \
                        absence={status}");
                    return Err(anyhow!("remote active page missing"));
                }
            };
            metadata_started(&label, &id);
            let meta = observer.metadata(&id, &path).await?;
            metadata_record(&label, &id, &meta);
            /*
             * 正常状態を確認してから64KiB比較へ期待readerを渡す
             */
            ensure!(!meta.page_info.deleted && !meta.page_info.locked
                && meta.page_info.path.kind == "current", "remote metadata");
            let revision = meta.revision_info.as_ref().map(|v| v.revision)
                .ok_or_else(|| anyhow!("remote revision missing"))?;
            observer.source_equals_reader(&id, &path, page.open()?).await?;
            eprintln!("MS51 page={label} id={id} revision={revision} \
                bytes_equal=true");
            states.push(State { id, revision });
        }
        Ok(states)
    }

    ///
    /// 許可repo/版/資格情報と全期待を単一の実CLIへ渡す。
    ///
    /// # 概要
    /// 正当なprivate名があるCapturedは生表示せず有限aliasへ写す。
    ///
    /// # 引数
    /// * `environment` - 固定実Wiki環境
    /// * `auth` - 一方式だけのWiki認証
    /// * `mode` - 衝突mode
    /// * `grant` - 子fixtureが検査済みの取得許可
    /// * `pages` - 同じ版の全期待view
    /// * `rejected` - 同pathの単回probeで拒否された資格情報
    ///
    /// # 戻り値
    /// 生表示禁止の子結果。未知/欠落は成功へ補完しない。
    ///
    pub fn run_remote(&self, environment: &Environment, auth: Auth, mode: Mode,
        grant: &RemoteGrant, pages: &[ExpectedPage<'_>],
        rejected: Option<RejectedCredential>) -> Result<Captured>
    {
        ensure!(!pages.is_empty() && pages.len() <= 99, "remote page count");
        ensure!(pages.len() == grant.expected.len(), "remote snapshot count");
        for (page, (name, file, size)) in pages.iter().zip(&grant.expected) {
            ensure!(page.name == name.as_str() && matches!(page.body,
                ExpectedBody::File { path, size: actual }
                if path == file.as_path() && actual == *size),
                "remote snapshot binding");
        }
        let config = self.area.path().join("remote-empty.toml");
        /*
         * 自分の領域だけへ設定を保存し、Wiki/取得認証を分離する
         */
        if !config.exists() {
            OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(&config).map_err(|_| anyhow!("remote config"))?;
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
        environment.configure_child(&mut command, auth);
        if let Some(proof) = &rejected {
            proof.configure(&mut command, &self.expected_path(pages[0].name)?)?;
        }
        command.arg("--config-path").arg(config)
            .args(["--prefix", &self.prefix, "--mode", mode.as_str()])
            .stdin(Stdio::null()).env("XDG_CONFIG_HOME", self.area.path())
            .env("RUST_LOG", "trace");
        grant.append(&mut command);
        let captured = capture(&mut command)?;
        /*
         * 本文短値の偶然一致を拒否せず、認証と構造境界を確認する
         */
        environment.check_output(&captured.stdout)?;
        environment.check_output(&captured.stderr)?;
        if let Some(proof) = &rejected {
            proof.check_output(&captured.stdout)?;
            proof.check_output(&captured.stderr)?;
        }
        if let Some(token) = &grant.credential {
            for text in [&captured.stdout, &captured.stderr] {
                ensure!(!text.contains(&format!("Bearer {token}"))
                    && (token.len() < 16 || !text.contains(token)),
                    "remote credential output rejected");
            }
        }
        ensure!(!captured.stdout.lines().any(|line|
            line.starts_with("warning:") || line.starts_with("error:"))
            && !captured.stderr.contains("result: exit="), "remote stream mix");
        eprintln!("MS51 source={} version={} snapshot_bound=true",
            grant.source_id, grant.version);
        eprintln!("{}", captured.safe_summary().replacen("MS43", "MS51", 1));
        eprintln!("{}", captured.safe_global().replacen("MS43", "MS51", 1));
        captured.require_remote_plan()?;
        for (index, page) in pages.iter().enumerate() {
            eprintln!("{}", captured.alias_record(
                &self.expected_path(page.name)?,
                &alias(index)?));
        }
        /*
         * Report全件を同じ取得種別/元entry/投影順へ結合する
         */
        let lines: Vec<_> = captured.stdout.lines()
            .filter(|line| line.starts_with("page=")).collect();
        ensure!(lines.len() == pages.len(), "remote Report count");
        let source = if matches!(grant.source.kind, SourceKind::GitHub)
            { "GitHub" } else { "HttpsGit" };
        for (line, page) in lines.iter().zip(pages) {
            ensure!(line.starts_with(&format!("page={} ",
                self.expected_path(page.name)?))
                && line.contains(&format!(" source={source}:{} outcome=",
                    page.name)), "remote Report binding");
        }
        Ok(captured)
    }
}

///
/// remote基本セルのN/S/D/Rを全ページで検証する。
///
/// # 概要
/// 二版期待をHTTP前に準備し、各CLIのReportと独立状態を照合する。
///
/// # 引数
/// * `mode` - 衝突mode
/// * `github` - GitHub入力か
/// * `id` - 固定source安全ID
///
/// # 戻り値
/// 四phase全件の一致
///
pub async fn run_remote_cell(mode: Mode, github: bool, id: SourceId)
    -> Result<()>
{
    let fixture = RemoteFixture::load(id)?;
    let environment = Environment::load_ms51()?;
    let auth = if github { Auth::Bearer } else { Auth::Basic };
    let case = Case::new("remote-cell")?;
    let mut observer = case.observer(&environment, auth)?;
    let first = fixture.v0.views();
    let second = fixture.v1.views();
    case.absent_expected(&mut observer, &first).await?;
    /*
     * Nでは全page作成、Sでは同一版の拒否またはskipを確認する
     */
    eprintln!("MS51 phase=N");
    let created = case.run_remote(&environment, auth, mode,
        &fixture.grant(github, false)?, &first, None)?;
    created.expect_ms51(0)?;
    created.expect_counts([first.len() as u64, 0, 0, 0, 0, 0])?;
    let before = case.observe_expected(&mut observer, &first).await?;
    verify_identities(&case, &created, &first, &before, "Created", "draft_id")?;
    let mut latest = Vec::new();
    for (phase, changed) in [("S", false), ("D", true), ("R", true)] {
        eprintln!("MS51 phase={phase}");
        let pages = if changed { &second } else { &first };
        let result = case.run_remote(&environment, auth, mode,
            &fixture.grant(github, changed)?, pages, None)?;
        let expected = if matches!(mode, Mode::Error) { &first } else { pages };
        let after = case.observe_expected(&mut observer, expected).await?;
        /*
         * errorは最初の一件で停止し、残全pageと全版の不変を確認する
         */
        if matches!(mode, Mode::Error) {
            expect_conflict(&case, &result, pages)?;
            same_states(&before, &after)?;
        } else if phase == "D" {
            verify_changed(&case, &mut observer, &result, pages, mode,
                &before, &after, &fixture.different).await?;
        } else {
            result.expect_ms51(0)?;
            result.expect_counts([0, pages.len() as u64, 0, 0, 0, 0])?;
            for page in pages {
                result.page(&case.expected_path(page.name)?,
                    "SkippedIdentical")?;
            }
            let prior = if phase == "S" { &before } else { &latest };
            same_states(prior, &after)?;
        }
        latest = after;
    }
    Ok(())
}

///
/// 計画順の全Report IDを独立観測へ結合する。
///
/// # 引数
/// * `case` - 固定case
/// * `result` - 生表示しない製品出力
/// * `pages` - 全期待view
/// * `states` - 全観測状態
/// * `outcome` - 期待結果
/// * `field` - ID field名
///
/// # 戻り値
/// 全件一致
///
fn verify_identities(case: &Case, result: &Captured,
    pages: &[ExpectedPage<'_>], states: &[State], outcome: &str, field: &str)
    -> Result<()>
{
    ensure!(pages.len() == states.len(), "remote state cardinality");
    for (page, state) in pages.iter().zip(states) {
        let path = case.expected_path(page.name)?;
        result.page(&path, outcome)?;
        ensure!(result.id(&path, field)? == state.id, "remote Report ID");
    }
    Ok(())
}

///
/// errorセルの先頭失敗と残未実行を確認する。
///
/// # 引数
/// * `case` - 固定case
/// * `result` - 製品出力
/// * `pages` - 全期待
///
/// # 戻り値
/// 無警告exit1/Lookup Conflictと全件数の一致
///
fn expect_conflict(case: &Case, result: &Captured,
    pages: &[ExpectedPage<'_>]) -> Result<()>
{
    result.expect_ms51(1)?;
    result.expect_counts([0, 0, 0, 0, 1, pages.len() as u64 - 1])?;
    ensure!(result.stderr.contains("category=Conflict stage=Lookup"),
        "remote conflict classification");
    for (index, page) in pages.iter().enumerate() {
        result.page(&case.expected_path(page.name)?,
            if index == 0 { "Failed" } else { "NotExecuted" })?;
    }
    Ok(())
}

///
/// 全pageのID/revision不変を表示せず確認する。
///
/// # 引数
/// * `before` - 前回状態
/// * `after` - 今回状態
///
/// # 戻り値
/// 全件不変
///
fn same_states(before: &[State], after: &[State]) -> Result<()> {
    ensure!(before.len() == after.len(), "remote state cardinality");
    for (left, right) in before.iter().zip(after) {
        ensure!(left.id == right.id && left.revision == right.revision,
            "remote identity/revision changed");
    }
    Ok(())
}

///
/// 相違するpageだけの更新/再作成と旧状態を確認する。
///
/// # 概要
/// 本文差を独立比較したmaskから件数を算出し、旧soft-deleteも照合する。
///
/// # 引数
/// * `case` - case所有者
/// * `observer` - 独立観測者
/// * `result` - 製品出力
/// * `pages` - V1全期待
/// * `mode` - updateまたはoverwrite
/// * `before` - V0全状態
/// * `after` - V1全状態
/// * `different` - 独立bytes比較mask
///
/// # 戻り値
/// 相違/同一全件の期待一致
///
async fn verify_changed(case: &Case, observer: &mut Observer,
    result: &Captured, pages: &[ExpectedPage<'_>], mode: Mode,
    before: &[State], after: &[State], different: &[bool]) -> Result<()>
{
    ensure!(pages.len() == before.len() && before.len() == after.len()
        && after.len() == different.len(), "remote difference cardinality");
    let changed = different.iter().filter(|value| **value).count() as u64;
    let skipped = pages.len() as u64 - changed;
    result.expect_ms51(0)?;
    result.expect_counts(if matches!(mode, Mode::Update) {
        [0, skipped, changed, 0, 0, 0]
    } else { [0, skipped, 0, changed, 0, 0] })?;
    /*
     * 同一pageと相違pageをそれぞれ全件の観測へ照合する
     */
    for (index, page) in pages.iter().enumerate() {
        let old = &before[index];
        let new = &after[index];
        let path = case.expected_path(page.name)?;
        if !different[index] {
            result.page(&path, "SkippedIdentical")?;
            same_states(std::slice::from_ref(old), std::slice::from_ref(new))?;
        } else if matches!(mode, Mode::Update) {
            result.page(&path, "Updated")?;
            ensure!(result.id(&path, "old_id")? == new.id
                && old.id == new.id
                && old.revision.checked_add(1) == Some(new.revision),
                "remote update identity/revision");
            result.expect_old_recovery(&path, &old.id, old.revision, false)?;
        } else {
            result.page(&path, "Recreated")?;
            ensure!(result.id(&path, "draft_id")? == new.id && old.id != new.id,
                "remote recreated identity");
            result.expect_old_recovery(&path, &old.id, old.revision, true)?;
            let meta = observer.metadata(&old.id, &path).await?;
            metadata_record(&alias(index)?, &old.id, &meta);
            ensure!(meta.page_info.deleted
                && meta.page_info.path.kind == "last_deleted"
                && meta.revision_info.as_ref()
                    .is_some_and(|value| value.revision == old.revision),
                "remote old soft-delete");
        }
    }
    Ok(())
}

///
/// remote参照の全準備を済ませてから各新caseへ投入する。
///
/// # 概要
/// main/masterは指定された一役割、branch/tag/commitはすべて必須。
/// 一つでも未準備ならWiki HTTP前に停止する。
///
/// # 引数
/// * `github` - 製品の取得方式
///
/// # 戻り値
/// 四参照代表の全件一致
///
pub async fn run_remote_references(github: bool) -> Result<()> {
    let fixture = RemoteFixture::load(SourceId::Public)?;
    let refs = &fixture.spec.refs;
    ensure!(refs.contains_key("main") != refs.contains_key("master"),
        "prepare exactly one default reference role");
    let default = if refs.contains_key("main") { "main" } else { "master" };
    let mut prepared = Vec::new();
    /*
     * 各local/remote refのcommit結合を一括でWiki操作前に確定する
     */
    for role in [default, "branch", "tag", "commit"] {
        prepared.push((role, fixture.reference(github, role).await?));
    }
    let environment = Environment::load_ms51()?;
    let auth = if github { Auth::Bearer } else { Auth::Basic };
    for (role, (grant, expected)) in prepared {
        eprintln!("MS51 reference_role={role}");
        let case = Case::new("remote-reference")?;
        let mut observer = case.observer(&environment, auth)?;
        let pages = expected.views();
        case.absent_expected(&mut observer, &pages).await?;
        let result = case.run_remote(&environment, auth, Mode::Error,
            &grant, &pages, None)?;
        result.expect_ms51(0)?;
        result.expect_counts([pages.len() as u64, 0, 0, 0, 0, 0])?;
        let states = case.observe_expected(&mut observer, &pages).await?;
        verify_identities(&case, &result, &pages, &states,
            "Created", "draft_id")?;
    }
    Ok(())
}

///
/// 取得負例を合成認証のloopback Wiki sinkへ限定する。
///
/// # 概要
/// private匿名は本文を読まず許可を作り、想定外成功も外部Wikiへ入れない。
///
/// # 引数
/// * `grant` - 負例用に閉じた取得許可
/// * `target` - この試験所有のloopback URL
///
/// # 戻り値
/// 生表示しないCLI出力
///
pub fn run_sink(grant: &RemoteGrant, target: &str) -> Result<Captured> {
    let url = url::Url::parse(target).map_err(|_| anyhow!("sink URL"))?;
    ensure!(url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
        && url.port().is_some() && url.username().is_empty()
        && url.password().is_none() && url.path() == "/"
        && url.query().is_none() && url.fragment().is_none(), "sink rejected");
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700)).tempdir()
        .map_err(|_| anyhow!("sink area"))?;
    let config = area.path().join("empty.toml");
    OpenOptions::new().write(true).create_new(true).mode(0o600)
        .open(&config).map_err(|_| anyhow!("sink config"))?;
    /*
     * 実Wiki資格情報を読まず、合成値と指定sinkだけを子へ渡す
     */
    let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
    command.env_clear().env("PATH", "/usr/bin:/bin").env("LANG", "C.UTF-8");
    isolate_child(&mut command);
    command.arg("--config-path").arg(config)
        .args(["--prefix", "/fixture", "--mode", "error"])
        .env("LUWIKI_URL", target).env("LUWIKI_TOKEN", "synthetic-only")
        .env("XDG_CONFIG_HOME", area.path()).env("RUST_LOG", "trace")
        .stdin(Stdio::null());
    grant.append(&mut command);
    let result = capture(&mut command)?;
    for text in [&result.stdout, &result.stderr] {
        ensure!(!text.contains("Bearer synthetic-only"), "sink credential");
        if let Some(token) = &grant.credential {
            ensure!(!text.contains(token), "acquisition credential");
        }
    }
    eprintln!("{}", result.safe_summary().replacen("MS43", "MS51", 1));
    eprintln!("{}", result.safe_global().replacen("MS43", "MS51", 1));
    Ok(result)
}

///
/// 同pathの拒否probe後に実Wiki初回Lookup拒否を確認する。
///
/// # 概要
/// 正常observer/probe/製品を分離し、前後の全path不存在を照合する。
///
/// # 引数
/// * `status` - 401または403
///
/// # 戻り値
/// 正常取得後のLookup段階停止と状態不変
///
pub async fn run_wiki_rejection(status: u16) -> Result<()> {
    ensure!(matches!(status, 401 | 403), "negative status");
    let fixture = RemoteFixture::load(SourceId::Public)?;
    let environment = Environment::load_ms51()?;
    let case = Case::new("remote-wiki-rejection")?;
    let pages = fixture.v0.views();
    let mut observer = case.observer(&environment, Auth::Bearer)?;
    case.absent_expected(&mut observer, &pages).await?;
    /*
     * 403の専用資格は一度だけ読み、probeとCLIが同じ値を保持する
     */
    let token = if status == 401 { "ms51-invalid-wiki-dummy-token".to_owned() }
        else { std::env::var("LUWIKI_MS51_LOOKUP403_TOKEN")
            .map_err(|_| anyhow!("Lookup403 credential missing"))? };
    ensure!(!token.is_empty(), "negative credential empty");
    let proof = environment.probe_rejection(
        &case.expected_path(pages[0].name)?, token, status).await?;
    let result = case.run_remote(&environment, Auth::Bearer, Mode::Error,
        &fixture.grant(true, false)?, &pages, Some(proof))?;
    result.expect_ms51(1)?;
    result.expect_counts([0, 0, 0, 0, 1, pages.len() as u64 - 1])?;
    let category = if status == 401 { "Authentication" } else { "Permission" };
    ensure!(result.stderr.contains(&format!(
        "category={category} stage=Lookup")), "Wiki rejection classification");
    result.failure(category, "Lookup", "Lookup", status)?;
    for (index, page) in pages.iter().enumerate() {
        result.page(&case.expected_path(page.name)?,
            if index == 0 { "Failed" } else { "NotExecuted" })?;
    }
    case.absent_expected(&mut observer, &pages).await
}

///
/// 生tree parserを合成入力だけで検査できる形へ写す。
///
/// # 引数
/// * `bytes` - 合成ls-tree bytes
///
/// # 戻り値
/// 全検査後の計画順Markdown名
///
pub fn expected_names(bytes: &[u8]) -> Result<Vec<String>> {
    Ok(parse_tree(bytes)?.into_iter().map(|blob| blob.name).collect())
}
