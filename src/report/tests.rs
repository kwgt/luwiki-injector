/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 合成結果で報告・終了コード・復旧情報の境界を検証する
//!

use crate::cmd_args::CollectionError;
use crate::cmd_args::resolve::{ValidationError, Warning};
use crate::secret::Secret;

use super::details::{RecoveryFacts, ResultDetails};
use super::types::*;
use super::{Failure, Report};

// 合成テスト専用の記録補助を本体implから分離する。
impl Report {
    ///
    /// 既存の警告なし合成結果を新しい一体記録へ渡す。
    ///
    /// # 引数
    /// * `index` - 合成計画の位置
    /// * `outcome` - 合成結果
    /// * `stage` - 最後段階
    /// * `recovery` - 合成の復旧事実
    ///
    /// # 戻り値
    /// 一体記録の検証結果
    ///
    fn record(&mut self, index: usize, outcome: PageOutcome,
        stage: ProcessingStage, recovery: Option<RecoveryInfo>)
        -> Result<(), ReportError>
    {
        self.record_with_warnings(index, outcome, stage, recovery, Vec::new())
    }

    ///
    /// 合成結果と独立警告を製品の記録入口へ通す。
    ///
    /// # 引数
    /// * `index` - 合成計画の位置
    /// * `outcome` - 合成結果
    /// * `stage` - 最後段階
    /// * `recovery` - 合成復旧情報
    /// * `warnings` - 結果と独立した合成警告
    ///
    /// # 戻り値
    /// 二重記録や関連不整合も含む製品の検証結果
    ///
    fn record_with_warnings(&mut self, index: usize, outcome: PageOutcome,
        stage: ProcessingStage, recovery: Option<RecoveryInfo>,
        warnings: Vec<OperationWarning>) -> Result<(), ReportError>
    {
        let identity = self.pages.get(index).ok_or(ReportError::InvalidIndex)?
            .identity.clone();
        self.record_page(index, PageResult { identity, outcome, stage, recovery,
            details: ResultDetails { warnings, ..ResultDetails::default() } })
    }
}

///
/// 合成ページの安全な識別情報を用意する。
///
/// # 引数
/// * `index` - 合成ページの番号
///
/// # 戻り値
/// 本文を持たないページ識別情報
///
fn identity(index: usize) -> PageIdentity {
    PageIdentity {
        path: PagePath::new(&format!("/page{index}")).expect("ページパス"),
        source: SourceReference::new(SourceKind::Zip,
            &format!("root/page{index}.md")).expect("入力参照"),
    }
}

///
/// 識別子の不正値が報告へ入らないことを確認する。
///
/// # 概要
/// 入力参照・Wikiパス・ページIDの不正値を拒否し、検証済みULIDの保持を確認する。
///
/// # 注記
/// URL、制御文字、破損文字と不正IDを拒否し、実形式のIDを保持する。
///
/// # 戻り値
/// なし。安全な識別子の許容・不正値の拒否が異なる場合はassertで失敗する。
///
#[test]
fn identifiers_reject_urls_bodies_and_invalid_ids() {
    /*
     * 入力参照へのURL・危険名・秘密値混入を拒否する
     */
    for value in [
        "https://user:secret@host/page", "../page.md", "/root/page.md",
        "Authorization: secret\n", "page\u{fffd}.md", "a\\b.md",
    ] {
        assert_eq!(SourceReference::new(SourceKind::GitHub, value).err(),
            Some(ReportError::InvalidIdentity));
    }
    /*
     * 不正なWiki絶対パスを拒否する
     */
    for value in ["relative", "/a//b", "/a/../b", "/body\ntext"] {
        assert!(PagePath::new(value).is_err());
    }
    /*
     * ページIDの不正値を拒否して検証済みULIDを保持する
     */
    assert!(PageId::new("unique-lock-token").is_err());
    assert!(PageId::new("ZZZZZZZZZZZZZZZZZZZZZZZZZZ").is_err());
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    assert_eq!(PageId::new(id).expect("ULID").as_str(), id);
}

///
/// 七つのページ結果と先行成功・未実行の集約を確認する。
///
/// # 概要
/// 全結果を計画順に用意し、警告付き適用を成功と警告の両方へ数える。
///
/// # 注記
/// 合成結果を記録し、最初の失敗位置と全件の表示を照合する。
///
/// # 戻り値
/// なし。結果件数・先行成功・最初の失敗位置・表示が異なればassertで失敗する。
///
#[test]
fn aggregates_all_outcomes_without_rolling_back_prior_success() {
    let mut report = Report::planned((0..7).map(identity).collect())
        .expect("合成計画");
    let outcomes = [
        PageOutcome::Created, PageOutcome::SkippedIdentical,
        PageOutcome::Updated, PageOutcome::Recreated,
        PageOutcome::AppliedWithWarning { action: Action::Recreate },
        PageOutcome::Failed {
            category: FailureCategory::Conflict,
            stage: ProcessingStage::Recheck,
        },
    ];
    /*
     * 最後のページは失敗による未実行のまま保持する
     */
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let stage = if index == 5 {
            ProcessingStage::Recheck
        } else { ProcessingStage::Finished };
        let warnings = if index == 4 { vec![OperationWarning {
            operation: Operation::Delete,
            response: ResponseCondition::Http(500),
            observed: ObservedState::Deleted, detail: None,
        }] } else { Vec::new() };
        report.record_with_warnings(index, outcome, stage, None, warnings)
            .expect("結果記録");
    }
    let summary = report.summary();
    assert_eq!(summary.created, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.updated, 1);
    assert_eq!(summary.recreated, 2);
    assert_eq!(summary.warnings, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.not_executed, 1);
    assert_eq!(summary.first_failure, Some(5));
    assert_eq!(summary.completed(), 5);
    assert_eq!(report.exit_code(), 1);
    let rendered = report.render();
    assert!(rendered.results.contains("first_failure=/page5"));
    assert!(rendered.results.contains("outcome=NotExecuted"));
    assert!(rendered.diagnostics.contains("response=Http(500)"));
}

///
/// 終了コードの0/1/2/3と優先順位を確認する。
///
/// # 概要
/// 警告・入力エラー・実行失敗を累積し、上位の状態を維持する。
///
/// # 注記
/// 合成報告へ順に状態を追加し、処理時間切れも実行失敗にする。
///
/// # 戻り値
/// なし。終了コードの優先順位や警告保持が異なる場合はassertで失敗する。
///
#[test]
fn exit_codes_follow_failure_priority() {
    let mut report = Report::preflight();
    assert_eq!(report.exit_code(), 0);
    report.add_warnings(&[Warning::CliSecret]);
    assert_eq!(report.exit_code(), 3);
    report.fail(Failure::Input);
    assert_eq!(report.exit_code(), 2);
    report.fail(Failure::Execution {
        category: FailureCategory::Deadline,
        stage: ProcessingStage::Preflight,
    });
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.summary().warnings, 1);
    assert!(!report.render().results.contains("plan=established"));
}

///
/// 警告付き適用と計画成立後の未実行を区別する。
///
/// # 概要
/// 警告付き適用を記録した合成報告と未実行計画を比較し、入力失敗の終了状態も確認する。
///
/// # 注記
/// 同じ計画から、確定適用・停止・入力失敗の三状態を作る。
///
/// # 戻り値
/// なし。警告付き適用・未実行・入力失敗の集約が異なる場合はassertで失敗する。
///
#[test]
fn applied_warning_and_incomplete_plan_are_distinct() {
    /*
     * 警告付き適用の合成結果と終了状態を照合する
     */
    let mut applied = Report::planned(vec![identity(0)]).expect("計画");
    applied.record_with_warnings(0, PageOutcome::AppliedWithWarning {
        action: Action::Update,
    }, ProcessingStage::Finished, None, vec![OperationWarning {
        operation: Operation::Put,
        response: ResponseCondition::ResponseUncertain,
        observed: ObservedState::Applied, detail: None,
    }]).expect("警告付き適用");
    assert_eq!(applied.exit_code(), 3);
    assert_eq!(applied.summary().updated, 1);
    /*
     * 未実行計画と入力失敗の終了状態を照合する
     */
    let mut unfinished = Report::planned(vec![identity(0)]).expect("計画");
    assert_eq!(unfinished.exit_code(), 1);
    unfinished.fail(Failure::Input);
    assert_eq!(unfinished.exit_code(), 1);
}

///
/// 二重記録・異なるページの復旧情報・段階不整合を拒否する。
///
/// # 概要
/// 重複ページ・復旧関連・段階不整合を拒否し、正常記録後の二重記録と結果保持を確認する。
///
/// # 注記
/// 合成計画へ不正な関連付けを行い、状態が変更されないことを確認する。
///
/// # 戻り値
/// なし。重複・復旧関連・段階不整合の拒否や結果保持が異なればassertで失敗する。
///
#[test]
fn prevents_duplicate_records_and_inconsistent_recovery() {
    /*
     * 計画の重複ページを拒否する
     */
    assert_eq!(Report::planned(vec![identity(0), identity(0)]).err(),
        Some(ReportError::DuplicatePage));
    /*
     * 異なる復旧関連と段階不整合を拒否する
     */
    let mut report = Report::planned(vec![identity(0)]).expect("計画");
    let recovery = RecoveryInfo {
        identity: identity(1), facts: RecoveryFacts {
            old_id: None, old_revision: None, draft_id: None,
            ..RecoveryFacts::default() },
        stage: ProcessingStage::CreateDraft,
        observed: ObservedState::Unknown,
        advice: Some(RecoveryAdvice::InspectBeforeRetry),
    };
    assert_eq!(report.record(0, PageOutcome::Created,
        ProcessingStage::Finished, Some(recovery)),
        Err(ReportError::InconsistentRecovery));
    assert_eq!(report.record(0, PageOutcome::Failed {
        category: FailureCategory::Uncertain,
        stage: ProcessingStage::CreateDraft,
    }, ProcessingStage::PutSource, None),
        Err(ReportError::InconsistentStage));
    /*
     * 正常結果を記録してから二重記録を拒否する
     */
    report.record(0, PageOutcome::Created, ProcessingStage::Finished, None)
        .expect("成功記録");
    assert_eq!(report.record(0, PageOutcome::Updated,
        ProcessingStage::Finished, None), Err(ReportError::AlreadyRecorded));
    assert_eq!(report.summary().created, 1);
}

///
/// 復旧情報の未確定値を推測で補完しない。
///
/// # 概要
/// POST応答喪失を表す合成復旧情報を記録し、未知値の保持と手動再実行の案内を確認する。
///
/// # 注記
/// POST応答喪失を表す合成情報にIDを与えず、表示を照合する。
///
/// # 戻り値
/// なし。未確定ID・revisionの保持や手動判断の案内が異なればassertで失敗する。
///
#[test]
fn preserves_unknown_recovery_fields_and_manual_advice() {
    /*
     * IDが未確定の合成復旧情報を失敗結果へ記録する
     */
    let id = identity(0);
    let mut report = Report::planned(vec![id.clone()]).expect("計画");
    let recovery = RecoveryInfo {
        identity: id, facts: RecoveryFacts {
            old_id: None, old_revision: None, draft_id: None,
            ..RecoveryFacts::default() },
        stage: ProcessingStage::CreateDraft,
        observed: ObservedState::Unknown,
        advice: Some(RecoveryAdvice::InspectBeforeRetry),
    };
    report.record(0, PageOutcome::Failed {
        category: FailureCategory::Uncertain,
        stage: ProcessingStage::CreateDraft,
    }, ProcessingStage::CreateDraft, Some(recovery)).expect("不確定記録");
    /*
     * 未知値を補わない表示と手動判断の案内を照合する
     */
    let rendered = report.render();
    assert!(rendered.results.contains("old_id=None"));
    assert!(rendered.results.contains("revision=None"));
    assert!(rendered.results.contains("draft_id=None"));
    assert!(rendered.results.contains("再実行は手動で判断"));
    assert_eq!(report.exit_code(), 1);
}

///
/// 公開エラー・Debug・復旧情報に秘密値の欄がないことを確認する。
///
/// # 概要
/// 秘密値を報告へ渡さず安全な失敗と警告だけを記録し、全表示と秘密型の非露出を確認する。
///
/// # 注記
/// 外部の秘密値を渡さず安全な分類へ変換し、両表示経路を照合する。
///
/// # 戻り値
/// なし。報告・診断・秘密型の表示が本文や秘密値を含む場合はassertで失敗する。
///
#[test]
fn report_and_diagnostics_exclude_secret_and_body_values() {
    /*
     * ダミー秘密値と安全な失敗・警告だけの報告を準備する
     */
    let secrets = [
        "wiki-secret", "github-secret", "git-secret", "lock-secret",
        "unique-page-body",
    ];
    let mut report = Report::preflight();
    report.fail(Failure::Collection(CollectionError::Validation(
        ValidationError::Url,
    )));
    report.add_warnings(&[Warning::CliSecret, Warning::ConfigPermissions]);
    /*
     * 報告・診断・秘密型の表示から本文と秘密値が除かれることを照合する
     */
    let rendered = report.render();
    let output = format!("{report:?} {} {}",
        rendered.results, rendered.diagnostics);
    for value in secrets {
        assert!(!output.contains(value));
        assert!(!format!("{:?}", Secret::new(value.to_owned()))
            .contains(value));
    }
    assert!(rendered.diagnostics.contains("対象Wiki URL"));
    assert!(rendered.results.contains("exit=2"));
}

///
/// 確定した旧ID・revisionと手動再作成案内を保持する。
///
/// # 概要
/// 旧IDとrevisionが確定した合成失敗を記録し、未知の新IDを補わず手動再作成を案内する。
///
/// # 注記
/// DELETE後の再作成失敗を合成し、未確定ドラフトを補完しない。
///
/// # 戻り値
/// なし。確定旧ID・revisionの保持や未確定値の非補完が異なればassertで失敗する。
///
#[test]
fn confirmed_recovery_preserves_old_id_and_revision() {
    /*
     * 確定旧IDとrevisionだけを持つ再作成失敗を記録する
     */
    let identity = identity(0);
    let mut report = Report::planned(vec![identity.clone()]).expect("計画");
    let old = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let recovery = RecoveryInfo {
        identity, facts: RecoveryFacts {
            old_id: Some(PageId::new(old).expect("旧ページID")),
            old_revision: Some(7), draft_id: None,
            ..RecoveryFacts::default() },
        stage: ProcessingStage::DeleteOld,
        observed: ObservedState::Deleted,
        advice: Some(RecoveryAdvice::RecreateManually),
    };
    report.record(0, PageOutcome::Failed {
        category: FailureCategory::RecreationFailed,
        stage: ProcessingStage::CreateDraft,
    }, ProcessingStage::CreateDraft, Some(recovery)).expect("失敗記録");
    /*
     * 既知情報の保持と新IDの非補完・自動復元なしを照合する
     */
    let rendered = report.render();
    assert!(rendered.results.contains(old));
    assert!(rendered.results.contains("revision=Some(7)"));
    assert!(rendered.results.contains("draft_id=None"));
    assert!(rendered.results.contains("旧ページは自動復元されません"));
}

///
/// NotExecutedの確定記録も二重記録から保護し元payloadを残す。
///
/// # 概要
/// 未実行を記録した後に成功を二重記録し、未実行と拒否結果を区別する。
///
/// # 注記
/// 製品record_pageへ直接渡し、拒否したCreatedの所有を参照する。
///
/// # 戻り値
/// なし。二重記録成功や元payload消失で失敗する。
///
#[test]
fn not_executed_record_is_not_an_unrecorded_slot() {
    /*
     * 未実行も確定記録として保護する
     */
    let mut report = Report::planned(vec![identity(0)]).unwrap();
    report.record_page(0, PageResult { identity: identity(0),
        outcome: PageOutcome::NotExecuted, stage: ProcessingStage::Preflight,
        recovery: None, details: ResultDetails::default() }).unwrap();
    assert_eq!(report.record_page(0, PageResult { identity: identity(0),
        outcome: PageOutcome::Created, stage: ProcessingStage::Finished,
        recovery: None, details: ResultDetails::default() }),
        Err(ReportError::AlreadyRecorded));
    /*
     * 拒否した成功payloadを消さず未実行件数も維持する
     */
    assert_eq!(report.summary().created, 0);
    assert_eq!(report.summary().not_executed, 1);
    assert!(matches!(&report.rejected[0],
        super::details::RejectedExecution::Page { page, .. }
            if page.outcome == PageOutcome::Created));
    assert_eq!(report.exit_code(), 1);
}

///
/// 失敗ページでも複数操作警告を一件ずつ数え起動警告も維持する。
///
/// # 概要
/// 最終Failedを警告数や成功ページ数へ重複加算しない。
///
/// # 注記
/// 合成二警告と起動警告一件を独立に記録し、順序と両出力を確認。
///
/// # 戻り値
/// なし。警告消失・二重計上・出力先混同で失敗する。
///
#[test]
fn failed_page_keeps_multiple_warnings_without_success_count() {
    let mut report = Report::planned(vec![identity(0)]).unwrap();
    report.add_warnings(&[Warning::CliSecret]);
    let warnings = vec![OperationWarning { operation: Operation::Delete,
        response: ResponseCondition::Http(500),
        observed: ObservedState::Deleted,
        detail: None }, OperationWarning { operation: Operation::Put,
        response: ResponseCondition::ResponseUncertain,
        observed: ObservedState::Unknown, detail: None }];
    report.record_with_warnings(0, PageOutcome::Failed {
        category: FailureCategory::Uncertain, stage: ProcessingStage::PutSource,
    }, ProcessingStage::PutSource, None, warnings).unwrap();
    /*
     * 結果は失敗一件のまま、警告だけを独立に集計する
     */
    assert_eq!(report.summary().warnings, 3);
    assert_eq!(report.summary().completed(), 0);
    assert_eq!(report.summary().failed, 1);
    assert_eq!(report.exit_code(), 1);
    let output = report.render();
    let delete = output.diagnostics.find("operation=Delete").unwrap();
    let put = output.diagnostics.find("operation=Put").unwrap();
    assert!(delete < put);
    assert!(!output.results.contains("warning:"));
    assert!(output.diagnostics.contains("response=ResponseUncertain"));
}

///
/// 無警告の警告付き結果や不正位置の元結果を失わず拒否する。
///
/// # 概要
/// 一体記録入口で拒否しても、元の結果と計画内の未実行を保持する。
///
/// # 注記
/// 適用結果と警告の不整合、存在しない位置を別に記録して保持を確認。
///
/// # 戻り値
/// なし。不正を成功へ補完するか元結果を破棄すれば失敗する。
///
#[test]
fn invalid_warning_and_position_retain_original_pages() {
    /*
     * 警告付き結果と警告列の矛盾を拒否する
     */
    let mut report = Report::planned(vec![identity(0)]).unwrap();
    let page = PageResult { identity: identity(0),
        outcome: PageOutcome::AppliedWithWarning { action: Action::Update },
        stage: ProcessingStage::Finished, recovery: None,
        details: ResultDetails::default() };
    assert_eq!(report.record_page(0, page),
        Err(ReportError::InconsistentWarning));
    /*
     * 無効位置への記録も元payloadを別に保持する
     */
    let page = PageResult { identity: identity(0),
        outcome: PageOutcome::Created,
        stage: ProcessingStage::Finished, recovery: None,
        details: ResultDetails::default() };
    assert_eq!(report.record_page(99, page), Err(ReportError::InvalidIndex));
    assert_eq!(report.rejected.len(), 2);
    assert_eq!(report.summary().completed(), 0);
    assert_eq!(report.exit_code(), 1);
}

///
/// 不正な結果へ混入した主失敗・警告・解除の事実を拒否後も保持する。
///
/// # 概要
/// 成功/skip/未実行と詳細の矛盾、解除試行と段階の矛盾を検査する。
///
/// # 注記
/// 各不整合を独立した報告へ記録し、拒否payloadとexit1を確認する。
///
/// # 戻り値
/// なし。不整合を正常記録した場合や元詳細を捨てた場合に失敗する。
///
#[test]
fn contradictory_details_are_retained_without_counting_success() {
    use super::details::{DetailKind, DetailOperation, FailureDetail,
        FailureOrigin, RequestState};

    /*
     * 異なる結果と詳細の矛盾をそれぞれ独立に検査する
     */
    for case in 0..9 {
        let mut report = Report::planned(vec![identity(0)]).unwrap();
        let mut page = PageResult { identity: identity(0),
            outcome: PageOutcome::Created, stage: ProcessingStage::Finished,
            recovery: None, details: ResultDetails::default() };
        let detail = FailureDetail { origin: FailureOrigin::Wiki,
            operation: Some(DetailOperation::ReleaseLock),
            kind: DetailKind::Http, stage: Some(ProcessingStage::ReleaseLock),
            sent: Some(RequestState::Started), status: Some(500),
            known_id: None, diagnostics: None, cause: None };
        /*
         * 元の矛盾情報を一つだけ変えて成功・失敗・未実行へ載せる
         */
        match case {
            /*
             * 成功または失敗に矛盾する主・補助・解除の情報を用意する
             */
            0 => page.details.primary = Some(detail),
            1 => page.details.auxiliary.push(detail),
            2 => page.details.cleanup = Some(detail),
            3 => page.details.cleanup_attempted = true,
            4 | 5 => {
                page.outcome = PageOutcome::Failed {
                    category: FailureCategory::Rejected,
                    stage: ProcessingStage::PutSource };
                page.stage = ProcessingStage::PutSource;
                page.details.cleanup_attempted = case == 5;
                page.details.cleanup = Some(detail);
            }
            /*
             * skip/未実行へ警告を混入させた場合も検査する
             */
            6 | 7 => {
                page.outcome = if case == 6 { PageOutcome::SkippedIdentical }
                    else { PageOutcome::NotExecuted };
                page.stage = ProcessingStage::Preflight;
                page.details.warnings.push(OperationWarning {
                    operation: Operation::Delete,
                    response: ResponseCondition::ResponseUncertain,
                    observed: ObservedState::Unknown, detail: None });
            }
            _ => page.outcome = PageOutcome::NotExecuted,
        }
        /*
         * 元payloadを保持し、成功件数に加算せず実行失敗へ分類する
         */
        assert!(report.record_page(0, page).is_err(), "case={case}");
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.summary().completed(), 0);
        assert_eq!(report.exit_code(), 1);
        let super::details::RejectedExecution::Page { page, .. } =
            &report.rejected[0] else { panic!("元ページが必要") };
        match case {
            0 => assert!(page.details.primary.is_some()),
            1 => assert_eq!(page.details.auxiliary.len(), 1),
            2 | 4 | 5 => assert!(page.details.cleanup.is_some()),
            3 => assert!(page.details.cleanup_attempted),
            6 | 7 => assert_eq!(page.details.warnings.len(), 1),
            _ => assert_eq!(page.stage, ProcessingStage::Finished),
        }
    }
}
