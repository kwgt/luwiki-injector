/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! PUT/DELETE失敗後の単回再照合と元操作・補助失敗の独立所有
//!

use std::sync::Arc;

use crate::report::types::{ObservedState, PageId, ProcessingStage};
use crate::wiki_client::{LookupResult, WikiFailure};
use crate::wiki_client::adapter::{execution_failure, operation_evidence};
use crate::wiki_client::types::{FailureKind, PathKind, PathRelation,
    SendState, WikiOperation};

pub(super) use self::delete::{DeleteProgress, after_delete, eligible_delete};

mod delete;

use super::Executor;
use super::compare::{BodyRelation, compare_planned_at};
use super::types::{Completion, ExecutorFailure, KnownPageFacts, OperationRecord,
    PageDetails, PageStep, StopReason};

///
/// 目的状態・部分観測と再照合失敗を区別する内部結果。
///
struct PutObservation {
    /// 同じID/pathの目的本文を最後まで確認した。
    applied: bool,
    /// 読み取りが確認した状態。異なる本文から未適用を推測しない。
    observed: ObservedState,
    /// 再照合metadataで確認したlocked。所有の証明には使わない。
    locked: Option<bool>,
    /// 最後に成功したpath照会。未確認と不存在を区別する。
    path: Option<LookupResult>,
    /// 元PUTを上書きしてはならない補助失敗。
    failure: Option<ExecutorFailure>,
}

impl PutObservation {
    ///
    /// 何も確認していない状態を作る。
    ///
    /// # 戻り値
    /// 未確認の観測
    ///
    fn new() -> Self {
        Self { applied: false, observed: ObservedState::Unknown,
            locked: None, path: None, failure: None }
    }
}

///
/// PUTの送信開始・status・kindから再照合可能な異常を選ぶ。
///
/// # 概要
/// 停止済みや確定拒否を、成功statusの不完全応答と混同しない。
///
/// # 引数
/// * `failure` - 元の実行側失敗
///
/// # 戻り値
/// 同じ停止境界内で一度だけ状態確認を検討するか
///
pub(super) fn eligible(failure: &ExecutorFailure) -> bool {
    let ExecutorFailure::Wiki(failure) = failure else { return false; };
    let facts = failure.facts();
    if failure.stage() != ProcessingStage::PutSource
        || facts.operation() != WikiOperation::PutSource
        || facts.sent() != SendState::Started
        || matches!(facts.kind(), FailureKind::Deadline
            | FailureKind::Cancelled)
    { return false; }
    matches!(facts.status(), None | Some(500) | Some(200..=299))
}

///
/// 同じ停止境界で次の再照合を始められるか確認する。
///
/// # 引数
/// * `executor` - 元操作と同じcontextの所有者
///
/// # 戻り値
/// 継続可能、または再照合段階の元停止
///
fn checkpoint(executor: &Executor) -> Result<(), ExecutorFailure> {
    #[cfg(test)]
    tests::before_checkpoint(&executor.stop);
    executor.stop.checkpoint().map_err(|error|
        ExecutorFailure::stopped(error, ProcessingStage::Reconcile))
}

///
/// 単発GETの元事実へ再照合caller段階を結び付ける。
///
/// # 引数
/// * `failure` - 本文/秘密を持たない元GET失敗
///
/// # 戻り値
/// 主PUTとは独立した失敗
///
fn wiki(failure: WikiFailure) -> ExecutorFailure {
    ExecutorFailure::Wiki(execution_failure(failure,
        ProcessingStage::Reconcile, Vec::new()))
}

///
/// 同ID・current path・目的本文を順に一度だけ確認する。
///
/// # 概要
/// 条件不成立時に残りのGETを省略し、適用有無は補完しない。
///
/// # 引数
/// * `executor` - 同じ計画/client/context
/// * `index` - 成立計画内の位置
/// * `id` - PUT前から確定した対象ID
/// * `observation` - 成功した部分観測だけを残す受け手
///
/// # 戻り値
/// 観測完了、または元PUTを置き換えない補助失敗
///
async fn check_put(executor: &Executor, index: usize, id: &PageId,
    observation: &mut PutObservation) -> Result<(), ExecutorFailure>
{
    let stage = ProcessingStage::Reconcile;
    let page = executor.plan.pages().get(index).ok_or_else(||
        ExecutorFailure::local(super::types::LocalKind::PlanPosition, stage))?;
    let path = &page.identity().path;
    /*
     * 有効pathが同じ既知IDを示す場合だけmetadataへ進む
     */
    checkpoint(executor)?;
    let found = executor.client.lookup(path).await.map_err(wiki)?;
    let same_id = matches!(&found,
        LookupResult::Found(found) if found == id);
    observation.path = Some(found);
    if !same_id { return Ok(()); }
    checkpoint(executor)?;
    let meta = executor.client.read_meta(id, path).await.map_err(wiki)?;
    observation.locked = Some(meta.locked());
    if meta.deleted() {
        observation.observed = ObservedState::Deleted;
        return Ok(());
    }
    if meta.path_kind() != PathKind::Current
        || meta.path_relation() != PathRelation::MatchesExpected
    { return Ok(()); }
    /*
     * 不変入力と独立受信領域のbytesを固定bufferで照合する
     */
    checkpoint(executor)?;
    let body = executor.client.read_source(id).await.map_err(wiki)?;
    checkpoint(executor)?;
    #[cfg(test)]
    tests::before_compare(&body);
    let (relation, body) = compare_planned_at(Arc::clone(&executor.plan),
        index, body, executor.stop.clone(), stage).await?;
    drop(body);
    checkpoint(executor)?;
    if relation == BodyRelation::Identical {
        observation.applied = true;
        observation.observed = ObservedState::Applied;
    }
    Ok(())
}

///
/// 元PUTを警告へmoveし、adapter対象外の事実も失わない。
///
/// # 概要
/// statusなし非Transportは旧adapter対象外なので独立所有で残す。
///
/// # 引数
/// * `failure` - 元operation/段階/先行事実
/// * `details` - 警告と補助情報の受け手
///
/// # 戻り値
/// なし。元失敗の情報を破棄しない。
///
fn keep_warning(failure: ExecutorFailure, details: &mut PageDetails) {
    let ExecutorFailure::Wiki(failure) = failure else {
        details.retained.push(failure);
        return;
    };
    let (facts, stage, prior) = failure.into_parts();
    details.warnings.extend(prior.into_iter().map(|evidence|
        OperationRecord { stage: None, evidence }));
    /*
     * 消費型adapterの対象条件を先に検査しNoneで失わない
     */
    if facts.status().is_some() || facts.kind() == FailureKind::Transport {
        if let Some(evidence) = operation_evidence(facts, details.observed) {
            details.warnings.push(OperationRecord {
                stage: Some(stage), evidence });
        }
    } else {
        details.retained.push(ExecutorFailure::Wiki(execution_failure(
            facts, stage, Vec::new())));
    }
}

///
/// PUT異常を単回照合し適用・警告・停止を元情報と共に返す。
///
/// # 概要
/// 新規/updateは目的状態時だけ継続し、再作成は確認済みでも停止する。
///
/// # 引数
/// * `executor` - 元操作と同じ所有者
/// * `index` - 同じ成立計画位置
/// * `id` - 確定したPUT対象ID
/// * `failure` - 元PUT失敗。正常応答へ偽装しない。
/// * `completion` - Create/Update/Recreateの由来
/// * `known` - 旧情報と確定新ID
///
/// # 戻り値
/// 元主失敗か警告付き適用。draft/不確定leaseを解除しない。
///
pub(super) async fn after_put(executor: &Executor, index: usize,
    id: &PageId, failure: ExecutorFailure, completion: Completion,
    mut known: KnownPageFacts) -> PageStep
{
    if !eligible(&failure) { return PageStep::failed(failure, known, None); }
    let mut observation = PutObservation::new();
    observation.failure = check_put(executor, index, id, &mut observation)
        .await.err();
    let mut details = PageDetails { observed: observation.observed,
        lock_observed: observation.locked, path_observed: observation.path,
        last_stage: Some(ProcessingStage::Reconcile),
        ..PageDetails::default() };
    /*
     * 旧ページと新draftのlock観測を混ぜず補助失敗を別所有する
     */
    if completion == Completion::Updated {
        if observation.locked.is_some() {
            known.old_lock_observed = observation.locked;
        }
    }
    if let Some(auxiliary) = observation.failure {
        details.auxiliary.push(auxiliary);
    }
    if !observation.applied {
        return PageStep::failed(failure, known, None).with_details(details);
    }
    /*
     * 目的状態でも元異常を警告として残し再作成の厳しい停止を保つ
     */
    keep_warning(failure, &mut details);
    if completion == Completion::Recreated {
        details.stop = Some(StopReason::RecreationUncertain);
    }
    PageStep::applied(completion, known).with_details(details)
}

#[cfg(test)]
mod tests;
