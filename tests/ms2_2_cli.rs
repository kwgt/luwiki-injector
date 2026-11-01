/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS2.2のエントリポイントと終了状態を実バイナリで検証する
//!

use std::fs::File;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

// 共有fixtureの応答生成は後続CLI系列が利用する。
#[allow(dead_code)]
#[path = "support/cli_http.rs"]
mod cli_http;

///
/// ユーザーの設定と資格情報を渡さずにCLIを起動する。
///
/// # 引数
/// * `args` - プログラムへ渡す引数
///
/// # 戻り値
/// 終了状態と標準出力・標準エラー
///
fn execute(args: &[&str]) -> Output {
    let temporary = tempdir().expect("一時設定ディレクトリ");
    let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
    command.args(args)
        .env("XDG_CONFIG_HOME", temporary.path())
        .stdin(Stdio::null());
    for name in [
        "LUWIKI_URL", "LUWIKI_TOKEN", "LUWIKI_USER", "LUWIKI_PASSWORD",
        "GITHUB_TOKEN", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy",
    ] {
        command.env_remove(name);
    }
    command.output().expect("CLI起動")
}

///
/// help/versionが設定読込と入力検証より先に成功する。
///
/// # 注記
/// 存在しない明示設定を付け、正常表示が終了コード0になるか調べる。
///
/// # 戻り値
/// なし。help/versionの先行表示・終了0・出力先が異なればassertで失敗する。
///
#[test]
fn help_and_version_bypass_configuration() {
    for option in ["--help", "-v"] {
        let output = execute(&[
            "--config-path", "/missing/config.toml", option,
        ]);
        assert_eq!(output.status.code(), Some(0));
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

///
/// 不正設定と非TTYの情報不足は終了コード2で止まる。
///
/// # 注記
/// 認証なし、パスワード不足、URLとmodeの不正値を照合する。
///
/// # 戻り値
/// なし。実行前の設定拒否や診断の秘密非露出が異なる場合はassertで失敗する。
///
#[test]
fn validation_errors_stop_before_run_without_exposing_values() {
    /*
     * 不正入力は実行関数へ渡されない
     */
    for args in [
        vec!["owner/repo"],
        vec!["--user", "user", "owner/repo"],
        vec!["-t", "unique-secret", "owner/repo", "https://host/path"],
        vec!["-t", "unique-secret", "-m", "invalid-mode", "owner/repo"],
    ] {
        let output = execute(&args);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(!stderr.contains("unique-secret"));
        assert!(!stderr.contains("invalid-mode"));
        assert!(!stderr.contains("実行処理はまだ利用できません"));
    }
}

///
/// 検証成功時だけ既存の実行関数へ到達する。
///
/// # 注記
/// 正常ZIPを専用loopbackへ渡し、lookup503と安全な警告を確認する。
///
/// # 戻り値
/// なし。正常ZIPのlookup失敗・起動警告・秘密非露出が異なれば失敗する。
///
#[test]
fn valid_input_reaches_run_and_keeps_warning_safe() {
    let area = tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    zip.start_file("root/page.md", SimpleFileOptions::default()).unwrap();
    zip.write_all(b"unique-body").unwrap();
    zip.finish().unwrap();
    /*
     * 実サービスへ接続せず、成立計画から一件の照会へ進める
     */
    let server = cli_http::Server::new(Vec::new());
    let output = execute(&["-t", "unique-secret", path.to_str().unwrap(),
        &server.url]);
    assert_eq!(server.inspect(|requests| requests.len()), 1);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("category=Http(503)"));
    assert!(stderr.contains("warning:"));
    assert!(!stderr.contains("unique-secret"));
}
