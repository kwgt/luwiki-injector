/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 合成loopbackでの設定7群と、明示実行する合成PTYの2入口
//!

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, ensure};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[path = "support/cli_http.rs"]
mod cli_http;
// 実環境load/observerは使用せず、共有captureの型だけを利用する。
#[allow(dead_code)]
#[path = "support/real_cli_http.rs"]
mod real_cli_http;
// 無警告の実Wiki用expectを変更せず、合成専用期待を下で定義する。
#[allow(dead_code)]
#[path = "support/real_cli_report.rs"]
mod real_cli_report;
// Git/実入力は生成せず、共有harnessの既存型依存だけを満たす。
#[allow(dead_code)]
#[path = "support/source_fixture.rs"]
mod source_fixture;
#[allow(dead_code)]
#[path = "support/ms5_1_harness.rs"]
mod ms5_1_harness;

use cli_http::{Reply, Server};
use real_cli_report::Captured;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const LOCK: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const LOCK2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const LOCK3: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";
const BODY: &[u8] = b"synthetic-config-page-body\r\n";
const TOKEN: &str = "synthetic-selected-token";
const LOWER: &str = "synthetic-lower-token";
const USER: &str = "synthetic-user";
const PASSWORD: &str = "synthetic-password-only";

///
/// 合成入力/設定だけを所有し、ユーザーの環境を子へ渡さない。
///
struct Fixture {
    /// 設定/ZIP/statusを閉じ込める一時領域。
    area: TempDir,
    /// 明示する合成設定。
    config: PathBuf,
    /// 外部ファイルを含まないZIP。
    source: PathBuf,
}

impl Fixture {
    ///
    /// 合成設定と指定した固定entry集合を保存する。
    ///
    /// # 概要
    /// 0700領域へZIPと設定を作り、設定権限だけを試験条件で変更する。
    ///
    /// # 引数
    /// * `document` - 秘密を含まない合成TOML
    /// * `files` - 固定entry/bytes
    /// * `permissions` - 設定ファイルの試験権限
    ///
    /// # 戻り値
    /// 子の回収まで保持するfixture
    ///
    fn new(document: &str, files: &[(&str, &[u8])], permissions: u32)
        -> Result<Self>
    {
        /*
         * statusの親を生成時から0700に限定する
         */
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir()
            .map_err(|_| anyhow!("config area"))?;
        let mode = area.path().metadata()
            .map_err(|_| anyhow!("config area metadata"))?.permissions().mode();
        ensure!(mode & 0o777 == 0o700, "config area permissions");
        let config = area.path().join("config.toml");
        let source = area.path().join("input.zip");
        /*
         * 合成設定だけを明示権限で保存する
         */
        fs::write(&config, document).map_err(|_| anyhow!("config fixture"))?;
        fs::set_permissions(&config, fs::Permissions::from_mode(permissions))
            .map_err(|_| anyhow!("config permissions"))?;
        let file = File::create(&source).map_err(|_| anyhow!("config ZIP"))?;
        let mut zip = ZipWriter::new(file);
        /*
         * コード内の合成entryだけをアーカイブへ書き込む
         */
        for (name, body) in files {
            zip.start_file(format!("root/{name}"), SimpleFileOptions::default())
                .map_err(|_| anyhow!("config ZIP entry"))?;
            zip.write_all(body).map_err(|_| anyhow!("config ZIP body"))?;
        }
        if files.is_empty() {
            zip.add_directory("root/", SimpleFileOptions::default())
                .map_err(|_| anyhow!("config ZIP root"))?;
        }
        zip.finish().map_err(|_| anyhow!("config ZIP finish"))?;
        Ok(Self { area, config, source })
    }

    ///
    /// 環境を空にした実製品CLIの構造だけを作る。
    ///
    /// # 戻り値
    /// 未起動の子。targetと合成認証は各試験で設定する。
    ///
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
        command.env_clear().env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.area.path()).env("RUST_LOG", "trace")
            .arg("--config-path").arg(&self.config).stdin(Stdio::null());
        command
    }

    ///
    /// 入力と任意の採用targetを付けて有限captureで一回起動する。
    ///
    /// # 引数
    /// * `command` - 合成値だけを設定した未起動CLI
    /// * `target` - 明示loopback。Noneならenv/TOMLのloopbackを採用。
    ///
    /// # 戻り値
    /// 生表示しない回収済み出力
    ///
    fn run(&self, mut command: Command, target: Option<&str>)
        -> Result<Captured>
    {
        command.arg(&self.source);
        if let Some(target) = target { command.arg(target); }
        ms5_1_harness::capture(&mut command)
    }
}

///
/// 実Wiki用の無警告期待と分離した合成結果の完全な期待。
///
struct SyntheticExpectation {
    /// 製品の期待終了コード。
    exit: i32,
    /// 計画が成立したか。
    planned: bool,
    /// 起動警告の期待件数。
    warnings: u64,
    /// created/skipped/updated/recreated/failed/not_executed。
    counts: [u64; 6],
    /// 主原因を識別する固定診断。成功時はNone。
    cause: Option<&'static str>,
}

impl SyntheticExpectation {
    ///
    /// plan/exit/件数/警告/主原因と非露出を照合する。
    ///
    /// # 概要
    /// 不一致でもraw出力やダミー値を診断へ転記しない。
    ///
    /// # 引数
    /// * `captured` - 両streamを回収した合成CLI結果
    ///
    /// # 戻り値
    /// 期待一致、または固定診断
    ///
    fn check(&self, captured: &Captured) -> Result<()> {
        let plan = if self.planned { "established" } else { "not-established" };
        ensure!(captured.code == Some(self.exit)
            && captured.stdout.starts_with(&format!(
                "result: exit={} plan={plan}\n", self.exit)), "config result");
        let [created, skipped, updated, recreated, failed, not_executed] =
            self.counts;
        let summary = format!("created={created} skipped={skipped} \
            updated={updated} recreated={recreated} warnings={} \
            failed={failed} not_executed={not_executed}", self.warnings);
        /*
         * 集計の完全一致と独立した起動警告行数を確認する
         */
        let summaries: Vec<_> = captured.stdout.lines()
            .filter(|line| line.starts_with("created=")).collect();
        ensure!(summaries == [summary.as_str()], "config summary");
        ensure!(captured.stderr.lines().filter(|line|
            line.starts_with("warning:")).count() as u64 == self.warnings,
            "config warning count");
        let errors: Vec<_> = captured.stderr.lines()
            .filter(|line| line.starts_with("error:")).collect();
        match self.cause {
            Some(cause) => ensure!(!errors.is_empty()
                && errors.iter().any(|line| line.contains(cause)),
                "config primary cause"),
            None => ensure!(errors.is_empty(), "unexpected config failure"),
        }
        /*
         * 合成秘密/本文も出力させず、stdout/stderr分離を維持する
         */
        check_private(captured)?;
        eprintln!("MS51 synthetic exit={} plan={plan} warnings={}",
            self.exit, self.warnings);
        Ok(())
    }
}

///
/// 固定ダミー値と認証header/本文の非露出を検査する。
///
/// # 引数
/// * `captured` - 非公開の子出力
///
/// # 戻り値
/// 非露出/出力分離の一致
///
fn check_private(captured: &Captured) -> Result<()> {
    let basic = basic()?;
    for text in [&captured.stdout, &captured.stderr] {
        /*
         * 平文だけでなく完全なBasic認証表現も外部出力から拒否する
         */
        ensure!(!text.contains(basic.to_str()
            .map_err(|_| anyhow!("synthetic header"))?),
            "synthetic authorization output");
        for value in [TOKEN, LOWER, USER, PASSWORD, LOCK, LOCK2, LOCK3,
            "synthetic-lower-user", "synthetic-lower-password",
            "synthetic-config-page-body", "Authorization", "X-Lock-"]
        { ensure!(!text.contains(value), "synthetic private output"); }
    }
    ensure!(!captured.stdout.contains("error:")
        && !captured.stdout.contains("warning:")
        && !captured.stderr.contains("result: exit="), "config stream");
    Ok(())
}

///
/// 正常作成の期待を組み立てる。
///
/// # 引数
/// * `warnings` - 起動警告件数
///
/// # 戻り値
/// 正常適用は警告あり3、なし0
///
fn created(warnings: u64) -> SyntheticExpectation {
    SyntheticExpectation { exit: if warnings == 0 { 0 } else { 3 },
        planned: true, warnings, counts: [1, 0, 0, 0, 0, 0], cause: None }
}

///
/// 起動拒否の期待を組み立てる。
///
/// # 引数
/// * `warnings` - 拒否後にも残る起動警告件数
/// * `cause` - 固定主原因
///
/// # 戻り値
/// 計画未成立/終了2
///
fn rejected(warnings: u64, cause: &'static str) -> SyntheticExpectation {
    SyntheticExpectation { exit: 2, planned: false, warnings,
        counts: [0; 6], cause: Some(cause) }
}

///
/// 合成Bearerの期待headerを生成する。
///
/// # 引数
/// * `token` - コード内のダミー値
///
/// # 戻り値
/// loopback照合専用header
///
fn bearer(token: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow!("synthetic bearer"))
}

///
/// 合成Basicの期待headerをHTTPなしで生成する。
///
/// # 戻り値
/// dummy user/passwordの正規Basic header
///
fn basic() -> Result<HeaderValue> {
    reqwest::Client::builder().no_proxy().build()
        .map_err(|_| anyhow!("synthetic client"))?.get("http://127.0.0.1/")
        .basic_auth(USER, Some(PASSWORD)).build()
        .map_err(|_| anyhow!("synthetic basic"))?.headers()
        .get(AUTHORIZATION).cloned().ok_or_else(|| anyhow!("synthetic basic"))
}

///
/// 一件の新規作成を完了させる固定応答列。
///
/// # 戻り値
/// lookup/POST/renew/source PUTの四応答
///
fn creation() -> Vec<Reply> {
    vec![Reply::new(404, b"{}"),
        Reply::new(201, format!("{{\"id\":\"{ID}\"}}").as_bytes())
            .locked(LOCK), Reply::new(204, b"").locked(LOCK2),
        Reply::new(204, b"")]
}

///
/// 完全要求数と合成認証一致を確認する。
///
/// # 引数
/// * `server` - 専用loopback
/// * `count` - 期待要求数
///
/// # 戻り値
/// 余分な要求なし/認証一致
///
fn requests(server: &Server, count: usize) -> Result<()> {
    server.inspect(|values| {
        ensure!(values.len() == count
            && values.iter().all(|value| value.authenticated
                && value.lock_auth == (value.method == "PUT")),
            "synthetic request mismatch");
        Ok(())
    })
}

///
/// URL優先の一代表を採用先と不採用listenerで照合する。
///
/// # 概要
/// CLI/env/TOMLの各採用先だけへ四要求が届くことを確認する。
///
/// # 引数
/// * `level` - CLI=0、env=1、TOML=2
///
/// # 戻り値
/// 優先と不採用先HTTP0の一致
///
fn url_case(level: usize) -> Result<()> {
    let selected = Server::with_auth(creation(), bearer(TOKEN)?);
    let lower_env = Server::new(Vec::new());
    let lower_toml = Server::new(Vec::new());
    let config_url = if level == 2 { &selected.url } else { &lower_toml.url };
    let document = format!("[server]\nurl='{config_url}'\ntoken='{TOKEN}'\n");
    let fixture = Fixture::new(&document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    /*
     * 最上位だけが採用loopbackを指すように候補を分ける
     */
    if level < 2 {
        let env_url = if level == 1 { &selected.url } else { &lower_env.url };
        command.env("LUWIKI_URL", env_url);
    }
    let target = if level == 0 { Some(selected.url.as_str()) } else { None };
    created(0).check(&fixture.run(command, target)?)?;
    requests(&selected, 4)?;
    requests(&lower_env, 0)?;
    requests(&lower_toml, 0)
}

///
/// URLのCLI/env/TOML優先と不採用listener要求0を確認する。
///
/// # 注記
/// 各採用先は専用loopback。既定8080へは接続しない。
///
/// # 戻り値
/// 三つの採用経路の期待一致
///
#[test]
fn ms51_config_url_priority_and_unused_targets() -> Result<()> {
    for level in 0..3 { url_case(level)?; }
    Ok(())
}

///
/// 指定prefixの既存相違pageへupdateを適用する応答列。
///
/// # 戻り値
/// 旧ID/revisionとlock再照合を含む十応答
///
fn updating() -> Vec<Reply> {
    let identity = || Reply::new(200,
        format!("{{\"id\":\"{ID}\"}}").as_bytes());
    let metadata = |locked| Reply::new(200, format!(
        "{{\"page_info\":{{\"path\":{{\"kind\":\"current\",\
        \"value\":\"/cli/page\"}},\"deleted\":false,\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}").as_bytes());
    vec![identity(), metadata(false), Reply::new(200, b"before"),
        Reply::new(204, b"").locked(LOCK),
        Reply::new(204, b"").locked(LOCK2), metadata(true), identity(),
        Reply::new(200, b"before"), Reply::new(204, b"").locked(LOCK3),
        Reply::new(204, b"")]
}

///
/// prefix/modeのCLI指定がTOMLより優先されることを確認する。
///
/// # 概要
/// TOMLはoverwrite/別prefix、CLIはupdate/既定の専用prefixへ上書きする。
///
/// # 注記
/// 更新一回、削除なし、lookup pathと送信本文を要求列で照合する。
///
/// # 戻り値
/// CLI優先と実updateの一致
///
#[test]
fn ms51_config_mode_and_prefix_cli_override() -> Result<()> {
    let server = Server::with_auth(updating(), bearer(TOKEN)?);
    let document = format!("[server]\ntoken='{TOKEN}'\n\
        [page]\nprefix='/toml'\nmode='overwrite'\n");
    let fixture = Fixture::new(&document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    command.args(["--prefix", "/cli", "--mode", "update"]);
    let result = fixture.run(command, Some(&server.url))?;
    SyntheticExpectation { exit: 0, planned: true, warnings: 0,
        counts: [0, 0, 1, 0, 0, 0], cause: None }.check(&result)?;
    /*
     * 別prefixやoverwriteに進まず、本文PUTが一回だけである
     */
    requests(&server, 10)?;
    result.page("/cli/page", "Updated")?;
    server.inspect(|values| {
        let url = url::Url::parse(&format!("{}{}", server.url,
            values[0].target)).map_err(|_| anyhow!("fixture query"))?;
        ensure!(url.query_pairs().any(|(key, value)|
            key == "path" && value == "/cli/page"), "selected prefix");
        ensure!(!values.iter().any(|value| value.method == "DELETE"),
            "unexpected overwrite");
        let puts: Vec<_> = values.iter().filter(|value|
            value.target.ends_with("/source") && value.method == "PUT")
            .collect();
        ensure!(puts.len() == 1 && puts[0].body == BODY, "update bytes");
        Ok(())
    })
}

///
/// Bearer項目のCLI/env/TOML優先を一回の作成で確認する。
///
/// # 引数
/// * `level` - CLI=0、env=1、TOML=2
///
/// # 戻り値
/// headerの一致と警告/終了の一致
///
fn bearer_case(level: usize) -> Result<()> {
    let server = Server::with_auth(creation(), bearer(TOKEN)?);
    let configured = if level == 2 { TOKEN } else { LOWER };
    let document = format!("[server]\ntoken='{configured}'\n");
    let fixture = Fixture::new(&document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    /*
     * 同方式内で選ぶ値だけを変え、方式の競合と分離する
     */
    if level == 0 {
        command.args(["--token", TOKEN]).env("LUWIKI_TOKEN", LOWER);
    } else if level == 1 { command.env("LUWIKI_TOKEN", TOKEN); }
    created(u64::from(level == 0))
        .check(&fixture.run(command, Some(&server.url))?)?;
    requests(&server, 4)
}

///
/// Basic各項目を異なる入力元から採用する代表を確認する。
///
/// # 引数
/// * `cli_user` - CLI user/env password、またはenv user/TOML password
///
/// # 戻り値
/// 正規Basic headerと期待警告件数の一致
///
fn basic_case(cli_user: bool) -> Result<()> {
    let server = Server::with_auth(creation(), basic()?);
    let document = format!("[server]\nuser='synthetic-lower-user'\n\
        password='{}'\n", if cli_user { "synthetic-lower-password" }
        else { PASSWORD });
    let fixture = Fixture::new(&document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    /*
     * user/passwordは独立に解決し、実資格情報を一切使用しない
     */
    if cli_user {
        command.args(["--user", USER]).env("LUWIKI_USER",
            "synthetic-lower-user").env("LUWIKI_PASSWORD", PASSWORD);
    } else { command.env("LUWIKI_USER", USER); }
    created(u64::from(cli_user))
        .check(&fixture.run(command, Some(&server.url))?)?;
    requests(&server, 4)
}

///
/// TOML userとCLI passwordを独立採用する代表を確認する。
///
/// # 注記
/// 下位のenv/TOML passwordは採用せず、合成Basic headerを照合する。
///
/// # 戻り値
/// userのTOML採用とpasswordのCLI優先、警告1/終了3の一致
///
fn basic_password_case() -> Result<()> {
    let server = Server::with_auth(creation(), basic()?);
    let document = format!("[server]\nuser='{USER}'\n\
        password='synthetic-lower-password'\n");
    let fixture = Fixture::new(&document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    command.args(["--password", PASSWORD])
        .env("LUWIKI_PASSWORD", "synthetic-lower-password");
    created(1).check(&fixture.run(command, Some(&server.url))?)?;
    requests(&server, 4)
}

///
/// 認証の項目別優先と方式混在の拒否を確認する。
///
/// # 注記
/// 合成Bearer三経路/Basic三経路と、下位Basicを隠さない競合を検査する。
///
/// # 戻り値
/// 採用headerと方式排他の一致
///
#[test]
fn ms51_config_auth_field_priority_and_conflict() -> Result<()> {
    /*
     * 同方式の項目別優先をそれぞれ独立した正常作成で識別する
     */
    for level in 0..3 { bearer_case(level)?; }
    for cli_user in [true, false] { basic_case(cli_user)?; }
    basic_password_case()?;
    /*
     * 下位の異方式資格情報は隠さず、HTTP前の競合として拒否する
     */
    let server = Server::new(Vec::new());
    let fixture = Fixture::new("", &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    command.args(["--token", TOKEN]).env("LUWIKI_USER", USER);
    rejected(1, "BearerとBasicが同時指定されています")
        .check(&fixture.run(command, Some(&server.url))?)?;
    requests(&server, 0)
}

///
/// 不採用TOMLでも構文/型/未知キー検証を省略しない。
///
/// # 注記
/// CLI/envの上位正常値を用意し、全caseで起動2・要求0を期待する。
///
/// # 戻り値
/// TOML読込時検証の一致
///
#[test]
fn ms51_config_toml_structure_is_always_validated() -> Result<()> {
    for document in ["[server\n", "[server]\nurl=17\n",
        "[server]\nunknown='synthetic-value'\n"]
    {
        let server = Server::new(Vec::new());
        let fixture = Fixture::new(document, &[("page.md", BODY)], 0o600)?;
        let mut command = fixture.command();
        command.env("LUWIKI_TOKEN", TOKEN);
        rejected(0, "設定ファイル")
            .check(&fixture.run(command, Some(&server.url))?)?;
        requests(&server, 0)?;
    }
    Ok(())
}

///
/// 採用値だけの意味検証と空入力のHTTP0を確認する。
///
/// # 概要
/// 上位正常値は下位不正値を覆い、採用不正URLはHTTP前に拒否する。
///
/// # 注記
/// 空ZIPでも専用loopbackを明示し、既定8080へ接続させない。
///
/// # 戻り値
/// 値選択の意味検証と空計画/HTTP0の一致
///
#[test]
fn ms51_config_only_selected_values_are_validated() -> Result<()> {
    let server = Server::with_auth(creation(), bearer(TOKEN)?);
    let document = "[server]\nurl='invalid'\ntoken=''\n\
        [page]\nprefix='invalid'\nmode='invalid'\n";
    let fixture = Fixture::new(document, &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    command.args(["--mode", "update", "--prefix", "/fixture"])
        .env("LUWIKI_URL", "invalid")
        .env("LUWIKI_TOKEN", TOKEN);
    created(0).check(&fixture.run(command, Some(&server.url))?)?;
    requests(&server, 4)?;
    /*
     * 上位に採用した不正値は下位正常値へfallbackさせない
     */
    let fixture = Fixture::new("", &[("page.md", BODY)], 0o600)?;
    let mut command = fixture.command();
    command.env("LUWIKI_TOKEN", TOKEN).env("LUWIKI_URL", &server.url);
    let invalid_target = format!("{}/invalid", server.url);
    rejected(0, "対象Wiki URLが不正です")
        .check(&fixture.run(command, Some(&invalid_target))?)?;
    requests(&server, 4)?;
    /*
     * 空入力も明示loopbackに限定し、Wiki HTTP0を直接観測する
     */
    let empty_target = Server::new(Vec::new());
    let fixture = Fixture::new("", &[("plain.txt", b"ordinary")], 0o600)?;
    let mut command = fixture.command();
    command.env("LUWIKI_TOKEN", TOKEN);
    SyntheticExpectation { exit: 0, planned: true, warnings: 0,
        counts: [0; 6], cause: None }
        .check(&fixture.run(command, Some(&empty_target.url))?)?;
    requests(&empty_target, 0)
}

///
/// 警告caseごとに正常3・実行失敗1・入力不正2の優先を確認する。
///
/// # 概要
/// 合成CLI秘密と広い権限の合成認証設定を別々に検査する。
///
/// # 引数
/// * `wide` - 広いTOML権限、falseなら合成CLI秘密
/// * `outcome` - 正常0、実行失敗1、入力不正2のfixture選択
///
/// # 戻り値
/// 警告保持と終了優先の一致
///
fn warning_case(wide: bool, outcome: usize) -> Result<()> {
    let replies = if outcome == 0 { creation() }
        else { vec![Reply::new(403, b"{}")] };
    let server = Server::with_auth(replies, bearer(TOKEN)?);
    let document = if wide { format!("[server]\ntoken='{TOKEN}'\n") }
        else { String::new() };
    let bytes = if outcome == 2 { &b"\xff"[..] } else { BODY };
    let fixture = Fixture::new(&document, &[("page.md", bytes)],
        if wide { 0o644 } else { 0o600 })?;
    let mut command = fixture.command();
    if !wide { command.args(["--token", TOKEN]); }
    let captured = fixture.run(command, Some(&server.url))?;
    /*
     * 入力不正と実行失敗は警告だけの終了3より優先する
     */
    match outcome {
        0 => created(1).check(&captured)?,
        1 => SyntheticExpectation { exit: 1, planned: true, warnings: 1,
            counts: [0, 0, 0, 0, 1, 0],
            cause: Some("category=Permission stage=Lookup") }
            .check(&captured)?,
        2 => rejected(1, "入力検証に失敗しました").check(&captured)?,
        _ => return Err(anyhow!("synthetic warning selection")),
    }
    requests(&server, if outcome == 0 { 4 }
        else if outcome == 1 { 1 } else { 0 })
}

///
/// 合成秘密argv/広い権限TOMLの警告と終了優先を確認する。
///
/// # 注記
/// 各警告種別で正常3/実行失敗1/入力不正2を別起動する。
///
/// # 戻り値
/// 六つの警告条件の期待一致
///
#[test]
fn ms51_config_warning_exit_precedence() -> Result<()> {
    for wide in [false, true] {
        for outcome in 0..3 { warning_case(wide, outcome)?; }
    }
    Ok(())
}

///
/// 非TTYの認証不足/排他は対話もHTTPも行わず拒否する。
///
/// # 注記
/// 実秘密を含めず、認証なし/passwordのみ/userのみ/方式混在を確認する。
///
/// # 戻り値
/// 各起動2とloopback要求0
///
#[test]
fn ms51_config_non_tty_authentication_stops_before_http() -> Result<()> {
    /*
     * 認証不足と方式競合を独立した非TTY子で拒否する
     */
    for (values, cause) in [
        (vec![], "Wiki認証情報が不足しています"),
        (vec![("LUWIKI_PASSWORD", PASSWORD)], "Wiki認証情報が不足しています"),
        (vec![("LUWIKI_USER", USER)], "Basicパスワードが不足しています"),
        (vec![("LUWIKI_TOKEN", TOKEN), ("LUWIKI_USER", USER)],
            "BearerとBasicが同時指定されています")]
    {
        let server = Server::new(Vec::new());
        let fixture = Fixture::new("", &[("page.md", BODY)], 0o600)?;
        let mut command = fixture.command();
        command.envs(values);
        rejected(0, cause).check(&fixture.run(command, Some(&server.url))?)?;
        requests(&server, 0)?;
    }
    Ok(())
}

///
/// PTY補助の閉じた有限status schema。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PtyStatus {
    /// この契約のschema番号。
    schema: u32,
    /// 補助の分類。成功時も製品exitと分離する。
    wrapper: String,
    /// 固定promptを観測したか。
    prompt_seen: bool,
    /// 合成passwordのechoを観測したか。
    echo_seen: bool,
    /// 補助期限に達したか。
    timed_out: bool,
    /// 三streamのいずれかが拒否されたか。
    capture_rejected: bool,
    /// 子の回収が完了したか。
    reaped: bool,
    /// 製品exit。signal終了を0へ補完しない。
    child_exit: Option<i32>,
    /// 製品signal。正常数値exitと混在させない。
    child_signal: Option<i32>,
}

///
/// 保護statusを有限に読み、補助/製品の終了状態を別に検査する。
///
/// # 概要
/// サイズ/権限/schemaを検査してから、正常回収した製品exitだけを返す。
///
/// # 引数
/// * `path` - 自分で0600/create_newしたstatus
/// * `captured` - 補助の非公開stdout/stderr/exit
///
/// # 戻り値
/// 信頼できる製品exit。欠落/異常は固定失敗。
///
fn pty_status(path: &PathBuf, captured: &Captured) -> Result<i32> {
    /*
     * 自分のstatusの型と権限を確認し、同じhandleから有限に読む
     */
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow!("PTY status metadata"))?;
    ensure!(metadata.file_type().is_file() && metadata.len() <= 4096
        && metadata.permissions().mode() & 0o777 == 0o600,
        "PTY status boundary");
    let file = File::open(path).map_err(|_| anyhow!("PTY status open"))?;
    let opened = file.metadata().map_err(|_| anyhow!("PTY status handle"))?;
    ensure!(opened.file_type().is_file() && opened.len() <= 4096
        && opened.permissions().mode() & 0o777 == 0o600,
        "PTY status handle boundary");
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)
        .map_err(|_| anyhow!("PTY status read"))?;
    ensure!(bytes.len() <= 4096, "PTY status size");
    ensure!(!bytes.is_empty(), "PTY status empty");
    let document = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("PTY status encoding"))?;
    /*
     * 閉じたschemaと補助の成功/回収を製品終了より先に確認する
     */
    let status: PtyStatus = toml::from_str(document)
        .map_err(|_| anyhow!("PTY status schema"))?;
    ensure!(captured.code == Some(0) && status.schema == 1
        && status.wrapper == "ok" && status.prompt_seen && !status.echo_seen
        && !status.timed_out && !status.capture_rejected && status.reaped
        && status.child_signal.is_none(), "PTY wrapper failure");
    let exit = status.child_exit.ok_or_else(|| anyhow!("PTY child exit"))?;
    ensure!((0..=255).contains(&exit), "PTY child exit range");
    eprintln!("MS51 PTY wrapper_exit=0 child_exit={exit} reaped=true");
    Ok(exit)
}

///
/// 合成passwordだけを実PTYへ一回送る明示試験を実施する。
///
/// # 概要
/// Python補助の成功/回収statusを検査してから製品出力を解析する。
///
/// # 引数
/// * `empty` - 空passwordを送るか
///
/// # 戻り値
/// 補助と製品の期待一致。実Wiki/実秘密は使わない。
///
fn pty_case(empty: bool) -> Result<()> {
    let server = Server::with_auth(creation(), basic()?);
    let fixture = Fixture::new("", &[("page.md", BODY)], 0o600)?;
    let status = fixture.area.path().join("status.toml");
    OpenOptions::new().write(true).create_new(true).mode(0o600)
        .open(&status).map_err(|_| anyhow!("PTY status preparation"))?;
    let mut command = Command::new("/usr/bin/python3");
    /*
     * 補助のargv/envも合成値だけに固定し、実環境を読ませない
     */
    command.env_clear().env("PATH", "/usr/bin:/bin").args(["-I", "-S"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/pty_cli.py"))
        .arg("--binary").arg(env!("CARGO_BIN_EXE_luwiki-injector"))
        .arg("--config").arg(&fixture.config)
        .arg("--source").arg(&fixture.source).arg("--url").arg(&server.url)
        .args(["--prefix", "/fixture", "--status"]).arg(&status)
        .args(["--password-mode", if empty { "empty" } else { "valid" }])
        .stdin(Stdio::null());
    let mut captured = ms5_1_harness::capture(&mut command)?;
    captured.code = Some(pty_status(&status, &captured)?);
    /*
     * wrapper exit0を製品成功に補完せず、製品exitを個別に照合する
     */
    if empty {
        rejected(0, "Basic passwordが空です").check(&captured)?;
        requests(&server, 0)
    } else {
        created(0).check(&captured)?;
        requests(&server, 4)
    }
}

///
/// 実PTYの非echo入力から合成Basicの正常作成を確認する。
///
/// # 注記
/// Linux/Python3の明示入口。合成ZIPとloopbackだけを使用する。
///
/// # 戻り値
/// prompt/非echo/回収と製品正常0の一致
///
#[test]
#[ignore = "requires explicit Linux Python3 PTY execution"]
fn ms51_cli_tty_password() -> Result<()> { pty_case(false) }

///
/// 実PTYの空passwordで起動拒否とHTTP0を確認する。
///
/// # 注記
/// 補助成功0と製品拒否2を別に照合する明示入口。
///
/// # 戻り値
/// prompt/非echo/回収と製品拒否2の一致
///
#[test]
#[ignore = "requires explicit Linux Python3 PTY execution"]
fn ms51_cli_tty_empty_password() -> Result<()> { pty_case(true) }
