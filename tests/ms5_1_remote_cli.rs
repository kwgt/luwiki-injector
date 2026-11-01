/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51 remote基本6/追加6入口と実環境不要の独立境界検査
//!

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use anyhow::{Result, ensure};

#[allow(dead_code)]
#[path = "support/cli_http.rs"]
mod cli_http;
#[allow(dead_code)]
#[path = "support/real_cli_http.rs"]
mod real_cli_http;
#[allow(dead_code)]
#[path = "support/real_cli_report.rs"]
mod real_cli_report;
#[allow(dead_code)]
#[path = "support/source_fixture.rs"]
mod source_fixture;
#[allow(dead_code)]
#[path = "support/ms5_1_harness.rs"]
mod ms5_1_harness;

use cli_http::Server;
use ms5_1_harness::{ExpectedBody, ExpectedPage, Mode, PreparedSource, Selector};
use ms5_1_harness::remote_fixture::{DATA_LIMIT, RemoteFixtureSpec, SourceId,
    expected_names, full_commit, invalid_public_grant, offline_command,
    run_remote_cell, run_remote_references, run_sink, run_wiki_rejection,
    same_body};

///
/// HTTPS Git errorの全page N/S/D/Rを確認する。
///
/// # 注記
/// public二版と実Wiki Basicで個別に実行する。
///
/// # 戻り値
/// 全phaseの状態/Report一致
///
#[tokio::test]
#[ignore = "requires prepared public remote and approved real Wiki"]
async fn ms51_real_https_git_error() -> Result<()> {
    run_remote_cell(Mode::Error, false, SourceId::Public).await
}

///
/// HTTPS Git updateの相違pageだけの改訂を確認する。
///
/// # 注記
/// public二版で全pageのN/S/D/Rを実行する。
///
/// # 戻り値
/// 同ID/revisionとbytesの一致
///
#[tokio::test]
#[ignore = "requires prepared public remote and approved real Wiki"]
async fn ms51_real_https_git_update() -> Result<()> {
    run_remote_cell(Mode::Update, false, SourceId::Public).await
}

///
/// HTTPS Git overwriteの旧soft-delete/新IDを確認する。
///
/// # 注記
/// public二版と実Wiki Basicで全pageを独立観測する。
///
/// # 戻り値
/// N/S/D/Rの全件一致
///
#[tokio::test]
#[ignore = "requires prepared public remote and approved real Wiki"]
async fn ms51_real_https_git_overwrite() -> Result<()> {
    run_remote_cell(Mode::Overwrite, false, SourceId::Public).await
}

///
/// GitHub errorの全page N/S/D/Rを確認する。
///
/// # 注記
/// public匿名取得と実Wiki Bearerを分離する。
///
/// # 戻り値
/// 先頭Conflict/残未実行と全版不変
///
#[tokio::test]
#[ignore = "requires prepared public remote and approved real Wiki"]
async fn ms51_real_github_error() -> Result<()> {
    run_remote_cell(Mode::Error, true, SourceId::Public).await
}

///
/// private GitHub updateの全二版を確認する。
///
/// # 注記
/// 全Markdown保存の別承認gateと取得tokenが必要。未準備は失敗する。
///
/// # 戻り値
/// private名/本文を表示しない全件一致
///
#[tokio::test]
#[ignore = "requires separate private import approval and prepared credentials"]
async fn ms51_real_github_update() -> Result<()> {
    run_remote_cell(Mode::Update, true, SourceId::Private).await
}

///
/// GitHub overwriteの全page N/S/D/Rを確認する。
///
/// # 注記
/// public匿名取得とWiki Bearerで旧新状態を独立観測する。
///
/// # 戻り値
/// 相違pageだけの再作成と旧soft-delete一致
///
#[tokio::test]
#[ignore = "requires prepared public remote and approved real Wiki"]
async fn ms51_real_github_overwrite() -> Result<()> {
    run_remote_cell(Mode::Overwrite, true, SourceId::Public).await
}

///
/// HTTPSのdefault/branch/tag/commitを独立事前照合する。
///
/// # 注記
/// main/masterの一役割と明示三役割を全準備後に4CLIで確認する。
///
/// # 戻り値
/// 指定refと全期待本文の一致
///
#[tokio::test]
#[ignore = "requires prepared reference roles and approved real Wiki"]
async fn ms51_real_https_git_reference() -> Result<()> {
    run_remote_references(false).await
}

///
/// GitHubのdefault/branch/tag/commitを独立事前照合する。
///
/// # 注記
/// 匿名HTTPSの同一repo広告とlocal commitをWiki HTTP前に照合する。
///
/// # 戻り値
/// 指定refと全期待本文の一致
///
#[tokio::test]
#[ignore = "requires prepared reference roles and approved real Wiki"]
async fn ms51_real_github_reference() -> Result<()> {
    run_remote_references(true).await
}

///
/// 無効GitHub tokenでWiki要求前に停止することを確認する。
///
/// # 注記
/// 合成無効tokenとpublic repoを使い、loopback sinkの要求0を確認する。
///
/// # 戻り値
/// Authentication/Preflight/exit1とWiki0
///
#[test]
#[ignore = "requires explicit public GitHub network execution"]
fn ms51_github_invalid_token_stops_before_wiki() -> Result<()> {
    let sink = Server::new(Vec::new());
    let result = run_sink(&invalid_public_grant()?, &sink.url)?;
    ensure!(result.code == Some(1)
        && result.stdout.starts_with("result: exit=1 plan=not-established\n")
        && result.stderr.contains("category=Authentication stage=Preflight"),
        "GitHub invalid credential classification");
    ensure!(sink.inspect(|records| records.is_empty()),
        "unexpected Wiki sink request");
    Ok(())
}

///
/// private匿名取得がWiki要求へ進まないことを確認する。
///
/// # 注記
/// private実本文/tokenを読まない。HTTP404自体を直接証明しない。
///
/// # 戻り値
/// plan未成立/exit2とWiki sink0
///
#[test]
#[ignore = "requires prepared private metadata and explicit anonymous network"]
fn ms51_github_private_anonymous_stops_before_wiki() -> Result<()> {
    let spec = RemoteFixtureSpec::load(SourceId::Private, false)?;
    let sink = Server::new(Vec::new());
    let result = run_sink(&spec.anonymous()?, &sink.url)?;
    ensure!(result.code == Some(2)
        && result.stdout.starts_with("result: exit=2 plan=not-established\n"),
        "private anonymous preflight stop");
    ensure!(sink.inspect(|records| records.is_empty()),
        "unexpected Wiki sink request");
    Ok(())
}

///
/// 正常取得後の実Wiki Lookup401代表を確認する。
///
/// # 注記
/// 同pathの単回negative probe後にだけ製品を起動する。
///
/// # 戻り値
/// Authentication/Lookup/exit1と全path不変
///
#[tokio::test]
#[ignore = "requires approved real Wiki and public remote"]
async fn ms51_real_remote_wiki_authentication_rejected() -> Result<()> {
    run_wiki_rejection(401).await
}

///
/// 正常取得後の実Wiki Lookup403代表を確認する。
///
/// # 注記
/// LUWIKI_MS51_LOOKUP403_TOKENを一度だけ読み、同pathでprobeする。
///
/// # 戻り値
/// Permission/Lookup/exit1と全path不変
///
#[tokio::test]
#[ignore = "requires a prepared Lookup403 credential and approved real Wiki"]
async fn ms51_real_remote_wiki_permission_rejected() -> Result<()> {
    run_wiki_rejection(403).await
}

///
/// 完全OIDと保護specのschema/source境界を確認する。
///
/// # 概要
/// 保護入力のclosed schemaを合成値だけで検査する。
///
/// # 注記
/// 合成TOMLだけをparseし、環境/保護file/Git/HTTPを読まない。
///
/// # 戻り値
/// 正常と拒否の一致
///
#[test]
fn ms51_remote_spec_is_closed_and_uses_full_commits() -> Result<()> {
    /*
     * 合成specの正常schemaとsource一致を確認する
     */
    let oid = "1".repeat(40);
    let text = format!("schema=1\nsource_id='PUB-01'\n\
        repository='kwgt/test-repository'\nlocal_repository='/synthetic'\n\
        v0='{oid}'\nv1='{}'\n", "2".repeat(40));
    let spec: RemoteFixtureSpec = toml::from_str(&text)?;
    spec.validate(SourceId::Public)?;
    assert!(spec.validate(SourceId::Private).is_err());
    /*
     * 未使用refも未知役割/版/selector/valueなら拒否する
     */
    for extra in ["[refs.unknown]\nselector='default'\nversion='v0'\n",
        "[refs.main]\nselector='default'\nversion='v2'\n",
        "[refs.main]\nselector='tag'\nversion='v0'\nvalue='safe'\n",
        "[refs.commit]\nselector='commit'\nversion='v0'\nvalue='safe'\n",
        "[refs.branch]\nselector='branch'\nversion='v0'\n",
        "[refs.tag]\nselector='tag'\nversion='v0'\nvalue='../bad'\n"]
    {
        let candidate: RemoteFixtureSpec =
            toml::from_str(&(text.clone() + extra))?;
        assert!(candidate.validate(SourceId::Public).is_err());
    }
    assert!(toml::from_str::<RemoteFixtureSpec>(
        &(text + "token='synthetic'\n")).is_err());
    assert!(full_commit(&oid));
    for value in ["", "1234", &"g".repeat(40), &"a".repeat(41)] {
        assert!(!full_commit(value));
    }
    Ok(())
}

///
/// directory/非Markdownも含む全treeの安全検査を確認する。
///
/// # 概要
/// 全entryの型/名前を検証し、Markdownだけを計画順へ投影する。
///
/// # 注記
/// 合成ls-tree bytesでsymlink/gitlink/危険名/不正UTF8を拒否する。
///
/// # 戻り値
/// 全entry検査と計画順の一致
///
#[test]
fn ms51_remote_tree_checks_every_entry() -> Result<()> {
    /*
     * 正常treeの全Markdown投影順を確認する
     */
    let oid = "1".repeat(40);
    let tree = format!("040000 tree {oid}\troot/dir\0\
        100644 blob {oid}\troot/dir/Z.md\0\
        100644 blob {oid}\troot/A.md\0\
        100644 blob {oid}\tbinary.bin\0");
    assert_eq!(expected_names(tree.as_bytes())?,
        ["root/A.md", "root/dir/Z.md"]);
    for (mode, kind, path) in [("120000", "blob", "link"),
        ("160000", "commit", "module"), ("100644", "blob", "../bad"),
        ("040000", "tree", "bad dir"), ("100644", "blob", "root/.md")]
    {
        let row = format!("{mode} {kind} {oid}\t{path}\0");
        assert!(expected_names(row.as_bytes()).is_err());
    }
    assert!(expected_names(b"100644 blob bad\tA.md\0").is_err());
    assert!(expected_names(&[0xff, 0]).is_err());
    Ok(())
}

///
/// treeの量/全件上限と重複/末尾欠落を確認する。
///
/// # 概要
/// 上限を超えた集合を部分集合の成功へ変換しない。
///
/// # 注記
/// 99/100境界を合成bytesで調べ、外部fixtureは生成しない。
///
/// # 戻り値
/// 有限/完全な集合だけを返すこと
///
#[test]
fn ms51_remote_tree_rejects_partial_or_oversized_sets() -> Result<()> {
    /*
     * framing/重複/bytes上限を確認してから件数境界を検査する
     */
    let oid = "1".repeat(40);
    let row = format!("100644 blob {oid}\troot/A.md\0");
    assert!(expected_names(row.trim_end_matches('\0').as_bytes()).is_err());
    assert!(expected_names((row.clone() + &row).as_bytes()).is_err());
    assert!(expected_names(&vec![0; DATA_LIMIT + 1]).is_err());
    let mut tree = String::new();
    for index in 0..99 {
        tree.push_str(&format!("100644 blob {oid}\troot/P{index:02}.md\0"));
    }
    assert_eq!(expected_names(tree.as_bytes())?.len(), 99);
    tree.push_str(&format!("100644 blob {oid}\troot/P99.md\0"));
    assert!(expected_names(tree.as_bytes()).is_err());
    Ok(())
}

///
/// offline期待Gitのcommand環境だけを確認する。
///
/// # 概要
/// 期待生成commandへ認証/transportを持ち込まない構造を確認する。
///
/// # 注記
/// commandを実行せず、transport/秘密/trace/対話の隔離を確認する。
///
/// # 戻り値
/// 構造上の隔離一致
///
#[test]
fn ms51_remote_expected_git_has_no_transport_or_credentials() -> Result<()> {
    /*
     * 子起動を行わず隔離された環境設定だけを参照する
     */
    let command = offline_command(Path::new("/synthetic"), &["cat-file", "-t"]);
    let values: std::collections::BTreeMap<_, _> = command.get_envs()
        .filter_map(|(key, value)| value.map(|value| (key, value))).collect();
    for name in ["GITHUB_TOKEN", "LUWIKI_TOKEN", "GIT_TRACE", "HTTPS_PROXY"] {
        assert!(!values.contains_key(std::ffi::OsStr::new(name)));
    }
    assert_eq!(values.get(std::ffi::OsStr::new("GIT_ALLOW_PROTOCOL"))
        .and_then(|value| value.to_str()), Some(""));
    assert_eq!(values.get(std::ffi::OsStr::new("GIT_NO_LAZY_FETCH"))
        .and_then(|value| value.to_str()), Some("1"));
    Ok(())
}

///
/// reader比較がBOM/改行/末尾を保存することを確認する。
///
/// # 概要
/// 所有fileとsliceの独立readerで完全bytesとsize不変を確認する。
///
/// # 注記
/// 合成sliceと所有tempfileだけを使い、同一/短長/改行差を調べる。
///
/// # 戻り値
/// UTF8 bytes完全一致の結果
///
#[test]
fn ms51_remote_owned_reader_preserves_exact_bytes() -> Result<()> {
    /*
     * 固定sliceのBOM/改行/末尾差を比較する
     */
    let first = ExpectedPage { name: "A.md",
        body: ExpectedBody::Bytes(b"\xef\xbb\xbfA\r\n") };
    let same = ExpectedPage { name: "A.md", body: first.body };
    assert!(same_body(&first, &same)?);
    for bytes in [b"A\r\n".as_slice(), b"\xef\xbb\xbfA\n",
        b"\xef\xbb\xbfA\r\nX", b"\xef\xbb\xbfA\r"]
    {
        let other = ExpectedPage { name: "A.md",
            body: ExpectedBody::Bytes(bytes) };
        assert!(!same_body(&first, &other)?);
    }
    let mut reader = first.open()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"\xef\xbb\xbfA\r\n");
    /*
     * 所有fileを封印し、独立offsetとslice同一性を確認する
     */
    let area = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700)).tempdir()?;
    assert_eq!(area.path().metadata()?.permissions().mode() & 0o777, 0o700);
    let path = area.path().join("body-0");
    let mut file = OpenOptions::new().write(true).create_new(true)
        .mode(0o600).open(&path)?;
    file.write_all(&bytes)?;
    file.set_permissions(fs::Permissions::from_mode(0o400))?;
    drop(file);
    let stored = ExpectedPage { name: "A.md", body: ExpectedBody::File {
        path: &path, size: bytes.len() as u64 } };
    assert!(same_body(&first, &stored)?);
    let mut left = stored.open()?;
    let mut right = stored.open()?;
    let mut one = [0; 1];
    let mut two = [0; 1];
    left.read_exact(&mut one)?;
    right.read_exact(&mut two)?;
    assert_eq!(one, two);
    /*
     * 封印時のsizeから変化したfileを成功viewにしない
     */
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    OpenOptions::new().append(true).open(&path)?.write_all(b"X")?;
    assert!(stored.open().is_err());
    Ok(())
}

///
/// typed sourceと負例grantの取得資格情報境界を確認する。
///
/// # 概要
/// 入力構文拒否と負例専用の合成資格情報を検査する。
///
/// # 注記
/// 合成commandだけを構成し、token環境の読取/子起動は行わない。
///
/// # 戻り値
/// 任意URL/ref拒否と合成tokenだけの設定
///
#[test]
fn ms51_remote_grants_do_not_reuse_real_credentials() -> Result<()> {
    /*
     * typed sourceの禁止URLと参照を確認する
     */
    for url in ["http://github.com/a/b", "https://u:p@github.com/a/b",
        "https://github.com/a/b?q=x", "https://github.com/a/b#x"]
    {
        assert!(PreparedSource::https(url, Selector::Default).is_err());
    }
    assert!(PreparedSource::github("a/b", Selector::Worktree).is_err());
    let mut command = Command::new("synthetic-never-executed");
    real_cli_http::isolate_child(&mut command);
    invalid_public_grant()?.append(&mut command);
    let token = command.get_envs().find(|(key, _)| *key == "GITHUB_TOKEN")
        .and_then(|(_, value)| value).and_then(|value| value.to_str());
    assert_eq!(token, Some("ms51-invalid-dummy-token"));
    Ok(())
}

///
/// 計画前の失敗を秘密原文なしの固定ラベルへ分類する。
///
/// # 概要
/// 入力/引数/収集/実行失敗を区別し、未知や重複は補完しない。
///
/// # 注記
/// 合成stdout/stderrだけを使用し、CLI/HTTP/環境読取を行わない。
///
/// # 戻り値
/// なし。分類または秘匿境界が異なればassertで失敗する。
///
#[test]
fn ms51_remote_startup_diagnostic_is_finite() {
    /*
     * 既知の失敗を固定分類へ写し、原文の追記や重複を拒否する
     */
    for (error, kind) in [
        ("入力検証に失敗しました", "InputValidation"),
        ("コマンドライン引数が不正です。--helpを確認してください",
            "CliArguments"),
        ("設定ファイルを読み取れません", "Collection"),
        ("ページソースが不正です", "Collection"),
        ("GITHUB_TOKENの文字列を読み取れません", "Collection"),
        ("GitHub tokenが空です", "Collection"),
        ("category=Authentication stage=Preflight", "Execution"),
        ("category=Permission stage=Lookup", "Execution"),
        ("category=synthetic-secret stage=Preflight", "unknown"),
        ("入力検証に失敗しました synthetic-secret", "unknown"),
        ("synthetic-secretが空です", "unknown"),
        ("入力検証に失敗しました\nerror: synthetic-secret", "unknown"),
    ] {
        let captured = real_cli_report::Captured { code: Some(2),
            stdout: "result: exit=2 plan=not-established\n".into(),
            stderr: format!("error: {error}\n") };
        let summary = captured.safe_startup();
        assert_eq!(summary,
            format!("MS51 startup plan=not-established failure={kind}"));
        assert!(!summary.contains("synthetic-secret"));
        assert!(captured.require_remote_plan().is_err());
    }
}

///
/// 計画行の欠落/重複/終了コード不一致を計画成立へ変換しない。
///
/// # 概要
/// establishedを厳密に要求し、計画未成立を件数検査より先に止める。
///
/// # 注記
/// 合成結果だけを使用し、計画成立の成功/実行失敗は両方受理する。
///
/// # 戻り値
/// なし。不正な計画行を受理した場合はassertで失敗する。
///
#[test]
fn ms51_remote_plan_requires_unique_matching_result() {
    /*
     * 正規の成立/未成立と終了コードの対応を確認する
     */
    for code in 0..=2 {
        for plan in ["established", "not-established"] {
            let captured = real_cli_report::Captured { code: Some(code),
                stdout: format!("result: exit={code} plan={plan}\n"),
                stderr: String::new() };
            assert_eq!(captured.safe_plan(), plan);
            assert_eq!(captured.require_remote_plan().is_ok(),
                plan == "established");
        }
    }
    /*
     * 任意文字列や重複結果から計画成立を推測しない
     */
    for (code, stdout) in [
        (Some(2), ""),
        (None, "result: exit=2 plan=established\n"),
        (Some(3), "result: exit=3 plan=established\n"),
        (Some(2), "result: exit=0 plan=established\n"),
        (Some(2), "result: exit=2 plan=synthetic-secret\n"),
        (Some(2), "result: exit=2 plan=established synthetic-secret\n"),
        (Some(2), concat!("result: exit=2 plan=established\n",
            "result: exit=2 plan=not-established\n")),
    ] {
        let captured = real_cli_report::Captured { code,
            stdout: stdout.into(), stderr: "synthetic-secret".into() };
        assert_eq!(captured.safe_plan(), "unknown");
        assert!(captured.require_remote_plan().is_err());
        assert!(!captured.safe_startup().contains("synthetic-secret"));
    }
}
