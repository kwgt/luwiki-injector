/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実行結果を秘密・本文・leaseを持たない最終Reportへ所有変換する
//!

use crate::report::Report;
use crate::report::details::{DetailKind, DetailOperation, DiagnosticKind,
    FailureDetail, FailureOrigin, PathObservation, RecoveryFacts,
    RejectedExecution, RequestState, ResultDetails, SafeDiagnostics, SafetyStop,
    UnassignedExecution};
use crate::report::types::{Action, FailureCategory, ObservedState, Operation,
    OperationWarning, PageIdentity, PageOutcome, PageResult, ProcessingStage,
    RecoveryAdvice, RecoveryInfo, ResponseCondition};
use crate::wiki_client::{LookupResult, WikiFailure};
use crate::wiki_client::adapter::OperationEvidence;
use crate::wiki_client::types::{FailureKind, FrontMatterKind, SendState,
    WikiOperation};

use super::types::{Completion, ExecutionBatch, ExecutorFailure, KnownPageFacts,
    LocalKind, PageDetails, PageExecution, StopReason};

///
/// 元の詳細Wiki操作を閉じたReport型へ写す。
///
/// # 引数
/// * `operation` - 元の単発操作
///
/// # 戻り値
/// 同じ意味の有限操作
///
fn operation(operation: WikiOperation) -> DetailOperation {
    /*
     * 詳細操作をcoarseな表示操作へ潰さない
     */
    match operation {
        WikiOperation::Initialize => DetailOperation::Initialize,
        WikiOperation::Lookup => DetailOperation::Lookup,
        WikiOperation::ReadSource => DetailOperation::ReadSource,
        WikiOperation::ReadMeta => DetailOperation::ReadMeta,
        WikiOperation::CreateDraft => DetailOperation::CreateDraft,
        WikiOperation::AcquireLock => DetailOperation::AcquireLock,
        WikiOperation::RenewLock => DetailOperation::RenewLock,
        WikiOperation::ReleaseLock => DetailOperation::ReleaseLock,
        WikiOperation::PutSource => DetailOperation::PutSource,
        WikiOperation::DeletePage => DetailOperation::DeletePage,
    }
}

///
/// 元のWiki kindをHTTP statusとは別の分類として写す。
///
/// # 概要
/// statusの有無から原因を補完せず、元の有限分類を一対一に保持する。
///
/// # 引数
/// * `kind` - 秘密/原文を持たない元分類
///
/// # 戻り値
/// Report専用の同義の有限分類
///
fn wiki_kind(kind: FailureKind) -> DetailKind {
    /*
     * statusありのProtocol/I/Oも元kindを保持する
     */
    match kind {
        /*
         * 元の送信・受信・保管失敗の分類を維持する
         */
        FailureKind::ContextMismatch => DetailKind::ContextMismatch,
        FailureKind::Preparation => DetailKind::Preparation,
        FailureKind::Authentication => DetailKind::Authentication,
        FailureKind::Http => DetailKind::Http,
        FailureKind::Transport => DetailKind::Transport,
        FailureKind::Protocol => DetailKind::Protocol,
        FailureKind::BodyUtf8 => DetailKind::BodyUtf8,
        FailureKind::BodyReplacement => DetailKind::BodyReplacement,
        FailureKind::Io => DetailKind::Io,
        FailureKind::Worker => DetailKind::Worker,
        /*
         * 停止とlock資源の分類をstatusから推測せず維持する
         */
        FailureKind::Deadline => DetailKind::Deadline,
        FailureKind::Cancelled => DetailKind::Cancelled,
        FailureKind::LockHeader => DetailKind::LockHeader,
        FailureKind::LockExpired => DetailKind::LockExpired,
        FailureKind::LeaseUnavailable => DetailKind::LeaseUnavailable,
        FailureKind::LeaseMismatch => DetailKind::LeaseMismatch,
    }
}

///
/// 元の局所分類をWiki応答へ偽装せず写す。
///
/// # 引数
/// * `kind` - 実行側の局所分類
///
/// # 戻り値
/// Report専用の同義の分類
///
fn local_kind(kind: LocalKind) -> DetailKind {
    match kind {
        LocalKind::ContextMismatch => DetailKind::ContextMismatch,
        LocalKind::PlanPosition => DetailKind::PlanPosition,
        LocalKind::PlanOpen => DetailKind::PlanOpen,
        LocalKind::ReceivedOpen => DetailKind::ReceivedOpen,
        LocalKind::CompareIo => DetailKind::CompareIo,
        LocalKind::Worker => DetailKind::Worker,
        LocalKind::Deadline => DetailKind::Deadline,
        LocalKind::Cancelled => DetailKind::Cancelled,
        LocalKind::ResultInvariant => DetailKind::ResultInvariant,
        LocalKind::Conflict => DetailKind::Conflict,
    }
}

///
/// 元WikiFailureの安全な全フィールドをReportの所有値へ写す。
///
/// # 概要
/// 識別子だけを複製し、HTTP要求・本文・任意errorを取り込まない。
///
/// # 引数
/// * `facts` - 型付きで秘匿済みの失敗
/// * `stage` - caller段階。旧先行事実の未指定値はNone。
///
/// # 戻り値
/// 元kind/status/診断と確定IDを保持する所有値
///
fn wiki_detail(facts: &WikiFailure, stage: Option<ProcessingStage>)
    -> FailureDetail
{
    /*
     * 任意messageのない診断の分類と数値だけを移す
     */
    let diagnostics = facts.diagnostics().map(|value| SafeDiagnostics {
        kind: match value.kind {
            FrontMatterKind::Syntax => DiagnosticKind::Syntax,
            FrontMatterKind::Validation => DiagnosticKind::Validation,
            FrontMatterKind::Unknown => DiagnosticKind::Unknown,
        }, line: value.line, column: value.column,
    });
    FailureDetail { origin: FailureOrigin::Wiki,
        operation: Some(operation(facts.operation())),
        kind: wiki_kind(facts.kind()), stage,
        sent: Some(match facts.sent() {
            SendState::NotStarted => RequestState::NotStarted,
            SendState::Started => RequestState::Started,
        }), status: facts.status(), known_id: facts.known_id().cloned(),
        diagnostics, cause: None }
}

///
/// 元の先行事実をstatusなしも含め一件の警告へ写す。
///
/// # 引数
/// * `evidence` - callerが独立した観測を加えた元事実
/// * `stage` - 指定済みの場合だけのcaller段階
///
/// # 戻り値
/// Noneで事実を落とさない最終警告
///
fn warning(evidence: &OperationEvidence, stage: Option<ProcessingStage>)
    -> OperationWarning
{
    let facts = evidence.facts();
    OperationWarning { operation: evidence.operation(),
        response: facts.status().map_or(ResponseCondition::ResponseUncertain,
            ResponseCondition::Http), observed: evidence.observed(),
        detail: Some(wiki_detail(facts, stage)) }
}

///
/// 実行失敗の元詳細と先行操作事実を独立したReport値へ変換する。
///
/// # 概要
/// 保管open causeを保持し、局所障害に架空の送信状態を付けない。
///
/// # 引数
/// * `failure` - 局所またはWikiの元失敗
/// * `warnings` - 先行列を発生順に受け取る所有者
///
/// # 戻り値
/// 主原因を別失敗で置き換えない詳細
///
fn failure_detail(failure: &ExecutorFailure,
    warnings: &mut Vec<OperationWarning>) -> FailureDetail
{
    /*
     * statusなしも含め先行列を安全な警告へ写す
     */
    warnings.extend(failure.evidence().iter().map(|value|
        warning(value, None)));
    match failure {
        ExecutorFailure::Wiki(value) =>
            wiki_detail(value.facts(), Some(value.stage())),
        ExecutorFailure::Local(value) => FailureDetail {
            origin: FailureOrigin::Local, operation: None,
            kind: local_kind(value.kind()), stage: Some(value.stage()),
            sent: None, status: None, known_id: None, diagnostics: None,
            cause: value.received_open_cause().map(|cause|
                Box::new(wiki_detail(cause, Some(value.stage())))),
        },
    }
}

///
/// adapterの対象外でも保持されたWiki異常の表示操作を選ぶ。
///
/// # 引数
/// * `operation` - 元の詳細Wiki操作
///
/// # 戻り値
/// 変更/照会操作。初期化は操作警告にしない。
///
fn coarse(operation: WikiOperation) -> Option<Operation> {
    match operation {
        WikiOperation::Initialize => None,
        WikiOperation::Lookup | WikiOperation::ReadSource
            | WikiOperation::ReadMeta => Some(Operation::Lookup),
        WikiOperation::CreateDraft => Some(Operation::Post),
        WikiOperation::PutSource => Some(Operation::Put),
        WikiOperation::DeletePage => Some(Operation::Delete),
        WikiOperation::AcquireLock | WikiOperation::RenewLock =>
            Some(Operation::Lock),
        WikiOperation::ReleaseLock => Some(Operation::Unlock),
    }
}

///
/// 元結果の通常完了と警告付き適用を排他的に表す。
///
/// # 概要
/// 確定した操作を保持し、警告付きskipだけを内部不整合へ分類する。
///
/// # 引数
/// * `completion` - 元の確定操作
/// * `warned` - 元結果が警告付き適用だったか
///
/// # 戻り値
/// 最終ページ結果。警告付きskipは内部不整合として停止する。
///
fn completion(completion: Completion, warned: bool) -> PageOutcome {
    /*
     * 通常完了は元の確定操作を保持する
     */
    if !warned {
        return match completion {
            Completion::Created => PageOutcome::Created,
            Completion::Updated => PageOutcome::Updated,
            Completion::Recreated => PageOutcome::Recreated,
            Completion::SkippedIdentical => PageOutcome::SkippedIdentical,
        };
    }
    /*
     * 警告付き適用だけを適用操作へ変換する
     */
    let action = match completion {
        Completion::Created => Action::Create,
        Completion::Updated => Action::Update,
        Completion::Recreated => Action::Recreate,
        Completion::SkippedIdentical => return PageOutcome::Failed {
            category: FailureCategory::Internal,
            stage: ProcessingStage::Preflight,
        },
    };
    PageOutcome::AppliedWithWarning { action }
}

///
/// 実行側の一件を計画識別子と独立した最終payloadへ写す。
///
/// # 概要
/// 拒否された元payloadにも同じ変換を使い情報を捨てない。
///
/// # 引数
/// * `known` - 旧新ID等の確定情報
/// * `execution` - 元の排他的結果
/// * `cleanup` - 独立した解除失敗
/// * `attempted` - 解除関数の呼出確認
/// * `source` - 元の観測/警告/補助失敗/停止
/// * `stage` - 最後の試行段階
///
/// # 戻り値
/// 本文/lease/元HTTP型から独立した所有済みpayload
///
fn payload(known: KnownPageFacts, execution: PageExecution,
    cleanup: Option<crate::wiki_client::adapter::ExecutionFailure>,
    attempted: bool, source: PageDetails, stage: ProcessingStage)
    -> UnassignedExecution
{
    let (old_id, old_revision, draft_id, deleted, locked) = known.into_parts();
    let observed = match &execution {
        PageExecution::Completed(Completion::SkippedIdentical) =>
            ObservedState::Present,
        PageExecution::Completed(_) => ObservedState::Applied,
        _ => source.observed,
    };
    let mut details = ResultDetails { cleanup_attempted: attempted,
        stop: source.stop.map(|reason| match reason {
            StopReason::RecreationUncertain => SafetyStop::RecreationUncertain,
        }), ..ResultDetails::default() };
    /*
     * 先行警告とadapter対象外の元事実を発生順に移す
     */
    details.warnings.extend(source.warnings.iter().map(|value|
        warning(&value.evidence, value.stage)));
    for retained in &source.retained {
        let detail = failure_detail(retained, &mut details.warnings);
        if let ExecutorFailure::Wiki(value) = retained {
            if let Some(operation) = coarse(value.facts().operation()) {
                details.warnings.push(OperationWarning { operation,
                    response: detail.status.map_or(
                        ResponseCondition::ResponseUncertain,
                        ResponseCondition::Http), observed: source.observed,
                    detail: Some(detail) });
                continue;
            }
        }
        details.auxiliary.push(detail);
    }
    /*
     * 主/再照合/解除を独立保持し、ページ適用結果を一度だけ決める
     */
    let outcome = match execution {
        /*
         * 排他的な完了結果と未実行を写す
         */
        PageExecution::Completed(value) => completion(value, false),
        PageExecution::AppliedWithWarning(value) => completion(value, true),
        PageExecution::NotExecuted => PageOutcome::NotExecuted,
        PageExecution::Failed(failure) => {
            /*
             * 主失敗を保持し、削除確認時の元DELETE警告も残す
             */
            let detail = failure_detail(&failure, &mut details.warnings);
            // 削除を確認した停止でも異常を警告として表示し非0を保つ。
            if observed == ObservedState::Deleted {
                if let ExecutorFailure::Wiki(value) = &failure {
                    if value.facts().operation() == WikiOperation::DeletePage {
                        let facts = value.facts();
                        details.warnings.push(OperationWarning {
                            operation: Operation::Delete,
                            response: facts.status().map_or(
                                ResponseCondition::ResponseUncertain,
                                ResponseCondition::Http), observed,
                            detail: Some(wiki_detail(facts,
                                Some(value.stage()))) });
                    }
                }
            }
            /*
             * 最後段階とは別に元の主原因段階を保存する
             */
            let outcome = PageOutcome::Failed { category: detail.category(),
                stage: failure.stage() };
            details.primary = Some(detail);
            outcome
        }
    };
    for auxiliary in &source.auxiliary {
        details.auxiliary.push(failure_detail(auxiliary,
            &mut details.warnings));
    }
    if let Some(cleanup) = cleanup {
        details.cleanup = Some(failure_detail(&ExecutorFailure::Wiki(cleanup),
            &mut details.warnings));
    }
    let facts = RecoveryFacts { old_id, old_revision, draft_id,
        old_delete_confirmed: deleted, old_lock_observed: locked,
        lock_observed: source.lock_observed,
        path_observed: match source.path_observed {
            None => PathObservation::Unknown,
            Some(LookupResult::NotFound) => PathObservation::Absent,
            Some(LookupResult::Deleted) => PathObservation::Deleted,
            Some(LookupResult::Found(id)) => PathObservation::Found(id),
        } };
    UnassignedExecution { outcome, stage, facts, observed, details }
}

///
/// 確定状態から自動操作を指示しない手動確認方針を選ぶ。
///
/// # 概要
/// 失敗/安全停止の観測と確定IDから、状態確認の案内だけを選ぶ。
///
/// # 引数
/// * `value` - 所有変換後の一件
///
/// # 戻り値
/// 失敗/安全停止時だけの方針
///
fn advice(value: &UnassignedExecution) -> Option<RecoveryAdvice> {
    /*
     * 正常完了には手動復旧を要求しない
     */
    if !matches!(value.outcome, PageOutcome::Failed { .. })
        && value.details.stop.is_none()
    { return None; }
    if matches!(value.outcome, PageOutcome::Failed {
        category: FailureCategory::Conflict, .. })
    { return Some(RecoveryAdvice::ResolveConflict); }
    /*
     * 確認済みの状態だけから有限な手動確認方針を選ぶ
     */
    if value.details.stop.is_some() && value.observed == ObservedState::Applied
    {
        return Some(RecoveryAdvice::InspectBeforeRetry);
    }
    if value.facts.draft_id.is_some() {
        return Some(RecoveryAdvice::InspectDraft);
    }
    if value.facts.old_delete_confirmed {
        return Some(RecoveryAdvice::RecreateManually);
    }
    Some(RecoveryAdvice::InspectBeforeRetry)
}

///
/// 元payloadを成立計画の識別子へ結び付ける。
///
/// # 概要
/// 未実行に混入した観測も破棄せず、記録側が不整合として保持できる。
///
/// # 引数
/// * `identity` - 成立計画の識別子
/// * `value` - 変換済みの元payload
///
/// # 戻り値
/// 元事実を保持した記録要求
///
fn assigned(identity: PageIdentity, value: UnassignedExecution) -> PageResult {
    let advice = advice(&value);
    let cause_stage = match &value.outcome {
        PageOutcome::Failed { stage, .. } => *stage,
        _ => value.stage,
    };
    /*
     * 未実行でも異常な元事実を消さず、拒否後の表示へ引き継ぐ
     */
    let recovery = if matches!(value.outcome, PageOutcome::NotExecuted)
        && value.facts.is_empty() && value.observed == ObservedState::Unknown
    {
        None
    } else { Some(RecoveryInfo { identity: identity.clone(),
        facts: value.facts, stage: cause_stage,
        observed: value.observed, advice }) };
    PageResult { identity, outcome: value.outcome, stage: value.stage,
        recovery, details: value.details }
}

///
/// 開始前の元失敗を成立計画の報告へ追加する。
///
/// # 引数
/// * `report` - 既存の成立計画・先行結果を保持する受け手
/// * `failure` - HTTP/局所の有限事実と先行警告
///
/// # 戻り値
/// なし。本文/秘密を渡さず元分類と警告を保持する。
///
pub(crate) fn record_failure(report: &mut Report, failure: ExecutorFailure) {
    let mut warnings = Vec::new();
    let detail = failure_detail(&failure, &mut warnings);
    report.fail_detail(detail, warnings);
}

///
/// 先行結果/拒否payloadも保持して実行結果を最終Reportへ変換する。
///
/// # 概要
/// HTTPを追加せず、開始前停止と安全停止を成功完了へ補完しない。
///
/// # 引数
/// * `batch` - 計画順の実行結果と元失敗の所有者
///
/// # 戻り値
/// 元事実と累積警告を持つ最終報告。CLI接続自体は呼出側の責務。
///
pub(crate) fn into_report(batch: ExecutionBatch) -> Report {
    let (pages, first, global, rejected) = batch.into_parts();
    let mut converted = Vec::with_capacity(pages.len());
    /*
     * 各ページの復旧事実と主/補助/解除情報を一体で変換する
     */
    for page in pages {
        let (identity, known, stage, execution, cleanup, attempted, details) =
            page.into_parts();
        let value = payload(known, execution, cleanup, attempted, details,
            stage);
        converted.push(assigned(identity, value));
    }
    /*
     * 計画不整合時も変換済みの元ページを破棄しない
     */
    let identities = converted.iter().map(|page| page.identity.clone())
        .collect();
    let mut report = match Report::planned(identities) {
        Ok(report) => report,
        Err(_) => Report::preflight(),
    };
    for (index, page) in converted.into_iter().enumerate() {
        // 拒否時もrecord_page自身が元payloadを保持する。
        let _ = report.record_page(index, page);
    }
    report.set_execution_first_failure(first);
    if let Some(global) = global {
        record_failure(&mut report, global);
    }
    /*
     * 実行側で拒否された元payloadも仮のページへ割り当てず保持する
     */
    for (known, execution, cleanup, attempted, details) in rejected {
        let stage = if attempted { ProcessingStage::ReleaseLock }
            else { details.last_stage.unwrap_or(match &execution {
                PageExecution::Failed(failure) => failure.stage(),
                _ => ProcessingStage::Finished,
            }) };
        report.retain_rejected(RejectedExecution::Unassigned(payload(known,
            execution, cleanup, attempted, details, stage)));
    }
    report
}

#[cfg(test)]
mod tests;
