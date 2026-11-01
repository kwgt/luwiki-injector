/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 相違既存の通常更新/再作成と確実な既存lockだけの明示解放
//!

use crate::cmd_args::resolve::Mode;
use crate::report::types::{PageId, ProcessingStage};
use crate::source::{CancelOnDrop, StopContext};
use crate::wiki_client::{LockLease, LookupResult, Origin, ReceivedBody,
    WikiFailure};
use crate::wiki_client::adapter::execution_failure;
use crate::wiki_client::types::{PathKind, PathRelation, SendState};

use super::{Executor, PageProcessor, reconcile};
use super::compare::{BodyRelation, compare_received};
use super::prepare::{ExistingSnapshot, PageProgress, create_page, prepare};
use super::types::{Completion, ExecutorFailure, KnownPageFacts, LocalKind,
    PageStep};

///
/// 初回分岐と相違通常処理を一件ずつ接続する私有processor。
///
pub(super) struct NormalProcessor;

impl PageProcessor for NormalProcessor {
    ///
    /// 初回の確定結果または相違既存の実通常処理を返す。
    ///
    /// # 引数
    /// * `index` - 次に一度だけ処理する計画位置
    /// * `executor` - 同じ計画/client/停止
    ///
    /// # 戻り値
    /// 実処理の結果。相違を成功stubへしない。
    ///
    async fn process(&mut self, index: usize, executor: &Executor) -> PageStep {
        #[cfg(test)]
        let action = tests::take_action();
        match prepare(executor, index).await {
            PageProgress::Finished(step) => step,
            PageProgress::ExistingDifferent(snapshot) => {
                #[cfg(test)]
                let step = change(executor, index, snapshot, action).await;
                #[cfg(not(test))]
                let step = change(executor, index, snapshot).await;
                step
            }
        }
    }
}

///
/// 取得由来の旧leaseと非draft/保持の独立した根拠。
///
struct HeldExisting {
    /// 同じclient/旧IDへ結び付いた秘匿資源。
    lease: LockLease,
    /// lock後metaがmandatoryrevisionを確認した根拠。
    non_draft: bool,
    /// 正常取得/延長とremote lock観測が矛盾していない。
    holding: bool,
}

impl HeldExisting {
    ///
    /// 補完GETや期限猶予なしで解放可能性を確認する。
    ///
    /// # 引数
    /// * `id` - 同じ旧ID
    /// * `stop` - 同じ境界
    ///
    /// # 戻り値
    /// 現在の根拠で一度だけ明示解放を検討できるか
    ///
    fn releasable(&self, id: &PageId, stop: &StopContext) -> bool {
        self.non_draft && self.holding
            && self.lease.origin() == Origin::Acquired
            && self.lease.id() == id && self.lease.usable()
            && stop.checkpoint().is_ok()
    }
}

///
/// 一件だけの旧情報/本文/leaseを所有する通常処理状態。
///
struct Attempt {
    /// 初回lookupで確定した旧ID。
    id: PageId,
    /// lock後の新本文と比較する独立事前本文。
    before: Option<ReceivedBody>,
    /// 旧ID/revision・削除確認/lock観測を残す受け手。
    known: KnownPageFacts,
    /// 旧ページの資源。新draft leaseとは混ぜない。
    held: Option<HeldExisting>,
    /// 次の一件だけにmoveされた同期操作。
    #[cfg(test)]
    action: Option<tests::Action>,
}

impl Attempt {
    ///
    /// snapshotの旧情報と事前本文を一件へ移す。
    ///
    /// # 引数
    /// * `id` - 旧ID
    /// * `revision` - 事前revision
    /// * `before` - 独立readonly本文
    /// * `action` - 試験だけの所有された同期操作
    ///
    /// # 戻り値
    /// まだlockを持たない状態
    ///
    fn new(id: PageId, revision: u64, before: ReceivedBody,
        #[cfg(test)] action: Option<tests::Action>) -> Self
    {
        let known = KnownPageFacts { old_id: Some(id.clone()),
            old_revision: Some(revision), ..KnownPageFacts::default() };
        Self { id, before: Some(before), known, held: None,
            #[cfg(test)] action,
        }
    }

    ///
    /// 次の明示操作前に同じ絶対期限/取消を確認する。
    ///
    /// # 引数
    /// * `stop` - 同じcontext
    /// * `stage` - 次のcaller段階
    ///
    /// # 戻り値
    /// 継続可能または元停止kindの局所失敗
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
    /// 元単発失敗をそのままcaller段階へ結び付ける。
    ///
    /// # 引数
    /// * `failure` - 元kind/status/既知ID
    /// * `stage` - caller段階
    ///
    /// # 戻り値
    /// 再照合/警告継続を開始しない失敗
    ///
    fn wiki(failure: WikiFailure, stage: ProcessingStage) -> ExecutorFailure {
        ExecutorFailure::Wiki(execution_failure(failure, stage, Vec::new()))
    }

    ///
    /// 次のprotected操作前に一回だけ延長し有効new tokenへ切り替える。
    ///
    /// # 概要
    /// 送信後失敗は保持不明、送信前は元usable状態を尊重する。
    ///
    /// # 引数
    /// * `executor` - 同じclient/停止
    /// * `stage` - Recheck/PutSource/DeleteOld
    ///
    /// # 戻り値
    /// 成功new lease、または旧tokenで変更へ進まない元失敗
    ///
    async fn renew(&mut self, executor: &Executor, stage: ProcessingStage)
        -> Result<(), ExecutorFailure>
    {
        self.checkpoint(&executor.stop, stage).await?;
        let held = self.held.as_mut().ok_or_else(||
            ExecutorFailure::local(LocalKind::ResultInvariant, stage))?;
        /*
         * clientの旧lease無効化/新token検査を独自に巻き戻さない
         */
        match executor.client.renew_lock(&mut held.lease).await {
            Ok(()) => Ok(()),
            Err(failure) => {
                if failure.sent() == SendState::Started {
                    held.holding = false;
                }
                Err(Self::wiki(failure, stage))
            }
        }
    }

    ///
    /// 主失敗を保ち根拠が揃う既存資源だけ一度解放する。
    ///
    /// # 概要
    /// BeforeSend失敗も試行として保持し、HTTP実送信とは区別する。
    ///
    /// # 引数
    /// * `executor` - 同じclient/停止
    /// * `failure` - 元の主原因
    ///
    /// # 戻り値
    /// 主原因/解除失敗/試行を別所有する結果
    ///
    async fn failed(&mut self, executor: &Executor, failure: ExecutorFailure)
        -> PageStep
    {
        let step = PageStep::failed(failure, std::mem::take(&mut self.known),
            None);
        /*
         * 非draft/保持根拠と有効境界がない場合は解放を開始しない
         */
        if !self.held.as_ref().is_some_and(|held|
            held.releasable(&self.id, &executor.stop))
        { return step; }
        if let Err(failure) = self.checkpoint(&executor.stop,
            ProcessingStage::ReleaseLock).await
        { return step.with_auxiliary(failure); }
        let Some(held) = self.held.as_mut() else { return step; };
        if !held.releasable(&self.id, &executor.stop) { return step; }
        /*
         * client関数呼出を数え、成功でも主原因は成功へ戻さない
         */
        let result = executor.client.release_lock(&self.id, &mut held.lease)
            .await.map(|_| ()).map_err(|failure|
                execution_failure(failure, ProcessingStage::ReleaseLock,
                    Vec::new()));
        step.with_cleanup(result)
    }
}

///
/// snapshot結合を確認し相違一件だけを通常処理する。
///
/// # 概要
/// 元失敗返却ではguard解除し実dropだけ共有取消を通知する。
///
/// # 引数
/// * `executor` - 同じ所有者
/// * `index` - 同じ計画位置
/// * `snapshot` - private生成口の事前情報
/// * `action` - 試験専用同期操作
///
/// # 戻り値
/// 正常Updated/Recreatedまたは主/解除失敗
///
async fn change(executor: &Executor, index: usize, snapshot: ExistingSnapshot,
    #[cfg(test)] action: Option<tests::Action>) -> PageStep
{
    let mut guard = CancelOnDrop::new(executor.stop.clone());
    let (id, revision, body) = match snapshot.into_parts_for(&executor.plan,
        index)
    {
        Ok(parts) => parts,
        Err(failure) => {
            guard.disarm();
            return PageStep::failed(failure, KnownPageFacts::default(), None);
        }
    };
    #[cfg(test)]
    let mut attempt = Attempt::new(id, revision, body, action);
    #[cfg(not(test))]
    let mut attempt = Attempt::new(id, revision, body);
    let result = apply(executor, index, &mut attempt).await;
    /*
     * DELETE確定後だけ旧情報を新draftの独立した作成へ引き渡す
     */
    let step = match result {
        /*
         * 正常変更の完了種別と新旧所有情報を確定する
         */
        Ok(true) => create_page(executor, index,
            std::mem::take(&mut attempt.known), Completion::Recreated).await,
        Ok(false) => {
            #[cfg(test)]
            tests::after_completion(&executor.stop, &mut attempt.action);
            PageStep::completed(Completion::Updated,
                std::mem::take(&mut attempt.known))
        }
        /*
         * 元異常を単回照合または保持確実な既存lockの解放へ渡す
         */
        Err(failure) if reconcile::eligible_delete(&failure) => {
            recreate_after_failure(executor, index, &mut attempt, failure)
                .await
        }
        Err(failure) if reconcile::eligible(&failure) => {
            reconcile::after_put(executor, index, &attempt.id, failure,
                Completion::Updated, std::mem::take(&mut attempt.known))
                .await
        }
        Err(failure) => attempt.failed(executor, failure).await,
    };
    guard.disarm();
    step
}

///
/// 不確定DELETEを照合し完全500の削除確定時だけ再作成する。
///
/// # 概要
/// 先行警告を新POST/PUTの失敗・停止にもmoveして残す。
///
/// # 引数
/// * `executor` - 同じ成立計画/client/context
/// * `index` - 同じ計画位置
/// * `attempt` - 旧情報と資源を所有する一件
/// * `failure` - 元DELETE失敗
///
/// # 戻り値
/// 復旧情報を持つ停止、または警告付き再作成結果
///
async fn recreate_after_failure(executor: &Executor, index: usize,
    attempt: &mut Attempt, failure: ExecutorFailure) -> PageStep
{
    let known = std::mem::take(&mut attempt.known);
    /*
     * 一度だけ照合し不確定leaseの再使用や補完解除をしない
     */
    match reconcile::after_delete(executor, index, &attempt.id, failure,
        known).await
    {
        reconcile::DeleteProgress::Finished(step) => step,
        reconcile::DeleteProgress::Recreate(known, details) => {
            /*
             * 新POST前の共有停止でも先行DELETEの警告を失わない
             */
            if let Err(failure) = attempt.checkpoint(&executor.stop,
                ProcessingStage::CreateDraft).await
            {
                return PageStep::failed(failure, known, None)
                    .with_prior_details(details);
            }
            create_page(executor, index, known, Completion::Recreated)
                .await.with_prior_details(details)
        }
    }
}

///
/// acquire・meta先行再照合・A延長とPUT/DELETEを一度ずつ要求する。
///
/// # 概要
/// 単発失敗は呼出側が再照合し、同じ変更要求を再送しない。
///
/// # 引数
/// * `executor` - 同一client/計画/停止
/// * `index` - 検査済み位置
/// * `attempt` - 旧本文/lease/判明情報の所有者
///
/// # 戻り値
/// trueは正常DELETE確定、falseは正常PUT確定、または元失敗
///
async fn apply(executor: &Executor, index: usize, attempt: &mut Attempt)
    -> Result<bool, ExecutorFailure>
{
    let stage = ProcessingStage::AcquireLock;
    attempt.checkpoint(&executor.stop, stage).await?;
    let lease = executor.client.acquire_lock(&attempt.id).await
        .map_err(|failure| Attempt::wiki(failure, stage))?;
    attempt.held = Some(HeldExisting { lease, non_draft: false,
        holding: true });
    attempt.renew(executor, ProcessingStage::Recheck).await?;
    /*
     * 失敗後の補完GETなしで非draft/lock観測を先に得る
     */
    recheck(executor, index, attempt).await?;
    let stage = match &executor.mode {
        Mode::Update => ProcessingStage::PutSource,
        Mode::Overwrite => ProcessingStage::DeleteOld,
        Mode::Error => return Err(ExecutorFailure::local(
            LocalKind::ResultInvariant, ProcessingStage::Preflight)),
    };
    attempt.renew(executor, stage).await?;
    attempt.checkpoint(&executor.stop, stage).await?;
    let held = attempt.held.as_mut().ok_or_else(||
        ExecutorFailure::local(LocalKind::ResultInvariant, stage))?;
    /*
     * 同じ有効資源だけで更新または旧削除を実行する
     */
    if executor.mode == Mode::Update {
        let body = executor.plan.upload_body(index).map_err(|_|
            ExecutorFailure::local(LocalKind::PlanPosition, stage))?;
        executor.client.put_source(&attempt.id, &mut held.lease, body).await
            .map_err(|failure| Attempt::wiki(failure, stage))?;
        Ok(false)
    } else {
        executor.client.delete_page(&attempt.id, &mut held.lease).await
            .map_err(|failure| Attempt::wiki(failure, stage))?;
        attempt.known.old_delete_confirmed = true;
        attempt.checkpoint(&executor.stop, ProcessingStage::CreateDraft)
            .await?;
        Ok(true)
    }
}

///
/// 旧meta/ID/path/事前bytesを確認し入力一致で競合を隠さない。
///
/// # 概要
/// mandatoryrevision成功と保持観測を分けて旧情報を保つ。
///
/// # 引数
/// * `executor` - 同じclient/停止
/// * `index` - 同じ位置
/// * `attempt` - 独立事前本文と旧lease
///
/// # 戻り値
/// 相違なし、または元失敗/観測Conflict
///
async fn recheck(executor: &Executor, index: usize, attempt: &mut Attempt)
    -> Result<(), ExecutorFailure>
{
    let stage = ProcessingStage::Recheck;
    let conflict = || ExecutorFailure::local(LocalKind::Conflict, stage);
    attempt.checkpoint(&executor.stop, stage).await?;
    let path = &executor.plan.pages()[index].identity().path;
    let meta = executor.client.read_meta(&attempt.id, path).await
        .map_err(|failure| Attempt::wiki(failure, stage))?;
    attempt.known.old_lock_observed = Some(meta.locked());
    /*
     * 固定APIのmandatoryrevision成功が非draft根拠でcurrentだけではない
     */
    let held = attempt.held.as_mut().ok_or_else(||
        ExecutorFailure::local(LocalKind::ResultInvariant, stage))?;
    held.non_draft = true;
    if !meta.locked() {
        held.holding = false;
        return Err(conflict());
    }
    if meta.path_kind() != PathKind::Current || meta.deleted()
        || meta.path_relation() != PathRelation::MatchesExpected
    { return Err(conflict()); }
    /*
     * 同じpathの有効IDを旧IDへ照合し他者の再作成を見逃さない
     */
    attempt.checkpoint(&executor.stop, stage).await?;
    let found = executor.client.lookup(path).await
        .map_err(|failure| Attempt::wiki(failure, stage))?;
    if !matches!(found, LookupResult::Found(id) if id == attempt.id) {
        return Err(conflict());
    }
    /*
     * 再取得本文と独立事前本文をbytesで比較し入力一致で補完しない
     */
    attempt.checkpoint(&executor.stop, stage).await?;
    let after = executor.client.read_source(&attempt.id).await
        .map_err(|failure| Attempt::wiki(failure, stage))?;
    attempt.checkpoint(&executor.stop, stage).await?;
    let before = attempt.before.take().ok_or_else(||
        ExecutorFailure::local(LocalKind::ResultInvariant, stage))?;
    let (relation, before, after) = compare_received(before, after,
        executor.stop.clone()).await?;
    attempt.before = Some(before);
    drop(after);
    attempt.checkpoint(&executor.stop, stage).await?;
    if relation == BodyRelation::Different { return Err(conflict()); }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests;
