/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! lock前の初回分岐と新規作成・既存拒否・同一スキップ
//!

use std::fmt;
use std::sync::Arc;

use crate::cmd_args::resolve::Mode;
use crate::projection::ProjectionPlan;
use crate::report::types::{PageId, ProcessingStage};
use crate::source::{CancelOnDrop, StopContext};
use crate::wiki_client::{LookupResult, ReceivedBody, WikiFailure};
use crate::wiki_client::adapter::execution_failure;
use crate::wiki_client::types::{PathKind, PathRelation, WikiOperation};

use super::{Executor, reconcile};
use super::compare::{BodyRelation, compare_planned};
use super::types::{Completion, ExecutorFailure, KnownPageFacts, LocalKind,
    PageStep};

///
/// 完了/失敗と、lock以後へ渡す未完の継続状態を区別する。
///
pub(super) enum PageProgress {
    /// 確定完了または元失敗を持つ結果。
    Finished(PageStep),
    /// 相違本文を保持してlock処理へ進む未完状態。
    ExistingDifferent(ExistingSnapshot),
}

impl fmt::Debug for PageProgress {
    ///
    /// 本文・識別情報を表示せず有限な状態だけを表示する。
    ///
    /// # 引数
    /// * `f` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finished(step) => f.debug_struct("Finished")
                .field("failed", &step.is_failed()).finish(),
            Self::ExistingDifferent(snapshot) => fmt::Debug::fmt(snapshot, f),
        }
    }
}

///
/// 比較済みの事前本文と確定識別情報をlock後まで所有する。
///
pub(super) struct ExistingSnapshot {
    /// 生成元executorの唯一の成立計画を所有し誤結合を防ぐ。
    plan: Arc<ProjectionPlan>,
    /// 同じ成立計画内の位置。
    index: usize,
    /// 初回lookupの確定ID。
    id: PageId,
    /// 正常metadataで確認した旧revision。
    revision: u64,
    /// 独立readonlyの事前受信領域。
    body: ReceivedBody,
}

impl ExistingSnapshot {
    ///
    /// 次の私有処理へ位置/識別情報/本文寿命をmoveする。
    ///
    /// # 引数
    /// * `plan` - 消費する同じexecutorの計画
    /// * `index` - 同じ計画位置
    ///
    /// # 戻り値
    /// 旧ID、旧revision、事前本文。不一致はHTTP前に拒否する。
    ///
    pub(super) fn into_parts_for(self, plan: &Arc<ProjectionPlan>, index: usize)
        -> Result<(PageId, u64, ReceivedBody), ExecutorFailure>
    {
        if index != self.index || !Arc::ptr_eq(&self.plan, plan) {
            return Err(ExecutorFailure::local(LocalKind::ResultInvariant,
                ProcessingStage::Preflight));
        }
        Ok((self.id, self.revision, self.body))
    }
}

impl fmt::Debug for ExistingSnapshot {
    ///
    /// 識別子・本文を表示せず実量だけを表示する。
    ///
    /// # 引数
    /// * `f` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExistingDifferent")
            .field("bytes", &self.body.byte_len()).finish()
    }
}

///
/// 一件の分岐中に判明した秘密を含まない情報。
///
struct Attempt {
    /// 失敗しても残す確定ID/revision。
    known: KnownPageFacts,
    /// 試験だけの段階同期。製品設定へ公開しない。
    #[cfg(test)]
    action: Option<tests::Action>,
}

impl Attempt {
    ///
    /// 推測した識別情報のない状態から一件を始める。
    ///
    /// # 戻り値
    /// 空の判明情報と試験内同期操作
    ///
    fn new() -> Self {
        Self { known: KnownPageFacts::default(),
            #[cfg(test)]
            action: tests::take_action(),
        }
    }

    ///
    /// 次の明示操作の前に同じ停止境界を確認する。
    ///
    /// # 引数
    /// * `stop` - 取得から同じ期限/取消
    /// * `stage` - 次のcaller段階
    ///
    /// # 戻り値
    /// 継続可能なら成功、停止なら元kindを保つ局所失敗
    ///
    async fn checkpoint(&mut self, stop: &StopContext,
        stage: ProcessingStage) -> Result<(), ExecutorFailure>
    {
        #[cfg(test)]
        tests::before_stage(stop, stage, &mut self.action).await;
        stop.checkpoint().map_err(|error|
            ExecutorFailure::stopped(error, stage))
    }

    ///
    /// 単発失敗を元kind/statusとcaller段階のまま所有する。
    ///
    /// # 概要
    /// POST完全受信で確定したIDだけを失敗時にも残す。
    ///
    /// # 引数
    /// * `failure` - 元の単発失敗
    /// * `stage` - callerが指定した段階
    ///
    /// # 戻り値
    /// 旧Reportへ投影しない実行側の失敗
    ///
    fn wiki(&mut self, failure: WikiFailure, stage: ProcessingStage)
        -> ExecutorFailure
    {
        /*
         * POST失敗の完全確定IDだけを残し部分候補を推測しない
         */
        if failure.operation() == WikiOperation::CreateDraft {
            if let Some(id) = failure.known_id() {
                self.known.new_id = Some(id.clone());
            }
        }
        ExecutorFailure::Wiki(execution_failure(failure, stage, Vec::new()))
    }

    ///
    /// 元失敗と判明情報を主結果へmoveする。
    ///
    /// # 引数
    /// * `failure` - Wikiまたは局所の元事実
    ///
    /// # 戻り値
    /// 自動解除のない一件の失敗
    ///
    fn failed(self, failure: ExecutorFailure) -> PageProgress {
        PageProgress::Finished(PageStep::failed(failure, self.known, None))
    }

    ///
    /// 成功が確認された分岐だけを確定結果にする。
    ///
    /// # 引数
    /// * `completion` - 実際に確認した完了
    ///
    /// # 戻り値
    /// 判明情報を所有する完了結果
    ///
    fn completed(&mut self, completion: Completion) -> PageProgress {
        PageProgress::Finished(PageStep::completed(completion,
            std::mem::take(&mut self.known)))
    }
}

///
/// 成立計画の一件を分岐し、相違既存は未完の所有状態で返す。
///
/// # 概要
/// 通常返却は取消guardを解除し、実future破棄だけ共有取消する。
/// 公開run/通常processorの不完全な成功stubは設けない。
///
/// # 引数
/// * `executor` - 計画/client/同一停止の所有者
/// * `index` - 成立計画の位置
///
/// # 戻り値
/// 確定結果/失敗、またはlock以後へ渡す既存相違
///
pub(super) async fn prepare(executor: &Executor, index: usize) -> PageProgress {
    let mut guard = CancelOnDrop::new(executor.stop.clone());
    let mut attempt = Attempt::new();
    let result = resolve(executor, index, &mut attempt).await;
    /*
     * 通常失敗でも共有取消を発火させず元の判明情報を返す
     */
    guard.disarm();
    match result {
        Ok(progress) => progress,
        Err(failure) => attempt.failed(failure),
    }
}

///
/// 初回照会だけの不存在を新規処理へ分岐する。
///
/// # 概要
/// error既存は本文/metaを読まず拒否し、他のstatusは再照会しない。
///
/// # 引数
/// * `executor` - 共通所有者
/// * `index` - 計画位置
/// * `attempt` - 判明情報と停止確認
///
/// # 戻り値
/// 初回分岐の結果または元失敗
///
async fn resolve(executor: &Executor, index: usize, attempt: &mut Attempt)
    -> Result<PageProgress, ExecutorFailure>
{
    /*
     * 計画位置を最初のHTTP前に検査し初回lookupだけを要求する
     */
    let page = executor.plan.pages().get(index).ok_or_else(||
        ExecutorFailure::local(LocalKind::PlanPosition,
            ProcessingStage::Preflight))?;
    let stage = ProcessingStage::Lookup;
    attempt.checkpoint(&executor.stop, stage).await?;
    let found = executor.client.lookup(&page.identity().path).await
        .map_err(|failure| attempt.wiki(failure, stage))?;
    /*
     * 正常初回不存在・既存拒否・既存比較を排他的に分岐する
     */
    match found {
        LookupResult::NotFound | LookupResult::Deleted =>
            create(executor, index, attempt, Completion::Created).await,
        LookupResult::Found(id) => {
            attempt.known.old_id = Some(id.clone());
            if executor.mode == Mode::Error {
                return Err(ExecutorFailure::local(LocalKind::Conflict, stage));
            }
            existing(executor, index, id, attempt).await
        }
    }
}

///
/// 新規/再作成の判明情報を保って共有の作成処理を呼ぶ。
///
/// # 概要
/// 再作成もprivate constructorで返し、Created結果を外から書き換えない。
///
/// # 引数
/// * `executor` - 同じ成立計画/client/停止
/// * `index` - 計画位置
/// * `known` - 旧情報と削除確認を含む判明情報
/// * `completion` - CreatedまたはRecreated
///
/// # 戻り値
/// 全判明情報を保持する確定結果または元失敗
///
pub(super) async fn create_page(executor: &Executor, index: usize,
    known: KnownPageFacts, completion: Completion) -> PageStep
{
    let mut guard = CancelOnDrop::new(executor.stop.clone());
    let mut attempt = Attempt::new();
    attempt.known = known;
    let result = create(executor, index, &mut attempt, completion).await;
    /*
     * 通常失敗の取消guardを解除し同じ判明情報から結果を返す
     */
    guard.disarm();
    match result {
        Ok(PageProgress::Finished(step)) => step,
        Ok(PageProgress::ExistingDifferent(_)) => PageStep::failed(
            ExecutorFailure::local(LocalKind::ResultInvariant,
                ProcessingStage::Preflight), attempt.known, None),
        Err(failure) => PageStep::failed(failure, attempt.known, None),
    }
}

///
/// 不存在の全モードでdraft POSTから確定IDへ本文PUTする。
///
/// # 概要
/// draft失敗で解放/削除/再POSTせずPUT前の明示延長を一度行う。
///
/// # 引数
/// * `executor` - 同一client/計画/停止
/// * `index` - 検査済み計画位置
/// * `attempt` - POST以後も残す判明情報
/// * `completion` - CreatedまたはRecreatedの内部指定
///
/// # 戻り値
/// 正常PUT返却だけ指定Created/Recreated、または元失敗
///
async fn create(executor: &Executor, index: usize, attempt: &mut Attempt,
    completion: Completion) -> Result<PageProgress, ExecutorFailure>
{
    /*
     * 内部の作成/再作成前提をHTTP前に確認する
     */
    if executor.plan.pages().get(index).is_none()
        || !matches!(completion, Completion::Created | Completion::Recreated)
        || (completion == Completion::Recreated
            && (!attempt.known.old_delete_confirmed
                || attempt.known.old_id.is_none()))
    {
        return Err(ExecutorFailure::local(LocalKind::ResultInvariant,
            ProcessingStage::Preflight));
    }
    let path = &executor.plan.pages()[index].identity().path;
    let stage = ProcessingStage::CreateDraft;
    attempt.checkpoint(&executor.stop, stage).await?;
    let draft = executor.client.create_draft(path).await
        .map_err(|failure| attempt.wiki(failure, stage))?;
    let (id, mut lease) = draft.into_parts();
    attempt.known.new_id = Some(id.clone());
    /*
     * 成立計画の送信入口だけを使い確定した新IDへ一度PUTする
     */
    let stage = ProcessingStage::PutSource;
    attempt.checkpoint(&executor.stop, stage).await?;
    executor.client.renew_lock(&mut lease).await
        .map_err(|failure| attempt.wiki(failure, stage))?;
    attempt.checkpoint(&executor.stop, stage).await?;
    let body = executor.plan.upload_body(index).map_err(|_|
        ExecutorFailure::local(LocalKind::PlanPosition, stage))?;
    if let Err(failure) = executor.client.put_source(&id, &mut lease, body)
        .await
    {
        let failure = attempt.wiki(failure, stage);
        return Ok(PageProgress::Finished(reconcile::after_put(executor,
            index, &id, failure, completion,
            std::mem::take(&mut attempt.known)).await));
    }
    #[cfg(test)]
    tests::after_completion(&executor.stop, &mut attempt.action);
    // 返却済み正常PUTの確定完了を、後続停止で未実行へ戻さない。
    Ok(attempt.completed(completion))
}

///
/// 既存本文をreadonly比較し同一だけスキップする。
///
/// # 概要
/// 相違はID/revision/事前本文を所有したままlock後処理へ渡す。
/// lockedフラグだけで同一の読み取りを拒否しない。
///
/// # 引数
/// * `executor` - 共通所有者
/// * `index` - 計画位置
/// * `id` - 初回lookupで確定したID
/// * `attempt` - 旧情報と停止確認
///
/// # 戻り値
/// 同一スキップ/相違継続、または元失敗
///
async fn existing(executor: &Executor, index: usize, id: PageId,
    attempt: &mut Attempt) -> Result<PageProgress, ExecutorFailure>
{
    let path = &executor.plan.pages()[index].identity().path;
    let stage = ProcessingStage::ReadExisting;
    attempt.checkpoint(&executor.stop, stage).await?;
    let meta = executor.client.read_meta(&id, path).await
        .map_err(|failure| attempt.wiki(failure, stage))?;
    attempt.known.old_revision = Some(meta.revision());
    /*
     * metadataの正常な状態不一致は観測した競合として停止する
     */
    if meta.path_kind() != PathKind::Current || meta.deleted()
        || meta.path_relation() != PathRelation::MatchesExpected
    {
        return Err(ExecutorFailure::local(LocalKind::Conflict, stage));
    }
    attempt.checkpoint(&executor.stop, stage).await?;
    let body = executor.client.read_source(&id).await
        .map_err(|failure| attempt.wiki(failure, stage))?;
    attempt.checkpoint(&executor.stop, stage).await?;
    let (relation, body) = compare_planned(Arc::clone(&executor.plan), index,
        body, executor.stop.clone()).await?;
    /*
     * 同一だけを完了とし相違は所有された未完の継続状態にする
     */
    attempt.checkpoint(&executor.stop, stage).await?;
    match relation {
        BodyRelation::Identical => {
            drop(body);
            Ok(attempt.completed(Completion::SkippedIdentical))
        }
        BodyRelation::Different => Ok(PageProgress::ExistingDifferent(
            ExistingSnapshot { plan: Arc::clone(&executor.plan), index, id,
                revision: meta.revision(), body })),
    }
}

#[cfg(test)]
pub(in crate::executor) mod tests;
