/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! readonly本文所有をworkerへ移し固定作業bufferでbytesを比較する
//!

use std::io::{ErrorKind, Read};
use std::sync::Arc;

use crate::projection::ProjectionPlan;
use crate::report::types::ProcessingStage;
use crate::source::{CancelOnDrop, StopContext};
use crate::wiki_client::ReceivedBody;

use super::types::{ExecutorFailure, LocalFailure, LocalKind};

// 比較用作業量であり、Wiki応答総量の上限ではない。
const CHUNK: usize = 64 * 1024;

///
/// 加工しない本文bytesの比較結果。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyRelation {
    /// 完全に同一のbytes。
    Identical,
    /// 実量またはbytesが異なる。
    Different,
}

///
/// 計画本文と受信本文を所有者ごとworkerへ渡して比較する。
///
/// # 概要
/// 成功時に受信所有者を返し、lock後再照合まで事前本文を保てる。
///
/// # 引数
/// * `plan` - 全件検証済み本文の寿命を持つ計画
/// * `index` - 計画内の本文位置
/// * `body` - 独立readonlyの受信所有者
/// * `stop` - 取得から共有する停止context
///
/// # 戻り値
/// 比較結果と受信所有者、またはHTTPを偽装しない安全な失敗
///
pub(crate) async fn compare_planned(plan: Arc<ProjectionPlan>, index: usize,
    body: ReceivedBody, stop: StopContext)
    -> Result<(BodyRelation, ReceivedBody), ExecutorFailure>
{
    compare_planned_at(plan, index, body, stop,
        ProcessingStage::ReadExisting).await
}

///
/// caller段階を保持して計画と受信本文をworkerへ所有させる。
///
/// # 概要
/// 再照合の局所I/O失敗を初回比較の段階へ戻さない。
///
/// # 引数
/// * `plan` - 成立計画の所有者
/// * `index` - 同じ計画位置
/// * `body` - 独立受信領域
/// * `stop` - 元操作から共有する停止境界
/// * `stage` - 呼出側の比較段階
///
/// # 戻り値
/// 比較結果と受信所有者、またはcaller段階の元失敗
///
pub(super) async fn compare_planned_at(plan: Arc<ProjectionPlan>,
    index: usize, body: ReceivedBody, stop: StopContext,
    stage: ProcessingStage)
    -> Result<(BodyRelation, ReceivedBody), ExecutorFailure>
{
    blocking(stop, stage, move |stop| {
        /*
         * FileだけでなくArc計画と受信領域もworker内で所有する
         */
        check(&stop, stage)?;
        let length = plan.pages().get(index).ok_or_else(||
            ExecutorFailure::local(LocalKind::PlanPosition, stage))?.byte_len();
        let mut left = plan.open_body(index).map_err(|_|
            ExecutorFailure::local(LocalKind::PlanOpen, stage))?;
        let mut right = body.open().map_err(|cause|
            ExecutorFailure::Local(LocalFailure::received_open(cause, stage)))?;
        let result = compare_readers(&mut left, &mut right, length,
            body.byte_len(), &stop, stage);
        /*
         * 実I/O終了まで両所有者を生存させて受信所有者を返す
         */
        drop(left);
        drop(right);
        drop(plan);
        result.map(|relation| (relation, body))
    }).await
}

///
/// 事前本文と再取得本文を独立した所有のまま比較する。
///
/// # 概要
/// 同一判定を入力との比較へ置き換えず、事前のbytesと照合する。
///
/// # 引数
/// * `before` - 事前に検証/保管した本文
/// * `after` - lock後等に再取得した本文
/// * `stop` - 同じ期限/取消
///
/// # 戻り値
/// 比較結果と両所有者、または安全な局所失敗
///
pub(crate) async fn compare_received(before: ReceivedBody,
    after: ReceivedBody, stop: StopContext)
    -> Result<(BodyRelation, ReceivedBody, ReceivedBody), ExecutorFailure>
{
    let stage = ProcessingStage::Recheck;
    blocking(stop, stage, move |stop| {
        /*
         * 事前/再取得のTempDirとFileを同じworkerへ閉じる
         */
        check(&stop, stage)?;
        let mut left = before.open().map_err(|cause|
            ExecutorFailure::Local(LocalFailure::received_open(cause, stage)))?;
        let mut right = after.open().map_err(|cause|
            ExecutorFailure::Local(LocalFailure::received_open(cause, stage)))?;
        let result = compare_readers(&mut left, &mut right,
            before.byte_len(), after.byte_len(), &stop, stage);
        drop(left);
        drop(right);
        result.map(|relation| (relation, before, after))
    }).await
}

///
/// worker終了と共有停止を競合させ、元の停止kindを維持する。
///
/// # 概要
/// 通常Io/JoinError返却前にguard解除し、実future破棄だけ取消する。
///
/// # 引数
/// * `stop` - 同一絶対期限/共有取消
/// * `stage` - caller段階
/// * `task` - 全File/本文所有者を持つ同期処理
///
/// # 戻り値
/// worker結果、またはjoin後も元の期限/取消を保つ失敗
///
async fn blocking<T, F>(stop: StopContext, stage: ProcessingStage, task: F)
    -> Result<T, ExecutorFailure>
where
    T: Send + 'static,
    F: FnOnce(StopContext) -> Result<T, ExecutorFailure> + Send + 'static,
{
    check(&stop, stage)?;
    let mut guard = CancelOnDrop::new(stop.clone());
    let worker_stop = stop.clone();
    let mut worker = tokio::task::spawn_blocking(move || task(worker_stop));
    /*
     * 停止kindを捕捉してworkerを待ち、先に所有物を回収しない
     */
    let result = tokio::select! {
        result = &mut worker => result.unwrap_or_else(|_|
            Err(ExecutorFailure::local(LocalKind::Worker, stage))),
        stopped = stop.wait_stopped() => {
            stop.cancel();
            let _ = worker.await;
            guard.disarm();
            return Err(ExecutorFailure::stopped(stopped, stage));
        }
    };
    /*
     * 通常失敗でも解除し、停止と同時の成功公開を防ぐ
     */
    guard.disarm();
    check(&stop, stage)?;
    result
}

///
/// 同じ停止contextだけを確認する。
///
/// # 引数
/// * `stop` - 共有境界
/// * `stage` - caller段階
///
/// # 戻り値
/// 継続可能なら成功
///
fn check(stop: &StopContext, stage: ProcessingStage)
    -> Result<(), ExecutorFailure>
{
    stop.checkpoint().map_err(|error| ExecutorFailure::stopped(error, stage))
}

///
/// short readを独立に満たしbytesの比較位置を一致させる。
///
/// # 概要
/// 中断readは停止確認後だけ再読込し、HTTP再送とは区別する。
///
/// # 引数
/// * `input` - readonly reader
/// * `bytes` - 固定作業buffer
/// * `stop` - 共有境界
/// * `stage` - caller段階
///
/// # 戻り値
/// 実読込量。buffer未満は正常EOF。
///
fn fill(input: &mut impl Read, bytes: &mut [u8], stop: &StopContext,
    stage: ProcessingStage) -> Result<usize, ExecutorFailure>
{
    let mut used = 0;
    /*
     * 両側のshort read幅に依存せずchunkかEOFまで満たす
     */
    while used < bytes.len() {
        check(stop, stage)?;
        let count = match input.read(&mut bytes[used..]) {
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(ExecutorFailure::local(LocalKind::CompareIo,
                stage)),
        };
        check(stop, stage)?;
        if count == 0 { break; }
        used += count;
    }
    Ok(used)
}

///
/// 実量と固定bufferの比較だけで完全bytes一致を判定する。
///
/// # 概要
/// 想定外EOFを異なる本文として成功させず保管整合失敗とする。
///
/// # 引数
/// * `left` - 一方のreadonly本文
/// * `right` - もう一方のreadonly本文
/// * `left_len` - 検証済みの実量（bytes）
/// * `right_len` - 受信済みの実量（bytes）
/// * `stop` - 共有境界
/// * `stage` - caller段階
///
/// # 戻り値
/// 完全一致/相違、または安全な局所失敗
///
fn compare_readers(left: &mut impl Read, right: &mut impl Read,
    left_len: u64, right_len: u64, stop: &StopContext,
    stage: ProcessingStage) -> Result<BodyRelation, ExecutorFailure>
{
    check(stop, stage)?;
    if left_len != right_len { return Ok(BodyRelation::Different); }
    let mut a = [0; CHUNK];
    let mut b = [0; CHUNK];
    let mut total = 0_u64;
    /*
     * 加工しないbytesを固定量ずつ比較する
     */
    loop {
        /*
         * 同量chunkとEOF整合を比較し不完全な保管を拒否する
         */
        let na = fill(left, &mut a, stop, stage)?;
        let nb = fill(right, &mut b, stop, stage)?;
        if na != nb {
            return Err(ExecutorFailure::local(LocalKind::CompareIo, stage));
        }
        total = total.checked_add(na as u64).ok_or_else(||
            ExecutorFailure::local(LocalKind::CompareIo, stage))?;
        if total > left_len || (na < CHUNK && total != left_len) {
            return Err(ExecutorFailure::local(LocalKind::CompareIo, stage));
        }
        if a[..na] != b[..nb] { return Ok(BodyRelation::Different); }
        if na < CHUNK {
            check(stop, stage)?;
            return Ok(BodyRelation::Identical);
        }
    }
}

#[cfg(test)]
mod tests;
