/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! 同期I/Oの実行境界、協調停止、一時データ回収を実証
//!

use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Result, bail, ensure};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

///
/// 一時Git fixture内でだけGitコマンドを実行
///
/// # 概要
/// 一時repoに限定した設定と環境でGitを実行し、終了成功を確認する。
///
/// # 引数
/// - `dir`: fixtureの作業ツリー
/// - `args`: Git CLI引数
///
/// # 戻り値
/// 成功、またはfixture作成エラー
///
fn fixture_git(dir: &Path, args: &[&str]) -> Result<()> {
    /*
     * 一時repo用の設定・環境でGitを実行する
     */
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()?;
    /*
     * 終了成功を確認してfixture作成結果を返す
     */
    ensure!(
        output.status.success(),
        "Git fixture setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

///
/// file transportで取得できるローカルGit fixtureを作成
///
/// # 引数
/// - `dir`: 新規fixtureを作る専用パス
///
/// # 戻り値
/// 初期コミット済みリポジトリ、または作成エラー
///
fn make_git_fixture(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("root"))?;
    fixture_git(dir, &["init", "-q", "-b", "main"])?;
    fs::write(dir.join("root/page.md"), b"fixture\n")?;
    fixture_git(dir, &["add", "root/page.md"])?;
    fixture_git(dir, &["commit", "-qm", "initial"])?;
    Ok(())
}

///
/// gixの準備、fetch、checkoutを同一同期関数内に閉じる
///
/// # 概要
/// file fixtureのclone準備・fetch・checkoutを同一同期処理で行い、commit IDを返す。
///
/// # 引数
/// - `source`: 一時fixtureのGit worktree
/// - `destination`: 一時fixture内の未使用パス
/// - `interrupted`: Gitに渡す協調停止フラグ
///
/// # 戻り値
/// 取得したコミットID、またはGitエラー
///
fn clone_file_repo(
    source: &Path,
    destination: &Path,
    interrupted: &AtomicBool,
) -> Result<String> {
    /*
     * file fixtureのclone準備とmain参照を指定する
     */
    let url = format!("file://{}", source.display());
    let mut prepare = gix::clone::PrepareFetch::new(
        url.as_str(),
        destination,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::isolated(),
    )?
    .with_revision(Some("refs/heads/main"))?;
    /*
     * 同じ同期処理でfetch・checkoutしてcommit IDを返す
     */
    let (mut checkout, _) =
        prepare.fetch_then_checkout(gix::progress::Discard, interrupted)?;
    let (repo, _) =
        checkout.main_worktree(gix::progress::Discard, interrupted)?;
    Ok(repo.head_commit()?.id.to_string())
}

///
/// 協調停止を確認しながらZIPの単一エントリを
/// 小分けに読む
///
/// # 概要
/// ZIPの単一entryを小分けに読み、停止確認とchunk通知を行って実読込量を返す。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `interrupted`: 読込停止フラグ
/// - `on_chunk`: 読込済みチャンク数を受け取る検証用関数
///
/// # 戻り値
/// 実際に読んだバイト数、または中断・読込エラー
///
fn read_zip_in_chunks(
    bytes: &[u8],
    interrupted: &AtomicBool,
    mut on_chunk: impl FnMut(usize),
) -> Result<usize> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut entry = archive.by_index(0)?;
    let mut buffer = [0_u8; 128];
    let mut total = 0;
    let mut chunks = 0;

    /*
     * 各read呼出しの直前で停止フラグを確認
     */
    loop {
        if interrupted.load(Ordering::Acquire) {
            bail!("ZIP read interrupted");
        }
        let count = entry.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count;
        chunks += 1;
        on_chunk(chunks);
    }
    Ok(total)
}

///
/// テスト専用ZIPをメモリ上に生成
///
/// # 戻り値
/// 1024バイトのエントリを含むZIP
///
fn make_zip_fixture() -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored);
    writer.start_file("root/page.md", options)?;
    writer.write_all(&[b'a'; 1024])?;
    Ok(writer.finish()?.into_inner())
}

///
/// 一時ディレクトリを所有する作業と回収結果を確認
///
/// # 引数
/// - `scenario`: 一時領域内で実行する検証処理
///
/// # 戻り値
/// 作業の結果と回収確認、または一時領域作成エラー
///
fn temp_scenario(
    scenario: impl FnOnce(&Path) -> Result<()>,
) -> Result<Result<()>> {
    let temp = TempDir::new()?;
    let path: PathBuf = temp.path().to_path_buf();
    let outcome = scenario(&path);
    drop(temp);
    ensure!(!path.exists(), "temporary directory remains");
    Ok(outcome)
}

///
/// gixの非Send準備値をspawn_blockingの外へ出さず取得
///
/// # 戻り値
/// cloneとcheckoutが終われば成功
///
/// # 注記
/// fixture作成だけGit CLIを使い、取得はgixで行う。
///
#[tokio::test]
async fn gix_clone_within_one_blocking_closure() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    make_git_fixture(&source)?;
    let id = tokio::task::spawn_blocking(move || {
        let interrupted = AtomicBool::new(false);
        clone_file_repo(&source, &destination, &interrupted)
    })
    .await??;
    ensure!(!id.is_empty(), "empty clone commit id");
    Ok(())
}

///
/// Git fetch/checkoutが同じ停止フラグを受け取ることを実証
///
/// # 戻り値
/// 中断要求で取得が失敗し、
/// 途中cloneが残らなければ成功
///
/// # 注記
/// flagを開始前に立て、gixが中断を観測するか調べる。
///
#[test]
fn gix_interrupt_and_partial_clone_cleanup() -> Result<()> {
    let temp = TempDir::new()?;
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    make_git_fixture(&source)?;
    let interrupted = AtomicBool::new(true);
    let result = clone_file_repo(&source, &destination, &interrupted);
    ensure!(result.is_err(), "gix did not observe the interrupt");
    ensure!(!destination.exists(), "partial clone remains");
    Ok(())
}

///
/// ZIPを分割して読み正常完了と途中中断を確認
///
/// # 概要
/// 正常読込量を確認してから二chunk後に取消し、停止検出と読込回数を照合する。
///
/// # 戻り値
/// 正常時の総量と中断時の停止が一致すれば成功
///
/// # 注記
/// 2チャンク後に検証用コールバックが
/// 停止フラグを立てる。
///
#[test]
fn zip_chunked_read_cooperatively_stops() -> Result<()> {
    /*
     * 正常時にZIP全量を読めることを照合する
     */
    let bytes = make_zip_fixture()?;
    let interrupted = AtomicBool::new(false);
    let total = read_zip_in_chunks(&bytes, &interrupted, |_| {})?;
    assert_eq!(total, 1024);

    /*
     * 二chunk後に取消し停止結果と読込回数を照合する
     */
    let chunks = AtomicUsize::new(0);
    let result = read_zip_in_chunks(&bytes, &interrupted, |count| {
        chunks.store(count, Ordering::Release);
        if count == 2 {
            interrupted.store(true, Ordering::Release);
        }
    });
    ensure!(result.is_err(), "ZIP read ignored the interrupt");
    assert_eq!(chunks.load(Ordering::Acquire), 2);
    Ok(())
}

///
/// 正常、失敗、中断時のTempDir回収を実証
///
/// # 概要
/// 正常・注入失敗・ZIP取消の三経路を所有TempDir内で実行し、回収結果を確認する。
///
/// # 戻り値
/// 全経路で所有ディレクトリが消えれば成功
///
/// # 注記
/// 中断経路ではZIPの協調停止を一時領域内で実行する。
///
#[test]
fn owned_tempdir_cleanup_on_all_paths() -> Result<()> {
    /*
     * 正常処理とTempDir回収を確認する
     */
    let normal = temp_scenario(|dir| {
        fs::write(dir.join("normal"), b"ok")?;
        Ok(())
    })?;
    ensure!(normal.is_ok(), "normal scenario failed");

    /*
     * 途中失敗とTempDir回収を確認する
     */
    let failure = temp_scenario(|dir| {
        fs::write(dir.join("failure"), b"partial")?;
        bail!("intentional failure")
    })?;
    ensure!(failure.is_err(), "failure scenario succeeded");

    /*
     * ZIP取消処理とTempDir回収を確認する
     */
    let bytes = make_zip_fixture()?;
    let interrupted = AtomicBool::new(true);
    let cancelled = temp_scenario(|dir| {
        fs::write(dir.join("interrupted"), b"partial")?;
        read_zip_in_chunks(&bytes, &interrupted, |_| {})?;
        Ok(())
    })?;
    ensure!(cancelled.is_err(), "interrupt scenario succeeded");
    Ok(())
}

///
/// GitのHTTP接続失敗時に一時ディレクトリを回収
///
/// # 概要
/// 閉じたloopbackポートへGit接続し、失敗結果と所有TempDir回収を確認する。
///
/// # 戻り値
/// 接続失敗が観測され、一時領域が消えれば成功
///
/// # 注記
/// 空きポートを取得して閉じる。
/// 外部サービスへ接続せず失敗させる。
///
#[test]
fn git_transport_failure_cleans_tempdir() -> Result<()> {
    /*
     * 空きloopbackポートを取得して閉じる
     */
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);

    /*
     * 専用TempDirでGitの接続失敗と回収を確認する
     */
    let outcome = temp_scenario(|path| {
        let url = format!("http://{address}/missing.git");
        let destination = path.join("destination");
        let interrupted = AtomicBool::new(false);
        let mut prepare = gix::clone::PrepareFetch::new(
            url.as_str(),
            &destination,
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )?;
        let (checkout, _) =
            prepare.fetch_then_checkout(gix::progress::Discard, &interrupted)?;
        drop(checkout);
        Ok(())
    })?;
    ensure!(outcome.is_err(), "transport unexpectedly succeeded");
    Ok(())
}

///
/// 実行境界モジュールが統合テストへ接続済みか確認
///
/// # 注記
/// グループ1で用意した共有ヘルパーを確認する。
///
/// # 戻り値
/// なし。実行境界実証モジュールの共有ヘルパー接続が異なればassertで失敗する。
///
#[test]
fn runtime_probe_is_connected() {
    assert_eq!(super::support::probe_group_name(), "ms1.2");
}
