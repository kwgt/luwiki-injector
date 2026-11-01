/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 取得中の専用領域と期限を監視してgixへ協調停止を通知する
//!

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::source::{SourceError, StopContext};

///
/// panic経路でも監視threadを終了させるscope内の通知所有者。
///
struct Finish<'a>(&'a AtomicBool);

impl Drop for Finish<'_> {
    ///
    /// workerが終了したことを監視側へ通知する。
    ///
    /// # 戻り値
    /// なし
    ///
    fn drop(&mut self) { self.0.store(true, Ordering::Relaxed); }
}

///
/// worker終了まで監視し、取得中超過を安全な上限エラーへ写す。
///
/// # 概要
/// 監視は通信を即時中断しない。gixへ渡した共有flagを更新するだけである。
/// 通信中の一時容量超過を許容し、phase境界の厳格検査と区別する。
///
/// # 引数
/// * `root` - worker所有の取得領域
/// * `maximum` - 専用領域上限（bytes）
/// * `stop` - 期限と停止flag
/// * `task` - gix非Send値を外へ出さない同期作業
///
/// # 戻り値
/// 成功結果または生の外部エラーを持たない失敗分類
///
pub(in crate::source) fn run<T>(root: &Path, maximum: u64, stop: &StopContext,
    task: impl FnOnce() -> Result<T, SourceError>) -> Result<T, SourceError>
{
    let finished = AtomicBool::new(false);
    let exceeded = AtomicBool::new(false);
    /*
     * 監視threadはgix値を持たず、通知だけをworkerへ返す
     */
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            /*
             * 期限と取得サイズを監視し、共有停止flagだけを変更する
             */
            while !finished.load(Ordering::Relaxed) {
                if stop.checkpoint().is_err() {
                    stop.cancel();
                    break;
                }
                if matches!(super::size(root, maximum, stop),
                    Err(SourceError::Limit("max_git_bytes")))
                {
                    exceeded.store(true, Ordering::Relaxed);
                    stop.cancel();
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(20));
            }
        });
        /*
         * taskの終了通知後に監視threadを回収し、失敗分類を確定する
         */
        let guard = Finish(&finished);
        let result = task();
        drop(guard);
        watcher.thread().unpark();
        watcher.join().map_err(|_| SourceError::Worker)?;
        if exceeded.load(Ordering::Relaxed) {
            return Err(SourceError::Limit("max_git_bytes"));
        }
        stop.checkpoint()?;
        result
    })
}

///
/// phase終了時の全体サイズを厳格に検査する。
///
/// # 引数
/// * `root` - 書込phaseが終了した取得領域
/// * `maximum` - 全体上限（bytes）
/// * `stop` - 停止境界
///
/// # 戻り値
/// 全領域が上限内なら成功
///
pub(super) fn check_size(root: &Path, maximum: u64, stop: &StopContext)
    -> Result<(), SourceError>
{
    super::size(root, maximum, stop).map(|_| ())
}
