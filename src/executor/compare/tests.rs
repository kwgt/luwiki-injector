/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 分割読込・保管整合・停止kind・worker所有寿命の比較試験
//!

use std::io::{self, Cursor, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::executor::tests::{body, location, no_status, plan, stop};
use crate::source::SourceError;

use super::*;

///
/// 分割幅・中断・途中失敗を独立に注入するreadonly試験reader。
///
struct SplitReader {
    /// 元fixture bytesと現在位置。
    input: Cursor<Vec<u8>>,
    /// 一回の最大読込量。
    width: usize,
    /// 最初の一回だけInterruptedを返す。
    interrupt: bool,
    /// 指定位置以後の読込を失敗させる。
    fail_after: Option<u64>,
}

impl Read for SplitReader {
    ///
    /// fixtureのshort read/中断/途中I/Oを注入する。
    ///
    /// # 引数
    /// * `bytes` - 固定作業buffer
    ///
    /// # 戻り値
    /// 実量またはfixtureのI/O失敗
    ///
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.interrupt {
            self.interrupt = false;
            return Err(io::Error::from(ErrorKind::Interrupted));
        }
        if self.fail_after.is_some_and(|limit| self.input.position() >= limit) {
            return Err(io::Error::from(ErrorKind::Other));
        }
        let width = bytes.len().min(self.width);
        self.input.read(&mut bytes[..width])
    }
}

///
/// 大きな本文と独立short readをbytes変換なしで比較する。
///
/// # 概要
/// 64KiB境界を超える多byte/CRLF/BOMを異なる分割幅で照合する。
///
/// # 注記
/// 一方のInterruptedも含め、最終byte相違と異サイズを独立確認する。
///
/// # 戻り値
/// なし。read境界が同一判定へ影響すれば失敗する。
///
#[test]
fn split_reads_and_interruption_preserve_exact_large_bytes() {
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend("日本語\r\n".repeat(9000).as_bytes());
    let mut left = SplitReader { input: Cursor::new(bytes.clone()), width: 1,
        interrupt: true, fail_after: None };
    let mut right = SplitReader { input: Cursor::new(bytes.clone()), width: 7,
        interrupt: false, fail_after: None };
    let shared = stop();
    let stage = ProcessingStage::ReadExisting;
    assert_eq!(compare_readers(&mut left, &mut right, bytes.len() as u64,
        bytes.len() as u64, &shared, stage).unwrap(), BodyRelation::Identical);
    /*
     * 同じ実量でも末尾のbyte相違と異サイズを見逃さない
     */
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() = b'x';
    assert_eq!(compare_readers(&mut Cursor::new(&bytes),
        &mut Cursor::new(changed), bytes.len() as u64, bytes.len() as u64,
        &shared, stage).unwrap(), BodyRelation::Different);
    assert_eq!(compare_readers(&mut Cursor::new(b""),
        &mut Cursor::new(b"x"), 0, 1, &shared, stage).unwrap(),
        BodyRelation::Different);
    assert!(shared.checkpoint().is_ok());
}

///
/// 同じ宣言量の途中EOFとread I/Oを相違成功へ変換しない。
///
/// # 注記
/// 両側の同時切詰めと片側切詰め、実I/O注入を区別する。
///
/// # 戻り値
/// なし。不完全保管を成功させれば失敗する。
///
#[test]
fn unexpected_eof_and_read_error_are_local_failures() {
    let shared = stop();
    let stage = ProcessingStage::Recheck;
    /*
     * 両側/片側の想定外EOFを比較I/O失敗として確認する
     */
    for right in [b"ab".as_slice(), b"abc".as_slice()] {
        let error = compare_readers(&mut Cursor::new(b"ab"),
            &mut Cursor::new(right), 3, 3, &shared, stage).unwrap_err();
        assert!(matches!(error, ExecutorFailure::Local(error)
            if error.kind() == LocalKind::CompareIo));
    }
    let mut left = SplitReader { input: Cursor::new(b"abcd".to_vec()),
        width: 1, interrupt: false, fail_after: Some(2) };
    assert!(compare_readers(&mut left, &mut Cursor::new(b"abcd"), 4, 4,
        &shared, stage).is_err());
    assert!(shared.checkpoint().is_ok());
}

///
/// 受信所有者を返し再照合の事前本文を維持する。
///
/// # 概要
/// 製品HTTP受信と製品計画を比較し、両受信の独立比較も確認する。
///
/// # 注記
/// BOM/CRLF/末尾ありなしを保ち、Fileがreadonlyのままか確認する。
///
/// # 戻り値
/// なし。所有返却または完全bytes比較が異なれば失敗する。
///
#[tokio::test]
async fn planned_and_received_comparison_return_owned_bodies() {
    let shared = stop();
    let bytes = b"\xef\xbb\xbfpayload\r\n";
    let plan = Arc::new(plan(&[(b"page.md", bytes)], &shared).await);
    let received = body(bytes, &shared).await;
    let path = location(&received.open().unwrap());
    /*
     * readonly所有を比較後も受け取り事前本文として保持する
     */
    let (relation, received) = compare_planned(plan, 0, received,
        shared.clone()).await.unwrap();
    assert_eq!(relation, BodyRelation::Identical);
    assert!(path.exists());
    assert!(received.open().unwrap().write_all(b"mutation").is_err());
    let after = body(b"\xef\xbb\xbfpayload\n", &shared).await;
    let (relation, before, after) = compare_received(received, after,
        shared.clone()).await.unwrap();
    assert_eq!(relation, BodyRelation::Different);
    assert!(path.exists());
    drop(before);
    drop(after);
    assert!(!path.parent().unwrap().exists());
}

///
/// 保管open失敗と先行statusなし事実を元kind/段階のまま保持する。
///
/// # 概要
/// fixtureの本文Fileだけを削除し、受信元200のcauseと結合列を確認。
///
/// # 注記
/// 比較openは新しいHTTPではなく、受信所有者の保管失敗として扱う。
///
/// # 戻り値
/// なし。cause・先行事実・取消非発火が異なれば失敗する。
///
#[tokio::test]
async fn received_open_cause_and_prior_evidence_survive_move() {
    let shared = stop();
    let input = Arc::new(plan(&[(b"a.md", b"a")], &shared).await);
    let received = body(b"a", &shared).await;
    let path = location(&received.open().unwrap());
    std::fs::remove_file(path).unwrap();
    let evidence = no_status(&shared).await;
    /*
     * 補助causeを保持したまま先行のstatusなし事実を結合する
     */
    let failure = compare_planned(input, 0, received, shared.clone())
        .await.unwrap_err().with_prior_evidence(vec![evidence]).unwrap();
    let ExecutorFailure::Local(local) = failure else { panic!("local cause") };
    assert_eq!(local.kind(), LocalKind::ReceivedOpen);
    assert_eq!(local.stage(), ProcessingStage::ReadExisting);
    assert_eq!(local.received_open_cause().unwrap().status(), Some(200));
    assert!(local.evidence()[0].facts().status().is_none());
    assert!(local.evidence()[0].warning().is_none());
    assert!(shared.checkpoint().is_ok());
}

///
/// 通常Ioとworker panicの返却では共有取消を通知しない。
///
/// # 注記
/// task結果とJoinErrorを別注入し両方のobserverを確認する。
///
/// # 戻り値
/// なし。通常失敗でcontextが取消されれば失敗する。
///
#[tokio::test]
async fn ordinary_io_and_worker_failure_disarm_cancel_guard() {
    let shared = stop();
    let stage = ProcessingStage::ReadExisting;
    let error = blocking::<(), _>(shared.clone(), stage, move |_| {
        Err(ExecutorFailure::local(LocalKind::CompareIo, stage))
    }).await.unwrap_err();
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::CompareIo));
    assert!(shared.checkpoint().is_ok());
    let error = blocking::<(), _>(shared.clone(), stage, |_| {
        panic!("fixture worker failure")
    }).await.unwrap_err();
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::Worker));
    assert!(shared.checkpoint().is_ok());
}

///
/// 期限を捕捉しworker終了後もDeadlineのまま返す。
///
/// # 概要
/// 固定期限のworker開始を同期し停止観測後のjoinを確認する。
///
/// # 注記
/// workerを期限より遅く終了させ、返却前にjoinを確認する。
///
/// # 戻り値
/// なし。取消への読み替えやworker終了前返却で失敗する。
///
#[tokio::test]
async fn deadline_is_preserved_after_worker_join() {
    let shared = StopContext::new(Instant::now() + Duration::from_secs(2));
    let finished = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&finished);
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(blocking(shared, ProcessingStage::Recheck,
        move |worker_stop| {
            let _ = started.send(());
            /*
             * 固定deadlineを変えず到達をworkerで観測してから終了する
             */
            while worker_stop.checkpoint().is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
            finished.store(true, Ordering::Release);
            Ok(())
        }));
    ready.await.unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert!(observed.load(Ordering::Acquire));
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::Deadline));
}

///
/// 共有取消を捕捉しworker終了後もCancelledとして返す。
///
/// # 概要
/// worker開始を同期し、取消後の終了と返却を別々に確認する。
///
/// # 注記
/// 単なるsleep幅から取消到達を推測せず開始通知と解除channelを使う。
///
/// # 戻り値
/// なし。元kindまたはjoinの確認が失われれば失敗する。
///
#[tokio::test]
async fn shared_cancel_is_preserved_after_worker_join() {
    let shared = stop();
    let observer = shared.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&finished);
    let (started, ready) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    let task = tokio::spawn(blocking(shared, ProcessingStage::Recheck,
        move |_| {
            let _ = started.send(());
            let _ = release.recv();
            finished.store(true, Ordering::Release);
            Ok(())
        }));
    /*
     * 取消とworker解除を同期し通常返却前のjoinを確認する
     */
    ready.await.unwrap();
    observer.cancel();
    resume.send(()).unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert!(observed.load(Ordering::Acquire));
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::Cancelled));
}

///
/// 存在しない計画位置をHTTPなしで局所失敗にする。
///
/// # 注記
/// 成立計画と実受信所有者を使い、失敗時の受信領域回収も確認する。
///
/// # 戻り値
/// なし。位置不整合が成功扱いされたら失敗する。
///
#[tokio::test]
async fn absent_plan_position_is_not_an_input_or_http_failure() {
    let shared = stop();
    let input = Arc::new(plan(&[(b"a.md", b"a")], &shared).await);
    let received = body(b"a", &shared).await;
    let path = location(&received.open().unwrap());
    let error = compare_planned(input, 1, received, shared.clone())
        .await.unwrap_err();
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::PlanPosition));
    assert!(!path.parent().unwrap().exists());
    assert!(shared.checkpoint().is_ok());
}

///
/// 外側future破棄後もworkerが計画/受信領域を終了まで保つ。
///
/// # 概要
/// Fileを開いたworkerを同期点で止め、abortと回収を別時点で確認する。
///
/// # 注記
/// Linux fd観察を試験だけで使い製品へpath getterを追加しない。
///
/// # 戻り値
/// なし。worker終了前の領域消失や取消未共有で失敗する。
///
#[tokio::test]
async fn dropped_future_keeps_both_body_owners_until_worker_finishes() {
    let shared = stop();
    let observer = shared.clone();
    let input = Arc::new(plan(&[(b"a.md", b"a")], &shared).await);
    let received = body(b"a", &shared).await;
    let (started, ready) = tokio::sync::oneshot::channel();
    let (resume, release) = std::sync::mpsc::channel();
    let task = tokio::spawn(blocking(shared, ProcessingStage::ReadExisting,
        move |worker_stop| {
            let left = input.open_body(0).unwrap();
            let right = received.open().unwrap();
            let _ = started.send((location(&left), location(&right)));
            let _ = release.recv();
            /*
             * 同期点解除後までFileと両TempDir所有者を明示保持する
             */
            drop(left);
            drop(right);
            drop(input);
            drop(received);
            check(&worker_stop, ProcessingStage::ReadExisting)
        }));
    let (left, right) = tokio::time::timeout(Duration::from_secs(2), ready)
        .await.unwrap().unwrap();
    /*
     * 実future破棄は取消するがworker所有領域は先に消さない
     */
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(observer.checkpoint(), Err(SourceError::Cancelled));
    assert!(left.exists());
    assert!(right.exists());
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while left.parent().unwrap().exists()
            || right.parent().unwrap().exists()
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
}
