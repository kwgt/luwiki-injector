/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 最終結果と診断の出力先・終了コード・秘匿を実バイナリで確認する
//!

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

// 共有fixtureの応答生成は後続CLI系列が利用する。
#[allow(dead_code)]
#[path = "support/cli_http.rs"]
mod cli_http;

///
/// 実設定と資格情報を渡さず、一時設定だけでCLIを実行する。
///
/// # 概要
/// 標準入力を非TTYにし、関連環境変数を隔離して出力を回収する。
///
/// # 引数
/// * `args` - CLI引数
/// * `config` - 任意のテストTOML
///
/// # 戻り値
/// 終了状態と二つの出力
///
fn execute(args: &[&str], config: Option<&str>) -> Output {
    let temporary = tempdir().expect("一時設定領域");
    let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
    /*
     * テスト専用の明示設定と非TTYを用意する
     */
    if let Some(document) = config {
        let path = temporary.path().join("settings.toml");
        fs::write(&path, document).expect("設定作成");
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("設定権限");
        command.arg("--config-path").arg(path);
    }
    command.args(args).stdin(Stdio::null())
        .env("XDG_CONFIG_HOME", temporary.path())
        .env("RUST_LOG", "trace");
    for name in [
        "LUWIKI_URL", "LUWIKI_TOKEN", "LUWIKI_USER", "LUWIKI_PASSWORD",
        "GITHUB_TOKEN", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy",
    ] { command.env_remove(name); }
    command.output().expect("CLI実行")
}

///
/// 正常表示は結果報告やログを伴わず終了コード0になる。
///
/// # 注記
/// 不正TOMLを付けてもhelp/versionが先に表示されるか確認する。
///
/// # 戻り値
/// なし。正常表示の終了0・stdout内容・stderr非出力が異なればassertで失敗する。
///
#[test]
fn help_and_version_remain_clean_stdout() {
    for option in ["--help", "-v"] {
        let output = execute(&[option], Some("[broken\n"));
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        assert!(!stdout.contains("result: exit="));
        assert!(stdout.contains("luwiki-injector"));
        assert!(output.stderr.is_empty());
    }
}

///
/// 起動失敗の最終結果と診断が指定した出力先へ出る。
///
/// # 概要
/// 各種起動不正をCLIへ渡し、終了2と結果/診断の出力先・入力値非露出を確認する。
///
/// # 注記
/// CLI構文、認証不足、TOML構文・型、URLエラーを照合する。
///
/// # 戻り値
/// なし。起動失敗の終了2・結果/診断出力先・秘密非露出が異なれば失敗する。
///
#[test]
fn startup_failures_report_exit_two_without_echoing_input() {
    /*
     * CLI・認証・TOML・URLの起動不正を用意する
     */
    for (args, config) in [
        (vec!["--unknown", "unique-secret"], None),
        (vec!["owner/repo"], None),
        (vec!["-u", "user", "owner/repo"], None),
        (vec!["-t", "unique-secret", "owner/repo"],
         Some("[server]\ntoken=[unique-secret\n")),
        (vec!["-t", "unique-secret", "owner/repo"],
         Some("[limits]\nmax_pages='unique-body'\n")),
        (vec!["-t", "unique-secret", "owner/repo", "https://host/path"], None),
    ] {
        /*
         * 終了2・出力先・秘密非露出を各入力で照合する
         */
        let output = execute(&args, config);
        assert_eq!(output.status.code(), Some(2));
        let stdout = String::from_utf8(output.stdout).expect("stdout");
        let stderr = String::from_utf8(output.stderr).expect("stderr");
        assert!(stdout.contains("result: exit=2 plan=not-established"));
        assert!(stderr.contains("error:"));
        for secret in ["unique-secret", "unique-body"] {
            assert!(!stdout.contains(secret));
            assert!(!stderr.contains(secret));
        }
    }
}

///
/// 検証失敗時にもCLIの安全な警告を残す。
///
/// # 注記
/// 秘密指定と不正modeを併用し、警告より起動失敗を優先する。
///
/// # 戻り値
/// なし。検証失敗時の警告保持・終了優先順位・非露出が異なれば失敗する。
///
#[test]
fn validation_failure_preserves_warning_and_prioritizes_error() {
    let output = execute(&[
        "-t", "unique-secret", "-m", "invalid", "owner/repo",
    ], None);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stdout.contains("warnings=1"));
    assert!(stderr.contains("warning:"));
    assert!(stderr.contains("error:"));
    assert!(!stderr.contains("unique-secret"));
}

///
/// 警告を伴うWiki照会失敗もReportへ集約される。
///
/// # 概要
/// ローカルZIPから計画まで進め、外部GitHubへの要求を発行しない。
///
/// # 注記
/// 入力検証成功時の実行失敗1、補助ログ、秘密値非露出を確認する。
///
/// # 戻り値
/// なし。Wiki照会失敗のReport・ログ・警告保持・非露出が異なれば失敗する。
///
#[test]
fn execution_failure_keeps_warning_in_report_and_safe_logs() {
    let area = tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file("root/page.md", SimpleFileOptions::default()).unwrap();
    zip.write_all(b"unique-body").unwrap();
    zip.finish().unwrap();
    /*
     * 専用loopbackのlookup503と警告だけを観測する
     */
    let server = cli_http::Server::new(Vec::new());
    let output = execute(&["-t", "unique-secret", path.to_str().unwrap(),
        &server.url], None);
    assert_eq!(server.inspect(|requests| requests.len()), 1);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    let stderr = String::from_utf8(output.stderr).expect("stderr");
    assert!(stdout.contains("result: exit=1"));
    assert!(stdout.contains("warnings=1"));
    assert!(stderr.contains("category=Http(503)"));
    assert!(stderr.contains("[INFO] report exit=1"));
    assert!(!stdout.contains("unique-secret"));
    assert!(!stderr.contains("unique-secret"));
}
