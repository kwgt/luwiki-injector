/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51のローカル正常受け入れ12入口と実環境不要の境界検査
//!

use std::fs;
use std::io::{self, Cursor, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};

use anyhow::{Result, ensure};

// MS43 actor等の共有APIはMS51の通常入口では使用しない。
#[allow(dead_code)]
#[path = "support/real_cli_http.rs"]
mod real_cli_http;
// MS43専用の失敗詳細APIも同じparserのまま保持する。
#[allow(dead_code)]
#[path = "support/real_cli_report.rs"]
mod real_cli_report;
#[path = "support/source_fixture.rs"]
mod source_fixture;
#[path = "support/ms5_1_harness.rs"]
mod ms5_1_harness;

use ms5_1_harness::{CAPTURE_LIMIT, Case, Mode, PreparedSource, Selector,
    absent_path, decode_capture, drain, ordered, report_identity, run_cell,
    status_record, validate_output};
use real_cli_http::{Auth, Environment, MS51_ROOT, ROOT, approved_target,
    within};
use real_cli_report::Captured;
use source_fixture::{F0, F1, GitFixture, Page, ZipFixture, ZipKind, safe_entry};

///
/// ZIP errorの新規/同一拒否/相違拒否/明示再実行を確認する。
///
/// # 注記
/// 新caseで4CLIを順次実行し、各phaseの実状態を独立観測する。
///
/// # 戻り値
/// 基本セルB-Z-Eの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_zip_error() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Error, false).await
}

///
/// ZIP updateのN/S/D/Rと同ID/revision進行を確認する。
///
/// # 注記
/// Basicで4CLIと各phaseのbytes/ID/revision観測を実施する。
///
/// # 戻り値
/// 基本セルB-Z-Uの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_zip_update() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Update, false).await
}

///
/// ZIP overwriteのN/S/D/Rと旧soft-delete/新IDを確認する。
///
/// # 注記
/// 4CLIを順次実行し、相違時の旧新IDと復旧表示を独立状態と照合する。
///
/// # 戻り値
/// 基本セルB-Z-Oの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_zip_overwrite() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Overwrite, false).await
}

///
/// 完全commit指定のLocalGit errorで4phaseを確認する。
///
/// # 注記
/// BearerでF0/F1を投入し、S/D/Rは旧本文不変の期待拒否とする。
///
/// # 戻り値
/// 基本セルB-L-Eの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_error() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Error, true).await
}

///
/// 完全commit指定のLocalGit updateで4phaseを確認する。
///
/// # 注記
/// Bearerで同一skip/相違更新/再実行と完全bytesを独立観測する。
///
/// # 戻り値
/// 基本セルB-L-Uの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_update() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Update, true).await
}

///
/// 完全commit指定のLocalGit overwriteで4phaseを確認する。
///
/// # 注記
/// Bearerで旧soft-delete/新ID/復旧表示を確認し、自動復元しない。
///
/// # 戻り値
/// 基本セルB-L-Oの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_overwrite() -> Result<()> {
    run_cell(&Environment::load_ms51()?, Mode::Overwrite, true).await
}

///
/// 日本語/親子/親Markdownなしの子を逆順で含む固定集合を返す。
///
/// # 戻り値
/// LF/CRLF/BOM/末尾有無を分散した5ページ
///
fn order_pages() -> [Page; 5] {
    [Page { name: "親なし/子.md", body: F1 },
        Page { name: "a/子.md", body: b"MS51-child\r\n" },
        Page { name: "日本語.md", body: "MS51日本語".as_bytes() },
        Page { name: "z.md", body: b"MS51-z\n" },
        Page { name: "a.md", body: F0 }]
}

///
/// 新caseへ一回投入して全Created/bytesと除外pathを確認する。
///
/// # 概要
/// Report順と実状態を確認する。実HTTP発行順の直接観測ではない。
///
/// # 引数
/// * `environment` - 明示実環境
/// * `name` - 固定case名
/// * `auth` - Wiki認証方式
/// * `source` - 所有者が生存している入力
/// * `pages` - 投入される固定ページ集合
/// * `excluded` - 未生成を確認する固定相対path
///
/// # 戻り値
/// 一回のCLIと独立観測の期待一致
///
async fn created_case(environment: &Environment, name: &str, auth: Auth,
    source: &PreparedSource, pages: &[Page], excluded: &[&str]) -> Result<()>
{
    let case = Case::new(name)?;
    let mut observer = case.observer(environment, auth)?;
    case.absent(&mut observer, pages).await?;
    /*
     * 除外pathも新case内で初期不存在を確認する
     */
    for (index, name) in excluded.iter().enumerate() {
        absent_path(&mut observer, &case.path(name)?,
            &format!("E{:02}", index + 1)).await?;
    }
    let result = case.run(environment, auth, Mode::Update, source, pages)?;
    result.expect_ms51(0)?;
    result.expect_counts([pages.len() as u64, 0, 0, 0, 0, 0])?;
    /*
     * 作成IDを独立観測へ照合し、親/除外pageの非生成も確認する
     */
    let states = case.observe(&mut observer, pages).await?;
    for (page, state) in ordered(pages)?.iter().zip(&states) {
        report_identity(&case, &result, page, "Created", "draft_id", state)?;
    }
    for (index, name) in excluded.iter().enumerate() {
        absent_path(&mut observer, &case.path(name)?,
            &format!("E{:02}", index + 1)).await?;
    }
    eprintln!("MS51 case_result=accepted cli_invocations=1");
    Ok(())
}

///
/// 逆順ZIPのReport順と5ページbytes/親非生成を確認する。
///
/// # 注記
/// Basicの1CLI。製品要求順は既存loopback証拠と区別する。
///
/// # 戻り値
/// A-ORDER/A-BYTESのZIP代表一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_zip_order_and_bytes() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let pages = order_pages();
    let zip = ZipFixture::new(&pages, ZipKind::Normal)?;
    created_case(&environment, "zip-order", Auth::Basic,
        &PreparedSource::zip(zip.path()), &pages, &["親なし"]).await
}

///
/// コミット済みLocalGitのReport順とbytes/親非生成を確認する。
///
/// # 注記
/// Bearerの1CLI。fixture所有者を観測終了まで保持する。
///
/// # 戻り値
/// A-ORDER/A-BYTESのLocalGit代表一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_order_and_bytes() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let pages = order_pages();
    let git = GitFixture::new(&pages, "main")?;
    let source = PreparedSource::local(git.path(), Selector::Default)?;
    created_case(&environment, "local-order", Auth::Bearer, &source,
        &pages, &["親なし"]).await
}

///
/// 元fixtureのHEADとstatusが変化していないことを確認する。
///
/// # 引数
/// * `git` - fixture所有者
/// * `head` - 実行前の完全commit
/// * `status` - 実行前の固定fixture状態
///
/// # 戻り値
/// 元repoの不変。raw状態を表示しない。
///
fn unchanged_git(git: &GitFixture, head: &str, status: &str) -> Result<()> {
    ensure!(git.git(&["rev-parse", "HEAD"])? == head
        && git.git(&["status", "--porcelain"])? == status,
        "fixture repository changed");
    Ok(())
}

///
/// worktreeとコミット済み入力の対象集合を実Wikiまで比較する。
///
/// # 概要
/// stage後再変更/未追跡/ignore/削除を異なる専用caseへ投入する。
///
/// # 注記
/// Bearerの計2CLI。Gitはfixture生成だけに使用し元状態を照合する。
///
/// # 戻り値
/// A-WTの対象集合とbytesの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_worktree() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let committed = [Page { name: "page.md", body: F0 },
        Page { name: "staged.md", body: F0 },
        Page { name: "deleted.md", body: F0 },
        Page { name: "tracked.skip.md", body: F0 }];
    let git = GitFixture::new(&committed, "main")?;
    /*
     * 追跡後にignore規則を追加し、stage後の実bytesも変更する
     */
    git.commit(&[Page { name: ".gitignore", body: b"*.skip.md\n" }])?;
    git.write(&[Page { name: "page.md", body: F1 },
        Page { name: "staged.md", body: b"MS51-stage\n" }])?;
    git.git(&["add", "root/staged.md"])?;
    git.write(&[Page { name: "staged.md", body: F1 },
        Page { name: "new.md", body: F1 },
        Page { name: "ignored.skip.md", body: F1 }])?;
    fs::remove_file(git.path().join("root/deleted.md"))
        .map_err(|_| anyhow::anyhow!("fixture deletion"))?;
    let head = git.git(&["rev-parse", "HEAD"])?;
    let status = git.git(&["status", "--porcelain"])?;
    /*
     * 同じdirty repoをcommittedとworktreeで別caseへ投入する
     */
    let source = PreparedSource::local(git.path(),
        Selector::Commit(head.clone()))?;
    created_case(&environment, "local-committed", Auth::Bearer, &source,
        &committed, &["new", "ignored.skip"]).await?;
    unchanged_git(&git, &head, &status)?;
    let worktree = [Page { name: "page.md", body: F1 },
        Page { name: "staged.md", body: F1 },
        Page { name: "new.md", body: F1 },
        Page { name: "tracked.skip.md", body: F0 }];
    let source = PreparedSource::local(git.path(), Selector::Worktree)?;
    created_case(&environment, "local-worktree", Auth::Bearer, &source,
        &worktree, &["deleted", "ignored.skip"]).await?;
    unchanged_git(&git, &head, &status)
}

///
/// main優先/master fallback/明示branch/tag/commitを確認する。
///
/// # 概要
/// HEADを旧branchに保ちmainだけをF1へ進め、defaultと明示refを識別する。
///
/// # 注記
/// 新caseへ各1CLI、計5CLI。別master-only repoと元HEAD/statusを保持する。
///
/// # 戻り値
/// A-LREFの5代表と完全bytesの期待一致
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_local_git_refs() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let old = [Page { name: "page.md", body: F0 }];
    let new = [Page { name: "page.md", body: F1 }];
    let git = GitFixture::new(&old, "main")?;
    /*
     * master/branch/注釈tagはF0、mainだけF1にする
     */
    let first = git.git(&["rev-parse", "HEAD"])?;
    git.git(&["branch", "master"])?;
    git.git(&["branch", "fixture-branch"])?;
    git.git(&["tag", "-a", "fixture-tag", "-m", "synthetic tag"])?;
    git.commit(&new)?;
    git.git(&["checkout", "-q", "fixture-branch"])?;
    let head = git.git(&["rev-parse", "HEAD"])?;
    let status = git.git(&["status", "--porcelain"])?;
    /*
     * 既定mainと明示した3参照を異なるcaseで照合する
     */
    for (name, selector, pages) in [
        ("ref-main", Selector::Default, &new),
        ("ref-branch", Selector::Branch("fixture-branch".to_owned()), &old),
        ("ref-tag", Selector::Tag("fixture-tag".to_owned()), &old),
        ("ref-commit", Selector::Commit(first), &old)]
    {
        let source = PreparedSource::local(git.path(), selector)?;
        created_case(&environment, name, Auth::Bearer, &source, pages, &[])
            .await?;
        unchanged_git(&git, &head, &status)?;
    }
    let master = GitFixture::new(&old, "master")?;
    let head = master.git(&["rev-parse", "HEAD"])?;
    let status = master.git(&["status", "--porcelain"])?;
    let source = PreparedSource::local(master.path(), Selector::Default)?;
    created_case(&environment, "ref-master", Auth::Bearer, &source, &old, &[])
        .await?;
    unchanged_git(&master, &head, &status)
}

///
/// 小文字MarkdownがないZIP/LocalGitの空Reportを確認する。
///
/// # 概要
/// 実結果を確認する。製品HTTP0の直接証拠は別loopback試験と区別する。
///
/// # 注記
/// ZIP Basic/LocalGit Bearerの計2CLI。observer HTTPを発行しない。
///
/// # 戻り値
/// A-EMPTYの空計画/終了0/全件数0
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_empty_inputs() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let files = [Page { name: "plain.txt", body: b"ordinary" },
        Page { name: "upper.MD", body: b"MS51-upper\n" }];
    let zip = ZipFixture::new(&files, ZipKind::Normal)?;
    let git = GitFixture::new(&files, "main")?;
    /*
     * ファイルは存在するが小文字.md対象はない入力を各一回実行する
     */
    for (name, auth, source) in [
        ("empty-zip", Auth::Basic, PreparedSource::zip(zip.path())),
        ("empty-local", Auth::Bearer,
            PreparedSource::local(git.path(), Selector::Default)?)]
    {
        let case = Case::new(name)?;
        let result = case.run(&environment, auth, Mode::Update, &source, &[])?;
        result.expect_ms51(0)?;
        result.expect_counts([0; 6])?;
    }
    eprintln!("MS51 empty=accepted product_http0=not_directly_observed");
    Ok(())
}

///
/// footer有/無の小規模ZIP64を実CLIから投影してbytesを確認する。
///
/// # 注記
/// Basic/updateの新規Nだけを各1CLIで実施する。S/D/R/相違overwriteは未被覆。
///
/// # 戻り値
/// A-Z64の2形式の代表一致。巨大/特殊ZIP64の証明ではない。
///
#[tokio::test]
#[ignore = "requires approved MS51 real Wiki environment"]
async fn ms51_real_small_zip64() -> Result<()> {
    let environment = Environment::load_ms51()?;
    let pages = [Page { name: "page.md", body: F0 }];
    for (name, kind) in [("zip64-entry", ZipKind::Entry64),
        ("zip64-footer", ZipKind::Footer64)]
    {
        let zip = ZipFixture::new(&pages, kind)?;
        created_case(&environment, name, Auth::Basic,
            &PreparedSource::zip(zip.path()), &pages, &[]).await?;
    }
    Ok(())
}

///
/// 実環境を読まずMS51 scopeと近似targetの拒否を確認する。
///
/// # 注記
/// 固定文字列だけを使い、HTTP/実資格情報/子プロセスを使用しない。
///
/// # 戻り値
/// なし。許可境界を拡張する誤りはassertで検出する。
///
#[test]
fn ms51_guards_reject_foreign_scopes_and_targets() {
    assert!(approved_target("https://localhost:8040"));
    assert!(within(&format!("{MS51_ROOT}/run/case"), MS51_ROOT));
    for path in [ROOT.to_owned(), MS51_ROOT.to_owned(),
        format!("{MS51_ROOT}-other/case"), format!("{MS51_ROOT}/../case"),
        format!("{MS51_ROOT}/bad name"), format!("{MS51_ROOT}//case")]
    { assert!(!within(&path, MS51_ROOT)); }
    for url in ["http://localhost:8040", "https://localhost:8080",
        "https://127.0.0.1:8040", "https://localhost:8040/other"]
    { assert!(!approved_target(url)); }
    for name in ["../page.md", "/page.md", "a//b.md", "a/.git/config"] {
        assert!(!safe_entry(name));
    }
}

///
/// 四入力builderが既存CLI構文だけを作ることを確認する。
///
/// # 概要
/// 任意argvを受けず、Local/HTTPSはgitとして構成することを照合する。
///
/// # 注記
/// Commandは生成/参照だけで実行せず、source名は全て合成値を使う。
///
/// # 戻り値
/// 構文とworktree/ref制約の一致
///
#[test]
fn ms51_typed_sources_keep_existing_cli_syntax() -> Result<()> {
    let local = Path::new("/synthetic/repo");
    let cases = [
        (PreparedSource::zip(Path::new("/synthetic/input.zip")), "zip"),
        (PreparedSource::local(local, Selector::Worktree)?, "git"),
        (PreparedSource::https("https://example.invalid/repo",
            Selector::Default)?, "git"),
        (PreparedSource::github("synthetic/repo", Selector::Default)?,
            "github")];
    /*
     * 子は起動せず型付きsourceが付加した引数だけを検査する
     */
    for (source, kind) in cases {
        let mut command = Command::new("synthetic-command");
        source.append(&mut command);
        let args: Vec<_> = command.get_args().map(|v| v.to_string_lossy())
            .collect();
        assert_eq!(args[0], "--source-type");
        assert_eq!(args[1], kind);
        assert!(!args.iter().any(|v| matches!(v.as_ref(),
            "--token" | "--password" | "--prefix" | "--config-path")));
    }
    assert!(PreparedSource::https("https://example.invalid/repo",
        Selector::Worktree).is_err());
    assert!(PreparedSource::github("synthetic/repo", Selector::Worktree)
        .is_err());
    assert!(PreparedSource::https("https://secret@example.invalid/repo",
        Selector::Default).is_err());
    Ok(())
}

///
/// .md除去後の投影順と空basename拒否を確認する。
///
/// # 注記
/// a.md/a-foo.mdの反転例を固定期待で照合し、ファイルI/Oは行わない。
///
/// # 戻り値
/// 投影先基準の順序一致
///
#[test]
fn ms51_expected_order_uses_projected_page_names() -> Result<()> {
    let pages = [Page { name: "a-foo.md", body: F0 },
        Page { name: "a/子.md", body: F0 }, Page { name: "a.md", body: F0 }];
    let names: Vec<_> = ordered(&pages)?.iter().map(|v| v.name).collect();
    assert_eq!(names, ["a.md", "a-foo.md", "a/子.md"]);
    assert!(ordered(&[Page { name: "dir/.md", body: F0 }]).is_err());
    assert!(ordered(&[Page { name: ".md", body: F0 }]).is_err());
    Ok(())
}

///
/// 読取失敗を固定再現する合成reader。
///
struct BrokenReader;

impl Read for BrokenReader {
    ///
    /// 任意の元エラー文を含む失敗を合成する。
    ///
    /// # 引数
    /// * `_buffer` - 今回は書き込まないbuffer
    ///
    /// # 戻り値
    /// 非公開に保つ合成I/O失敗
    ///
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("synthetic-private-diagnostic"))
    }
}

///
/// 有限captureの境界/超過後drain/読取失敗を確認する。
///
/// # 概要
/// 固定合成readerだけで1MiB境界と両stream拒否を確認する。
///
/// # 注記
/// 実子/HTTP/資格情報は使わず、超過後もreaderがEOFへ到達するか調べる。
///
/// # 戻り値
/// なし。断片を成功解析すれば失敗する。
///
#[test]
fn ms51_capture_limits_drain_and_reject_incomplete_streams() {
    let exact = drain(Cursor::new(vec![b'x'; CAPTURE_LIMIT]));
    assert!(!exact.rejected);
    assert_eq!(exact.bytes.len(), CAPTURE_LIMIT);
    /*
     * 片側超過でもEOFまで読み、他側の内容から成功を補完しない
     */
    let mut reader = Cursor::new(vec![b'x'; CAPTURE_LIMIT + 8193]);
    let oversized = drain(&mut reader);
    assert_eq!(reader.position(), (CAPTURE_LIMIT + 8193) as u64);
    assert!(oversized.rejected && oversized.bytes.is_empty());
    assert!(decode_capture(Some(0), oversized, drain(Cursor::new(b"")))
        .is_err());
    assert!(decode_capture(Some(0), drain(Cursor::new(b"")),
        drain(BrokenReader)).is_err());
    assert!(decode_capture(Some(0), drain(Cursor::new([0xff])),
        drain(Cursor::new(b""))).is_err());
}

///
/// signal終了を有限記録し、終了0へ補完しないことを確認する。
///
/// # 注記
/// Unixの合成ExitStatusを使い、signalを実送信しない。
///
/// # 戻り値
/// signal数値保持と成功拒否の一致
///
#[test]
fn ms51_capture_keeps_signal_and_rejects_missing_exit() -> Result<()> {
    let status = ExitStatus::from_raw(9);
    assert_eq!(status_record(&status),
        "MS51 subprocess_exit=None signal=Some(9)");
    let output = decode_capture(None, drain(Cursor::new(b"")),
        drain(Cursor::new(b"")))?;
    assert!(output.expect_ms51(0).is_err());
    Ok(())
}

///
/// 件数と旧復旧情報の重複/欠落/不一致を検出する。
///
/// # 概要
/// 有限出力だけを合成し、aliasへ私的path原文を残さないことも確認する。
///
/// # 注記
/// 実環境/子/HTTPを使わず、全7件数と旧ID/revision/削除確定を変造する。
///
/// # 戻り値
/// 不正出力拒否と既知の正しい復旧情報の採用
///
#[test]
fn ms51_parser_rejects_counts_and_recovery_ambiguity() -> Result<()> {
    let path = format!("{MS51_ROOT}/pure/case/private-name");
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let summary = "created=0 skipped=0 updated=0 recreated=1 warnings=0 \
        failed=0 not_executed=0\n";
    let recovery = format!("recovery: path={path} old_id=Some(\"{id}\") \
        revision=Some(7) draft_id=None old_delete_confirmed=true \
        old_lock_observed=None\n");
    let good = Captured { code: Some(0), stdout: format!("{summary}{recovery}"),
        stderr: String::new() };
    good.expect_counts([0, 0, 0, 1, 0, 0])?;
    good.expect_old_recovery(&path, id, 7, true)?;
    /*
     * 七つのfieldすべてで欠落/重複を拒否する
     */
    for field in summary.split_ascii_whitespace() {
        let missing = Captured { code: Some(0),
            stdout: summary.replace(field, ""), stderr: String::new() };
        assert!(missing.expect_counts([0, 0, 0, 1, 0, 0]).is_err());
        let duplicate = Captured { code: Some(0),
            stdout: format!("{} {field}\n", summary.trim_end()),
            stderr: String::new() };
        assert!(duplicate.expect_counts([0, 0, 0, 1, 0, 0]).is_err());
    }
    let duplicate = Captured { code: Some(0),
        stdout: format!("{recovery}{recovery}"), stderr: String::new() };
    assert!(duplicate.expect_old_recovery(&path, id, 7, true).is_err());
    assert!(good.expect_old_recovery(&path, id, 8, true).is_err());
    assert!(good.expect_old_recovery(&path, id, 7, false).is_err());
    assert!(!good.alias_record(&path, "P01").contains("private-name"));
    assert!(good.alias_record(&path, "raw-private").contains("rejected"));
    Ok(())
}

///
/// 合成資格情報の方式選択と取得token/親設定の非継承を確認する。
///
/// # 概要
/// 子を起動せず、合成envの上書き/除去とargv非露出を照合する。
///
/// # 注記
/// loadは呼ばず固定合成値を使う。環境の可変Gitキーは名前だけを除去する。
///
/// # 戻り値
/// Wikiと取得認証の分離、および合成漏出の拒否
///
#[test]
fn ms51_child_auth_is_selected_without_acquisition_credentials() -> Result<()> {
    let environment = Environment::synthetic_ms51()?;
    for auth in [Auth::Basic, Auth::Bearer] {
        let mut command = Command::new("synthetic-command");
        command.env("GITHUB_TOKEN", "synthetic-acquisition")
            .env("LUWIKI_PASSWORD", "synthetic-foreign-password")
            .env("LUWIKI_TOKEN", "synthetic-foreign-token")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "synthetic.key")
            .env("GIT_CONFIG_VALUE_0", "synthetic-config")
            .env("HTTPS_PROXY", "https://synthetic.invalid")
            .env("LUWIKI_MS42_TEST_BEARER_TOKEN", "synthetic-parent");
        /*
         * 実行せず、子に明示設定される値だけを固定期待へ照合する
         */
        environment.configure_child(&mut command, auth);
        let value = |name: &str| command.get_envs()
            .find(|(key, _)| *key == name).and_then(|(_, value)| value);
        for name in ["GITHUB_TOKEN", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0", "HTTPS_PROXY",
            "LUWIKI_MS42_TEST_BEARER_TOKEN"]
        { assert!(value(name).is_none()); }
        match auth {
            Auth::Basic => {
                assert_eq!(value("LUWIKI_USER").and_then(|v| v.to_str()),
                    Some("synthetic-user"));
                assert_eq!(value("LUWIKI_PASSWORD").and_then(|v| v.to_str()),
                    Some("synthetic-password-only"));
                assert!(value("LUWIKI_TOKEN").is_none());
            }
            Auth::Bearer => {
                assert_eq!(value("LUWIKI_TOKEN").and_then(|v| v.to_str()),
                    Some("synthetic-bearer-only"));
                assert!(value("LUWIKI_USER").is_none());
                assert!(value("LUWIKI_PASSWORD").is_none());
            }
        }
        assert_eq!(command.get_args().count(), 0);
    }
    assert!(environment.check_output("synthetic-bearer-only").is_err());
    assert!(environment.observer(ROOT, Auth::Basic).is_err());
    assert!(environment.observer(MS51_ROOT, Auth::Basic).is_err());
    Ok(())
}

///
/// 本文/認証表現/出力先混在を生表示せず拒否する。
///
/// # 注記
/// 全て合成文字列を用い、実環境・子・HTTPを使用しない。
///
/// # 戻り値
/// なし。本文や認証を許可すれば失敗する。
///
#[test]
fn ms51_output_rejects_body_headers_and_stream_mix() {
    for (stdout, stderr) in [("MS51合成初回", ""),
        ("", "Authorization: synthetic"), ("", "X-Lock-Authentication"),
        ("warning: synthetic", ""), ("", "result: exit=0")]
    {
        let captured = Captured { code: Some(0), stdout: stdout.to_owned(),
            stderr: stderr.to_owned() };
        assert!(validate_output(&captured, &[]).is_err());
    }
}
