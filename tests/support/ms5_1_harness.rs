/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51の型付き入力・有限子出力・正常phaseと独立観測
//!

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, ensure};
use tempfile::TempDir;
use url::Url;

use super::real_cli_http::{Auth, Environment, Lookup, MS51_ROOT, Metadata,
    Observer, within};
use super::real_cli_report::Captured;
use super::source_fixture::{F0, F1, GitFixture, Page, ZipFixture, ZipKind,
    safe_entry};

// G4/configのcrateはremote実入口を使用しないが共有型の結合を維持する。
#[allow(dead_code)]
#[path = "remote_fixture.rs"]
pub(crate) mod remote_fixture;

///
/// 固定sliceまたは保護storeを借用する期待本文。
///
#[derive(Clone, Copy)]
pub enum ExpectedBody<'a> {
    /// 既存G4の固定bytes。
    Bytes(&'a [u8]),
    /// private名を持たない連番file。所有者はviewより長く生存する。
    File {
        /// 保護一時領域内の本文file。
        path: &'a Path,
        /// 封印時に確認したbytes長。
        size: u64,
    },
}

///
/// 本文を複製せず入力名とreaderを結合する借用view。
///
#[derive(Clone, Copy)]
pub struct ExpectedPage<'a> {
    /// 内部比較だけに使う相対Markdown名。
    pub name: &'a str,
    /// 所有者が生存する期待本文。
    pub body: ExpectedBody<'a>,
}

impl ExpectedPage<'_> {
    ///
    /// 独立offsetのreaderを開く。try_cloneでoffsetを共有しない。
    ///
    /// # 概要
    /// file期待は通常fileと封印sizeを確認して独立openする。
    ///
    /// # 戻り値
    /// viewの寿命内だけ使用する本文reader
    ///
    pub fn open(&self) -> Result<Box<dyn Read + '_>> {
        /*
         * 固定bytesと保護fileを共通の独立readerへ写す
         */
        match self.body {
            ExpectedBody::Bytes(bytes) => {
                Ok(Box::new(std::io::Cursor::new(bytes)))
            },
            ExpectedBody::File { path, size } => {
                let meta = std::fs::symlink_metadata(path)
                    .map_err(|_| anyhow!("expected metadata"))?;
                ensure!(meta.is_file() && meta.len() == size,
                    "expected file changed");
                let file = std::fs::File::open(path)
                    .map_err(|_| anyhow!("expected file open"))?;
                ensure!(file.metadata().map_err(|_| anyhow!("expected file"))?
                    .len() == size, "expected file size changed");
                Ok(Box::new(file))
            }
        }
    }
}

/// 各streamのハーネス保持量。製品の受信量制限ではない。
pub const CAPTURE_LIMIT: usize = 1_048_576;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

///
/// source固有の参照指定。任意argvを格納しない。
///
#[derive(Clone)]
pub enum Selector {
    /// 製品既定のmain、次にmaster。
    Default,
    /// 明示branch名。
    Branch(
        /// 検査してからargvへ渡すbranch名。
        String,
    ),
    /// 明示tag名。
    Tag(
        /// 検査してからargvへ渡すtag名。
        String,
    ),
    /// 完全commit ID。
    Commit(
        /// 40桁の完全commit ID。
        String,
    ),
    /// LocalGitだけのworktree。
    Worktree,
}

impl Selector {
    ///
    /// ハーネス参照を単一安全値へ限定する。
    ///
    /// # 戻り値
    /// 構文一致、またはHTTP/子起動前の固定拒否
    ///
    fn validate(&self) -> Result<()> {
        /*
         * selectorごとに値を検査し、任意optionやrevision式を拒否する
         */
        match self {
            Self::Default | Self::Worktree => Ok(()),
            Self::Branch(value) | Self::Tag(value) => {
                ensure!(safe_entry(value) && !value.starts_with('-')
                    && !value.contains([':', '?', '*', '[', '~', '^'])
                    && !value.contains(".."), "fixture selector");
                Ok(())
            }
            Self::Commit(value) => {
                ensure!(value.len() == 40
                    && value.bytes().all(|v| v.is_ascii_hexdigit()),
                    "fixture commit");
                Ok(())
            }
        }
    }
}

///
/// 四入力を区別する内部dispatch。
///
#[derive(Clone, Copy)]
enum SourceKind {
    /// ローカルZIP。
    Zip,
    /// ローカルGit。
    LocalGit,
    /// 匿名HTTPS Git。GitHub tokenを転用しない。
    HttpsGit,
    /// GitHub owner/repo。実取得/秘密読取はG5で追加する。
    GitHub,
}

///
/// 構文検査済み入力。実remote入口や取得資格情報を持たない。
///
pub struct PreparedSource {
    /// 単一位置引数。
    input: OsString,
    /// sourceの取得方式。
    kind: SourceKind,
    /// 型付き参照指定。
    selector: Selector,
}

impl PreparedSource {
    ///
    /// 一時ZIPから入力引数を作る。
    ///
    /// # 引数
    /// * `path` - fixture所有のZIP path
    ///
    /// # 戻り値
    /// ZIP入力指定
    ///
    pub fn zip(path: &Path) -> Self {
        Self { input: path.as_os_str().to_owned(), kind: SourceKind::Zip,
            selector: Selector::Default }
    }

    ///
    /// 一時Gitから入力とselectorを作る。
    ///
    /// # 引数
    /// * `path` - fixture所有のworktree root
    /// * `selector` - 内部で定めた参照方針
    ///
    /// # 戻り値
    /// LocalGit入力指定、または値を含まない拒否
    ///
    pub fn local(path: &Path, selector: Selector) -> Result<Self> {
        selector.validate()?;
        Ok(Self { input: path.as_os_str().to_owned(),
            kind: SourceKind::LocalGit, selector })
    }

    ///
    /// 匿名HTTPS Gitのargv構造だけを準備する。
    ///
    /// # 引数
    /// * `input` - userinfo/query/fragmentのないHTTPS URL
    /// * `selector` - worktree以外の参照
    ///
    /// # 戻り値
    /// 構文検査済み指定。G4 runnerでは実行を拒否する。
    ///
    pub fn https(input: &str, selector: Selector) -> Result<Self> {
        let url = Url::parse(input).map_err(|_| anyhow!("source URL"))?;
        ensure!(url.scheme() == "https" && url.host_str().is_some()
            && url.username().is_empty() && url.password().is_none()
            && url.query().is_none() && url.fragment().is_none()
            && !matches!(selector, Selector::Worktree), "source URL rejected");
        selector.validate()?;
        Ok(Self { input: input.into(), kind: SourceKind::HttpsGit, selector })
    }

    ///
    /// GitHub owner/repoのargv構造だけを準備する。
    ///
    /// # 引数
    /// * `input` - 合成検査または後続で承認するowner/repo
    /// * `selector` - worktree以外の参照
    ///
    /// # 戻り値
    /// 構文検査済み指定。秘密環境やprivate内容は読まない。
    ///
    pub fn github(input: &str, selector: Selector) -> Result<Self> {
        ensure!(input.split('/').count() == 2 && safe_entry(input)
            && !input.contains(':') && !input.contains('?')
            && !input.contains('#') && !input.starts_with('-')
            && !matches!(selector, Selector::Worktree), "GitHub source");
        selector.validate()?;
        Ok(Self { input: input.into(), kind: SourceKind::GitHub, selector })
    }

    ///
    /// sourceと参照だけを製品CLIの既存構文へ写す。
    ///
    /// # 引数
    /// * `command` - prefix/target/authを別責務で固定する製品コマンド
    ///
    /// # 戻り値
    /// なし。取得資格情報を注入するAPIは持たない。
    ///
    pub fn append(&self, command: &mut Command) {
        /*
         * 方式を既存CLIの3種類へ写し、selectorは一方式だけ付ける
         */
        let kind = match self.kind {
            SourceKind::Zip => "zip",
            SourceKind::LocalGit | SourceKind::HttpsGit => "git",
            SourceKind::GitHub => "github",
        };
        command.args(["--source-type", kind]);
        match &self.selector {
            Selector::Default => {},
            Selector::Branch(value) => { command.arg("--branch").arg(value); }
            Selector::Tag(value) => { command.arg("--tag").arg(value); }
            Selector::Commit(value) => { command.arg("--commit").arg(value); }
            Selector::Worktree => { command.arg("--worktree"); }
        }
        command.arg(&self.input);
    }
}

///
/// 子remote_fixtureが検査後に構築する限定取得許可。
///
pub struct RemoteGrant {
    /// 完全commitまたは承認refに閉じたsource。
    source: PreparedSource,
    /// 該当GitHub子だけへ渡す非表示資格情報。
    credential: Option<String>,
    /// 外部記録へ出せる固定source ID。
    source_id: &'static str,
    /// 外部記録へ出せる固定版ID。
    version: &'static str,
    /// 本文を複製しない全期待の名前/所有file/size結合。
    expected: Vec<(String, std::path::PathBuf, u64)>,
}

impl RemoteGrant {
    ///
    /// sourceと該当資格情報だけを単一CLIへ設定する。
    ///
    /// # 引数
    /// * `command` - Wiki認証を別責務で設定済みの子
    ///
    /// # 戻り値
    /// なし。Commandは表示しない。
    ///
    pub(crate) fn append(&self, command: &mut Command) {
        self.source.append(command);
        if let Some(token) = &self.credential {
            command.env("GITHUB_TOKEN", token);
        }
    }
}

///
/// 固定の衝突mode。文字列による任意引数を許さない。
///
#[derive(Clone, Copy)]
pub enum Mode {
    /// 既存を無条件拒否する。
    Error,
    /// 同一skip、相違update。
    Update,
    /// 同一skip、相違soft-delete/再作成。
    Overwrite,
}

impl Mode {
    ///
    /// 既存CLIが受け付ける固定語彙を返す。
    ///
    /// # 戻り値
    /// mode位置へ渡す固定文字列
    ///
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Update => "update",
            Self::Overwrite => "overwrite",
        }
    }
}

///
/// Wikiへ自動cleanupしない、新run/caseのローカル所有者。
///
pub struct Case {
    /// 許可済みms51 subtree内の新case。
    prefix: String,
    /// caseの明示設定ファイルを所有する。
    area: TempDir,
}

impl Case {
    ///
    /// HTTP発行前に新しいcase領域を生成する。
    ///
    /// # 引数
    /// * `name` - ハーネス内の固定ASCIIラベル
    ///
    /// # 戻り値
    /// 新しいcase。旧runの再使用はしない。
    ///
    pub fn new(name: &str) -> Result<Self> {
        ensure!(!name.is_empty() && name.bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'-'), "case name");
        let tick = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("case clock"))?.as_nanos();
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("{MS51_ROOT}/cli-{}-{tick}-{seq}/{name}",
            std::process::id());
        ensure!(within(&prefix, MS51_ROOT), "case scope");
        eprintln!("MS51 case={name} run={}-{}-{seq}", std::process::id(), tick);
        Ok(Self { prefix, area: tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir()
            .map_err(|_| anyhow!("case area"))? })
    }

    ///
    /// 固定page名をcase配下へ結合する。
    ///
    /// # 引数
    /// * `name` - 拡張子なしの相対page名
    ///
    /// # 戻り値
    /// 許可されたpage path
    ///
    pub fn path(&self, name: &str) -> Result<String> {
        ensure!(safe_entry(name), "case relative path");
        let path = format!("{}/{name}", self.prefix);
        ensure!(within(&path, &self.prefix), "case page scope");
        Ok(path)
    }

    ///
    /// 別invocation/期限のobserverを作る。
    ///
    /// # 引数
    /// * `environment` - 明示実入口だけが読む実環境
    /// * `auth` - Wiki認証方式
    ///
    /// # 戻り値
    /// このcaseに閉じる観測者
    ///
    pub fn observer(&self, environment: &Environment, auth: Auth)
        -> Result<Observer>
    {
        eprintln!("MS51 observer=independent deadline_seconds=300");
        environment.observer(&self.prefix, auth)
    }

    ///
    /// 製品CLIを一回だけ実行し有限事実へ限定する。
    ///
    /// # 概要
    /// 入力/設定の所有者を保持し、子/両pipeの回収後だけ解析へ進む。
    ///
    /// # 引数
    /// * `environment` - 承認済み対象と秘匿認証
    /// * `auth` - Wiki認証方式
    /// * `mode` - 固定衝突mode
    /// * `source` - 型付きローカル入力
    /// * `pages` - 期待する固定Markdown集合
    ///
    /// # 戻り値
    /// 生表示禁止の解析済み子出力
    ///
    pub fn run(&self, environment: &Environment, auth: Auth, mode: Mode,
        source: &PreparedSource, pages: &[Page]) -> Result<Captured>
    {
        ensure!(matches!(source.kind, SourceKind::Zip | SourceKind::LocalGit),
            "remote execution deferred to G5");
        let config = self.area.path().join("empty.toml");
        /*
         * 既存設定を読ませず、秘密を含まない0600設定を初回生成する
         */
        if !config.exists() {
            OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(&config).map_err(|_| anyhow!("case config"))?;
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
        environment.configure_child(&mut command, auth);
        command.arg("--config-path").arg(&config)
            .args(["--prefix", &self.prefix, "--mode", mode.as_str()])
            .stdin(Stdio::null()).env("XDG_CONFIG_HOME", self.area.path())
            .env("RUST_LOG", "trace");
        source.append(&mut command);
        /*
         * 両streamを有限保持し、超過/失敗の断片を解析しない
         */
        let captured = capture(&mut command)?;
        if environment.check_output(&captured.stdout).is_err()
            || environment.check_output(&captured.stderr).is_err()
            || validate_output(&captured, pages).is_err()
        {
            eprintln!("MS51 output=rejected recovery=unknown");
            return Err(anyhow!("CLI output boundary"));
        }
        eprintln!("{}", captured.safe_summary().replacen("MS43", "MS51", 1));
        eprintln!("{}", captured.safe_global().replacen("MS43", "MS51", 1));
        let sorted = ordered(pages)?;
        for (index, page) in sorted.iter().enumerate() {
            let path = self.page_path(page)?;
            eprintln!("{}", captured.alias_record(&path, &alias(index)?));
        }
        check_order(self, &captured, &sorted, source.kind)?;
        Ok(captured)
    }

    ///
    /// Markdown entryから期待page pathを得る。
    ///
    /// # 引数
    /// * `page` - 固定Markdown fixture
    ///
    /// # 戻り値
    /// 拡張子だけを除いたcase内path
    ///
    pub fn page_path(&self, page: &Page) -> Result<String> {
        let name = page.name.strip_suffix(".md")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("page extension"))?;
        self.path(name)
    }

    ///
    /// 全対象の初期不存在を一回ずつ独立照会する。
    ///
    /// # 引数
    /// * `observer` - このcase専用の観測者
    /// * `pages` - 対象ページ集合
    ///
    /// # 戻り値
    /// 有効path不存在。不明draftの不存在は保証しない。
    ///
    pub async fn absent(&self, observer: &mut Observer, pages: &[Page])
        -> Result<()>
    {
        for (index, page) in ordered(pages)?.iter().enumerate() {
            absent_path(observer, &self.page_path(page)?, &alias(index)?)
                .await?;
        }
        Ok(())
    }

    ///
    /// 正常ページのlookup/meta/sourceを各一回だけ確認する。
    ///
    /// # 概要
    /// ID/path/revisionとbytesを内部比較し、本文やraw pathは表示しない。
    ///
    /// # 引数
    /// * `observer` - このcase専用の観測者
    /// * `pages` - 固定期待bytes
    ///
    /// # 戻り値
    /// 計画順の独立観測状態
    ///
    pub async fn observe(&self, observer: &mut Observer, pages: &[Page])
        -> Result<Vec<State>>
    {
        let mut states = Vec::new();
        /*
         * 観測ごとに有効IDと正常metadataを確認してから本文を比較する
         */
        for (index, page) in ordered(pages)?.iter().enumerate() {
            let label = alias(index)?;
            let path = self.page_path(page)?;
            let id = match observer.lookup(&path).await? {
                Lookup::Found(id) => id,
                Lookup::Missing(status) => {
                    eprintln!("MS51 page={label} observation=lookup \
                        absence={status}");
                    return Err(anyhow!("active page missing"));
                }
            };
            eprintln!("MS51 page={label} observation=lookup id={id}");
            metadata_started(&label, &id);
            /*
             * metadataの正常状態確認後にだけsourceのbytesを照合する
             */
            let meta = observer.metadata(&id, &path).await?;
            metadata_record(&label, &id, &meta);
            ensure!(!meta.page_info.deleted && !meta.page_info.locked
                && meta.page_info.path.kind == "current", "normal metadata");
            let revision = meta.revision_info.as_ref().map(|v| v.revision)
                .ok_or_else(|| anyhow!("normal revision missing"))?;
            let expected = ExpectedPage { name: page.name,
                body: ExpectedBody::Bytes(page.body) };
            observer.source_equals_reader(&id, &path, expected.open()?)
                .await?;
            eprintln!("MS51 page={label} id={id} revision={revision} \
                deleted=false locked=false bytes_equal=true");
            states.push(State { id, revision });
        }
        Ok(states)
    }
}

///
/// 本文を保持しない独立観測済みの正常状態。
///
pub struct State {
    /// 有効page ID。
    pub id: String,
    /// 必須metadataから得たrevision。
    pub revision: u64,
}

///
/// metadata開始時点と潜在cleanupを有限記録する。
///
/// # 引数
/// * `label` - コード内のpage alias
/// * `id` - 形式検査済みpage ID
///
/// # 戻り値
/// なし。別observerの一回の観測開始を記録する。
///
fn metadata_started(label: &str, id: &str) {
    let tick = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis()).ok();
    eprintln!("MS51 page={label} id={id} observation=metadata phase=begin \
        observed_unix_ms={tick:?} count=1 potential_expiry_cleanup=true");
}

///
/// 期待assert前に検査済みmetadataの有限状態を記録する。
///
/// # 引数
/// * `label` - コード内のpage alias
/// * `id` - 形式検査済みpage ID
/// * `meta` - schema/path/kindを検査済みの観測
///
/// # 戻り値
/// なし。本文/raw pathや任意JSONを表示しない。
///
fn metadata_record(label: &str, id: &str, meta: &Metadata) {
    let tick = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis()).ok();
    eprintln!("MS51 page={label} id={id} observation=metadata phase=end \
        observed_unix_ms={tick:?} count=1 kind={} deleted={} locked={} \
        revision={:?}", meta.page_info.path.kind, meta.page_info.deleted,
        meta.page_info.locked, meta.revision_info.as_ref().map(|v| v.revision));
}

///
/// 安全な連番page aliasを得る。
///
/// # 引数
/// * `index` - 0から始まる計画位置
///
/// # 戻り値
/// P01～P99。任意pathを表示用ラベルにしない。
///
fn alias(index: usize) -> Result<String> {
    ensure!(index < 99, "page alias capacity");
    Ok(format!("P{:02}", index + 1))
}

///
/// 固定ページを深さ、UTF-8文字列順へ並べる。
///
/// # 引数
/// * `pages` - Markdownだけの期待集合
///
/// # 戻り値
/// 元bytesを変更しない期待計画順
///
pub fn ordered(pages: &[Page]) -> Result<Vec<Page>> {
    let mut sorted = pages.to_vec();
    for page in &sorted {
        ensure!(safe_entry(page.name) && page.name.strip_suffix(".md")
            .is_some_and(safe_entry),
            "expected Markdown entry");
    }
    /*
     * 拡張子を除いた投影先で比較し、aとa-fooの順序も維持する
     */
    sorted.sort_by(|a, b| a.name.matches('/').count()
        .cmp(&b.name.matches('/').count()).then(
            a.name.strip_suffix(".md").cmp(&b.name.strip_suffix(".md"))));
    ensure!(sorted.windows(2).all(|pair| pair[0].name != pair[1].name),
        "duplicate expected page");
    Ok(sorted)
}

///
/// Reportの全page行が期待集合と計画順に一致するか確認する。
///
/// # 概要
/// sourceとpageの結合を確認し、生出力や未知pathは表示しない。
///
/// # 引数
/// * `case` - 許可されたcase
/// * `captured` - 生表示しない子出力
/// * `pages` - 整列済みの期待ページ
/// * `kind` - 製品Reportへ対応付ける取得方式
///
/// # 戻り値
/// 完全一致。実HTTP発行順そのものの証明ではない。
///
fn check_order(case: &Case, captured: &Captured, pages: &[Page],
    kind: SourceKind) -> Result<()>
{
    /*
     * 四入力の製品Report語彙を固定する
     */
    let source = match kind {
        SourceKind::Zip => "Zip",
        SourceKind::LocalGit => "LocalGit",
        SourceKind::HttpsGit => "HttpsGit",
        SourceKind::GitHub => "GitHub",
    };
    let lines: Vec<_> = captured.stdout.lines()
        .filter(|v| v.starts_with("page=")).collect();
    ensure!(lines.len() == pages.len(), "CLI page count");
    for (line, page) in lines.iter().zip(pages) {
        ensure!(line.starts_with(&format!("page={} ", case.page_path(page)?)),
            "CLI page order");
        ensure!(line.contains(&format!(" source={source}:root/{} outcome=",
            page.name)), "CLI source reference");
    }
    Ok(())
}

///
/// 子の出力から本文/認証表現と出力先混在を拒否する。
///
/// # 概要
/// 固定本文の全文/固有断片を検査し、失敗にも元文字列を含めない。
///
/// # 引数
/// * `captured` - 両streamを回収した出力
/// * `pages` - 今回の期待固定本文
///
/// # 戻り値
/// 非露出/出力分離の検査結果
///
pub fn validate_output(captured: &Captured, pages: &[Page]) -> Result<()> {
    /*
     * 期待本文と両版の固定sentinelを生表示前に検査する
     */
    for text in [&captured.stdout, &captured.stderr] {
        for bytes in pages.iter().map(|v| v.body).chain([F0, F1]) {
            let body = std::str::from_utf8(bytes)
                .map_err(|_| anyhow!("fixture UTF8"))?;
            ensure!(body.is_empty() || !text.contains(body), "body output");
        }
        for marker in ["MS51合成", "Authorization", "X-Lock-"] {
            ensure!(!text.contains(marker), "private output");
        }
    }
    ensure!(!captured.stdout.contains("warning:")
        && !captured.stdout.contains("error:")
        && !captured.stderr.contains("result: exit="), "CLI stream boundary");
    Ok(())
}

///
/// 既知pathの不存在を一回照会し、有限statusだけを記録する。
///
/// # 引数
/// * `observer` - case専用観測者
/// * `path` - 内部だけで使う許可path
/// * `label` - 呼出側の固定ラベル
///
/// # 戻り値
/// 有効path不存在。不明状態を不存在へ写さない。
///
pub async fn absent_path(observer: &mut Observer, path: &str, label: &str)
    -> Result<()>
{
    match observer.lookup(path).await? {
        Lookup::Missing(status) => {
            eprintln!("MS51 page={label} observation=lookup absence={status}");
            Ok(())
        }
        Lookup::Found(_) => Err(anyhow!("unexpected existing page")),
    }
}

///
/// 一定量だけ保持して残りもEOFまで捨て読みするstream結果。
///
pub struct Drained {
    /// 上限以下の非公開出力。
    pub bytes: Vec<u8>,
    /// 超過または読取失敗。断片解析を禁止する。
    pub rejected: bool,
}

///
/// 固定bufferでstreamを最後まで読み取り、超過を保持しない。
///
/// # 概要
/// 超過後も捨て読みを継続し、読取失敗は成功断片へ変換しない。
///
/// # 引数
/// * `reader` - 子の片側pipeまたは合成reader
///
/// # 戻り値
/// 有限保持bytesと拒否フラグ
///
pub fn drain(mut reader: impl Read) -> Drained {
    let mut result = Drained { bytes: Vec::with_capacity(CAPTURE_LIMIT),
        rejected: false };
    let mut buffer = [0_u8; 8192];
    /*
     * Interrupted以外の失敗は拒否し、超過後は追加保持しない
     */
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !result.rejected
                    && count <= CAPTURE_LIMIT - result.bytes.len()
                { result.bytes.extend_from_slice(&buffer[..count]); }
                else { result.rejected = true; result.bytes.clear(); }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => { result.rejected = true; result.bytes.clear(); break; }
        }
    }
    result
}

/// pipeの到着前に起動するworkerへ渡す非公開reader。
type ReadJob = Option<Box<dyn Read + Send>>;

///
/// 子を起動する前にdrain workerを準備する。
///
/// # 引数
/// * `receiver` - pipeまたは起動中止を受け取るchannel
///
/// # 戻り値
/// 回収可能なworker。準備失敗時には子がまだ存在しない。
///
fn drain_worker(receiver: Receiver<ReadJob>) -> Result<JoinHandle<Drained>> {
    thread::Builder::new().spawn(move || match receiver.recv() {
        Ok(Some(reader)) => drain(reader),
        _ => Drained { bytes: Vec::new(), rejected: true },
    }).map_err(|_| anyhow!("capture worker"))
}

///
/// 子と両pipeを回収してから出力を解析可能な形へ返す。
///
/// # 概要
/// worker準備を子起動前に済ませ、子起動後は途中returnせず回収する。
///
/// # 引数
/// * `command` - 認証/対象を固定した製品コマンド
///
/// # 戻り値
/// 両stream完全回収時だけの出力。拒否時は固定診断だけ。
///
pub(crate) fn capture(command: &mut Command) -> Result<Captured> {
    let (out_tx, out_rx) = channel();
    let (err_tx, err_rx) = channel();
    let out_worker = drain_worker(out_rx)?;
    let err_worker = match drain_worker(err_rx) {
        Ok(worker) => worker,
        Err(error) => {
            let _sent = out_tx.send(None);
            let _joined = out_worker.join();
            return Err(error);
        }
    };
    /*
     * 両workerが準備できた後にだけ製品子を起動する
     */
    let spawned = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(_) => {
            let _out_sent = out_tx.send(None);
            let _err_sent = err_tx.send(None);
            let _out_joined = out_worker.join();
            let _err_joined = err_worker.join();
            return Err(anyhow!("CLI spawn"));
        }
    };
    let out_sent = out_tx.send(child.stdout.take()
        .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)).is_ok();
    let err_sent = err_tx.send(child.stderr.take()
        .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)).is_ok();
    /*
     * 期待外でもchildと他pipeを残したまま解析や失敗returnへ進まない
     */
    let status = wait_child(&mut child);
    match &status {
        Ok(status) => eprintln!("{}", status_record(status)),
        Err(_) => eprintln!("MS51 subprocess_exit=unknown signal=unknown"),
    }
    let stdout = out_worker.join();
    let stderr = err_worker.join();
    let code = status?.code();
    let stdout = stdout.map_err(|_| {
        eprintln!("MS51 output=rejected recovery=unknown");
        anyhow!("stdout worker")
    })?;
    let stderr = stderr.map_err(|_| {
        eprintln!("MS51 output=rejected recovery=unknown");
        anyhow!("stderr worker")
    })?;
    if !out_sent || !err_sent || stdout.rejected || stderr.rejected {
        eprintln!("MS51 output=rejected recovery=unknown");
        return Err(anyhow!("capture rejected"));
    }
    decode_capture(code, stdout, stderr)
}

///
/// 終了状態を数値のexit/signalだけへ写す。
///
/// # 引数
/// * `status` - 回収済みの子終了状態
///
/// # 戻り値
/// 生出力を含まない固定形式
///
pub fn status_record(status: &ExitStatus) -> String {
    format!("MS51 subprocess_exit={:?} signal={:?}",
        status.code(), status.signal())
}

///
/// 両streamが完全でUTF-8の場合だけ解析用出力へ変換する。
///
/// # 引数
/// * `code` - 子の実終了コード
/// * `stdout` - 回収済み標準出力
/// * `stderr` - 回収済み標準エラー
///
/// # 戻り値
/// 完全出力、または断片を表示しない固定拒否
///
pub fn decode_capture(code: Option<i32>, stdout: Drained, stderr: Drained)
    -> Result<Captured>
{
    let decoded = if stdout.rejected || stderr.rejected { None } else {
        String::from_utf8(stdout.bytes).ok()
            .zip(String::from_utf8(stderr.bytes).ok())
    };
    match decoded {
        Some((stdout, stderr)) => Ok(Captured { code, stdout, stderr }),
        None => {
            eprintln!("MS51 output=rejected recovery=unknown");
            Err(anyhow!("capture rejected"))
        }
    }
}

///
/// 割込を再待機して子の終了を回収する。
///
/// # 引数
/// * `child` - 実起動済みの製品子
///
/// # 戻り値
/// 実終了状態。時間保証や強制停止は追加しない。
///
fn wait_child(child: &mut Child) -> Result<ExitStatus> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(anyhow!("CLI wait")),
        }
    }
}

///
/// 単一pageの期待結果と独立観測IDを比較する。
///
/// # 引数
/// * `case` - case所有者
/// * `captured` - 期待表示を照合する子出力
/// * `page` - 単一page fixture
/// * `outcome` - 固定の期待結果
/// * `field` - 旧IDまたは新draft ID
/// * `state` - 独立観測状態
///
/// # 戻り値
/// 表示と確定IDの一致
///
pub fn report_identity(case: &Case, captured: &Captured, page: &Page,
    outcome: &str, field: &str, state: &State) -> Result<()>
{
    let path = case.page_path(page)?;
    captured.page(&path, outcome)?;
    ensure!(captured.id(&path, field)? == state.id, "CLI observed ID");
    Ok(())
}

///
/// 基本セルのN/S/D/Rを各別CLIで実施する。
///
/// # 概要
/// errorのS/D/Rも不変観測し、期待外では後続へ進まない。
///
/// # 引数
/// * `environment` - 明示実環境
/// * `mode` - 固定衝突mode
/// * `local_git` - ZIPまたはLocalGitの基本セル選択
///
/// # 戻り値
/// 4phaseの期待一致。Wiki自動復元やcleanupは行わない。
///
pub async fn run_cell(environment: &Environment, mode: Mode, local_git: bool)
    -> Result<()>
{
    let auth = if local_git { Auth::Bearer } else { Auth::Basic };
    let label = if local_git { "local" } else { "zip" };
    let case = Case::new(&format!("{label}-{}", mode.as_str()))?;
    let first = [Page { name: "page.md", body: F0 }];
    let changed = [Page { name: "page.md", body: F1 }];
    let git = if local_git { Some(GitFixture::new(&first, "main")?) }
        else { None };
    let mut observer = case.observer(environment, auth)?;
    case.absent(&mut observer, &first).await?;
    /*
     * 初回Nの作成と同一Sを確認し、それぞれの確定状態を保持する
     */
    eprintln!("MS51 phase=N");
    let created = run_version(&case, environment, auth, mode, &first,
        git.as_ref())?;
    created.expect_ms51(0)?;
    created.expect_counts([1, 0, 0, 0, 0, 0])?;
    let before = case.observe(&mut observer, &first).await?;
    report_identity(&case, &created, &first[0], "Created", "draft_id",
        &before[0])?;
    eprintln!("MS51 phase=S");
    let same = run_version(&case, environment, auth, mode, &first,
        git.as_ref())?;
    expect_existing(&case, &same, &first[0], mode)?;
    let same_state = case.observe(&mut observer, &first).await?;
    unchanged(&before[0], &same_state[0])?;
    /*
     * LocalGitだけはコミット版を変更してからDと明示Rへ進む
     */
    if let Some(git) = &git { git.commit(&changed)?; }
    eprintln!("MS51 phase=D");
    let difference = run_version(&case, environment, auth, mode, &changed,
        git.as_ref())?;
    let expected = if matches!(mode, Mode::Error) { &first } else { &changed };
    if matches!(mode, Mode::Error) {
        expect_existing(&case, &difference, &changed[0], mode)?;
    } else {
        difference.expect_ms51(0)?;
        if matches!(mode, Mode::Update) {
            difference.expect_counts([0, 0, 1, 0, 0, 0])?;
        } else { difference.expect_counts([0, 0, 0, 1, 0, 0])?; }
    }
    let after = case.observe(&mut observer, expected).await?;
    verify_difference(&case, &mut observer, &difference, &first[0], mode,
        &before[0], &after[0]).await?;
    eprintln!("MS51 phase=R");
    let rerun = run_version(&case, environment, auth, mode, &changed,
        git.as_ref())?;
    expect_existing(&case, &rerun, &changed[0], mode)?;
    let final_state = case.observe(&mut observer, expected).await?;
    unchanged(&after[0], &final_state[0])?;
    eprintln!("MS51 cell=accepted phases=4");
    Ok(())
}

///
/// 版ごとの入力所有者を子終了まで維持して一回実行する。
///
/// # 引数
/// * `case` - 設定とprefixの所有者
/// * `environment` - 実環境
/// * `auth` - Wiki認証方式
/// * `mode` - 衝突mode
/// * `pages` - 期待版の固定bytes
/// * `git` - コミット済みfixture、またはZIP選択
///
/// # 戻り値
/// 子の有限解析結果
///
fn run_version(case: &Case, environment: &Environment, auth: Auth, mode: Mode,
    pages: &[Page], git: Option<&GitFixture>) -> Result<Captured>
{
    if let Some(git) = git {
        let commit = git.git(&["rev-parse", "HEAD"])?;
        let source = PreparedSource::local(git.path(),
            Selector::Commit(commit.clone()))?;
        eprintln!("MS51 source=LocalGit commit={commit}");
        case.run(environment, auth, mode, &source, pages)
    } else {
        let zip = ZipFixture::new(pages, ZipKind::Normal)?;
        case.run(environment, auth, mode,
            &PreparedSource::zip(zip.path()), pages)
    }
}

///
/// 同一/再実行のmode別表示を確認する。
///
/// # 引数
/// * `case` - 固定case
/// * `captured` - 子結果
/// * `page` - 固定page
/// * `mode` - 衝突mode
///
/// # 戻り値
/// errorはFailed/Conflict/Lookup/1、その他はskip/0
///
fn expect_existing(case: &Case, captured: &Captured, page: &Page, mode: Mode)
    -> Result<()>
{
    /*
     * 同一/再実行の拒否またはskipを件数とともに検査する
     */
    let path = case.page_path(page)?;
    if matches!(mode, Mode::Error) {
        captured.expect_ms51(1)?;
        captured.expect_counts([0, 0, 0, 0, 1, 0])?;
        captured.page(&path, "Failed")?;
        ensure!(captured.stderr.contains("category=Conflict stage=Lookup"),
            "existing conflict facts");
    } else {
        captured.expect_ms51(0)?;
        captured.expect_counts([0, 1, 0, 0, 0, 0])?;
        captured.page(&path, "SkippedIdentical")?;
    }
    Ok(())
}

///
/// 独立観測したID/revisionの不変を確認する。
///
/// # 引数
/// * `before` - 先行観測
/// * `after` - 後続観測
///
/// # 戻り値
/// 同一ID/revision。本文は各observeで別途確認する。
///
fn unchanged(before: &State, after: &State) -> Result<()> {
    ensure!(before.id == after.id && before.revision == after.revision,
        "identity/revision changed");
    Ok(())
}

///
/// Dのmode別ID/revisionと旧soft-deleteを確認する。
///
/// # 概要
/// overwrite後の旧metaは既知IDに限り一回観測し復元しない。
///
/// # 引数
/// * `case` - 固定case
/// * `observer` - case独立観測者
/// * `captured` - Dの子結果
/// * `page` - 固定page
/// * `mode` - 衝突mode
/// * `before` - Nの独立状態
/// * `after` - Dの独立状態
///
/// # 戻り値
/// mode別変化の期待一致
///
async fn verify_difference(case: &Case, observer: &mut Observer,
    captured: &Captured, page: &Page, mode: Mode, before: &State, after: &State)
    -> Result<()>
{
    match mode {
        Mode::Error => unchanged(before, after),
        Mode::Update => {
            report_identity(case, captured, page, "Updated", "old_id", after)?;
            captured.expect_old_recovery(&case.page_path(page)?, &before.id,
                before.revision, false)?;
            ensure!(before.id == after.id
                && before.revision.checked_add(1) == Some(after.revision),
                "update identity/revision");
            Ok(())
        }
        Mode::Overwrite => {
            /*
             * 新IDの確定結果と旧IDの削除観測を分離する
             */
            report_identity(case, captured, page, "Recreated", "draft_id",
                after)?;
            captured.expect_old_recovery(&case.page_path(page)?, &before.id,
                before.revision, true)?;
            ensure!(before.id != after.id, "overwrite ID unchanged");
            metadata_started("P01", &before.id);
            let meta = observer.metadata(&before.id, &case.page_path(page)?)
                .await?;
            metadata_record("P01", &before.id, &meta);
            ensure!(meta.page_info.deleted
                && meta.page_info.path.kind == "last_deleted"
                && meta.revision_info.as_ref()
                    .is_some_and(|v| v.revision == before.revision),
                "old soft-delete not observed");
            eprintln!("MS51 page=P01 old_id={} deleted=true kind=last_deleted",
                before.id);
            Ok(())
        }
    }
}
