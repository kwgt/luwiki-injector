/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! blocking作業へ停止を伝え、期限超過時も作業の終了を待つ境界
//!

use super::{SourceError, StopContext};

///
/// 呼出しfutureの破棄時もworkerへ停止を通知する。
///
pub(crate) struct CancelOnDrop(Option<StopContext>);

impl CancelOnDrop {
    ///
    /// 呼出し側が完了するまで共有取消の通知を保持する。
    ///
    /// # 引数
    /// * `stop` - workerと共有する取消境界
    ///
    /// # 戻り値
    /// 破棄時に取消を通知するguard
    ///
    pub(crate) fn new(stop: StopContext) -> Self { Self(Some(stop)) }

    ///
    /// 正常完了時に破棄による取消通知を解除する。
    ///
    /// # 戻り値
    /// なし。guardは共有contextを解放する。
    ///
    pub(crate) fn disarm(&mut self) { self.0 = None; }
}

impl Drop for CancelOnDrop {
    ///
    /// workerに停止を要求する。leaseはworker側が所有する。
    ///
    /// # 戻り値
    /// なし。即時停止・即時回収は保証しない。
    ///
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            crate::measurement::event(crate::measurement::Event::GuardDrop,
                stop.trace_id(), 0, crate::measurement::Role::Other, 0);
            stop.cancel();
        }
    }
}

///
/// 同期処理を開始し、期限超過時は終了まで待ってから失敗する。
///
/// # 概要
/// 外側のtimeoutでworkerをabortせず、作業中のleaseを先に回収しない。
///
/// # 引数
/// * `stop` - 共有停止境界
/// * `task` - worker側で全準備・I/O・回収を所有する同期処理
///
/// # 戻り値
/// 作業結果、または安全な停止/worker失敗分類
///
pub(crate) async fn run<T, F>(stop: StopContext, task: F)
    -> Result<T, SourceError>
where
    T: Send + 'static,
    F: FnOnce(StopContext) -> Result<T, SourceError> + Send + 'static,
{
    stop.checkpoint()?;
    let worker_stop = stop.clone();
    let mut guard = CancelOnDrop::new(stop.clone());
    let worker_id = crate::measurement::id();
    let context_id = stop.trace_id();
    crate::measurement::event(crate::measurement::Event::WorkerScheduled,
        worker_id, context_id, crate::measurement::Role::Source, 0);
    let mut worker = tokio::task::spawn_blocking(move || {
        let _scope = crate::measurement::Span::new(
            crate::measurement::Event::WorkerStart,
            crate::measurement::Event::WorkerEnd, worker_id, context_id,
            crate::measurement::Role::Source);
        task(worker_stop)
    });
    /*
     * 期限超過でもworkerの所有物を呼出し側で破棄しない
     */
    crate::measurement::event(crate::measurement::Event::JoinStart,
        worker_id, context_id, crate::measurement::Role::Source, 0);
    let result = tokio::select! {
        result = &mut worker => {
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                worker_id, context_id, crate::measurement::Role::Source, 0);
            result.map_err(|_| SourceError::Worker)?
        },
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
            stop.deadline,
        )) => {
            /*
             * 期限到達を共有し、worker終了を待ってからjoin完了を記録する
             */
            stop.cancel();
            let _ = worker.await;
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                worker_id, context_id, crate::measurement::Role::Source, 0);
            Err(SourceError::Deadline)
        }
    };
    guard.disarm();
    stop.checkpoint()?;
    result
}
