/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 旧ID削除とpath不存在を個別確認し完全500だけ再作成へ渡す
//!

use crate::executor::Executor;
use crate::executor::types::{ExecutorFailure, KnownPageFacts, LocalKind,
    PageDetails, PageStep};
use crate::report::types::{ObservedState, PageId, ProcessingStage};
use crate::wiki_client::LookupResult;
use crate::wiki_client::types::{FailureKind, PathKind, PathRelation,
    SendState, WikiOperation};

use super::{checkpoint, keep_warning, wiki};

///
/// 削除確認後の再作成許可と、停止済み結果を区別する。
///
pub(in crate::executor) enum DeleteProgress {
    /// 旧情報と先行警告を新draft処理へmoveする。
    Recreate(KnownPageFacts, PageDetails),
    /// 追加変更を開始してはならない主失敗と観測。
    Finished(PageStep),
}

///
/// 削除とパスの状態を因果推測せずに個別保持する。
///
struct DeleteObservation {
    /// 旧IDに対する削除済みmetadataを確認した。
    deleted: bool,
    /// expected last_deleted pathと有効path不存在の双方を確認した。
    available: bool,
    /// 一回の再照合の有限状態/補助失敗。
    details: PageDetails,
}

impl DeleteObservation {
    ///
    /// 削除・パス未確認の初期観測を作る。
    ///
    /// # 戻り値
    /// 削除や再作成可否を補完しない受け手
    ///
    fn new() -> Self {
        Self { deleted: false, available: false,
            details: PageDetails { last_stage: Some(ProcessingStage::Reconcile),
                ..PageDetails::default() } }
    }
}

///
/// 元DELETEの送信開始と異常種別から単回照合の対象を選ぶ。
///
/// # 引数
/// * `failure` - 元の単発/局所失敗
///
/// # 戻り値
/// 500/状態不明/不正な成功statusを境界内で照合可能か
///
pub(in crate::executor) fn eligible_delete(failure: &ExecutorFailure) -> bool {
    let ExecutorFailure::Wiki(failure) = failure else { return false; };
    let facts = failure.facts();
    failure.stage() == ProcessingStage::DeleteOld
        && facts.operation() == WikiOperation::DeletePage
        && facts.sent() == SendState::Started
        && !matches!(facts.kind(), FailureKind::Deadline
            | FailureKind::Cancelled)
        && matches!(facts.status(), None | Some(500) | Some(200..=299))
}

///
/// 旧IDの削除と期待pathの有効ページ不存在を一度ずつ確認する。
///
/// # 概要
/// metadata404やpath404片方だけから削除を補完しない。
///
/// # 引数
/// * `executor` - 元DELETEから同じclient/期限/取消
/// * `index` - 成立計画の位置
/// * `id` - 旧DELETEへ渡した確定ID
/// * `observation` - 成功した部分観測とpath結果を残す受け手
///
/// # 戻り値
/// 観測完了、または元DELETEを置き換えない補助失敗
///
async fn check_delete(executor: &Executor, index: usize, id: &PageId,
    observation: &mut DeleteObservation) -> Result<(), ExecutorFailure>
{
    let page = executor.plan.pages().get(index).ok_or_else(||
        ExecutorFailure::local(LocalKind::PlanPosition,
            ProcessingStage::Reconcile))?;
    let path = &page.identity().path;
    /*
     * 旧IDの状態を先に取得し、誤ったpath/状態では以後を開始しない
     */
    checkpoint(executor)?;
    let meta = executor.client.read_meta(id, path).await.map_err(wiki)?;
    observation.details.lock_observed = Some(meta.locked());
    observation.deleted = meta.deleted();
    observation.details.observed = if meta.deleted() { ObservedState::Deleted }
        else { ObservedState::Present };
    if !meta.deleted() || meta.path_kind() != PathKind::LastDeleted
        || meta.path_relation() != PathRelation::MatchesExpected
    { return Ok(()); }
    /*
     * 旧削除確認とは独立に、同じpathへ他者が再作成していないか確認する
     */
    checkpoint(executor)?;
    let found = executor.client.lookup(path).await.map_err(wiki)?;
    let available = matches!(&found,
        LookupResult::NotFound | LookupResult::Deleted);
    observation.details.path_observed = Some(found);
    checkpoint(executor)?;
    observation.available = available;
    Ok(())
}

///
/// 削除確認済みの完全500だけを再作成許可へ変える。
///
/// # 概要
/// 通信/応答不完全や不正成功は削除確認済みでも停止する。
///
/// # 引数
/// * `executor` - 同じ所有者
/// * `index` - 同じ成立計画位置
/// * `id` - 旧確定ID
/// * `failure` - 消してはならない元DELETE失敗
/// * `known` - 旧ID/revision等の確定情報
///
/// # 戻り値
/// 警告と旧情報を持つ再作成許可、または元失敗と観測を持つ停止
///
pub(in crate::executor) async fn after_delete(executor: &Executor,
    index: usize, id: &PageId, failure: ExecutorFailure,
    mut known: KnownPageFacts) -> DeleteProgress
{
    let complete_500 = matches!(&failure, ExecutorFailure::Wiki(value)
        if value.facts().status() == Some(500)
            && value.facts().kind() == FailureKind::Http);
    let mut observation = DeleteObservation::new();
    /*
     * 照合失敗でも成功した旧削除/lock観測は残す
     */
    if let Err(auxiliary) = check_delete(executor, index, id,
        &mut observation).await
    {
        observation.details.auxiliary.push(auxiliary);
    }
    known.old_delete_confirmed |= observation.deleted;
    if observation.details.lock_observed.is_some() {
        known.old_lock_observed = observation.details.lock_observed;
    }
    /*
     * 完全500以外や不十分な照合からPOSTを開始しない
     */
    if !complete_500 || !observation.available {
        return DeleteProgress::Finished(PageStep::failed(failure, known, None)
            .with_details(observation.details));
    }
    keep_warning(failure, &mut observation.details);
    DeleteProgress::Recreate(known, observation.details)
}
