/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! 設定ファイルの8種類の入力上限を計測する技術実証
//!

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tempfile::TempDir;
use tokio::sync::Semaphore;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

///
/// 上限キーの単位と計数境界を示す実証用定義
///
struct LimitMeaning {
    key: &'static str,
    unit: &'static str,
    boundary: &'static str,
}

const LIMIT_MEANINGS: [LimitMeaning; 8] = [
    LimitMeaning {
        key: "max_pages",
        unit: "pages",
        boundary: "source validation before Wiki write",
    },
    LimitMeaning {
        key: "max_page_bytes",
        unit: "UTF-8 bytes per Markdown page",
        boundary: "each page body before Wiki write",
    },
    LimitMeaning {
        key: "max_zip_entries",
        unit: "all central-directory entries",
        boundary: "before ZIP decompression",
    },
    LimitMeaning {
        key: "max_zip_compressed_bytes",
        unit: "whole ZIP archive bytes",
        boundary: "before ZIP parsing",
    },
    LimitMeaning {
        key: "max_zip_uncompressed_bytes",
        unit: "sum of uncompressed bytes",
        boundary: "metadata preflight and chunked reading",
    },
    LimitMeaning {
        key: "max_git_bytes",
        unit: "dedicated temporary directory bytes after fetch",
        boundary: "after fetch, before checkout and Wiki write",
    },
    LimitMeaning {
        key: "max_processing_seconds",
        unit: "wall-clock seconds",
        boundary: "phase transitions and cooperative checks",
    },
    LimitMeaning {
        key: "max_http_concurrency",
        unit: "in-flight HTTP requests",
        boundary: "permit before dispatch, release on completion",
    },
];

///
/// 正常ZIPから測った件数とデータサイズ
///
#[derive(Debug)]
struct ZipMeasure {
    entries: usize,
    archive_bytes: u64,
    compressed_payload: u64,
    uncompressed: u64,
    actual: u64,
}

///
/// 非負整数上限の境界を検査
///
/// # 引数
/// - `observed`: 計測値
/// - `limit`: 設定された最大値
///
/// # 戻り値
/// 境界値以下なら成功、超過または加算overflowならエラー
///
fn under_limit(observed: u64, limit: u64) -> Result<()> {
    ensure!(observed <= limit, "limit exceeded");
    Ok(())
}

///
/// 集計値をoverflowと上限を確認しながら増やす
///
/// # 引数
/// - `total`: 現在までの集計値
/// - `addition`: 新たに計数する値
/// - `limit`: 許容する最大値
///
/// # 戻り値
/// 更新済み集計値、または超過・overflowエラー
///
fn add_bounded(total: u64, addition: u64, limit: u64) -> Result<u64> {
    let next = total
        .checked_add(addition)
        .ok_or_else(|| anyhow::anyhow!("counter overflow"))?;
    under_limit(next, limit)?;
    Ok(next)
}

///
/// 投入対象となるMarkdownファイル件数を計数
///
/// # 引数
/// - `paths`: 入力ルートからの相対パス列
///
/// # 戻り値
/// `.md`ファイルの件数
///
fn count_pages(paths: &[&str]) -> usize {
    paths.iter().filter(|path| path.ends_with(".md")).count()
}

///
/// 1ページの本文をUTF-8バイト単位で計数
///
/// # 引数
/// - `body`: Markdown本文
///
/// # 戻り値
/// 正規化前の本文バイト数
///
fn page_bytes(body: &str) -> usize {
    body.len()
}

///
/// 小さな代表ZIPを生成
///
/// # 戻り値
/// 圧縮方式と本文サイズを固定した2エントリのZIP
///
fn representative_zip() -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated);
    for name in ["root/first.md", "root/second.md"] {
        writer.start_file(name, options)?;
        writer.write_all(&[b'a'; 1024])?;
    }
    Ok(writer.finish()?.into_inner())
}

///
/// ZIPの宣言サイズと実読込量を計測
///
/// # 概要
/// ZIPの宣言サイズを全entryから数え、別の実読込量には指定上限を適用する。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `actual_limit`: 読込中に適用する展開量上限
///
/// # 戻り値
/// 件数・圧縮後・展開後・実読込量
///
fn measure_zip(bytes: &[u8], actual_limit: u64) -> Result<ZipMeasure> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut measure = ZipMeasure {
        entries: archive.len(),
        archive_bytes: u64::try_from(bytes.len())?,
        compressed_payload: 0,
        uncompressed: 0,
        actual: 0,
    };

    /*
     * 全エントリのメタデータ値を事前計数
     */
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index)?;
        measure.compressed_payload = add_bounded(
            measure.compressed_payload,
            file.compressed_size(),
            u64::MAX,
        )?;
        measure.uncompressed =
            add_bounded(measure.uncompressed, file.size(), u64::MAX)?;
    }

    /*
     * 宣言値とは別に実際の読込量を小分けに計数
     */
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let mut buffer = [0_u8; 128];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            measure.actual = add_bounded(
                measure.actual,
                u64::try_from(count)?,
                actual_limit,
            )?;
        }
    }
    Ok(measure)
}

///
/// 取得済みGit一時領域のファイルサイズを計数
///
/// # 概要
/// 各階層のsymlinkを拒否し、通常ファイルサイズを残り上限内で再帰的に計量する。
///
/// # 引数
/// - `path`: Git cloneを格納する一時領域
/// - `limit`: 許容する総ファイルサイズ
///
/// # 戻り値
/// 一時領域内の総ファイルサイズ、または超過エラー
///
fn post_fetch_disk_bytes(path: &Path, limit: u64) -> Result<u64> {
    /*
     * 各entryのsymlinkを拒否して階層ごとのサイズを求める
     */
    let mut total = 0;
    for item in fs::read_dir(path)? {
        let item = item?;
        let metadata = fs::symlink_metadata(item.path())?;
        if metadata.file_type().is_symlink() {
            bail!("symlink in Git temporary data");
        }
        let bytes = if metadata.is_dir() {
            post_fetch_disk_bytes(&item.path(), limit - total)?
        } else {
            metadata.len()
        };
        /*
         * 残り上限と合計量を照合する
         */
        total = add_bounded(total, bytes, limit)?;
    }
    Ok(total)
}

///
/// 一時Git fixtureに限定してGitコマンドを実行
///
/// # 概要
/// 一時repo向けの設定と環境でGitを実行し、終了成功だけを返す。
///
/// # 引数
/// - `dir`: 一時リポジトリのパス
/// - `args`: Git CLIに渡す引数
///
/// # 戻り値
/// fixture作成に成功すれば成功
///
fn fixture_git(dir: &Path, args: &[&str]) -> Result<()> {
    /*
     * fixture用の設定・環境でGitを実行する
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
/// gix取得専用の一時Gitリポジトリを作成
///
/// # 引数
/// - `dir`: fixtureを新規作成するパス
///
/// # 戻り値
/// mainにコミットを持つGitリポジトリの作成結果
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
/// 処理開始からの壁時計経過時間を計測
///
/// # 引数
/// - `started`: 処理全体の開始時刻
///
/// # 戻り値
/// 計測時点の経過時間
///
fn processing_elapsed(started: Instant) -> Duration {
    started.elapsed()
}

///
/// 処理時間上限を経過時間と比較
///
/// # 引数
/// - `elapsed`: 計測された経過時間
/// - `max_seconds`: 設定値（秒）
///
/// # 戻り値
/// 境界値以下なら成功、超過ならエラー
///
fn check_processing_time(elapsed: Duration, max_seconds: u64) -> Result<()> {
    ensure!(elapsed <= Duration::from_secs(max_seconds), "time limit");
    Ok(())
}

///
/// 8キーの単位と計数境界が空でないことを確認
///
/// # 戻り値
/// 全キーが一意で情報を持てば成功
///
/// # 注記
/// 数値の既定値はこの実証では定めない。
///
#[test]
fn eight_limit_meanings_are_recorded() -> Result<()> {
    let mut keys = std::collections::HashSet::new();
    for meaning in LIMIT_MEANINGS {
        ensure!(keys.insert(meaning.key), "duplicate limit key");
        ensure!(!meaning.unit.is_empty(), "unit is empty");
        ensure!(!meaning.boundary.is_empty(), "boundary is empty");
    }
    assert_eq!(keys.len(), 8);
    Ok(())
}

///
/// 投入対象ページ数の代表・境界・超過を確認
///
/// # 戻り値
/// `.md`だけが対象となり境界値で判定できれば成功
///
/// # 注記
/// ディレクトリと非Markdownファイルは件数へ含めない。
///
#[test]
fn max_pages_boundary() -> Result<()> {
    let count = count_pages(&["root/a.md", "root/b.md", "root/note.txt"]);
    assert_eq!(count, 2);
    under_limit(u64::try_from(count)?, 2)?;
    ensure!(under_limit(u64::try_from(count)?, 1).is_err());
    Ok(())
}

///
/// 1ページのUTF-8バイト長を文字数と区別
///
/// # 戻り値
/// 3バイト本文を境界値で受理し2バイトで拒否すれば成功
///
/// # 注記
/// 改行・BOM等の変換は行わない。
///
#[test]
fn max_page_bytes_boundary() -> Result<()> {
    let count = page_bytes("あ");
    assert_eq!(count, 3);
    under_limit(u64::try_from(count)?, 3)?;
    ensure!(under_limit(u64::try_from(count)?, 2).is_err());
    Ok(())
}

///
/// ZIPエントリ数をディレクトリ以外も含めて比較
///
/// # 戻り値
/// 2件を境界値で受理し1件で拒否すれば成功
///
/// # 注記
/// 重複名を含む生エントリの検出は
/// グループ2のprobeを参照する。
///
#[test]
fn max_zip_entries_boundary() -> Result<()> {
    let bytes = representative_zip()?;
    let measure = measure_zip(&bytes, u64::MAX)?;
    assert_eq!(measure.entries, 2);
    under_limit(u64::try_from(measure.entries)?, 2)?;
    ensure!(under_limit(u64::try_from(measure.entries)?, 1).is_err());
    Ok(())
}

///
/// ZIPファイル全体サイズの境界を確認
///
/// # 戻り値
/// ヘッダー等を含む全バイト数を
/// 境界値で判定できれば成功
///
/// # 注記
/// 圧縮payloadに加えてヘッダーと
/// 中央ディレクトリも含める。
///
#[test]
fn max_zip_compressed_bytes_boundary() -> Result<()> {
    let bytes = representative_zip()?;
    let measure = measure_zip(&bytes, u64::MAX)?;
    ensure!(measure.archive_bytes > 0, "empty ZIP file");
    under_limit(measure.archive_bytes, measure.archive_bytes)?;
    ensure!(
        under_limit(measure.archive_bytes, measure.archive_bytes - 1).is_err()
    );
    Ok(())
}

///
/// ZIP展開後宣言量と実読込量の境界を確認
///
/// # 戻り値
/// 2048バイトを境界値で受理し超過を拒否すれば成功
///
/// # 注記
/// 実読込上限はメタデータとは独立に検査する。
///
#[test]
fn max_zip_uncompressed_bytes_boundary() -> Result<()> {
    let bytes = representative_zip()?;
    let measure = measure_zip(&bytes, 2048)?;
    assert_eq!(measure.uncompressed, 2048);
    assert_eq!(measure.actual, 2048);
    under_limit(measure.uncompressed, 2048)?;
    ensure!(under_limit(measure.uncompressed, 2047).is_err());
    ensure!(measure_zip(&bytes, 2047).is_err());
    Ok(())
}

///
/// Git取得後の一時領域サイズを境界値で比較
///
/// # 戻り値
/// 10バイトを受理し9バイトで拒否すれば成功
///
/// # 注記
/// 通信中の転送量制限ではなく事後計測の実証である。
///
#[test]
fn max_git_bytes_post_fetch_proxy_boundary() -> Result<()> {
    let temp = TempDir::new()?;
    fs::create_dir_all(temp.path().join("objects"))?;
    fs::write(temp.path().join("objects/a"), b"four")?;
    fs::write(temp.path().join("objects/b"), b"sixsix")?;
    assert_eq!(post_fetch_disk_bytes(temp.path(), 10)?, 10);
    ensure!(post_fetch_disk_bytes(temp.path(), 9).is_err());
    Ok(())
}

///
/// gix fetch後に低い一時領域上限を適用しcheckoutを拒否
///
/// # 概要
/// file fixtureをfetchしてcheckout前の容量超過を拒否し、所有領域回収を確認する。
///
/// # 戻り値
/// checkout前の拒否と一時領域回収を確認できれば成功
///
/// # 注記
/// file transportのGit fixtureを使う。
/// 転送中の厳密なbytes上限は試さない。
///
#[test]
fn max_git_bytes_rejects_before_checkout() -> Result<()> {
    let temp = TempDir::new()?;
    let temp_path = temp.path().to_path_buf();
    let source = temp.path().join("source");
    let destination = temp.path().join("fetched");
    make_git_fixture(&source)?;

    /*
     * gixで取得し、checkout前の専用領域を計測
     */
    let url = format!("file://{}", source.display());
    let mut prepare = gix::clone::PrepareFetch::new(
        url.as_str(),
        &destination,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::isolated(),
    )?
    .with_revision(Some("refs/heads/main"))?;
    let interrupted = AtomicBool::new(false);
    let (checkout, _) =
        prepare.fetch_then_checkout(gix::progress::Discard, &interrupted)?;
    let observed = post_fetch_disk_bytes(&destination, u64::MAX)?;
    ensure!(observed > 0, "empty fetched repository");
    ensure!(!destination.join("root/page.md").exists());

    /*
     * 低い上限で拒否し、所有する一時領域を破棄
     */
    let low_limit = observed - 1;
    ensure!(under_limit(observed, low_limit).is_err());
    drop(checkout);
    ensure!(!destination.exists(), "unaccepted clone remains");
    drop(temp);
    ensure!(!temp_path.exists(), "temporary directory remains");
    Ok(())
}

///
/// 処理時間の代表・境界・超過を比較
///
/// # 戻り値
/// 2秒を許容し1秒で拒否すれば成功
///
/// # 注記
/// 実測値は報告用であり固定の既定値にはしない。
///
#[test]
fn max_processing_seconds_boundary() -> Result<()> {
    let started = Instant::now();
    let observed = processing_elapsed(started);
    ensure!(observed < Duration::from_secs(1), "clock probe too slow");
    check_processing_time(Duration::from_secs(2), 2)?;
    ensure!(check_processing_time(Duration::from_secs(2), 1).is_err());
    Ok(())
}

///
/// HTTP同時リクエスト数をpermitで制限
///
/// # 戻り値
/// 2件目まで受理し3件目を待機させられれば成功
///
/// # 注記
/// リクエスト送信前に取得し、完了時のdropで解放する。
///
#[test]
fn max_http_concurrency_boundary() -> Result<()> {
    let permits = Semaphore::new(2);
    let first = permits.try_acquire()?;
    let second = permits.try_acquire()?;
    assert_eq!(permits.available_permits(), 0);
    ensure!(permits.try_acquire().is_err());
    drop(first);
    let third = permits.try_acquire()?;
    drop(second);
    drop(third);
    assert_eq!(permits.available_permits(), 2);
    Ok(())
}

///
/// 代表入力の8指標を測定し表示
///
/// # 概要
/// 人工fixtureのZIP・一時容量・時間・件数・同時処理指標を計測して表示する。
///
/// # 戻り値
/// 再現可能な人工fixtureの計測に成功すれば成功
///
/// # 注記
/// 出力値は候補検討用であり設定既定値ではない。
///
#[test]
fn representative_measurements() -> Result<()> {
    /*
     * 人工fixtureから上限検討用の指標を計測する
     */
    let started = Instant::now();
    let zip = representative_zip()?;
    let values = measure_zip(&zip, u64::MAX)?;
    let temp = TempDir::new()?;
    fs::write(temp.path().join("pack-sample"), &[0_u8; 4096])?;
    let disk = post_fetch_disk_bytes(temp.path(), u64::MAX)?;
    let elapsed = processing_elapsed(started);
    let pages = count_pages(&["root/first.md", "root/second.md"]);
    let page = page_bytes(&"a".repeat(1024));
    let concurrency = Semaphore::new(2);
    let first = concurrency.try_acquire()?;
    let second = concurrency.try_acquire()?;
    /*
     * 計測値を候補検討用として表示しpermitを解放する
     */
    eprintln!("max_pages observed={pages} pages");
    eprintln!("max_page_bytes observed={page} bytes");
    eprintln!("max_zip_entries observed={} entries", values.entries);
    eprintln!(
        "max_zip_compressed_bytes observed={} bytes",
        values.archive_bytes
    );
    eprintln!(
        "max_zip_uncompressed_bytes observed={} bytes",
        values.actual
    );
    eprintln!("max_git_bytes synthetic_tempdir_disk={disk} bytes");
    eprintln!("max_processing_seconds fixture_elapsed={elapsed:?}");
    eprintln!("max_http_concurrency observed_in_flight=2");
    drop((first, second));
    Ok(())
}

///
/// 指定件数の合成Markdownを含むZIPを生成
///
/// # 引数
/// - `pages`: Markdownファイル数
/// - `bytes_per_page`: 各ファイルの本文バイト数
///
/// # 戻り値
/// 完成したZIPバイト列
///
fn synthetic_zip(pages: usize, bytes_per_page: usize) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated);

    /*
     * 件数と本文サイズを固定した合成ページを作る
     */
    for index in 0..pages {
        let name = format!("root/page-{index:05}.md");
        let mut body = vec![b'a'; bytes_per_page];
        for (offset, byte) in body.iter_mut().enumerate() {
            let value = (offset + index * 17) % 26;
            *byte = b'a' + u8::try_from(value)?;
        }
        writer.start_file(name, options)?;
        writer.write_all(&body)?;
    }
    Ok(writer.finish()?.into_inner())
}

///
/// Linuxが記録するプロセス最大RSSを読む
///
/// # 戻り値
/// プロセス開始後の最大RSS（KiB）
///
fn peak_rss_kib() -> Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status.lines()
        .find(|line| line.starts_with("VmHWM:"))
        .context("VmHWM is absent")?;
    let value = line.split_whitespace()
        .nth(1)
        .context("VmHWM value is absent")?;
    Ok(value.parse()?)
}

///
/// 代表データ不在時の合成入力で処理規模を測る
///
/// # 概要
/// 三段階の合成ZIPを生成・読込し、規模別の時間と最大RSSを記録する。
///
/// # 戻り値
/// 3つの規模のZIP生成と読込に成功すれば成功
///
/// # 注記
/// 数値は初期上限の候補検討用である。
/// 実データの代用ではない。
///
#[test]
#[ignore = "synthetic workload measurement requested separately"]
fn synthetic_workload_measurements() -> Result<()> {

    /*
     * 3段階の入力規模を同じ手順で測定する
     */
    for (pages, page_bytes) in [
        (10_usize, 1024_usize),
        (100_usize, 4096_usize),
        (1000_usize, 4096_usize),
    ] {
        let started = Instant::now();
        let bytes = synthetic_zip(pages, page_bytes)?;
        let generated = started.elapsed();
        let measured = measure_zip(&bytes, u64::MAX)?;
        let elapsed = started.elapsed();
        assert_eq!(measured.entries, pages);
        assert_eq!(measured.actual, u64::try_from(pages * page_bytes)?);

        /*
         * 各規模の時間とプロセス最大RSSを記録する
         */
        eprintln!(
            "pages={pages} page_bytes={page_bytes} zip_bytes={} \
             compressed_payload={} generated={generated:?} \
             total={elapsed:?} peak_rss_kib={}",
            bytes.len(),
            measured.compressed_payload,
            peak_rss_kib()?,
        );
    }
    Ok(())
}

///
/// 入力上限モジュールが統合テストへ接続済みか確認
///
/// # 注記
/// グループ1の共有ヘルパーを確認する。
///
/// # 戻り値
/// なし。上限実証モジュールの共有ヘルパー接続が異なればassertで失敗する。
///
#[test]
fn limits_probe_is_connected() {
    assert_eq!(super::support::probe_group_name(), "ms1.2");
}
