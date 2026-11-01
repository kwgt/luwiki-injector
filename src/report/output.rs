/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 結果/復旧事実と累積警告/独立した失敗診断を安全に表示する
//!

use std::fmt::Write;

use super::details::{FailureDetail, PathObservation, RecoveryFacts,
    RejectedExecution, ResultDetails};
use super::types::{FailureCategory, OperationWarning, PageOutcome, PageResult,
    RecoveryAdvice};
use super::{Failure, Report};

///
/// stdoutの結果とstderrの診断を分離して所有する。
///
pub(crate) struct RenderedReport {
    /// ページ結果/集計/確定復旧情報。
    pub(crate) results: String,
    /// 警告/有限分類の診断。
    pub(crate) diagnostics: String,
}

impl std::fmt::Debug for RenderedReport {
    ///
    /// 明示出力の復旧ID/pathをDebugへ再露出させない。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 出力のサイズだけの表示
    ///
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderedReport")
            .field("result_bytes", &self.results.len())
            .field("diagnostic_bytes", &self.diagnostics.len()).finish()
    }
}

// 表示の責務を子モジュールへ分け、集約処理から独立させる。
impl Report {
    ///
    /// 全結果と警告・主/補助/解除失敗を独立したテキストへ写す。
    ///
    /// # 概要
    /// 失敗結果にも先行警告を表示し、未知のIDや原因を補完しない。
    ///
    /// # 戻り値
    /// stdout/stderr用の安全な文字列
    ///
    pub(crate) fn render(&self) -> RenderedReport {
        let mut output = RenderedReport { results: String::new(),
            diagnostics: String::new() };
        let summary = self.summary();
        let plan = if self.planned { "established" } else { "not-established" };
        /*
         * 計画と集計・最初の主失敗位置を確定結果から表示する
         */
        let _ = writeln!(output.results, "result: exit={} plan={plan}",
            self.exit_code());
        let _ = writeln!(output.results, "completed={}", summary.completed());
        let _ = writeln!(output.results,
            "created={} skipped={} updated={} recreated={} warnings={} \
             failed={} not_executed={}", summary.created, summary.skipped,
            summary.updated, summary.recreated, summary.warnings,
            summary.failed, summary.not_executed);
        if let Some(index) = summary.first_failure {
            let _ = writeln!(output.results, "first_failure={}",
                self.pages[index].identity.path.as_str());
        }
        if self.execution_first_failure != summary.first_failure {
            let _ = writeln!(output.results,
                "execution_first_failure={:?}", self.execution_first_failure);
        }
        for page in &self.pages { render_page(page, &mut output); }
        /*
         * 起動警告/全体失敗と拒否payloadもログ以外の結果へ残す
         */
        for warning in &self.warnings {
            let _ = writeln!(output.diagnostics, "warning: {warning}");
        }
        for warning in &self.global_warnings {
            render_warning("global", warning, &mut output);
        }
        for failure in &self.failures {
            let _ = writeln!(output.diagnostics, "error: {}",
                failure_text(failure));
        }
        for detail in &self.global_details {
            render_detail("global", "global", detail, &mut output);
        }
        for (index, rejected) in self.rejected.iter().enumerate() {
            /*
             * 計画位置が不整合でも元payloadの全事実を表示する
             */
            let label = format!("unassigned#{index}");
            match rejected {
                RejectedExecution::Page { index, page } => {
                    let _ = writeln!(output.results,
                        "rejected_record: requested_index={index}");
                    render_page(page, &mut output);
                }
                RejectedExecution::Unassigned(value) => {
                    let _ = writeln!(output.results,
                        "unassigned_result: record={index} outcome={:?} \
                         stage={:?} observed={:?}", value.outcome, value.stage,
                        value.observed);
                    render_facts(&label, &value.facts, &mut output);
                    render_details(&label, &value.details, &mut output);
                }
            }
        }
        if summary.not_executed > 0 && summary.failed == 0 {
            let _ = writeln!(output.diagnostics,
                "error: 未実行のページが残っています");
        }
        output
    }
}

///
/// 一件の適用結果と復旧情報を計画順に表示する。
///
/// # 概要
/// 最後段階と主原因段階は別に表示し、正常結果へ手動復旧を要求しない。
///
/// # 引数
/// * `page` - 記録済みの所有結果
/// * `output` - 分離済み出力先
///
/// # 戻り値
/// なし。本文/生応答は参照しない。
///
fn render_page(page: &PageResult, output: &mut RenderedReport) {
    let path = page.identity.path.as_str();
    let (kind, entry) = page.identity.source.parts();
    let _ = writeln!(output.results,
        "page={path} source={kind:?}:{entry} outcome={:?} stage={:?}",
        page.outcome, page.stage);
    /*
     * 復旧の明示出力だけに検証済みIDと入力参照を許す
     */
    if let Some(recovery) = &page.recovery {
        render_facts(path, &recovery.facts, output);
        let _ = writeln!(output.results,
            "recovery_state: path={path} stage={:?} observed={:?}",
            recovery.stage, recovery.observed);
        if let Some(advice) = recovery.advice {
            let _ = writeln!(output.results, "manual_action: {}",
                recovery_advice(advice));
        }
    }
    /*
     * 結果がFailedでも先行警告を失わず別の診断へ表示する
     */
    render_details(path, &page.details, output);
    if let PageOutcome::Failed { category, stage } = &page.outcome {
        let _ = writeln!(output.diagnostics,
            "error: page={path} category={category:?} stage={stage:?}");
    }
}

///
/// 観測した識別情報だけを明示復旧出力へ写す。
///
/// # 概要
/// 旧新ID・削除・lockとpath照会履歴を分け、未確認は補完しない。
///
/// # 引数
/// * `label` - 検証済みpathまたは固定の未割当識別子
/// * `facts` - 未確認を補完していない事実
/// * `output` - 分離済み出力先
///
/// # 戻り値
/// なし。path履歴を現在の状態として表示しない。
///
fn render_facts(label: &str, facts: &RecoveryFacts,
    output: &mut RenderedReport)
{
    /*
     * UnknownをNoneのまま表示しold/newと削除/lockの確認を分ける
     */
    let _ = writeln!(output.results,
        "recovery: path={label} old_id={:?} revision={:?} draft_id={:?} \
         old_delete_confirmed={} old_lock_observed={:?} \
         reconcile_lock_observed={:?}",
        facts.old_id.as_ref().map(|v| v.as_str()), facts.old_revision,
        facts.draft_id.as_ref().map(|v| v.as_str()),
        facts.old_delete_confirmed, facts.old_lock_observed,
        facts.lock_observed);
    let (state, id) = match &facts.path_observed {
        PathObservation::Unknown => ("Unknown", None),
        PathObservation::Absent => ("Absent", None),
        PathObservation::Deleted => ("Deleted", None),
        PathObservation::Found(id) => ("Found", Some(id.as_str())),
    };
    let _ = writeln!(output.results,
        "path_history: path={label} state={state} id={id:?}");
}

///
/// 結果とは独立した警告と失敗のロールを表示する。
///
/// # 概要
/// cleanupの関数試行とHTTP送信状態を混同せず安全停止も表示する。
///
/// # 引数
/// * `label` - 出力対象の安全な識別子
/// * `details` - 警告/主/補助/解除/停止
/// * `output` - 分離済み出力先
///
/// # 戻り値
/// なし。警告の順序を変更しない。
///
fn render_details(label: &str, details: &ResultDetails,
    output: &mut RenderedReport)
{
    /*
     * 先行操作の順序を保ち、異なる失敗のロールを明示する
     */
    for warning in &details.warnings { render_warning(label, warning, output); }
    if let Some(primary) = &details.primary {
        render_detail(label, "primary", primary, output);
    }
    for auxiliary in &details.auxiliary {
        render_detail(label, "auxiliary", auxiliary, output);
    }
    if let Some(cleanup) = &details.cleanup {
        render_detail(label, "cleanup", cleanup, output);
    }
    if details.primary.is_some() || !details.auxiliary.is_empty()
        || details.cleanup_attempted || details.stop.is_some()
    {
        let _ = writeln!(output.results,
            "execution_facts: path={label} cleanup_attempted={} stop={:?}",
            details.cleanup_attempted, details.stop);
    }
    if let Some(reason) = &details.stop {
        let _ = writeln!(output.diagnostics,
            "error: page={label} safety_stop={reason:?}");
    }
}

///
/// statusなしも一件の操作警告として表示する。
///
/// # 引数
/// * `label` - 安全な対象識別子
/// * `warning` - 確定観測と元詳細
/// * `output` - 分離済み出力先
///
/// # 戻り値
/// なし。Disconnectedを推測しない。
///
fn render_warning(label: &str, warning: &OperationWarning,
    output: &mut RenderedReport)
{
    let _ = writeln!(output.diagnostics,
        "warning: page={label} operation={:?} response={:?} observed={:?}",
        warning.operation, warning.response, warning.observed);
    if let Some(detail) = &warning.detail {
        render_detail(label, "warning", detail, output);
    }
}

///
/// 元詳細と保管causeをロールを分けて表示する。
///
/// # 引数
/// * `label` - 安全な対象識別子
/// * `role` - 呼出元の固定ロール
/// * `detail` - 原文を持たない有限事実
/// * `output` - 分離済み出力先
///
/// # 戻り値
/// なし。IDは明示復旧出力だけに許す。
///
fn render_detail(label: &str, role: &str, detail: &FailureDetail,
    output: &mut RenderedReport)
{
    let _ = writeln!(output.diagnostics,
        "detail: page={label} role={role} facts={detail:?}");
    if let Some(id) = &detail.known_id {
        let _ = writeln!(output.results,
            "operation_fact: path={label} role={role} id={}", id.as_str());
    }
    if let Some(cause) = &detail.cause {
        render_detail(label, "stored_cause", cause, output);
    }
}

///
/// 全体失敗の有限分類を表示する。
///
/// # 引数
/// * `failure` - 原文を持たない全体失敗
///
/// # 戻り値
/// 公開可能な診断
///
fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::Cli =>
            "コマンドライン引数が不正です。--helpを確認してください".to_owned(),
        Failure::Collection(error) => error.to_string(),
        Failure::Input => "入力検証に失敗しました".to_owned(),
        Failure::Execution { category: FailureCategory::ExecutionUnavailable,
                             .. } =>
            "実行処理はまだ利用できません".to_owned(),
        Failure::Execution { category, stage } =>
            format!("category={category:?} stage={stage:?}"),
    }
}

///
/// 自動操作を指示しない既存の手動確認方針を表示する。
///
/// # 引数
/// * `advice` - callerの有限方針
///
/// # 戻り値
/// 秘密や任意の外部messageを含まない文面
///
fn recovery_advice(advice: RecoveryAdvice) -> &'static str {
    match advice {
        RecoveryAdvice::InspectBeforeRetry =>
            "サーバー状態を照合し、再実行は手動で判断してください",
        RecoveryAdvice::RecreateManually =>
            "旧ページは自動復元されません。状態確認後に手動で再作成してください",
        RecoveryAdvice::InspectDraft =>
            "判明した作成先IDの状態を確認し、手動で対応してください",
        RecoveryAdvice::ResolveConflict =>
            "他者変更を確認し、競合解消後の実行を手動で判断してください",
    }
}
