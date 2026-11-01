/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 取得・検証・計画のCLI接続と実行失敗/入力拒否の必要試験
//!

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

// 共有fixtureの未使用操作は他のCLI系列が利用する。
#[allow(dead_code)]
#[path = "support/cli_http.rs"]
mod cli_http;

///
/// 一時ZIPへ任意のentry順と本文を保持する。
///
/// # 引数
/// * `entries` - ZIPへの格納順と本文
///
/// # 戻り値
/// 元入力の所有者とCLIへ渡すZIPパス
///
fn archive(entries: &[(&str, &[u8])]) -> (TempDir, PathBuf) {
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    /*
     * 圧縮順を固定し、非Markdownも同じ取得境界へ通す
     */
    for (name, bytes) in entries {
        zip.start_file(format!("root/{name}"), SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    if entries.is_empty() {
        zip.add_directory("root/", SimpleFileOptions::default()).unwrap();
    }
    zip.finish().unwrap();
    (area, path)
}

///
/// rootを自動付与せず、指定した生名のZIPを作る。
///
/// # 概要
/// root不在や外側metadataを製品CLIで検証する入力を分離する。
///
/// # 引数
/// * `entries` - そのまま格納する生の相対名と本文
///
/// # 戻り値
/// 元入力の所有者とZIPパス。
///
fn raw_archive(entries: &[(&str, &[u8])]) -> (TempDir, PathBuf) {
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("raw.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    /*
     * productのroot選択をfixture側で代行しない
     */
    for (name, bytes) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    (area, path)
}

///
/// root不在/大小文字違い/通常fileをWiki照会前に拒否する。
///
/// # 概要
/// 実CLIを専用loopbackへ接続し、exit2とHTTP0を確認する。
///
/// # 注記
/// source最上位へfallbackできるREADMEも、意図的にroot外へ置く。
///
/// # 戻り値
/// なし。旧ルートへfallbackした場合やHTTPを発行した場合は失敗する。
///
#[test]
fn root_requirement_rejects_before_wiki_http() {
    /*
     * rootを補わない三つの入力をそれぞれ別CLIへ渡す
     */
    for name in ["README.md", "Root/a.md", "root"] {
        let (_area, path) = raw_archive(&[(name, b"metadata")]);
        let (output, requests) = execute(&path, &[], false);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(requests, 0);
    }
}

///
/// root外の不正本文を除外し、root内だけをprefixへ投影する。
///
/// # 概要
/// 実際のlookup503まで進め、入力エラーとの区別と元source参照を確認する。
///
/// # 注記
/// READMEと.mdという外側metadataを不正UTF-8にしても検証対象にしない。
///
/// # 戻り値
/// なし。root外検証/投影や一段除去の不一致で失敗する。
///
#[test]
fn root_mapping_ignores_project_metadata_in_real_cli() {
    /*
     * root内だけをページとして送信準備し、外側は候補へ加えない
     */
    let (_area, path) = raw_archive(&[("README.md", b"\xff"),
        (".md", b"\xff"), ("root/a.md", b"unique-body")]);
    let (output, requests) = execute(&path, &["--prefix", "/target"], false);
    let (stdout, _) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(requests, 1);
    assert!(stdout.contains("page=/target/a source=Zip:root/a.md"));
    assert!(!stdout.contains("README"));
    assert!(stdout.contains("not_executed=0"));
}

///
/// Wiki接続先を監視し、関連環境を隔離して実CLIを実行する。
///
/// # 概要
/// 成立した非空計画のlookupへ503を返し、空/不正入力の要求ゼロも確認する。
/// 設定認証は0600、警告ケースだけダミーtokenをCLIにも指定する。
///
/// # 引数
/// * `input` - 取得対象のローカルfixture
/// * `options` - prefixなどの追加指定
/// * `warning` - CLI秘密値指定警告を発生させるか
///
/// # 戻り値
/// 終了結果とWiki接続数
///
fn execute(input: &Path, options: &[&str], warning: bool) -> (Output, usize) {
    let area = tempfile::tempdir().unwrap();
    let config = area.path().join("config.toml");
    fs::write(&config, "[server]\ntoken='unique-wiki-secret'\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    /*
     * 全要求を観察し、製品接続には専用503を返す
     */
    let server = cli_http::Server::new(Vec::new());
    /*
     * ユーザーの認証・設定・ログ環境を子プロセスへ渡さない
     */
    let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
    command.args(["--config-path"]).arg(config).args(options)
        .arg(input).arg(&server.url).stdin(Stdio::null())
        .env("XDG_CONFIG_HOME", area.path()).env("RUST_LOG", "trace");
    if warning { command.args(["--token", "unique-cli-secret"]); }
    for name in ["LUWIKI_URL", "LUWIKI_TOKEN", "LUWIKI_USER", "LUWIKI_PASSWORD",
        "GITHUB_TOKEN", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR",
        "GIT_INDEX_FILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy"]
    {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    let requests = server.inspect(|requests| requests.len());
    (output, requests)
}

///
/// 結果と診断をUTF-8として取り出して共通の非露出を確認する。
///
/// # 引数
/// * `output` - 子プロセスの二つの出力
///
/// # 戻り値
/// stdoutとstderr
///
fn text(output: &Output) -> (String, String) {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    for secret in ["unique-wiki-secret", "unique-cli-secret", "unique-body"] {
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
    }
    (stdout, stderr)
}

///
/// ZIP計画の決定順を保ち、最初のWiki照会失敗で後続を止める。
///
/// # 概要
/// 親不在の日本語子・ASCII親子を逆順で入力し、表示順を明示照合する。
///
/// # 注記
/// 先頭lookup503を終了1とし、後続ページへ要求を発行しない。
///
/// # 戻り値
/// なし。ZIP計画順・先頭失敗・後続未実行の契約が異なれば失敗する。
///
#[test]
fn zip_plan_stops_after_first_lookup_failure_in_order() {
    let (_area, path) = archive(&[
        ("日本語/子.md", b"unique-body\r\n"),
        ("a/deep.md", b"child"), ("z.md", b"last"), ("a.md", b"parent"),
    ]);
    let (output, requests) = execute(&path, &["--prefix", "/初期"], false);
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(requests, 1);
    assert!(stdout.contains("plan=established"));
    assert!(stdout.contains("completed=0"));
    assert!(stdout.contains("failed=1 not_executed=3"));
    assert!(stderr.contains("category=Http(503)"));
    /*
     * 計画順と非適用結果だけを確認し、本文は表示されない
     */
    let pages: Vec<_> = stdout.lines().filter(|line|
        line.starts_with("page=")).collect();
    assert_eq!(pages.len(), 4);
    for (index, (line, path)) in pages.iter().zip([
        "/初期/a", "/初期/z", "/初期/a/deep", "/初期/日本語/子",
    ]).enumerate() {
        assert!(line.starts_with(&format!("page={path} ")));
        let outcome = if index == 0 { "outcome=Failed" }
            else { "outcome=NotExecuted" };
        assert!(line.contains(outcome));
    }
}

///
/// 安全検査済み空計画は通常0、起動警告付きは3になる。
///
/// # 注記
/// 非Markdown本文の不正UTF-8を新しい制約にせず、.MDをページ化しない。
///
/// # 戻り値
/// なし。空計画の終了0/3・起動警告表示・Wiki要求ゼロが異なれば失敗する。
///
#[test]
fn empty_plan_distinguishes_normal_and_startup_warning() {
    let (_area, path) = archive(&[("plain.txt", b"\xff"), ("upper.MD", b"x")]);
    /*
     * 同じ空入力を警告の有無だけ変えてCLIへ渡す
     */
    for (warning, code) in [(false, 0), (true, 3)] {
        let (output, requests) = execute(&path, &[], warning);
        let (stdout, stderr) = text(&output);
        assert_eq!(output.status.code(), Some(code));
        assert_eq!(requests, 0);
        assert!(stdout.contains("plan=established"));
        assert!(stdout.contains("not_executed=0"));
        assert!(!stderr.contains("実行処理はまだ利用できません"));
        assert_eq!(stderr.contains("warning:"), warning);
    }
}

///
/// 本文不正と空の投影名は警告を保持して計画成立前の終了2になる。
///
/// # 概要
/// 非Markdownの危険名も安全検査対象として拒否する。
///
/// # 注記
/// 取得済み不正をsafe Inputへ写し、Wiki要求や部分ページ表示をしない。
///
/// # 戻り値
/// なし。入力不正の計画前終了2・警告保持・部分表示なしが異なれば失敗する。
///
#[test]
fn invalid_inputs_preserve_warnings_without_exposing_body() {
    /*
     * 名前と本文の拒否を製品CLIから確認する
     */
    for (name, body) in [("page.md", &b"\xffunique-body"[..]),
        (".md", &b"unique-body"[..]), ("bad name.txt", &b"plain"[..])]
    {
        let (_area, path) = archive(&[(name, body)]);
        let (output, requests) = execute(&path, &[], true);
        let (stdout, stderr) = text(&output);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(requests, 0);
        assert!(stdout.contains("plan=not-established"));
        assert!(stdout.contains("warnings=1"));
        assert!(!stdout.contains("page="));
        assert!(stderr.contains("入力検証に失敗"));
        assert!(stderr.contains("warning:"));
    }
}

///
/// 一時ローカルrepoでだけGit CLIを使いfixtureを構築する。
///
/// # 引数
/// * `path` - 外部へ接続しない一時repo
/// * `args` - Git構築用引数
///
/// # 戻り値
/// なし。構築の成功を確認する。
///
fn git(path: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"]).args(args).current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR").env_remove("GIT_INDEX_FILE")
        .env_remove("GITHUB_TOKEN").stdout(Stdio::null()).stderr(Stdio::null())
        .status().unwrap();
    assert!(status.success());
}

///
/// ローカルGitの製品dispatchも実Wiki照会の失敗報告へ接続される。
///
/// # 注記
/// committed mainから取得し、専用loopbackのlookup503で終了1にする。
///
/// # 戻り値
/// なし。ローカルGitのCLI計画・先頭失敗・要求一件が異なれば失敗する。
///
#[test]
fn local_git_plan_reaches_real_lookup_and_safe_failure() {
    let area = tempfile::tempdir().unwrap();
    git(area.path(), &["init", "-q", "-b", "main"]);
    fs::create_dir(area.path().join("root")).unwrap();
    fs::write(area.path().join("root/page.md"), b"unique-body").unwrap();
    git(area.path(), &["add", "."]);
    git(area.path(), &["commit", "-qm", "fixture"]);
    let (output, requests) = execute(area.path(), &[], false);
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(requests, 1);
    assert!(stdout.contains("page=/page source=LocalGit:root/page.md"));
    assert!(stdout.contains("outcome=Failed"));
    assert!(stderr.contains("category=Http(503)"));
}
