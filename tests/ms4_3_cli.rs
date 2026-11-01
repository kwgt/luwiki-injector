/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実CLIから確定計画・Wiki投入・最終報告までの代表的な接続試験
//!

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[path = "support/cli_http.rs"]
mod cli_http;

use cli_http::{Reply, Server};

// この系列だけの固定dummy識別子。実資格情報は使用しない。
const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
const PRIOR: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const TOKEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const TOKEN2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const TOKEN3: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";
const BODY: &[u8] = b"\xef\xbb\xbfunique-page-body\r\n";

///
/// 順序を意図的に指定した合成ZIPを作る。
///
/// # 引数
/// * `entries` - 生のentry名と本文bytes
///
/// # 戻り値
/// CLIが読了するまで保持する所有者とパス
///
fn archive(entries: &[(&str, &[u8])]) -> (TempDir, PathBuf) {
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    for (name, body) in entries {
        zip.start_file(format!("root/{name}"), SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body).unwrap();
    }
    if entries.is_empty() {
        zip.add_directory("root/", SimpleFileOptions::default()).unwrap();
    }
    zip.finish().unwrap();
    (area, path)
}

///
/// ダミー設定と専用loopbackだけで実CLIを起動する。
///
/// # 概要
/// ユーザーの認証・Git・proxy環境を渡さず、出力I/O失敗も子内で再現する。
///
/// # 引数
/// * `input` - 合成ZIPまたは一時ローカルGit
/// * `server` - この試験専用のREST応答列
/// * `mode` - 製品の衝突モード
/// * `warning` - CLI秘密指定の起動警告を加えるか
/// * `failed_output` - 失敗させる出力先。stdout/stderrだけを扱う。
///
/// # 戻り値
/// 子の終了状態と回収できた出力
///
fn execute(input: &Path, server: &Server, mode: &str, warning: bool,
    failed_output: Option<&str>) -> Output
{
    let area = tempfile::tempdir().unwrap();
    let config = area.path().join("config.toml");
    fs::write(&config, "[server]\ntoken='unique-wiki-secret'\n\
        [limits]\nmax_processing_seconds=10\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    /*
     * 全子プロセスで実設定・資格情報・proxyの影響を遮断する
     */
    let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
    command.arg("--config-path").arg(config).args(["--prefix", "/fixture",
        "--mode", mode]).arg(input).arg(&server.url).stdin(Stdio::null())
        .env("XDG_CONFIG_HOME", area.path()).env("RUST_LOG", "trace");
    for name in ["LUWIKI_URL", "LUWIKI_TOKEN", "LUWIKI_USER", "LUWIKI_PASSWORD",
        "GITHUB_TOKEN", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR",
        "GIT_INDEX_FILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy"]
    { command.env_remove(name); }
    /*
     * 製品に試験用設定を増やさず実出力のI/O失敗を注入する
     */
    if warning { command.args(["--token", "unique-cli-secret"]); }
    match failed_output {
        Some("stdout") => {
            command.stdout(Stdio::from(File::create("/dev/full").unwrap()));
        }
        Some("stderr") => {
            command.stderr(Stdio::from(File::create("/dev/full").unwrap()));
        }
        None => {}
        _ => panic!("未定義の試験出力先"),
    }
    command.output().unwrap()
}

///
/// stdout/stderrの分離と本文・資格情報・lock token非露出を確認する。
///
/// # 引数
/// * `output` - 子プロセスの出力
///
/// # 戻り値
/// 検証済みのstdout/stderr文字列
///
fn text(output: &Output) -> (String, String) {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    for secret in ["unique-wiki-secret", "unique-cli-secret",
        "unique-page-body", "raw-private", TOKEN, TOKEN2, TOKEN3]
    {
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
    }
    assert!(!stdout.contains("warning:"));
    assert!(!stdout.contains("error:"));
    assert!(!stderr.contains("result: exit="));
    (stdout, stderr)
}

///
/// 確定IDを完全JSONで返す。
///
/// # 引数
/// * `id` - 合成ID
/// * `draft` - lock付き201を返すか
///
/// # 戻り値
/// path照会またはdraft作成の応答
///
fn identity(id: &str, draft: bool) -> Reply {
    let body = format!("{{\"id\":\"{id}\"}}");
    if draft { Reply::new(201, body.as_bytes()).locked(TOKEN) }
    else { Reply::new(200, body.as_bytes()) }
}

///
/// 一件の正常新規作成、またはPUT異常までの応答列を作る。
///
/// 完全なエラーJSONを返し、不正応答ProtocolとHTTP失敗を混同しない。
///
/// # 引数
/// * `status` - 本文PUTの応答status
///
/// # 戻り値
/// lookup→POST→renew→PUTの四応答
///
fn creation(status: u16) -> Vec<Reply> {
    let body: &[u8] = if status == 204 { b"" } else { b"{}" };
    vec![Reply::new(404, b"{}"), identity(ID, true),
        Reply::new(204, b"").locked(TOKEN2), Reply::new(status, body)]
}

///
/// 旧IDの完全metadataを返す。
///
/// # 引数
/// * `deleted` - 削除済みの観測か
/// * `locked` - lockの観測
///
/// # 戻り値
/// 固定path/revisionのmetadata応答
///
fn metadata(deleted: bool, locked: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"{kind}\",\
        \"value\":\"/fixture/a\"}},\"deleted\":{deleted},\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}").as_bytes())
}

///
/// 既存ページのPUT/DELETEまでの通常操作列を作る。
///
/// DELETE500の条件付き再作成へ進める完全なエラーJSONを返す。
///
/// # 引数
/// * `status` - 最後の変更操作のstatus
///
/// # 戻り値
/// 十要求分の応答列
///
fn existing(status: u16) -> Vec<Reply> {
    let body: &[u8] = if status == 204 { b"" } else { b"{}" };
    vec![identity(ID, false), metadata(false, false),
        Reply::new(200, b"before"),
        Reply::new(204, b"").locked(TOKEN),
        Reply::new(204, b"").locked(TOKEN2), metadata(false, true),
        identity(ID, false), Reply::new(200, b"before"),
        Reply::new(204, b"").locked(TOKEN3), Reply::new(status, body)]
}

///
/// ZIPの入力順と異なる確定順で作成し、本文bytesをそのまま送信する。
///
/// # 概要
/// 実CLIの全件検証・順序・POST/PUT・Report・ログを一続きで確認する。
///
/// # 注記
/// 子が受信した全本文と要求列を比較し、暗黙の再送や追加HTTPも拒否する。
///
/// # 戻り値
/// なし。順序・本文寿命・正常0・秘匿のいずれかが異なれば失敗する。
///
#[test]
fn zip_creation_keeps_order_exact_bytes_and_clean_report() {
    let (_area, path) = archive(&[("a/child.md", b"child"), ("z.md", b"last"),
        ("a.md", BODY)]);
    let mut replies = Vec::new();
    for _ in 0..3 { replies.extend(creation(204)); }
    let server = Server::new(replies);
    let output = execute(&path, &server, "error", false, None);
    let (stdout, stderr) = text(&output);
    /*
     * 確定順の成功三件と安全な補助ログを照合する
     */
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("created=3 skipped=0 updated=0 recreated=0"));
    assert!(stdout.contains("warnings=0 failed=0 not_executed=0"));
    assert!(stderr.contains("[INFO] report exit=0"));
    assert!(!stderr.contains("warning:"));
    let pages: Vec<_> = stdout.lines().filter(|v| v.starts_with("page="))
        .collect();
    assert_eq!(pages.len(), 3);
    for (page, expected) in pages.iter().zip(["/fixture/a", "/fixture/z",
        "/fixture/a/child"])
    { assert!(page.starts_with(&format!("page={expected} "))); }
    /*
     * 全PUTを読み切ったbytesとpath照会順を製品入力へ対応付ける
     */
    server.inspect(|requests| {
        /*
         * 完全受信した全要求を三つの確定ページへ対応付ける
         */
        assert_eq!(requests.len(), 12);
        for (group, (path, body)) in requests.chunks_exact(4).zip([
            ("/fixture/a", BODY), ("/fixture/z", &b"last"[..]),
            ("/fixture/a/child", &b"child"[..])])
        {
            let query = url::Url::parse(&format!("http://fixture{}",
                group[0].target)).unwrap();
            assert!(query.query_pairs().any(|(key, value)|
                key == "path" && value == path));
            assert_eq!(group.iter().map(|v| v.method.as_str())
                .collect::<Vec<_>>(), ["GET", "POST", "PUT", "PUT"]);
            assert_eq!(group[3].body, body);
            assert!(group.iter().all(|v| v.authenticated));
            assert!(group[2].lock_auth && group[3].lock_auth);
        }
    });
}

///
/// 既存ページの同一skipとerror拒否を実CLIのmode設定から区別する。
///
/// # 概要
/// update/overwriteは本文完全一致で無変更、errorは本文取得前に停止する。
///
/// # 注記
/// modeごとの要求数とHTTP methodを比較し、隠れた書き込みを検出する。
///
/// # 戻り値
/// なし。mode・終了状態・同一本文の扱いが異なれば失敗する。
///
#[test]
fn identical_existing_pages_skip_or_fail_according_to_cli_mode() {
    let (_area, path) = archive(&[("a.md", BODY)]);
    for mode in ["error", "update", "overwrite"] {
        /*
         * 既存照会後にmodeが必要とする応答だけを用意する
         */
        let replies = if mode == "error" { vec![identity(ID, false)] }
            else { vec![identity(ID, false), metadata(false, false),
                Reply::new(200, BODY)] };
        let server = Server::new(replies);
        let output = execute(&path, &server, mode, false, None);
        let (stdout, stderr) = text(&output);
        let failed = mode == "error";
        assert_eq!(output.status.code(), Some(if failed { 1 } else { 0 }));
        /*
         * 拒否とskipの結果・非変更の要求列を独立に確認する
         */
        if failed {
            assert!(stdout.contains("outcome=Failed"));
            assert!(stderr.contains("category=Conflict"));
        } else {
            assert!(stdout.contains("outcome=SkippedIdentical"));
            assert!(stdout.contains("skipped=1"));
        }
        server.inspect(|requests| {
            assert_eq!(requests.len(), if failed { 1 } else { 3 });
            assert!(requests.iter().all(|v| v.method == "GET"));
        });
    }
}

///
/// 実CLIのupdate/overwrite指定が通常の更新/再作成へ接続される。
///
/// # 概要
/// 再照合前の同一本文と旧IDを保持し、変更操作をmode別に検査する。
///
/// # 注記
/// updateはPUT一件、overwriteはDELETEと新POST/PUT一件を確認する。
///
/// # 戻り値
/// なし。操作の取り違え・重複適用・秘密露出があれば失敗する。
///
#[test]
fn existing_modes_apply_update_and_recreation_once() {
    let (_area, path) = archive(&[("a.md", BODY)]);
    for mode in ["update", "overwrite"] {
        let mut replies = existing(204);
        /*
         * overwriteだけ旧削除の後で新しいdraftを作成する
         */
        if mode == "overwrite" {
            replies.extend([identity(NEW, true),
                Reply::new(204, b"").locked(TOKEN2), Reply::new(204, b"")]);
        }
        let server = Server::new(replies);
        let output = execute(&path, &server, mode, false, None);
        let (stdout, _) = text(&output);
        assert_eq!(output.status.code(), Some(0));
        let outcome = if mode == "update" { "outcome=Updated" }
            else { "outcome=Recreated" };
        assert!(stdout.contains(outcome));
        /*
         * 実送信本文と破壊操作の数をmode別に確認する
         */
        server.inspect(|requests| {
            assert_eq!(requests.len(), if mode == "update" { 10 } else { 13 });
            assert_eq!(requests.iter().filter(|v| v.method == "DELETE")
                .count(), usize::from(mode == "overwrite"));
            let puts: Vec<_> = requests.iter().filter(|v|
                v.method == "PUT" && v.target.ends_with("/source")).collect();
            assert_eq!(puts.len(), 1);
            assert_eq!(puts[0].body, BODY);
        });
    }
}

///
/// 先行成功の後で失敗すると、後続を未実行のまま報告する。
///
/// # 概要
/// 逆順ZIPの親を正常作成し、次lookup403で停止させて結果を照合する。
///
/// # 注記
/// 本文寿命・先行成功・最初の失敗・追加HTTPなしを実子で確認する。
///
/// # 戻り値
/// なし。先行成功を消した場合や三件目へ進めば失敗する。
///
#[test]
fn partial_success_keeps_prior_page_and_stops_later_requests() {
    let (_area, path) = archive(&[("c.md", b"later"), ("b.md", b"failed"),
        ("a.md", BODY)]);
    let mut replies = creation(204);
    replies.push(Reply::new(403, b"{\"reason\":\"raw-private\"}"));
    let server = Server::new(replies);
    let output = execute(&path, &server, "error", true, None);
    let (stdout, stderr) = text(&output);
    /*
     * 確定成功と失敗・未実行を一度ずつ数える
     */
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("created=1"));
    assert!(stdout.contains("warnings=1 failed=1 not_executed=1"));
    assert!(stdout.contains("first_failure=/fixture/b"));
    assert!(stderr.contains("category=Permission"));
    assert_eq!(stderr.matches("warning:").count(), 1);
    server.inspect(|requests| {
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[3].body, BODY);
        assert!(requests[4].target.ends_with("path=%2Ffixture%2Fb"));
    });
}

///
/// PUT500後に目的状態が確認できても警告付き終了3を維持する。
///
/// # 概要
/// 起動警告と操作警告があっても成功ページを重複計上しない。
///
/// # 注記
/// 起動警告の有無だけを変え、実CLIの警告数・出力先・要求数を照合する。
///
/// # 戻り値
/// なし。警告消失・二重計上・本文再送があれば失敗する。
///
#[test]
fn confirmed_put_warning_combines_with_startup_warning_once() {
    let (_area, path) = archive(&[("a.md", BODY)]);
    for warning in [false, true] {
        let mut replies = creation(500);
        replies.extend([identity(ID, false), metadata(false, false),
            Reply::new(200, BODY)]);
        let server = Server::new(replies);
        let output = execute(&path, &server, "update", warning, None);
        let (stdout, stderr) = text(&output);
        /*
         * 適用一件と独立警告だけを数え、正常0へ補完しない
         */
        assert_eq!(output.status.code(), Some(3));
        assert!(stdout.contains("created=1"));
        assert!(stdout.contains(&format!("warnings={} failed=0 not_executed=0",
            1 + usize::from(warning))));
        assert_eq!(stderr.matches("warning:").count(),
            1 + usize::from(warning));
        assert!(stderr.contains("operation=Put response=Http(500)"));
        assert!(stderr.lines().any(|line| line.contains("role=warning")
            && line.contains("operation: Some(PutSource)")
            && line.contains("kind: Http")
            && line.contains("status: Some(500)")));
        server.inspect(|requests| {
            assert_eq!(requests.len(), 7);
            assert_eq!(requests.iter().filter(|v|
                v.method == "PUT" && v.target.ends_with("/source")).count(), 1);
        });
    }
}

///
/// DELETE500の先行警告は再作成POST403で失敗しても表示する。
///
/// # 概要
/// 実CLIのoverwriteから条件付き再作成と失敗報告までを接続する。
///
/// # 注記
/// 起動警告＋DELETE警告、旧ID/revision/削除確認、後続未実行を照合する。
///
/// # 戻り値
/// なし。先行警告・復旧事実の消失や追加操作があれば失敗する。
///
#[test]
fn deletion_warning_survives_recreation_failure_in_cli() {
    let (_area, path) = archive(&[("b.md", b"later"), ("a.md", BODY)]);
    let mut replies = existing(500);
    replies.extend([metadata(true, false), Reply::new(404, b"{}"),
        Reply::new(403, b"{\"reason\":\"raw-private\"}")]);
    let server = Server::new(replies);
    let output = execute(&path, &server, "overwrite", true, None);
    let (stdout, stderr) = text(&output);
    /*
     * 成功件数へ加算せず先行警告と新POST主失敗を表示する
     */
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("completed=0"));
    assert!(stdout.contains("warnings=2 failed=1 not_executed=1"));
    for fact in [ID, "revision=Some(7)", "old_delete_confirmed=true"] {
        assert!(stdout.contains(fact));
    }
    assert_eq!(stderr.matches("warning:").count(), 2);
    assert!(stderr.contains("operation=Delete response=Http(500)"));
    assert!(stderr.lines().any(|line| line.contains("role=warning")
        && line.contains("operation: Some(DeletePage)")
        && line.contains("kind: Http")
        && line.contains("status: Some(500)")));
    assert!(stderr.contains("category=Permission stage=CreateDraft"));
    server.inspect(|requests| {
        assert_eq!(requests.len(), 13);
        assert_eq!(requests.iter().filter(|v| v.method == "DELETE").count(), 1);
        assert_eq!(requests.iter().filter(|v| v.method == "POST"
            && v.target.starts_with("/api/pages?")).count(), 1);
        assert_eq!(requests.iter().filter(|v| v.method == "POST"
            && v.target.ends_with("/lock")).count(), 1);
        assert!(!requests.iter().any(|v| v.target.ends_with("/source")
            && v.method == "PUT"));
    });
}

///
/// 再作成PUTの目的状態を確認しても実CLIの安全停止を維持する。
///
/// # 概要
/// 先行通常成功とDELETE/PUTの累積警告を保ち、最終ページでも終了1にする。
///
/// # 注記
/// 後続有無だけを変え、failed0/警告3/復旧事実と追加HTTPなしを確認する。
///
/// # 戻り値
/// なし。main接続で安全停止を失ったり再送/後続実行すれば失敗する。
///
#[test]
fn confirmed_recreation_stops_cli_with_prior_success_and_ordered_warnings() {
    for later in [false, true] {
        /*
         * 先行作成と旧DELETE500→新PUT500の目的確認を一計画へ通す
         */
        let mut entries = vec![("a.md", BODY), ("0.md", BODY)];
        if later { entries.push(("b.md", b"later")); }
        let (_area, path) = archive(&entries);
        let mut replies = creation(204);
        replies[1] = identity(PRIOR, true);
        replies.extend(existing(500));
        replies.extend([metadata(true, false), Reply::new(404, b"{}"),
            identity(NEW, true), Reply::new(204, b"").locked(TOKEN2),
            Reply::new(500, b"{}"), identity(NEW, false),
            metadata(false, false), Reply::new(200, BODY)]);
        let server = Server::new(replies);
        let output = execute(&path, &server, "overwrite", true, None);
        let (stdout, stderr) = text(&output);
        /*
         * 適用結果・独立安全停止・発生順の二操作警告を保持する
         */
        assert_eq!(output.status.code(), Some(1));
        assert!(stdout.contains("created=1 skipped=0 updated=0 recreated=1"));
        assert!(stdout.contains(&format!(
            "warnings=3 failed=0 not_executed={}", usize::from(later))));
        assert!(stdout.contains(
            "outcome=AppliedWithWarning { action: Recreate"));
        assert!(stdout.lines().any(|line| line.starts_with("page=/fixture/0 ")
            && line.contains("outcome=Created")));
        assert!(!stdout.contains("first_failure="));
        for fact in [ID, NEW, "revision=Some(7)", "old_delete_confirmed=true",
            "stop=Some(RecreationUncertain)"]
        { assert!(stdout.contains(fact)); }
        assert!(stderr.contains("safety_stop=RecreationUncertain"));
        assert_eq!(stderr.matches("warning:").count(), 3);
        let delete = stderr.find("operation=Delete response=Http(500)")
            .unwrap();
        let put = stderr.find("operation=Put response=Http(500)").unwrap();
        assert!(delete < put);
        assert_eq!(stderr.lines().filter(|line| line.contains("role=warning")
            && line.contains("kind: Http")
            && line.contains("status: Some(500)")).count(), 2);
        /*
         * 旧再送・自動回収・未実行ページへの要求を全列で拒否する
         */
        server.inspect(|requests| {
            /*
             * 旧IDだけの削除と先行/新IDへの本文PUTを一度ずつ確認する
             */
            assert_eq!(requests.len(), 22);
            assert_eq!(requests.iter().filter(|v| v.method == "DELETE")
                .count(), 1);
            assert!(requests.iter().filter(|v| v.method == "DELETE")
                .all(|v| v.target == format!("/api/pages/{ID}")));
            assert_eq!(requests.iter().filter(|v| v.method == "POST"
                && v.target.starts_with("/api/pages?")).count(), 2);
            let puts: Vec<_> = requests.iter().filter(|v|
                v.method == "PUT" && v.target.ends_with("/source")).collect();
            assert_eq!(puts.len(), 2);
            assert_eq!(puts[0].target, format!("/api/pages/{PRIOR}/source"));
            assert_eq!(puts[1].target, format!("/api/pages/{NEW}/source"));
            assert!(puts.iter().all(|v| v.body == BODY));
            assert!(!requests.iter().any(|v|
                v.target.contains("path=%2Ffixture%2Fb")));
        });
    }
}

///
/// 一時Git repositoryを実CLIの正常投入まで通す。
///
/// # 概要
/// Gitはfixture作成にだけ使い、製品取得はgixのcommitted mainを通す。
///
/// # 注記
/// ZIPだけの接続試験と区別してLocalGit参照と全PUT bytesを確認する。
///
/// # 戻り値
/// なし。Git取得・本文寿命・CLI結果が異なれば失敗する。
///
#[test]
fn local_git_creation_reaches_normal_cli_execution() {
    let area = tempfile::tempdir().unwrap();
    fs::create_dir(area.path().join("root")).unwrap();
    fs::write(area.path().join("root/a.md"), BODY).unwrap();
    /*
     * 外部設定・hooksを無効にしたローカルfixtureだけを作る
     */
    for args in [vec!["init", "-q", "-b", "main"], vec!["add", "."],
        vec!["commit", "-qm", "fixture"]]
    {
        let status = Command::new("git").current_dir(area.path())
            .args(["-c", "core.hooksPath=/dev/null"]).args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR").env_remove("GIT_INDEX_FILE")
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(status.success());
    }
    /*
     * 子の製品dispatchから正常作成しsource参照とbytesを確認する
     */
    let server = Server::new(creation(204));
    let output = execute(area.path(), &server, "error", false, None);
    let (stdout, _) = text(&output);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("page=/fixture/a source=LocalGit:root/a.md"));
    server.inspect(|requests| {
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3].body, BODY);
    });
}

///
/// 適用完了後のstdout/stderr表示失敗も終了1へ反映する。
///
/// # 概要
/// Linuxの失敗する出力先を子へ渡し、製品に試験用経路を加えず再現する。
///
/// # 注記
/// 警告付き作成が完了しても、最終表示失敗を終了3より優先する。
///
/// # 戻り値
/// なし。表示I/O失敗が正常/警告終了になれば失敗する。
///
#[cfg(target_os = "linux")]
#[test]
fn output_failure_overrides_successful_application_and_warnings() {
    let (_area, path) = archive(&[("a.md", BODY)]);
    for destination in ["stdout", "stderr"] {
        let server = Server::new(creation(204));
        let output = execute(&path, &server, "error", true, Some(destination));
        assert_eq!(output.status.code(), Some(1));
        text(&output);
        server.inspect(|requests| {
            assert_eq!(requests.len(), 4);
            assert_eq!(requests[3].body, BODY);
        });
    }
    /*
     * help/versionもpanicではなく表示失敗1とし、ログを開始しない
     */
    for option in ["--help", "-v"] {
        let output = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"))
            .arg(option).stdin(Stdio::null())
            .stdout(Stdio::from(File::create("/dev/full").unwrap()))
            .output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty());
    }
}
