/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 型付きの結果と終了状態を集約する報告境界
//!

pub(crate) mod details;
mod output;
pub(crate) mod types;

use std::collections::HashSet;

use crate::cmd_args::CollectionError;
use crate::cmd_args::resolve::Warning;

use self::types::{PageIdentity, PageOutcome, PageResult, ProcessingStage};
use self::types::ReportError;
use self::details::{FailureDetail, RejectedExecution};

///
/// 生の外部エラー文字列を持たない全体失敗。
///
#[derive(Debug)]
pub(crate) enum Failure {
    Cli,
    Collection(CollectionError),
    Input,
    Execution {
        category: types::FailureCategory,
        stage: ProcessingStage,
    },
}

///
/// 件数と最初の失敗位置。成功済みページは取り消さない。
///
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) created: usize,
    pub(crate) skipped: usize,
    pub(crate) updated: usize,
    pub(crate) recreated: usize,
    pub(crate) warnings: usize,
    pub(crate) failed: usize,
    pub(crate) not_executed: usize,
    pub(crate) first_failure: Option<usize>,
}

impl Summary {
    ///
    /// 適用済みまたは同一スキップのページ数を返す。
    ///
    /// # 戻り値
    /// 警告付き適用を含む先行成功件数
    ///
    pub(crate) fn completed(&self) -> usize {
        self.created + self.skipped + self.updated + self.recreated
    }
}

///
/// 計画成立の有無と安全な結果を所有する報告。
///
#[derive(Debug)]
pub(crate) struct Report {
    planned: bool,
    pages: Vec<PageResult>,
    warnings: Vec<Warning>,
    failures: Vec<Failure>,
    /// NotExecutedの正当な確定記録も二重記録と区別する。
    recorded: Vec<bool>,
    /// ページ開始前等の元詳細。
    global_details: Vec<FailureDetail>,
    /// 全体失敗に先行した操作警告。
    global_warnings: Vec<types::OperationWarning>,
    /// 位置/識別子/段階不整合で拒否された元結果。
    rejected: Vec<RejectedExecution>,
    /// 実行側が渡した最初の主失敗位置。
    execution_first_failure: Option<usize>,
}

impl Report {
    ///
    /// 計画未成立の報告を用意する。
    ///
    /// # 戻り値
    /// ページ処理を開始していない空の報告
    ///
    pub(crate) fn preflight() -> Self {
        Self {
            planned: false, pages: Vec::new(),
            warnings: Vec::new(), failures: Vec::new(), recorded: Vec::new(),
            global_details: Vec::new(), global_warnings: Vec::new(),
            rejected: Vec::new(), execution_first_failure: None,
        }
    }

    ///
    /// 確定計画の順序を保った未実行記録を用意する。
    ///
    /// # 概要
    /// 投影先パスの重複を拒否してから、計画順の未実行ページ群と空の全体結果を準備する。
    ///
    /// # 引数
    /// * `identities` - OP3で検証済みの順序付きページ識別情報
    ///
    /// # 戻り値
    /// 全件を未実行にした報告。重複ページは拒否する。
    ///
    pub(crate) fn planned(identities: Vec<PageIdentity>)
        -> Result<Self, ReportError>
    {
        /*
         * 計画内の投影先パス重複を確認する
         */
        let mut paths = HashSet::new();
        for identity in &identities {
            if !paths.insert(identity.path.as_str()) {
                return Err(ReportError::DuplicatePage);
            }
        }
        drop(paths);
        /*
         * 計画順に未実行記録を組み立てて報告を返す
         */
        let pages = identities.into_iter().map(|identity| PageResult {
            identity, outcome: PageOutcome::NotExecuted,
            stage: ProcessingStage::Preflight, recovery: None,
            details: details::ResultDetails::default(),
        }).collect::<Vec<_>>();
        let recorded = vec![false; pages.len()];
        Ok(Self { planned: true, pages, recorded,
            warnings: Vec::new(), failures: Vec::new(),
            global_details: Vec::new(), global_warnings: Vec::new(),
            rejected: Vec::new(), execution_first_failure: None })
    }

    ///
    /// 警告をそのまま保持する。
    ///
    /// # 引数
    /// * `warnings` - 秘密値のない起動警告
    ///
    /// # 戻り値
    /// なし。内部の警告一覧を追加する。
    ///
    pub(crate) fn add_warnings(&mut self, warnings: &[Warning]) {
        self.warnings.extend_from_slice(warnings);
    }

    ///
    /// 分類済みの全体失敗を追加する。
    ///
    /// # 引数
    /// * `failure` - 起動・入力・実行失敗の分類
    ///
    /// # 戻り値
    /// なし。内部の失敗一覧を追加する。
    ///
    pub(crate) fn fail(&mut self, failure: Failure) {
        self.failures.push(failure);
    }

    ///
    /// 同じ分類の全体実行失敗が既に記録されたか確認する。
    ///
    /// # 引数
    /// * `category` - 最終停止で重複を避ける有限分類
    ///
    /// # 戻り値
    /// 全体失敗の一致。ページ主原因や補助失敗は一致とみなさない。
    ///
    pub(crate) fn has_execution_failure(&self,
        category: types::FailureCategory) -> bool
    {
        self.failures.iter().any(|failure| matches!(failure,
            Failure::Execution { category: recorded, .. }
                if *recorded == category))
    }

    ///
    /// 元結果を一体で記録し、拒否したpayloadも別に保持する。
    ///
    /// # 引数
    /// * `index` - 成立計画内の位置
    /// * `page` - 識別子/適用結果/警告/復旧情報の所有者
    ///
    /// # 戻り値
    /// 記録成功、または元結果をrejectedへ保持した不整合
    ///
    pub(crate) fn record_page(&mut self, index: usize, page: PageResult)
        -> Result<(), ReportError>
    {
        if let Err(error) = self.validate_page(index, &page) {
            self.retain_rejected(RejectedExecution::Page { index, page });
            return Err(error);
        }
        self.pages[index] = page;
        self.recorded[index] = true;
        Ok(())
    }

    ///
    /// 記録前に識別子・二重記録・原因/最後段階の関係を検証する。
    ///
    /// # 概要
    /// cleanup/Reconcile後の最後段階を主原因段階へ強制しない。
    ///
    /// # 引数
    /// * `index` - 記録位置
    /// * `page` - まだmoveしていない元結果
    ///
    /// # 戻り値
    /// 適合または値を含まない不整合
    ///
    fn validate_page(&self, index: usize, page: &PageResult)
        -> Result<(), ReportError>
    {
        /*
         * 計画対応と復旧識別子を確認し、先行結果を上書きしない
         */
        let planned = self.pages.get(index).ok_or(ReportError::InvalidIndex)?;
        if self.recorded[index] { return Err(ReportError::AlreadyRecorded); }
        if planned.identity != page.identity {
            return Err(ReportError::InconsistentIdentity);
        }
        if page.recovery.as_ref().is_some_and(|v| v.identity != page.identity) {
            return Err(ReportError::InconsistentRecovery);
        }
        /*
         * 主原因が存在する場合だけ異なる最後段階を受け入れる
         */
        if let PageOutcome::Failed { stage, .. } = &page.outcome {
            let primary = page.details.primary.as_ref();
            if primary.is_some_and(|v| v.stage != Some(*stage))
                || (primary.is_none() && *stage != page.stage)
            { return Err(ReportError::InconsistentStage); }
        }
        if page.details.cleanup.as_ref().is_some_and(|v|
            v.stage != Some(ProcessingStage::ReleaseLock)
                || v.operation != Some(details::DetailOperation::ReleaseLock)
                || v.origin != details::FailureOrigin::Wiki)
            || (page.details.cleanup.is_some()
                && !page.details.cleanup_attempted)
            || (page.details.cleanup_attempted
                && (page.stage != ProcessingStage::ReleaseLock
                    || !matches!(page.outcome, PageOutcome::Failed { .. })))
        { return Err(ReportError::InconsistentStage); }
        /*
         * 未実行・成功の結果に矛盾する元事実も拒否payloadへ保持する
         */
        let failed = matches!(page.outcome, PageOutcome::Failed { .. });
        if !failed && (page.details.primary.is_some()
            || !page.details.auxiliary.is_empty()
            || page.details.cleanup.is_some())
        { return Err(ReportError::InconsistentStage); }
        if matches!(page.outcome, PageOutcome::NotExecuted)
            && (page.stage != ProcessingStage::Preflight
                || page.recovery.is_some() || page.details.cleanup_attempted
                || !page.details.warnings.is_empty()
                || page.details.stop.is_some())
        { return Err(ReportError::InconsistentStage); }
        let applied = matches!(page.outcome,
            PageOutcome::AppliedWithWarning { .. });
        if (applied && page.details.warnings.is_empty())
            || (page.details.stop.is_some() && !applied)
            || (matches!(page.outcome, PageOutcome::Created
                | PageOutcome::Updated | PageOutcome::Recreated
                | PageOutcome::SkippedIdentical)
                && !page.details.warnings.is_empty())
        { return Err(ReportError::InconsistentWarning); }
        Ok(())
    }

    ///
    /// 全体失敗の元詳細と先行警告を別々に保持する。
    ///
    /// # 引数
    /// * `detail` - 元の開始前/全体失敗
    /// * `warnings` - statusなしも含む先行事実
    ///
    /// # 戻り値
    /// なし。警告と実行失敗の双方を残す。
    ///
    pub(crate) fn fail_detail(&mut self, detail: FailureDetail,
        warnings: Vec<types::OperationWarning>)
    {
        self.fail(Failure::Execution { category: detail.category(),
            stage: detail.stage.unwrap_or(ProcessingStage::Preflight) });
        self.global_details.push(detail);
        self.global_warnings.extend(warnings);
    }

    ///
    /// 不整合の元結果を成功計上せず保持し実行失敗を記録する。
    ///
    /// # 引数
    /// * `rejected` - 元payloadの所有者
    ///
    /// # 戻り値
    /// なし。既存ページの結果を上書きしない。
    ///
    pub(crate) fn retain_rejected(&mut self, rejected: RejectedExecution) {
        self.rejected.push(rejected);
        self.fail(Failure::Execution {
            category: types::FailureCategory::Internal,
            stage: ProcessingStage::Preflight });
    }

    ///
    /// 実行側の主失敗位置を保持し実際の結果との不整合を拒否する。
    ///
    /// # 引数
    /// * `index` - 実行側が返した最初の主失敗
    ///
    /// # 戻り値
    /// なし。不整合でも値と全ページ結果は保持する。
    ///
    pub(crate) fn set_execution_first_failure(&mut self, index: Option<usize>) {
        self.execution_first_failure = index;
        if self.summary().first_failure != index {
            self.fail(Failure::Execution {
                category: types::FailureCategory::Internal,
                stage: ProcessingStage::Preflight });
        }
    }

    ///
    /// 順序付きの最終ページ結果を参照する。
    ///
    /// # 戻り値
    /// 本文や秘密を含まない記録列
    ///
    pub(crate) fn pages(&self) -> &[PageResult] { &self.pages }

    ///
    /// 全ページと起動警告の件数を集約する。
    ///
    /// # 概要
    /// 警告付き適用は適用操作の成功件数と警告数の両方に含める。
    ///
    /// # 戻り値
    /// 各状態の件数と最初の失敗位置
    ///
    pub(crate) fn summary(&self) -> Summary {
        let mut summary = Summary {
            warnings: self.warnings.len() + self.global_warnings.len(),
            ..Summary::default()
        };
        /*
         * 計画順の結果を数え、先行成功を維持する
         */
        for (index, page) in self.pages.iter().enumerate() {
            /*
             * 一件の操作警告は結果と独立に一度だけ数える
             */
            summary.warnings += page.details.warnings.len();
            match &page.outcome {
                /*
                 * 確定した適用操作を一つだけ加算する
                 */
                PageOutcome::Created => summary.created += 1,
                PageOutcome::SkippedIdentical => summary.skipped += 1,
                PageOutcome::Updated => summary.updated += 1,
                PageOutcome::Recreated => summary.recreated += 1,
                PageOutcome::AppliedWithWarning { action, .. } => {
                    match action {
                        types::Action::Create => summary.created += 1,
                        types::Action::Update => summary.updated += 1,
                        types::Action::Recreate => summary.recreated += 1,
                    }
                }
                /*
                 * 適用成功とは別に失敗位置と未実行を保持する
                 */
                PageOutcome::Failed { .. } => {
                    summary.failed += 1;
                    if summary.first_failure.is_none() {
                        summary.first_failure = Some(index);
                    }
                }
                PageOutcome::NotExecuted => summary.not_executed += 1,
            }
        }
        for rejected in &self.rejected {
            summary.warnings += match rejected {
                RejectedExecution::Page { page, .. } =>
                    page.details.warnings.len(),
                RejectedExecution::Unassigned(value) =>
                    value.details.warnings.len(),
            };
        }
        summary
    }

    ///
    /// 基本設計の優先順位で最終終了コードを返す。
    ///
    /// # 戻り値
    /// 正常0、実行失敗1、起動・入力エラー2、警告付き完了3
    ///
    pub(crate) fn exit_code(&self) -> i32 {
        let summary = self.summary();
        if summary.failed > 0 || self.pages.iter().any(|page|
            page.details.stop.is_some()) || self.failures.iter()
            .any(|v| matches!(v, Failure::Execution { .. }))
        {
            return 1;
        }
        if summary.not_executed > 0 { return 1; }
        if !self.failures.is_empty() { return 2; }
        if summary.warnings > 0 { return 3; }
        0
    }
}

#[cfg(test)]
mod tests;
