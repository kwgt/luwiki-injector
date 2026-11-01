/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 計画の内部組立・本文所有権・停止回収と安全な表示の契約試験
//!

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use ::zip::ZipWriter;
use ::zip::write::SimpleFileOptions;

use crate::cmd_args::resolve::RunLimits;
use crate::report::types::{FailureCategory, ProcessingStage, SourceKind};
use crate::report::Failure;
use crate::source::{EntryKind, SourceSnapshot};
use crate::validation::validate_snapshot;

use super::*;

thread_local! {
    // 同期組立を呼ぶ試験thread内だけに取消注入を限定する。
    static CANCEL_AFTER_PAGE: Cell<bool> = const { Cell::new(false) };
    static SORT_ACTION: RefCell<Option<SortAction>> =
        const { RefCell::new(None) };
    static CANCEL_BEFORE_PUBLISH: Cell<bool> = const { Cell::new(false) };
}

///
/// worker内の整列後にだけ停止・寿命試験を注入する操作。
///
pub(super) enum SortAction {
    Cancel,
    WaitForDeadline(Arc<AtomicBool>),
    Pause {
        started: tokio::sync::oneshot::Sender<PathBuf>,
        resume: std::sync::mpsc::Receiver<()>,
    },
}

///
/// 呼出しthreadの試験操作を一度だけworkerへ渡す。
///
/// # 戻り値
/// 同期workerに移動可能な試験操作
///
pub(super) fn take_sort_action() -> Option<SortAction> {
    SORT_ACTION.with(|action| action.borrow_mut().take())
}

///
/// 整列後に同期点・取消・期限到達を注入する。
///
/// # 概要
/// 比較関数を変更せず、停止検査直前のworker所有物を観察する。
///
/// # 引数
/// * `stop` - 共有停止context
/// * `plan` - workerがまだ所有する非公開計画
/// * `action` - 呼出しthreadから移された試験操作
///
/// # 戻り値
/// なし。設定された操作だけを実行する。
///
pub(super) fn after_sort(stop: &StopContext, plan: &ProjectionPlan,
    action: Option<SortAction>)
{
    /*
     * 整列の結果を作り終えたworker内だけで停止と寿命を制御する
     */
    match action {
        /*
         * 試験操作に応じて共有停止かworkerの寿命を制御する
         */
        Some(SortAction::Cancel) => stop.cancel(),
        Some(SortAction::WaitForDeadline(entered)) => {
            entered.store(true, Ordering::Relaxed);
            while stop.checkpoint().is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Some(SortAction::Pause { started, resume }) => {
            let path = storage_path(&plan.input);
            let _ = started.send(path);
            let _ = resume.recv();
        }
        None => {}
    }
}

///
/// worker返却後の最終公開確認直前で共有取消を注入する。
///
/// # 引数
/// * `stop` - 呼出し側がまだ共有している取消context
///
/// # 戻り値
/// なし。試験threadの設定を一度だけ消費する。
///
pub(super) fn before_publish(stop: &StopContext) {
    CANCEL_BEFORE_PUBLISH.with(|cancel| {
        if cancel.replace(false) { stop.cancel(); }
    });
}

///
/// 最初のmetadata組立直後に一度だけ取消を注入する。
///
/// # 引数
/// * `stop` - 試験で共有している取消境界
///
/// # 戻り値
/// なし。注入設定がある場合だけ共有フラグを更新する。
///
pub(super) fn after_page(stop: &StopContext) {
    CANCEL_AFTER_PAGE.with(|cancel| {
        if cancel.replace(false) { stop.cancel(); }
    });
}

///
/// 正常試験用の期限内停止境界を作る。
///
/// # 戻り値
/// 60秒の期限を持つ未停止の境界
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(60))
}

///
/// 計画試験で使用する共通検証済み入力を製品の検証経路で作る。
///
/// # 概要
/// cfg(test)のsnapshotだけを準備し、本文検証済み型の生成を省略しない。
///
/// # 引数
/// * `entries` - 入力順の相対名と本文
///
/// # 戻り値
/// 全件検証済みの所有された本文
///
async fn validated(entries: &[(&[u8], &[u8])]) -> ValidatedInput {
    let entries: Vec<_> = entries.iter().map(|(name, bytes)|
        (*name, EntryKind::File, *bytes)).collect();
    let snapshot = SourceSnapshot::validation_root_fixture(&entries).unwrap();
    /*
     * 製品の全名前・投影・本文検証へfixtureを通す
     */
    let limits = RunLimits {
        max_pages: 10, max_page_bytes: 1_000_000, max_zip_entries: 100,
        max_zip_compressed_bytes: 1_000_000,
        max_zip_uncompressed_bytes: 2_000_000, max_git_bytes: 3_000_000,
        max_processing_seconds: 60, max_http_concurrency: 1,
    };
    validate_snapshot(snapshot, "/private-prefix".to_owned(), &limits,
        stop()).await.unwrap()
}

///
/// 本文を表示せず読込handleからバイト列を得る。
///
/// # 引数
/// * `file` - 読込専用の本文handle
///
/// # 戻り値
/// 保管されたバイト列
///
fn bytes(mut file: File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

///
/// 本文leaseの回収観察用にLinuxのfd参照を取得する。
///
/// # 引数
/// * `input` - 全件検証済みの非空入力
///
/// # 戻り値
/// 試験だけで観察する専用領域内のファイルパス
///
fn storage_path(input: &ValidatedInput) -> PathBuf {
    let file = input.open_body(0).unwrap();
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
}

///
/// metadataの並べ替え後も元位置から正しい本文を読み取れる。
///
/// # 概要
/// 異なる本文を持つページmetadataだけを入れ替え、元位置の対応を試す。
///
/// # 注記
/// 手動swapは整列実装ではなく、対応保持だけを試す非公開試験操作。
/// BOM・CRLF・末尾改行あり/なしを別本文へ混ぜて対応を照合する。
///
/// # 戻り値
/// なし。並べ替え後の識別子・実サイズ・本文対応が異なればassertで失敗する。
///
#[tokio::test]
async fn keeps_body_identity_and_size_after_internal_reordering() {
    let original: &[(&[u8], &[u8])] = &[
        (b"first.md", b"\xef\xbb\xbfprivate-body\r\n"),
        (b"child/second.md", "日本語\n".as_bytes()),
        (b"third.md", b"no-final-newline"),
    ];
    let input = validated(original).await;
    let mut plan = assemble(input, &stop()).unwrap();
    plan.pages.swap(0, 2);
    /*
     * 計画位置と保管位置が異なっても識別子・実量・本文を対応させる
     */
    for (position, expected) in [2, 1, 0].into_iter().enumerate() {
        let page = &plan.pages()[position];
        let source_name = std::str::from_utf8(original[expected].0).unwrap();
        let target = format!("/private-prefix/{}",
            source_name.strip_suffix(".md").unwrap());
        assert_eq!(page.identity().path.as_str(), target);
        assert_eq!(page.identity().source.parts().1,
            format!("root/{source_name}"));
        assert_eq!(page.byte_len(), original[expected].1.len() as u64);
        assert_eq!(bytes(plan.open_body(position).unwrap()),
            original[expected].1);
    }
}

///
/// 計画が検証済み領域の寿命を引き継ぎ、破棄時に回収する。
///
/// # 注記
/// 移管前後で同じ保管fileを確認し、readonly handleの書込拒否を試す。
///
/// # 戻り値
/// なし。本文のreadonly性や計画所有による寿命・回収が異なればassertで失敗する。
///
#[tokio::test]
async fn owns_payload_until_drop_and_exposes_readonly_handles() {
    let input = validated(&[(b"page.md", b"payload")]).await;
    let path = storage_path(&input);
    let plan = assemble(input, &stop()).unwrap();
    assert!(path.exists());
    let mut file = plan.open_body(0).unwrap();
    assert_eq!(fs::read_link(format!("/proc/self/fd/{}",
        file.as_raw_fd())).unwrap(), path);
    assert!(file.write_all(b"mutation").is_err());
    drop(file);
    assert_eq!(bytes(plan.open_body(0).unwrap()), b"payload");
    drop(plan);
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists());
}

///
/// 送信用資源は成立計画を保持し、任意位置や書込handleを許さない。
///
/// # 概要
/// 送信資源のreadonly性と、計画破棄後の本文所有寿命を検査する。
///
/// # 注記
/// 外側Arcを破棄してもreadonly本文を読み、最終資源破棄で領域回収を確認。
///
/// # 戻り値
/// なし。所有・回収の契約と異なる場合はassertで失敗する。
///
#[tokio::test]
async fn planned_upload_owns_readonly_payload_until_last_owner_drops() {
    let input = validated(&[(b"private-name.md", b"private-body")]).await;
    let path = storage_path(&input);
    let plan = Arc::new(finalize(input, stop()).await.unwrap());
    assert!(plan.upload_body(1).is_err());
    let body = plan.upload_body(0).unwrap();
    let rendered = format!("{body:?}");
    assert!(!rendered.contains("private-name"));
    assert!(!rendered.contains("private-body"));
    assert_eq!(body.byte_len(), 12);
    /*
     * caller計画の破棄後も本文所有を送信資源が保持する
     */
    drop(plan);
    assert!(path.exists());
    let mut file = body.open().unwrap();
    assert!(file.write_all(b"mutation").is_err());
    assert_eq!(bytes(file), b"private-body");
    drop(body);
    assert!(!path.parent().unwrap().exists());
}

///
/// 安全検査済みの対象0件を空の内部組立結果として扱う。
///
/// # 注記
/// 製品検証を通した空入力を消費し、架空の本文位置を拒否する。
///
/// # 戻り値
/// なし。空計画や存在しない本文位置の拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn accepts_empty_input_and_rejects_absent_body_position() {
    let input = validated(&[]).await;
    let plan = assemble(input, &stop()).unwrap();
    assert!(plan.pages().is_empty());
    assert!(matches!(plan.open_body(0),
        Err(ValidationError::Source(SourceError::InvalidPath))));
}

///
/// 組立前の期限超過・取消で結果を返さず本文領域を回収する。
///
/// # 概要
/// 停止済みのcontextを渡し、所有者の破棄と既存失敗分類を照合する。
///
/// # 注記
/// 各停止原因を分けて検証し、既存Reportの失敗分類を確認する。
///
/// # 戻り値
/// なし。初期停止の分類や本文領域の回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn reclaims_payload_on_initial_deadline_or_cancellation() {
    /*
     * 期限超過と取消の双方で本文所有者を消費させる
     */
    for cancelled in [false, true] {
        let input = validated(&[(b"page.md", b"payload")]).await;
        let path = storage_path(&input);
        let stop = if cancelled { stop() }
            else { StopContext::new(Instant::now()) };
        if cancelled { stop.cancel(); }
        let error = assemble(input, &stop).unwrap_err();
        /*
         * 失敗の分類と専用領域の回収を照合する
         */
        let expected = if cancelled { FailureCategory::NotApplied }
            else { FailureCategory::Deadline };
        assert!(matches!(error.into_failure(), Failure::Execution {
            category, stage: ProcessingStage::Preflight,
        } if category == expected));
        assert!(!path.parent().unwrap().exists());
    }
}

///
/// 途中metadata組立と最終停止確認の失敗で所有領域を回収する。
///
/// # 注記
/// 2件では次反復、1件では最終checkpointが注入取消を検出する。
///
/// # 戻り値
/// なし。組立途中・最終停止時の取消と領域回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn reclaims_partial_and_complete_metadata_on_cancellation() {
    /*
     * 最終確認と次ページ確認の双方に同じ取消を注入する
     */
    for count in [1, 2] {
        let entries: &[(&[u8], &[u8])] = &[
            (b"first.md", b"first"), (b"second.md", b"second"),
        ];
        let input = validated(&entries[..count]).await;
        let path = storage_path(&input);
        let shared = stop();
        let observer = shared.clone();
        CANCEL_AFTER_PAGE.with(|cancel| cancel.set(true));
        assert!(matches!(assemble(input, &shared),
            Err(ValidationError::Source(SourceError::Cancelled))));
        assert_eq!(observer.checkpoint(), Err(SourceError::Cancelled));
        assert!(!path.parent().unwrap().exists());
    }
}

///
/// 計画・ページ・読込失敗の表示から本文と識別情報を除外する。
///
/// # 注記
/// 秘密sentinelと専用保管パスをDebug/Displayへ照合する。
///
/// # 戻り値
/// なし。計画や失敗の表示に本文・識別子・保管位置が含まれればassertで失敗する。
///
#[tokio::test]
async fn hides_payload_identifiers_and_storage_in_displays() {
    let input = validated(&[(b"private-name.md", b"private-body")]).await;
    let path = storage_path(&input);
    let plan = assemble(input, &stop()).unwrap();
    let error = plan.open_body(usize::MAX).unwrap_err();
    let rendered = format!("{plan:?} {:?} {error:?} {error}", plan.pages());
    for sentinel in ["private-name", "private-body", "private-prefix",
        path.to_str().unwrap()]
    {
        assert!(!rendered.contains(sentinel));
    }
}

///
/// 順序・実サイズ・本文を比較するため計画の内容を取り出す。
///
/// # 引数
/// * `plan` - 公開入口で確定した計画
///
/// # 戻り値
/// 順序を保持したpath・実サイズ・本文の表
///
fn contents(plan: &ProjectionPlan) -> Vec<(String, u64, Vec<u8>)> {
    plan.pages().iter().enumerate().map(|(index, page)| (
        page.identity().path.as_str().to_owned(), page.byte_len(),
        bytes(plan.open_body(index).unwrap()),
    )).collect()
}

///
/// 製品設定解決とZIP取得から計画までを試すfixtureを作る。
///
/// # 引数
/// * `body` - 保持または拒否を試す本文
///
/// # 戻り値
/// 元ZIPの所有者と検証済み実行設定
///
fn configured_zip(body: &[u8]) -> (TempDir, ResolvedRunConfig) {
    let area = tempfile::tempdir().unwrap();
    let path = area.path().join("input.zip");
    let mut zip = ZipWriter::new(File::create(&path).unwrap());
    zip.start_file("root/page.md", SimpleFileOptions::default()).unwrap();
    zip.write_all(body).unwrap();
    zip.finish().unwrap();
    let config = crate::cmd_args::validation_config_fixture(path,
        Instant::now() + Duration::from_secs(60), 1_000_000);
    (area, config)
}

///
/// 入力列挙順を変えても深さ・UTF-8 bytes順と本文の対応が一致する。
///
/// # 概要
/// ASCII・日本語・異なるUnicode表記と親不在の子を同じfixtureに含める。
///
/// # 注記
/// 逆順入力の計画全内容を照合し、期待順を独立した明示列で確認する。
///
/// # 戻り値
/// なし。深さ・UTF-8 bytes順や本文対応が入力列挙順で変わればassertで失敗する。
///
#[tokio::test]
async fn finalizes_deterministic_depth_and_utf8_order_without_normalization() {
    let entries: &[(&[u8], &[u8])] = &[
        ("日本語/子.md".as_bytes(), b"child\r\n"),
        (b"z.md", b"last-ascii"),
        ("é.md".as_bytes(), b"composed"),
        (b"a/deep/page.md", b"\xef\xbb\xbfdeep\n"),
        ("e\u{301}.md".as_bytes(), b"decomposed"),
        (b"a.md", b"parent"),
    ];
    let first = finalize(validated(entries).await, stop()).await.unwrap();
    let reversed: Vec<_> = entries.iter().copied().rev().collect();
    let second = finalize(validated(&reversed).await, stop()).await.unwrap();
    /*
     * 保管位置の違いに依存せず、同じ順序・本文・実サイズを確認する
     */
    assert_eq!(contents(&first), contents(&second));
    let paths: Vec<_> = first.pages().iter().map(|page|
        page.identity().path.as_str()).collect();
    assert_eq!(paths, vec!["/private-prefix/a", "/private-prefix/e\u{301}",
        "/private-prefix/z", "/private-prefix/é", "/private-prefix/日本語/子",
        "/private-prefix/a/deep/page"]);
    assert_eq!(bytes(first.open_body(5).unwrap()), b"\xef\xbb\xbfdeep\n");
}

///
/// 空計画の公開も共有停止確認を通り、余分なページを生成しない。
///
/// # 注記
/// 通常空計画とworker返却後の取消を同じ公開入口で区別する。
///
/// # 戻り値
/// なし。空計画の公開条件や停止時の拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn publishes_empty_plan_only_after_final_stop_check() {
    let plan = finalize(validated(&[]).await, stop()).await.unwrap();
    assert!(plan.pages().is_empty());
    let input = validated(&[]).await;
    CANCEL_BEFORE_PUBLISH.with(|cancel| cancel.set(true));
    assert!(matches!(finalize(input, stop()).await,
        Err(ValidationError::Source(SourceError::Cancelled))));
}

///
/// 整列直後とworker返却後の取消で計画を公開せず領域を回収する。
///
/// # 注記
/// comparatorは変更せず、停止点の両側へ決定的な取消を注入する。
///
/// # 戻り値
/// なし。整列後・worker返却後の取消や領域回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_cancellation_after_sort_and_before_publication() {
    /*
     * worker所有中と返却後の各境界に同じ取消を注入する
     */
    for after_worker in [false, true] {
        let input = validated(&[(b"page.md", b"payload")]).await;
        let path = storage_path(&input);
        if after_worker {
            CANCEL_BEFORE_PUBLISH.with(|cancel| cancel.set(true));
        } else {
            SORT_ACTION.with(|action|
                *action.borrow_mut() = Some(SortAction::Cancel));
        }
        assert!(matches!(finalize(input, stop()).await,
            Err(ValidationError::Source(SourceError::Cancelled))));
        assert!(!path.parent().unwrap().exists());
    }
}

///
/// workerが整列結果を所有する間の期限到達も公開せず回収する。
///
/// # 注記
/// 整列後に同じcontextの期限まで待ち、終了状態分類とlease回収を確認。
///
/// # 戻り値
/// なし。worker所有中の期限失敗や領域回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_deadline_while_worker_owns_sorted_plan() {
    let input = validated(&[(b"page.md", b"payload")]).await;
    let path = storage_path(&input);
    let entered = Arc::new(AtomicBool::new(false));
    SORT_ACTION.with(|action| *action.borrow_mut() =
        Some(SortAction::WaitForDeadline(Arc::clone(&entered))));
    let shared = StopContext::new(Instant::now() + Duration::from_secs(2));
    assert!(matches!(finalize(input, shared).await,
        Err(ValidationError::Source(SourceError::Deadline))));
    assert!(entered.load(Ordering::Relaxed));
    assert!(!path.parent().unwrap().exists());
}

///
/// 外側contextの取消を新入口が取得前と計画直前で共有する。
///
/// # 概要
/// 起動警告は設定側に保持し、先頭停止・入力拒否と整列後停止を分類する。
///
/// # 注記
/// 元ZIPを破棄しても先頭取消を優先し、本文不正ではInputだけを返す。
///
/// # 戻り値
/// なし。共通停止・起動警告保持・安全な失敗分類が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn configured_pipeline_shares_stop_and_preserves_safe_failures() {
    let (area, config) = configured_zip(b"valid\r\n");
    let warnings = config.warnings().to_vec();
    let plan = acquire_and_plan(&config, stop()).await.unwrap();
    assert_eq!(bytes(plan.open_body(0).unwrap()), b"valid\r\n");
    assert_eq!(config.warnings(), warnings.as_slice());
    drop(plan);
    /*
     * 元入力なしでも取得前の共有取消を優先する
     */
    drop(area);
    let shared = stop();
    shared.cancel();
    assert!(matches!(acquire_and_plan_for_report(&config, shared).await,
        Err(Failure::Execution { category: FailureCategory::NotApplied, .. })));
    let (_area, invalid) = configured_zip(b"\xffprivate-body");
    assert!(matches!(acquire_and_plan_for_report(&invalid, stop()).await,
        Err(Failure::Input)));
    /*
     * 計画worker側の取消も外側observerへ通知される
     */
    let (_area, config) = configured_zip(b"valid");
    let shared = stop();
    SORT_ACTION.with(|action| *action.borrow_mut() = Some(SortAction::Cancel));
    assert!(acquire_and_plan(&config, shared.clone()).await.is_err());
    assert_eq!(shared.checkpoint(), Err(SourceError::Cancelled));
}

///
/// poll済み統合futureの破棄を通知し、worker所有中は領域を残す。
///
/// # 概要
/// 整列済みworkerを同期点で止め、future破棄後の取消・最終回収を区別する。
///
/// # 注記
/// 未poll futureには作業がなく、ここでは開始通知を受けてからabortする。
///
/// # 戻り値
/// なし。future破棄時の取消・worker所有・終了後回収が異なればassertで失敗する。
///
#[tokio::test]
async fn dropped_pipeline_cancels_then_reclaims_after_worker_finishes() {
    let (_area, config) = configured_zip(b"payload");
    let shared = stop();
    let observer = shared.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    SORT_ACTION.with(|action| *action.borrow_mut() = Some(SortAction::Pause {
        started, resume: release,
    }));
    let task = tokio::spawn(async move {
        acquire_and_plan(&config, shared).await
    });
    let path = tokio::time::timeout(Duration::from_secs(2), ready)
        .await.unwrap().unwrap();
    /*
     * 停止通知とworkerの所有領域回収を異なる時点で観察する
     */
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(observer.checkpoint(), Err(SourceError::Cancelled));
    assert!(path.exists());
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.parent().unwrap().exists() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
}

///
/// 元入力破棄済み四方式snapshotから同じ順序・本文の計画を確定する。
///
/// # 概要
/// localは製品dispatch、remoteはfile transport fixtureの取得証拠を使う。
///
/// # 注記
/// HTTPS/GitHub実ネットワークdispatchの成功とは扱わない。
///
/// # 戻り値
/// なし。四方式fixtureの参照種別・計画順序・本文が異なればassertで失敗する。
///
#[tokio::test]
async fn four_adapter_fixtures_finalize_identical_paths_and_bytes() {
    let snapshots = source::validation_adapter_fixtures().await.unwrap();
    let mut expected = None;
    /*
     * 方式を保持しながら全検証・計画確定へ共通に接続する
     */
    for (snapshot, kind) in snapshots.into_iter().zip([
        SourceKind::Zip, SourceKind::LocalGit, SourceKind::HttpsGit,
        SourceKind::GitHub,
    ]) {
        /*
         * 各方式の取得済み本文を同じprefixと上限で検証する
         */
        let shared = stop();
        let limits = RunLimits {
            max_pages: 10, max_page_bytes: 1_000_000, max_zip_entries: 100,
            max_zip_compressed_bytes: 1_000_000,
            max_zip_uncompressed_bytes: 2_000_000, max_git_bytes: 3_000_000,
            max_processing_seconds: 60, max_http_concurrency: 1,
        };
        let input = validate_snapshot(snapshot, "/初期/prefix".to_owned(),
            &limits, shared.clone()).await.unwrap();
        let plan = finalize(input, shared).await.unwrap();
        /*
         * 方式固有参照は保持し、投入先と本文は方式非依存で比較する
         */
        assert!(plan.pages().iter().all(|page|
            page.identity().source.parts().0 == kind));
        let actual = contents(&plan);
        if let Some(expected) = &expected { assert_eq!(&actual, expected); }
        else { expected = Some(actual); }
    }
}
