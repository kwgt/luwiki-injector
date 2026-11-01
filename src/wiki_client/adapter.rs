/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 単発失敗の安全事実を実行側へmoveするadapter
//!

use std::fmt;

use crate::report::types::{ObservedState, Operation, OperationWarning,
    ProcessingStage, ResponseCondition};

use super::types::{FailureKind, SendState, WikiFailure, WikiOperation};

///
/// 最終失敗と先行操作事実を分離して所有する実行側の失敗。
///
pub(crate) struct ExecutionFailure {
    failure: WikiFailure,
    stage: ProcessingStage,
    warnings: Vec<OperationEvidence>,
}

impl ExecutionFailure {
    ///
    /// 主失敗とcaller段階・先行事実を所有ごと引き渡す。
    ///
    /// # 戻り値
    /// 秘密を持たない元失敗、段階、発生順の先行事実
    ///
    pub(crate) fn into_parts(self)
        -> (WikiFailure, ProcessingStage, Vec<OperationEvidence>)
    {
        (self.failure, self.stage, self.warnings)
    }

    ///
    /// callerが指定した最終段階を取得する。
    ///
    /// # 戻り値
    /// 実行側の段階
    ///
    pub(crate) fn stage(&self) -> ProcessingStage { self.stage }

    ///
    /// 最終単発失敗の安全事実を参照する。
    ///
    /// # 戻り値
    /// 分類・status・確定ID・有限診断
    ///
    pub(crate) fn facts(&self) -> &WikiFailure { &self.failure }

    ///
    /// 先行操作事実を発生順に参照する。
    ///
    /// # 戻り値
    /// status未観測も含む元事実の列
    ///
    pub(crate) fn warnings(&self) -> &[OperationEvidence] { &self.warnings }
}

impl fmt::Debug for ExecutionFailure {
    ///
    /// 安全な分類と先行事実の件数だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// ID・秘密・本文を含まない表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ExecutionFailure")
            .field("stage", &self.stage).field("failure", &self.failure)
            .field("warning_count", &self.warnings.len()).finish()
    }
}

impl fmt::Display for ExecutionFailure {
    ///
    /// 安全なDebug表示を返す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 実行側失敗の安全な表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

///
/// 元の失敗事実とcallerが別途観測した状態を所有する。
///
pub(crate) struct OperationEvidence {
    failure: WikiFailure,
    observed: ObservedState,
    operation: Operation,
}

impl OperationEvidence {
    ///
    /// 元の投影操作を返す。詳細操作はfactsへ残す。
    ///
    /// # 戻り値
    /// callerが保持した有限操作
    ///
    pub(crate) fn operation(&self) -> Operation { self.operation }

    ///
    /// 詳細操作・旧確定IDを含む安全事実を参照する。
    ///
    /// # 戻り値
    /// 投影前の単発失敗
    ///
    pub(crate) fn facts(&self) -> &WikiFailure { &self.failure }

    ///
    /// callerが明示した観測状態を取得する。
    ///
    /// # 戻り値
    /// statusから推測しない状態
    ///
    pub(crate) fn observed(&self) -> ObservedState { self.observed }

    ///
    /// 確定statusだけを既存の警告型へ投影する。
    ///
    /// # 戻り値
    /// statusなしはNone。元事実の消失や警告なしを意味しない。
    /// DNS・接続確立失敗を含み得るためDisconnectedを生成しない。
    ///
    pub(crate) fn warning(&self) -> Option<OperationWarning> {
        Some(OperationWarning { operation: self.operation,
            response: ResponseCondition::Http(self.failure.status()?),
            observed: self.observed, detail: None })
    }
}

impl fmt::Debug for OperationEvidence {
    ///
    /// 安全な失敗表示とcallerの観測だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// ID・秘密・本文を含まない表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OperationEvidence")
            .field("failure", &self.failure)
            .field("observed", &self.observed).finish()
    }
}

///
/// 最終失敗・段階・先行列を複製せず実行側へ引き渡す。
///
/// # 引数
/// * `failure` - 最終単発失敗の安全事実
/// * `stage` - callerが確定した実行段階
/// * `warnings` - callerが明示した順序付き先行事実
///
/// # 戻り値
/// 実行側失敗。入力不正や最終Report categoryへは変換しない。
///
pub(crate) fn execution_failure(failure: WikiFailure, stage: ProcessingStage,
    warnings: Vec<OperationEvidence>) -> ExecutionFailure
{
    ExecutionFailure { failure, stage, warnings }
}

///
/// 観測済み単発失敗を先行操作事実として引き渡す。
///
/// # 概要
/// 呼出し側の観測を保持し、継続可否や適用有無を判断しない。
///
/// # 引数
/// * `failure` - 元の安全な失敗事実
/// * `observed` - callerが独立に確認した状態またはUnknown
///
/// # 戻り値
/// 送信済みstatus観測またはstatusなしTransportならSome。
/// Initialize・送信前・その他statusなし失敗はNone。
///
pub(crate) fn operation_evidence(failure: WikiFailure,
    observed: ObservedState) -> Option<OperationEvidence>
{
    /*
     * 送信開始と確定statusの観測範囲だけを受け付ける
     */
    if failure.sent() != SendState::Started || (failure.status().is_none()
        && failure.kind() != FailureKind::Transport)
    {
        return None;
    }
    /*
     * 元の詳細操作は失敗へ残し、既存警告への投影先だけを選ぶ
     */
    let operation = match failure.operation() {
        WikiOperation::Initialize => return None,
        WikiOperation::Lookup | WikiOperation::ReadSource
            | WikiOperation::ReadMeta => Operation::Lookup,
        WikiOperation::CreateDraft => Operation::Post,
        WikiOperation::PutSource => Operation::Put,
        WikiOperation::DeletePage => Operation::Delete,
        WikiOperation::AcquireLock | WikiOperation::RenewLock =>
            Operation::Lock,
        WikiOperation::ReleaseLock => Operation::Unlock,
    };
    Some(OperationEvidence { failure, observed, operation })
}

///
/// 次の送信workerへ実量引渡し後のI/O失敗だけを注入する。
///
/// # 戻り値
/// なし。実資格情報/製品設定/HTTP再送へ接続しない試験専用入口。
///
#[cfg(test)]
pub(crate) fn fail_upload_exit_fixture() {
    super::upload::tests::set_action(super::upload::tests::Action::ExitIo);
}
