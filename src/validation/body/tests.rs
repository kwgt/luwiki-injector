/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 本文検証・専用保管・停止と取得からの共通境界の契約試験
//!

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Cursor};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use ::zip::ZipWriter;
use ::zip::write::SimpleFileOptions;

use crate::cmd_args::ResolvedRunConfig;
use crate::report::{Failure, Report};
use crate::report::types::SourceKind;
use crate::source::{EntryKind, SourceSnapshot};
use crate::validation::{ProjectionLimits, project, validate_snapshot};
use crate::validation::{acquire_and_validate, acquire_and_validate_for_report};

use super::*;

///
/// 本文保管後のmetadata変化と停止を試験だけで注入する区分。
///
enum AfterCopy { Modified, Cancelled, Expired }

thread_local! {
    // 同期validate_inner試験のthreadだけで有効にし、他試験と混ぜない。
    static AFTER_COPY: RefCell<Option<AfterCopy>> =
        const { RefCell::new(None) };
}

///
/// 本文読込・保管後のmetadata再照合直前へ試験操作を注入する。
///
/// # 引数
/// * `input` - snapshot本文の開いたhandle
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// なし。設定済みの注入区分を一度だけ消費する。
///
pub(super) fn after_copy(input: &mut File, stop: &StopContext) {
    AFTER_COPY.with(|action| match action.borrow_mut().take() {
        Some(AfterCopy::Modified) => {
            let modified = input.metadata().unwrap().modified().unwrap();
            input.set_modified(modified + Duration::from_secs(2)).unwrap();
        }
        Some(AfterCopy::Cancelled) => stop.cancel(),
        Some(AfterCopy::Expired) =>
            std::thread::sleep(Duration::from_millis(30)),
        None => {}
    });
}

///
/// 試験用の期限内停止境界を生成する。
///
/// # 戻り値
/// 60秒の期限を持つ未停止の境界
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(60))
}

///
/// 設定から共通上限を取り出す統合試験用の値を用意する。
///
/// # 戻り値
/// 小規模fixtureを検証できる設定上限
///
fn limits() -> RunLimits {
    RunLimits {
        max_pages: 10, max_page_bytes: 1_000_000, max_zip_entries: 100,
        max_zip_compressed_bytes: 1_000_000,
        max_zip_uncompressed_bytes: 2_000_000, max_git_bytes: 3_000_000,
        max_processing_seconds: 60, max_http_concurrency: 1,
    }
}

///
/// 指定された本文から投影検査済みfixtureを作る。
///
/// # 引数
/// * `bodies` - entry順の本文群
///
/// # 戻り値
/// 本文は未検証の所有された候補入力
///
async fn projected(bodies: &[&[u8]]) -> ProjectedInput {
    let names: Vec<_> = (0..bodies.len())
        .map(|v| format!("private-name-{v}.md")).collect();
    let entries: Vec<_> = names.iter().zip(bodies).map(|(name, bytes)|
        (name.as_bytes(), EntryKind::File, *bytes)).collect();
    let snapshot = SourceSnapshot::validation_root_fixture(&entries).unwrap();
    project(snapshot, "/private-prefix".to_owned(),
        ProjectionLimits::from(&limits()), stop()).await.unwrap()
}

///
/// 開いた本文handleから保管ファイルの場所を試験だけで観察する。
///
/// # 引数
/// * `file` - 観察する読込handle
///
/// # 戻り値
/// Linux fdに対応するファイルパス
///
fn file_path(file: &File) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
}

///
/// 全件の投影先と実サイズ・本文を表示せず比較可能な表へ写す。
///
/// # 引数
/// * `input` - 全本文検証済みの入力
///
/// # 戻り値
/// 投影先をキーとする本文対応表
///
fn contents(input: &ValidatedInput) -> BTreeMap<String, (u64, Vec<u8>)> {
    input.pages().iter().enumerate().map(|(index, page)| {
        let mut bytes = Vec::new();
        input.open_body(index).unwrap().read_to_end(&mut bytes).unwrap();
        (page.identity().path.as_str().to_owned(), (page.byte_len(), bytes))
    }).collect()
}

///
/// 実ZIPと製品設定解決経路から共通取得・検証入口のfixtureを作る。
///
/// # 引数
/// * `body` - ZIPへそのまま保存する本文
/// * `deadline` - 設定が持つ試験用絶対期限
///
/// # 戻り値
/// 元ZIPの所有者と、CLI秘密警告を保持する検証済み設定
///
fn configured_zip(body: &[u8], deadline: Instant)
    -> (TempDir, ResolvedRunConfig)
{
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    zip.start_file("root/entry.md", SimpleFileOptions::default()).unwrap();
    zip.write_all(body).unwrap();
    zip.finish().unwrap();
    let config = crate::cmd_args::validation_config_fixture(path, deadline,
        1_000_000);
    (area, config)
}

///
/// 新共通取得・検証入口を通して正常本文を返し、起動警告を保持する。
///
/// # 注記
/// 製品のZIP dispatchと設定を使い、本文・参照・呼出し側警告を照合する。
///
/// # 戻り値
/// なし。設定経由の取得・検証・本文/参照保持と起動警告保持が異なれば失敗する。
///
#[tokio::test]
async fn configured_entry_acquires_validates_and_preserves_startup_warnings() {
    let (_area, config) = configured_zip(b"exact\r\n",
        Instant::now() + Duration::from_secs(60));
    let warnings = config.warnings().to_vec();
    assert_eq!(warnings.len(), 1);
    let input = acquire_and_validate(&config).await.unwrap();
    assert_eq!(config.warnings(), warnings.as_slice());
    assert_eq!(input.pages()[0].identity().path.as_str(), "/entry");
    assert_eq!(input.pages()[0].identity().source.parts().0, SourceKind::Zip);
    let mut bytes = Vec::new();
    input.open_body(0).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"exact\r\n");
}

///
/// 取得時に拒否されない本文不正を共通入口で入力失敗へ写す。
///
/// # 注記
/// 実ZIPの不正UTF-8本文をReport wrapperへ渡し、終了2と警告を確認する。
///
/// # 戻り値
/// なし。本文不正の入力分類・終了2・起動警告保持・非露出が異なれば失敗する。
///
#[tokio::test]
async fn configured_report_entry_rejects_body_and_keeps_caller_warnings() {
    let (_area, config) = configured_zip(b"\xffprivate-body",
        Instant::now() + Duration::from_secs(60));
    let mut report = Report::preflight();
    report.add_warnings(config.warnings());
    let error = acquire_and_validate_for_report(&config).await.unwrap_err();
    assert!(matches!(error, Failure::Input));
    report.fail(error);
    assert_eq!(report.exit_code(), 2);
    assert_eq!(config.warnings().len(), 1);
    assert!(!report.render().diagnostics.contains("private-body"));
}

///
/// 設定済み絶対期限を新入口で再設定せず、取得前に期限超過を拒否する。
///
/// # 注記
/// 期限超過設定から元ZIPを消し、I/OよりDeadlineを先に返すことを確認する。
///
/// # 戻り値
/// なし。設定の絶対期限維持・取得前拒否・終了1の分類が異なれば失敗する。
///
#[tokio::test]
async fn configured_entry_preserves_absolute_deadline_before_acquisition() {
    let (area, config) = configured_zip(b"body",
        Instant::now() - Duration::from_secs(1));
    fs::remove_file(area.path().join("input.zip")).unwrap();
    let error = acquire_and_validate(&config).await.unwrap_err();
    assert_eq!(error, ValidationError::Source(SourceError::Deadline));
    let mut report = Report::preflight();
    report.fail(error.into_failure());
    assert_eq!(report.exit_code(), 1);
}

///
/// UTF-8本文のbytesを変換せず保管し、元snapshotを解放する。
///
/// # 概要
/// BOM・CRLF・末尾改行・非正規化Unicode・front matterを維持する。
///
/// # 注記
/// 複数本文を照合し、読込専用handleと両leaseの寿命を確認する。
///
/// # 戻り値
/// なし。元bytes・readonly性・取得元解放・検証領域回収が異なればassertで失敗する。
///
#[tokio::test]
async fn preserves_exact_bytes_and_releases_source_after_success() {
    let bodies: &[&[u8]] = &[
        b"\xef\xbb\xbf---\r\nkey: value\r\n---\r\nbody\r\n",
        "日本語e\u{301}\n".as_bytes(), b"no-final-newline", b"",
    ];
    let projected = projected(bodies).await;
    let source = file_path(&projected.open_unvalidated_body(0).unwrap());
    let source_area = source.parent().unwrap().to_owned();
    let result = validate_bodies(projected, BodyLimits::from(&limits()), stop())
        .await.unwrap();
    assert!(!source_area.exists());
    /*
     * 取得元解放後にも同じ本文を読め、書込handleを公開しない
     */
    for (index, expected) in bodies.iter().enumerate() {
        let mut file = result.open_body(index).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.as_slice(), *expected);
        assert_eq!(result.pages()[index].byte_len(), expected.len() as u64);
        assert!(file.write_all(b"mutation").is_err());
    }
    let area = file_path(&result.open_body(0).unwrap()).parent().unwrap()
        .to_owned();
    assert!(result.open_body(result.pages().len()).is_err());
    drop(result);
    assert!(!area.exists());
}

///
/// chunkをまたぐ正常UTF-8を許し、切断列・不正列・置換文字を拒否する。
///
/// # 概要
/// 64KiB境界に多byte列を置き、全体の厳格検査を確認する。
///
/// # 注記
/// 正常日本語・跨chunkのU+FFFD・切断列を同じ読込境界で試す。
///
/// # 戻り値
/// なし。chunkを跨ぐUTF-8の許容や不正列・置換文字の拒否分類が異なれば失敗する。
///
#[tokio::test]
async fn validates_strict_utf8_across_chunk_boundaries() {
    let mut valid = vec![b'a'; 64 * 1024 - 1];
    valid.extend_from_slice("あ".as_bytes());
    let input = validate_bodies(projected(&[&valid]).await,
        BodyLimits::from(&limits()), stop()).await.unwrap();
    assert_eq!(input.pages()[0].byte_len(), valid.len() as u64);
    /*
     * 跨chunk・不正先頭byte・切断末尾をすべて入力不正として拒否する
     */
    let mut replacement = vec![b'a'; 64 * 1024 - 1];
    replacement.extend_from_slice(b"\xef\xbf\xbd");
    let mut truncated = valid.clone();
    truncated.pop();
    for (bytes, error) in [
        (&replacement[..], ValidationError::BodyReplacement),
        (&truncated[..], ValidationError::BodyUtf8),
        (&b"\xff"[..], ValidationError::BodyUtf8),
        (&b"\xc0\xaf"[..], ValidationError::BodyUtf8),
        (&b"\xed\xa0\x80"[..], ValidationError::BodyUtf8),
    ] {
        assert_eq!(validate_bodies(projected(&[bytes]).await,
            BodyLimits::from(&limits()), stop()).await.unwrap_err(), error);
    }
}

///
/// 空本文と本文上限同量を許し、1byte超過を拒否する。
///
/// # 注記
/// metadataの期待値に頼らず、実読込量で上限を判定する。
///
/// # 戻り値
/// なし。空本文・上限端点の許容や実読込超過の拒否が異なればassertで失敗する。
///
#[tokio::test]
async fn checks_real_body_size_boundaries() {
    let bound = BodyLimits { max_page_bytes: 3 };
    let result = validate_bodies(projected(&[b"", b"abc"]).await, bound,
        stop()).await.unwrap();
    assert_eq!(result.pages()[0].byte_len(), 0);
    assert_eq!(result.pages()[1].byte_len(), 3);
    assert_eq!(validate_bodies(projected(&[b"abcd"]).await, bound, stop())
        .await.unwrap_err(), ValidationError::BodyLimit);
}

///
/// 取得時サイズとmetadataが変化した候補を実行失敗として拒否する。
///
/// # 注記
/// 投影後の所有本文を試験だけで変更してから全本文検証へ渡す。
///
/// # 戻り値
/// なし。取得元サイズ変更の拒否分類や領域回収が異なればassertで失敗する。
///
#[tokio::test]
async fn rejects_changed_snapshot_size() {
    let projected = projected(&[b"original"]).await;
    let path = file_path(&projected.open_unvalidated_body(0).unwrap());
    fs::write(&path, b"short").unwrap();
    assert_eq!(validate_bodies(projected, BodyLimits::from(&limits()), stop())
        .await.unwrap_err(), ValidationError::Source(SourceError::Changed));
    assert!(!path.parent().unwrap().exists());
}

///
/// 同じサイズでも読込後metadataの変更を拒否し、両leaseを回収する。
///
/// # 注記
/// 本文保管直後にmtimeを変え、Changedが結果全体を破棄するか確認する。
///
/// # 戻り値
/// なし。読込後metadata変更の拒否分類や両lease回収が異なればassertで失敗する。
///
#[tokio::test]
async fn rejects_metadata_change_after_read_and_reclaims_both_leases() {
    let projected = projected(&[b"stable-size"]).await;
    let source = file_path(&projected.open_unvalidated_body(0).unwrap());
    let area = tempfile::tempdir().unwrap();
    let path = area.path().to_owned();
    AFTER_COPY.with(|v| *v.borrow_mut() = Some(AfterCopy::Modified));
    let error = validate_inner(projected, area, BodyLimits::from(&limits()),
        &stop()).unwrap_err();
    assert_eq!(error, ValidationError::Source(SourceError::Changed));
    assert!(!source.parent().unwrap().exists());
    assert!(!path.exists());
}

///
/// 本文保管後の取消・期限到達でも検証済み結果を公開しない。
///
/// # 概要
/// 最終公開前の停止確認が両leaseの回収まで担当することを確認する。
///
/// # 注記
/// chunk処理後の試験専用注入で停止し、source/payload消滅を観察する。
///
/// # 戻り値
/// なし。保管後の停止分類・結果非公開・両lease回収が異なればassertで失敗する。
///
#[tokio::test]
async fn stops_after_storage_and_reclaims_source_and_payload() {
    for expired in [false, true] {
        let projected = projected(&[b"body"]).await;
        let source = file_path(&projected.open_unvalidated_body(0).unwrap());
        let area = tempfile::tempdir().unwrap();
        let path = area.path().to_owned();
        let context = if expired {
            StopContext::new(Instant::now() + Duration::from_millis(20))
        } else { stop() };
        AFTER_COPY.with(|v| *v.borrow_mut() = Some(if expired {
            AfterCopy::Expired
        } else { AfterCopy::Cancelled }));
        /*
         * 読込・保管終了後の停止で、結果の公開を禁止する
         */
        let error = validate_inner(projected, area,
            BodyLimits::from(&limits()), &context).unwrap_err();
        let expected = if expired { SourceError::Deadline }
            else { SourceError::Cancelled };
        assert_eq!(error, ValidationError::Source(expected));
        AFTER_COPY.with(|v| *v.borrow_mut() = None);
        assert!(!source.parent().unwrap().exists());
        assert!(!path.exists());
    }
}

///
/// future破棄時もworkerが停止を確認して両leaseを回収する。
///
/// # 概要
/// 本文検証と同じruntime契約に、開始通知だけを挿入する。
///
/// # 注記
/// workerが所有権を持った後にfutureをabortし、worker終了後の回収を待つ。
///
/// # 戻り値
/// なし。future破棄後のworker取消確認や所有領域回収が異なればassertで失敗する。
///
#[tokio::test]
async fn dropped_future_leaves_cleanup_to_owned_worker() {
    let projected = projected(&[b"body"]).await;
    let source = file_path(&projected.open_unvalidated_body(0).unwrap());
    let area = tempfile::tempdir().unwrap();
    let payload = area.path().to_owned();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    /*
     * worker内で両所有者を保持してから開始通知を送る
     */
    let task = tokio::spawn(async move {
        source::run_blocking(stop(), move |context| {
            started.send(()).unwrap();
            blocked.recv().unwrap();
            let result = validate_inner(projected, area,
                BodyLimits::from(&limits()), &context);
            let reclaimed = !source.parent().unwrap().exists()
                && !payload.exists();
            finished.send((result.unwrap_err(), reclaimed)).unwrap();
            Ok(())
        }).await
    });
    /*
     * future破棄を確定後、workerを解放して停止確認と回収を待つ
     */
    ready.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    let (error, reclaimed) = done.await.unwrap();
    assert_eq!(error, ValidationError::Source(SourceError::Cancelled));
    assert!(reclaimed);
}

///
/// readerの実量が期待量と異なる場合も取得元変更として拒否する。
///
/// # 注記
/// metadata以外の読込境界で、短い本文と長い本文を試す。
///
/// # 戻り値
/// なし。実読込量と期待量の不一致拒否や出力非公開が異なればassertで失敗する。
///
#[test]
fn rejects_actual_read_length_mismatch() {
    for expected in [2, 4] {
        let mut output = Vec::new();
        assert_eq!(copy_body(&mut Cursor::new(b"abc"), &mut output, expected,
            BodyLimits { max_page_bytes: 10 }, &stop()),
            Err(ValidationError::Source(SourceError::Changed)));
        assert!(output.is_empty());
    }
}

///
/// 生のエラー文字列を持つ失敗reader。
///
struct FailedReader;

impl Read for FailedReader {
    ///
    /// 読込失敗を注入する。
    ///
    /// # 引数
    /// * `_buffer` - 読込先
    ///
    /// # 戻り値
    /// 試験用の生messageを持つI/Oエラー
    ///
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("private-reader-message"))
    }
}

///
/// writeまたはflushで秘密のmessageを返すwriter。
///
struct FailedWriter { flush_only: bool }

impl Write for FailedWriter {
    ///
    /// 指定された失敗境界でwriteを試す。
    ///
    /// # 引数
    /// * `buffer` - 保存するbytes
    ///
    /// # 戻り値
    /// 書込数、または試験用messageを持つI/Oエラー
    ///
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.flush_only { Ok(buffer.len()) }
        else { Err(io::Error::other("private-writer-message")) }
    }

    ///
    /// flush失敗を注入する。
    ///
    /// # 戻り値
    /// 試験用messageを持つI/Oエラー
    ///
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("private-flush-message"))
    }
}

///
/// reader・writer・flushの生messageを保持・表示しない。
///
/// # 注記
/// 各I/O失敗を注入し、固定Source分類と報告終了1を照合する。
///
/// # 戻り値
/// なし。reader・writer・flushの安全なI/O分類や秘密非露出が異なれば失敗する。
///
#[test]
fn hides_raw_reader_writer_and_flush_failures() {
    let bound = BodyLimits { max_page_bytes: 10 };
    let error = copy_body(&mut FailedReader, &mut Vec::new(), 1, bound,
        &stop()).unwrap_err();
    assert_eq!(error, ValidationError::Source(SourceError::Io));
    assert!(!format!("{error:?} {error}").contains("private"));
    /*
     * 保存とflushの失敗もreaderと同じ安全なI/O分類へ写す
     */
    for flush_only in [false, true] {
        let error = copy_body(&mut Cursor::new(b"body"),
            &mut FailedWriter { flush_only }, 4, bound, &stop()).unwrap_err();
        assert_eq!(error, ValidationError::Source(SourceError::Io));
        assert!(!format!("{error:?} {error}").contains("private"));
    }
}

///
/// 最初のchunkを読み出してから共有停止を通知するreader。
///
struct CancelReader { stop: StopContext, bytes: Cursor<Vec<u8>> }

impl Read for CancelReader {
    ///
    /// 読込中の停止を決定的に注入する。
    ///
    /// # 引数
    /// * `buffer` - chunkの読込先
    ///
    /// # 戻り値
    /// 読込数。戻る直前に共有取消を設定する。
    ///
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let result = self.bytes.read(buffer);
        self.stop.cancel();
        result
    }
}

///
/// chunk読込直後の取消と期限を検知して未完成本文を保管しない。
///
/// # 注記
/// readerが通知する途中取消と、期限超過境界を同じ処理へ渡す。
///
/// # 戻り値
/// なし。chunk境界の取消・期限拒否や未完成本文の非保管が異なれば失敗する。
///
#[test]
fn observes_cancellation_between_bounded_chunks() {
    let context = stop();
    let mut reader = CancelReader { stop: context.clone(),
        bytes: Cursor::new(vec![b'a'; 100_000]) };
    let mut output = Vec::new();
    assert_eq!(copy_body(&mut reader, &mut output, 100_000,
        BodyLimits::from(&limits()), &context),
        Err(ValidationError::Source(SourceError::Cancelled)));
    assert!(output.is_empty());
    let expired = StopContext::new(Instant::now() - Duration::from_secs(1));
    assert_eq!(copy_body(&mut Cursor::new(b"body"), &mut output, 4,
        BodyLimits::from(&limits()), &expired),
        Err(ValidationError::Source(SourceError::Deadline)));
}

///
/// 後続候補の失敗時に成功候補を公開せず両一時領域を回収する。
///
/// # 注記
/// 第2候補の本文不正と処理停止について、source/payload消滅を確認する。
///
/// # 戻り値
/// なし。後続失敗時の部分結果非公開や両lease回収が異なればassertで失敗する。
///
#[tokio::test]
async fn reclaims_both_leases_without_partial_success() {
    for cancelled in [false, true] {
        let projected = projected(&[b"valid", b"\xff"]).await;
        let source = file_path(&projected.open_unvalidated_body(0).unwrap());
        let area = tempfile::tempdir().unwrap();
        let path = area.path().to_owned();
        let context = stop();
        if cancelled { context.cancel(); }
        let result = validate_inner(projected, area,
            BodyLimits::from(&limits()), &context);
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(!source.parent().unwrap().exists());
    }
}

///
/// 保管ファイル生成失敗でも入力と未完成payloadを回収する。
///
/// # 注記
/// 試験専用領域の連番名をdirectoryで占有し、安全なIo分類を確認する。
///
/// # 戻り値
/// なし。保管file生成失敗のI/O分類や両lease回収が異なればassertで失敗する。
///
#[tokio::test]
async fn reclaims_both_leases_when_payload_creation_fails() {
    let projected = projected(&[b"body"]).await;
    let source = file_path(&projected.open_unvalidated_body(0).unwrap());
    let area = tempfile::tempdir().unwrap();
    let payload = area.path().to_owned();
    fs::create_dir(payload.join("0")).unwrap();
    let error = validate_inner(projected, area, BodyLimits::from(&limits()),
        &stop()).unwrap_err();
    assert_eq!(error, ValidationError::Source(SourceError::Io));
    assert!(!source.parent().unwrap().exists());
    assert!(!payload.exists());
}

///
/// 正常検証後は取得元本文を書き換えても専用保管本文へ影響しない。
///
/// # 注記
/// 取得元の開いたhandleを保持し、検証後に同じinodeへ変更を試す。
///
/// # 戻り値
/// なし。取得元の後続変更から独立した検証済み本文を保持できなければ失敗する。
///
#[tokio::test]
async fn payload_is_independent_of_source_storage() {
    let projected = projected(&[b"original-body"]).await;
    let path = file_path(&projected.open_unvalidated_body(0).unwrap());
    let mut source_writer = OpenOptions::new().write(true).open(path).unwrap();
    let input = validate_bodies(projected, BodyLimits::from(&limits()), stop())
        .await.unwrap();
    source_writer.write_all(b"modified-body").unwrap();
    let mut bytes = Vec::new();
    input.open_body(0).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"original-body");
}

///
/// 全entry名検査済みの0候補を本文検証も正常に返す。
///
/// # 注記
/// 不正UTF-8の非MD本文を共通本文検査の対象にせず空集合を返す。
///
/// # 戻り値
/// なし。非Markdown本文を読まず空の検証結果を返せなければassertで失敗する。
///
#[tokio::test]
async fn validates_zero_candidates_without_reading_non_markdown() {
    let snapshot = SourceSnapshot::validation_root_fixture(&[
        (b"binary.dat", EntryKind::File, b"\xff"),
    ]).unwrap();
    let input = validate_snapshot(snapshot, "/".to_owned(), &limits(), stop())
        .await.unwrap();
    assert!(input.pages().is_empty());
    assert!(input.open_body(0).is_err());
}

///
/// 四方式から取得した同内容に同じ共通検証結果を返す。
///
/// # 概要
/// ZIP/localは実取得入口、HTTPS/GitHubは既存file fixtureを使用する。
/// 実ネットワーク接続と資格情報の実証を代替する試験ではない。
///
/// # 注記
/// 元入力破棄済みsnapshotから全検証し、結果と参照方式を比較する。
///
/// # 戻り値
/// なし。四方式fixtureの共通検証結果や参照種別保持が異なればassertで失敗する。
///
#[tokio::test]
async fn four_acquisition_adapters_share_validated_result() {
    let snapshots = source::validation_adapter_fixtures().await.unwrap();
    let mut expected = None;
    /*
     * 全方式を同じ名前・本文検査入口へ渡す
     */
    for (snapshot, kind) in snapshots.into_iter().zip([
        SourceKind::Zip, SourceKind::LocalGit, SourceKind::HttpsGit,
        SourceKind::GitHub])
    {
        let input = validate_snapshot(snapshot, "/".to_owned(), &limits(),
            stop()).await.unwrap();
        assert_eq!(input.pages().len(), 2);
        assert!(input.pages().iter().all(|p|
            p.identity().source.parts().0 == kind));
        let actual = contents(&input);
        if let Some(expected) = &expected { assert_eq!(&actual, expected); }
        else { expected = Some(actual); }
    }
}

///
/// 本文不正は入力終了2、変更・I/O・停止は実行終了1へ対応する。
///
/// # 注記
/// 本文エラーと既存Source分類をReportへ渡して終了状態を確認する。
///
/// # 戻り値
/// なし。本文不正・変更・I/O・停止のReport終了分類が異なればassertで失敗する。
///
#[test]
fn maps_body_failures_to_safe_report_exit_codes() {
    /*
     * 全本文失敗分類を安全なReport終了状態へ対応付ける
     */
    for (error, expected) in [
        (ValidationError::BodyUtf8, 2),
        (ValidationError::BodyReplacement, 2),
        (ValidationError::BodyLimit, 2),
        (ValidationError::Source(SourceError::Changed), 1),
        (ValidationError::Source(SourceError::Io), 1),
        (ValidationError::Source(SourceError::Cancelled), 1),
        (ValidationError::Source(SourceError::Deadline), 1),
    ] {
        let mut report = Report::preflight();
        let failure = error.into_failure();
        if expected == 2 { assert!(matches!(failure, Failure::Input)); }
        report.fail(failure);
        assert_eq!(report.exit_code(), expected);
    }
}

///
/// 全件検証済み型と本文失敗の表示に秘密値・保管先を含めない。
///
/// # 概要
/// 成功結果と本文失敗の全表示経路に入力値を持ち込まない。
///
/// # 注記
/// Debug/Displayを本文・元名・prefix・source/payloadパスと照合する。
///
/// # 戻り値
/// なし。検証済み型や本文失敗の表示に秘密値・保管先が含まれれば失敗する。
///
#[tokio::test]
async fn hides_values_in_validated_and_body_failure_displays() {
    let projected = projected(&[b"private-body"]).await;
    let source = file_path(&projected.open_unvalidated_body(0).unwrap());
    let input = validate_bodies(projected, BodyLimits::from(&limits()), stop())
        .await.unwrap();
    let payload = file_path(&input.open_body(0).unwrap());
    let mut rendered = format!("{input:?} {:?}", input.pages()[0]);
    /*
     * 本文不正にも値を持たせず、全失敗表示を固定分類にする
     */
    for error in [ValidationError::BodyUtf8, ValidationError::BodyReplacement,
        ValidationError::BodyLimit]
    {
        rendered.push_str(&format!("{error:?} {error}"));
    }
    for secret in ["private-body", "private-name", "private-prefix",
        source.to_str().unwrap(), payload.to_str().unwrap()]
    {
        assert!(!rendered.contains(secret));
    }
}
