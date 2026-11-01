/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! uploadの実量引渡し・worker失敗・readonly所有寿命の契約試験
//!

use std::cell::RefCell;
use std::fs::{self, File};
use std::future::poll_fn;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::wiki_client::tests::writes::planned;

use super::*;

thread_local! {
    static ACTION: RefCell<Option<Action>> = const { RefCell::new(None) };
    static JOIN: RefCell<Option<oneshot::Sender<()>>> =
        const { RefCell::new(None) };
}

///
/// upload worker内の失敗/実I/O直前所有観察/分割指定。
///
pub(in crate::wiki_client) enum Action {
    Io,
    ExitIo,
    Width(usize),
    Pause {
        ready: oneshot::Sender<PathBuf>,
        release: mpsc::Receiver<()>,
    },
    PauseExit {
        ready: oneshot::Sender<PathBuf>,
        release: mpsc::Receiver<()>,
    },
}

///
/// このasync操作の試験指定をworkerへ移す。
///
/// # 戻り値
/// 設定済み操作だけ
///
pub(super) fn take_action() -> Option<Action> {
    ACTION.with(|action| action.borrow_mut().take())
}

///
/// このthreadにだけ試験操作を設定する。
///
/// # 引数
/// * `action` - workerへ一度だけ移動する指定
///
/// # 戻り値
/// なし
///
pub(in crate::wiki_client) fn set_action(action: Action) {
    ACTION.with(|slot| *slot.borrow_mut() = Some(action));
}

///
/// transportが完了量を確定してjoinへ進む同期点を登録する。
///
/// # 引数
/// * `sender` - このthreadの通知先
///
/// # 戻り値
/// なし
///
pub(in crate::wiki_client) fn set_join_notice(sender: oneshot::Sender<()>) {
    JOIN.with(|slot| *slot.borrow_mut() = Some(sender));
}

///
/// 完了量観測後のjoin開始を試験へ通知する。
///
/// # 戻り値
/// なし。通知は一度だけ消費する。
///
pub(in crate::wiki_client) fn notify_join() {
    JOIN.with(|slot| {
        if let Some(sender) = slot.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
}

///
/// Fileを所有したworkerのI/O前で失敗または同期点を注入する。
///
/// # 引数
/// * `action` - このworkerだけの指定
/// * `file` - readonly保管handle
///
/// # 戻り値
/// 進行可能なら成功
///
pub(super) fn before_read(action: &Option<Action>, file: &File)
    -> Result<(), FailureKind>
{
    match action {
        Some(Action::Io) => Err(FailureKind::Io),
        Some(Action::Pause { .. }) => { let _ = file; Ok(()) }
        _ => Ok(()),
    }
}

///
/// このworkerの作業幅だけを返す。
///
/// # 引数
/// * `action` - このworkerだけの指定
///
/// # 戻り値
/// 固定作業上限以内の幅
///
pub(super) fn width(action: &Option<Action>) -> usize {
    match action {
        Some(Action::Width(width)) => (*width).clamp(1,
            crate::wiki_client::receive::WORK_CHUNK),
        _ => crate::wiki_client::receive::WORK_CHUNK,
    }
}

///
/// worker所有中の同期点を一度だけ消費する。
///
/// # 引数
/// * `action` - このworkerの注入指定
/// * `file` - worker所有readonly handle
///
/// # 戻り値
/// 分割指定など未消費の操作
///
pub(super) fn pause(action: Option<Action>, file: &File) -> Option<Action> {
    if let Some(Action::Pause { ready, release }) = action {
        let path = fs::read_link(format!("/proc/self/fd/{}",
            file.as_raw_fd())).unwrap();
        let _ = ready.send(path);
        let _ = release.recv_timeout(Duration::from_secs(10));
        None
    } else { action }
}

///
/// worker終了直前もownerとFileを保持したまま同期点を注入する。
///
/// # 引数
/// * `action` - このworkerの終了指定
/// * `file` - まだworkerが所有するreadonly handle
///
/// # 戻り値
/// 正常終了、または試験専用のI/O失敗
///
pub(super) fn after_loop(action: Option<Action>, file: &File)
    -> Result<(), FailureKind>
{
    if matches!(&action, Some(Action::ExitIo)) {
        return Err(FailureKind::Io);
    }
    /*
     * 終了待機の同期点では所有Fileを観察してから解除を待つ
     */
    if let Some(Action::PauseExit { ready, release }) = action {
        let path = fs::read_link(format!("/proc/self/fd/{}",
            file.as_raw_fd())).unwrap();
        let _ = ready.send(path);
        let _ = release.recv_timeout(Duration::from_secs(10));
    }
    Ok(())
}

///
/// 正常試験用の共有期限を作る。
///
/// # 戻り値
/// 60秒の期限
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(60))
}

///
/// 1byte引渡しでも多byte/BOM/改行を変換せず全量に一致する。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// streamへの引渡しを観察し、wire送信済み量とは扱わない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn hands_exact_bytes_to_stream_in_single_byte_pieces() {
    let bytes = "\u{feff}日本語\r\nline\n末尾".as_bytes();
    let plan = planned(bytes).await;
    let permits = Arc::new(Semaphore::new(1));
    let permit = Arc::new(permits.clone().acquire_owned().await.unwrap());
    set_action(Action::Width(1));
    let (mut owner, mut stream) = Upload::prepare_stream(
        plan.upload_body(0).unwrap(), stop(), permit).await.unwrap();
    let mut actual = Vec::new();
    /*
     * Read結果を一つずつ消費し、内部prefetchを増やさない
     */
    while let Some(piece) = poll_fn(|context|
        Pin::new(&mut stream).poll_next(context)).await
    {
        let piece = piece.unwrap();
        assert_eq!(piece.len(), 1);
        actual.extend(piece);
    }
    assert!(actual == bytes);
    assert!(owner.complete());
    owner.join().await.unwrap();
    assert_eq!(permits.available_permits(), 1);
}

///
/// 最後のworker結果が未引渡しなら完了に数えない。
///
/// # 注記
/// oneshot結果だけを取り出し、StreamのReady(Some)を経由させない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn does_not_count_unconsumed_last_worker_result() {
    let plan = planned(b"body").await;
    let permit = Arc::new(Arc::new(Semaphore::new(1))
        .acquire_owned().await.unwrap());
    let (mut owner, stream) = Upload::prepare_stream(
        plan.upload_body(0).unwrap(), stop(), permit).await.unwrap();
    let (reply, pending) = oneshot::channel();
    stream.state.sender.send(Command::Read(reply)).unwrap();
    let piece = pending.await.unwrap().unwrap();
    assert_eq!(piece.len(), 4);
    assert!(!owner.complete());
    drop(stream);
    owner.join().await.unwrap();
}

///
/// Content-LengthでEOF未pollでも全byte引渡し済みなら完了である。
///
/// # 注記
/// 最後のpiece直後にstreamを破棄し、追加read/EOFを要求しない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn accepts_full_delivery_without_polling_eof() {
    let plan = planned(b"body").await;
    let permit = Arc::new(Arc::new(Semaphore::new(1))
        .acquire_owned().await.unwrap());
    let (mut owner, mut stream) = Upload::prepare_stream(
        plan.upload_body(0).unwrap(), stop(), permit).await.unwrap();
    let piece = poll_fn(|context| Pin::new(&mut stream)
        .poll_next(context)).await.unwrap().unwrap();
    assert_eq!(piece.len(), 4);
    drop(stream);
    assert!(owner.complete());
    owner.join().await.unwrap();
}

///
/// worker I/O失敗を通常失敗としてjoinに保持する。
///
/// # 概要
/// 計画とpermitを所有するworkerへI/O失敗を注入する。
/// 準備またはjoinの失敗分類と共有取消非発火を照合する。
///
/// # 注記
/// 実ディスク故障ではなくFile取得後の固定分類注入である。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn preserves_worker_io_failure_without_shared_cancellation() {
    /*
     * 計画とpermitを所有するworkerへI/O失敗を注入する
     */
    let plan = planned(b"body").await;
    let shared = stop();
    let permit = Arc::new(Arc::new(Semaphore::new(1))
        .acquire_owned().await.unwrap());
    set_action(Action::Io);
    let prepared = Upload::prepare_stream(plan.upload_body(0).unwrap(),
        shared.clone(), permit).await;
    /*
     * 準備またはjoinの失敗分類と共有取消非発火を照合する
     */
    match prepared {
        Ok((mut owner, stream)) => {
            drop(stream);
            assert_eq!(owner.join().await, Err(FailureKind::Io));
        }
        Err(kind) => assert_eq!(kind, FailureKind::Io),
    }
    assert!(shared.checkpoint().is_ok());
}
