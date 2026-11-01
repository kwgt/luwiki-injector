/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実runの元事実からReport表示・集計・終了状態までの接続試験
//!

use std::sync::Arc;
use std::time::Duration;

use crate::cmd_args::resolve::{Mode, Warning};
use crate::executor::Executor;
use crate::executor::compare::compare_planned;
use crate::executor::prepare::tests::mock::{Reply, Server, TOKEN2, TOKEN3};
use crate::executor::tests::{body, location, no_status, plan, stop};
use crate::executor::types::{LocalFailure, PageStep};
use crate::source::{SourceError, StopContext};

use super::*;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
const TOKEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

///
/// 合成IDを型付きで取得する。
///
/// # 引数
/// * `id` - 固定fixture ID
///
/// # 戻り値
/// 検証済みID
///
fn id(id: &str) -> crate::report::types::PageId {
    crate::report::types::PageId::new(id).unwrap()
}

///
/// 確定IDまたはdraftの正常応答を作る。
///
/// # 引数
/// * `id` - fixture ID
/// * `created` - lock付き201か
///
/// # 戻り値
/// 完全JSONの応答
///
fn identity(id: &str, created: bool) -> Reply {
    let mut reply = Reply::new(if created { 201 } else { 200 },
        format!("{{\"id\":\"{id}\"}}").into_bytes());
    reply.lock = created;
    reply
}

///
/// 旧IDの状態を完全metadataで返す。
///
/// # 引数
/// * `deleted` - 削除確認か
/// * `locked` - lock観測
///
/// # 戻り値
/// current/last_deletedとrevisionを含む応答
///
fn metadata(deleted: bool, locked: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"{kind}\",\
        \"value\":\"/fixture/a\"}},\"deleted\":{deleted},\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}").into_bytes())
}

///
/// 一件/二件の成立計画を同じcontextで実runへ渡す。
///
/// # 引数
/// * `server` - 専用script
/// * `shared` - 同じ停止境界
/// * `mode` - 衝突モード
/// * `later` - 後続未実行確認を含むか
///
/// # 戻り値
/// 実行前の製品executor
///
async fn runner(server: &Server, shared: &StopContext, mode: Mode,
    later: bool) -> Executor
{
    let config = crate::cmd_args::resolve::wiki_config_fixture(&server.url,
        true, shared.deadline());
    let entries: &[(&[u8], &[u8])] = if later {
        &[(b"a.md", b"private-input-body"), (b"b.md", b"later")]
    } else { &[(b"a.md", b"private-input-body")] };
    let mut executor = Executor::new(plan(entries, shared).await, &config,
        shared.clone()).unwrap();
    executor.mode = mode;
    executor
}

///
/// 旧DELETEまたはupdate PUTまで正常に到達する応答を返す。
///
/// # 引数
/// * `status` - 元変更の応答status
///
/// # 戻り値
/// 元変更までの十要求分
///
fn existing(status: Option<u16>) -> Vec<Reply> {
    vec![identity(ID, false), metadata(false, false),
        Reply::new(200, b"before".to_vec()), Reply::locked(TOKEN),
        Reply::locked(TOKEN2), metadata(false, true), identity(ID, false),
        Reply::new(200, b"before".to_vec()), Reply::locked(TOKEN3),
        Reply { status, body: b"{}".to_vec(), lock: false, token: TOKEN }]
}

///
/// 新規作成PUT異常までの応答を準備する。
///
/// # 引数
/// * `status` - 元PUTの応答status
///
/// # 戻り値
/// POST/renew/PUTまでの四応答
///
fn creation(status: Option<u16>) -> Vec<Reply> {
    vec![Reply::new(404, Vec::new()), identity(ID, true),
        Reply::locked(TOKEN2), Reply { status, body: b"{}".to_vec(),
            lock: false, token: TOKEN }]
}

///
/// 新旧対象の目的本文が確認できる応答を追加する。
///
/// # 引数
/// * `replies` - 元PUTまでの応答
/// * `id` - 元PUTの確定ID
///
/// # 戻り値
/// なし。ID/meta/sourceの順を保つ。
///
fn applied(replies: &mut Vec<Reply>, id: &str) {
    replies.extend([identity(id, false), metadata(false, false),
        Reply::new(200, b"private-input-body".to_vec())]);
}

///
/// 明示復旧出力以外のDebugに識別子や本文・tokenが露出しないか確認。
///
/// # 引数
/// * `report` - 所有変換後の報告
///
/// # 戻り値
/// なし。Debugと最終両出力の本文/秘密漏出で失敗する。
///
fn private(report: &Report) {
    let rendered = report.render();
    let debug = format!("{report:?} {rendered:?}");
    for value in [ID, NEW, "/fixture", "a.md"] {
        assert!(!debug.contains(value));
    }
    for value in [TOKEN, TOKEN2, TOKEN3, "private-input-body", "raw-private"] {
        assert!(!debug.contains(value));
        assert!(!rendered.results.contains(value));
        assert!(!rendered.diagnostics.contains(value));
    }
}

///
/// 通常の全完了種別を一度だけ計上し識別情報も維持する。
///
/// # 概要
/// 実行側の成立計画と所有結果を製品変換/記録/表示へ通す。
///
/// # 注記
/// 成功操作は各一件、skipの観測と手動確認不要も照合する。
///
/// # 戻り値
/// なし。結果喪失や二重計上・Debug漏出で失敗する。
///
#[tokio::test]
async fn normal_outcomes_keep_order_and_recovery_facts_without_double_count() {
    let shared = stop();
    let input = plan(&[(b"a.md", b"a"), (b"b.md", b"b"), (b"c.md", b"c"),
        (b"d.md", b"d")], &shared).await;
    let mut batch = ExecutionBatch::new(&input);
    /*
     * 成立計画に通常結果を一件ずつ記録し最終Reportへ所有移動する
     */
    for (index, completion) in [Completion::Created,
        Completion::SkippedIdentical, Completion::Updated,
        Completion::Recreated]
        .into_iter().enumerate()
    {
        batch.record(index, PageStep::completed(completion,
            KnownPageFacts { old_id: Some(id(ID)), old_revision: Some(7),
                new_id: Some(id(NEW)), old_delete_confirmed: index == 3,
                old_lock_observed: None })).unwrap();
    }
    let report = into_report(batch);
    assert_eq!(report.exit_code(), 0);
    let summary = report.summary();
    assert_eq!((summary.created, summary.skipped, summary.updated,
        summary.recreated, summary.completed(), summary.warnings),
        (1, 1, 1, 1, 4, 0));
    let recovery = report.pages()[3].recovery.as_ref().unwrap();
    assert!(recovery.facts.old_delete_confirmed);
    assert_eq!(recovery.facts.old_revision, Some(7));
    assert_eq!(recovery.facts.draft_id.as_ref().unwrap().as_str(), NEW);
    assert!(recovery.advice.is_none());
    let output = report.render();
    assert!(output.results.contains(NEW));
    assert!(!output.results.contains("manual_action"));
    private(&report);
}

///
/// DELETE500警告は再作成POST失敗と起動警告があっても残る。
///
/// # 概要
/// 実runの主失敗と先行警告を最終表示・集計まで接続する。
///
/// # 注記
/// 先行削除を復旧事実へ保持し、後続未実行とexit1を確認する。
///
/// # 戻り値
/// なし。DELETE警告や旧情報が消えた場合に失敗する。
///
#[tokio::test]
async fn delete_warning_survives_recreation_failure_and_startup_warning() {
    let shared = stop();
    let mut replies = existing(Some(500));
    replies.extend([metadata(true, false), Reply::new(404, Vec::new()),
        Reply::new(403, b"{\"reason\":\"raw-private\"}".to_vec())]);
    let server = Server::new(replies).await;
    let mut report = into_report(runner(&server, &shared, Mode::Overwrite, true)
        .await.run().await);
    report.add_warnings(&[Warning::CliSecret]);
    /*
     * 旧削除の警告と新POSTの主失敗を独立して確認する
     */
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.summary().warnings, 2);
    assert_eq!(report.summary().failed, 1);
    assert_eq!(report.summary().not_executed, 1);
    let page = &report.pages()[0];
    assert!(matches!(page.outcome, PageOutcome::Failed {
        category: FailureCategory::Permission,
        stage: ProcessingStage::CreateDraft }));
    assert_eq!(page.details.warnings.len(), 1);
    let warning = &page.details.warnings[0];
    assert_eq!(warning.operation, Operation::Delete);
    assert_eq!(warning.response, ResponseCondition::Http(500));
    assert_eq!(warning.detail.as_ref().unwrap().stage,
        Some(ProcessingStage::DeleteOld));
    let recovery = page.recovery.as_ref().unwrap();
    assert!(recovery.facts.old_delete_confirmed);
    assert_eq!(recovery.facts.old_lock_observed, Some(false));
    assert!(recovery.facts.draft_id.is_none());
    assert_eq!(recovery.advice, Some(RecoveryAdvice::RecreateManually));
    let rendered = report.render();
    assert!(rendered.diagnostics
        .contains("operation=Delete response=Http(500)"));
    assert!(rendered.diagnostics.contains("role=primary"));
    assert!(rendered.results.contains("旧ページは自動復元されません"));
    server.inspect(|requests| assert_eq!(requests.len(), 13));
    private(&report);
}

///
/// 二つの操作警告と安全停止は最後のページでもexit1となる。
///
/// # 概要
/// 実DELETE500→新PUT500→目的状態確認を最終Reportまで通す。
///
/// # 注記
/// 成功一件/警告二件、failed0/未実行0でもStopを無視しない。
///
/// # 戻り値
/// なし。二重計上またはexit3への誤分類で失敗する。
///
#[tokio::test]
async fn confirmed_recreation_with_two_warnings_still_stops_on_last_page() {
    let shared = stop();
    let mut replies = existing(Some(500));
    replies.extend([metadata(true, false), Reply::new(410, Vec::new()),
        identity(NEW, true), Reply::locked(TOKEN2),
        Reply::new(500, b"{}".to_vec())]);
    applied(&mut replies, NEW);
    let server = Server::new(replies).await;
    let report = into_report(runner(&server, &shared, Mode::Overwrite, false)
        .await.run().await);
    /*
     * 警告順序と適用操作・独立停止を一つの最終記録で確認する
     */
    assert_eq!(report.exit_code(), 1);
    let summary = report.summary();
    assert_eq!((summary.recreated, summary.warnings, summary.failed,
        summary.not_executed), (1, 2, 0, 0));
    let page = &report.pages()[0];
    assert!(matches!(page.outcome,
        PageOutcome::AppliedWithWarning { action: Action::Recreate }));
    assert_eq!(page.details.warnings[0].operation, Operation::Delete);
    assert_eq!(page.details.warnings[1].operation, Operation::Put);
    assert!(page.details.stop.is_some());
    assert!(report.render().diagnostics.contains("safety_stop"));
    private(&report);
}

///
/// PUTの適用確認後は500/応答状態不明をそれぞれ警告として表示する。
///
/// # 概要
/// statusなしをDisconnectedや警告なしへ変換せずexit3へ接続する。
///
/// # 注記
/// 元kind/sent/status/確定IDを保持し手動復旧を正常適用へ要求しない。
///
/// # 戻り値
/// なし。statusなしの事実消失や原因断定で失敗する。
///
#[tokio::test]
async fn confirmed_put_reports_http_and_uncertain_response_as_warnings() {
    for status in [Some(500), None] {
        /*
         * 新規PUT異常を同IDの目的状態へ確認してReportへ所有移動する
         */
        let shared = stop();
        let mut replies = creation(status);
        applied(&mut replies, ID);
        let server = Server::new(replies).await;
        let report = into_report(runner(&server, &shared, Mode::Update, false)
            .await.run().await);
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.summary().created, 1);
        assert_eq!(report.summary().warnings, 1);
        let page = &report.pages()[0];
        let warning = &page.details.warnings[0];
        assert_eq!(warning.response, status.map_or(
            ResponseCondition::ResponseUncertain, ResponseCondition::Http));
        let detail = warning.detail.as_ref().unwrap();
        assert_eq!(detail.status, status);
        assert_eq!(detail.sent, Some(RequestState::Started));
        assert_eq!(detail.known_id.as_ref().unwrap().as_str(), ID);
        assert!(page.recovery.as_ref().unwrap().advice.is_none());
        let rendered = report.render();
        if status.is_none() {
            assert!(rendered.diagnostics
                .contains("response=ResponseUncertain"));
            assert!(!rendered.diagnostics.contains("Disconnected"));
            assert_eq!(detail.kind, DetailKind::Transport);
        }
        server.inspect(|requests| assert_eq!(requests.len(), 7));
        private(&report);
    }
}

///
/// 主PUTと独立した解除失敗/最後段階をReportで失わない。
///
/// # 概要
/// 完全400で保持した既存leaseだけの解除が500になる実runを通す。
///
/// # 注記
/// 主PutSourceと最後ReleaseLockが異なっても記録を拒否しない。
///
/// # 戻り値
/// なし。解除失敗への主原因置換や段階喪失で失敗する。
///
#[tokio::test]
async fn primary_and_cleanup_failures_keep_separate_stages() {
    let shared = stop();
    let mut replies = existing(Some(400));
    replies.push(Reply::new(500, b"{\"reason\":\"raw-private\"}".to_vec()));
    let server = Server::new(replies).await;
    let report = into_report(runner(&server, &shared, Mode::Update, false)
        .await.run().await);
    /*
     * 本来の主原因・最後段階・解除事実を独立参照する
     */
    let page = &report.pages()[0];
    assert_eq!(page.stage, ProcessingStage::ReleaseLock);
    assert!(matches!(page.outcome, PageOutcome::Failed {
        category: FailureCategory::Rejected,
        stage: ProcessingStage::PutSource }));
    let primary = page.details.primary.as_ref().unwrap();
    assert_eq!(primary.operation, Some(DetailOperation::PutSource));
    assert_eq!(primary.status, Some(400));
    let cleanup = page.details.cleanup.as_ref().unwrap();
    assert_eq!(cleanup.operation, Some(DetailOperation::ReleaseLock));
    assert_eq!(cleanup.status, Some(500));
    assert!(page.details.cleanup_attempted);
    assert_eq!(report.summary().warnings, 0);
    assert_eq!(report.exit_code(), 1);
    assert!(report.render().diagnostics.contains("role=cleanup"));
    server.inspect(|requests| assert_eq!(requests.len(), 11));
    private(&report);
}

///
/// 再照合の失敗を主PUTと別に記録し未確認を成功へ補完しない。
///
/// # 概要
/// 元PUTと失敗したlookupのstatus/操作/段階を別に保持する。
///
/// # 注記
/// 元PUT500後lookup503を返し、主/補助のoperation/statusを確認する。
///
/// # 戻り値
/// なし。主原因置換や再照合を初回比較へ戻すと失敗する。
///
#[tokio::test]
async fn reconciliation_failure_remains_auxiliary_and_unknown() {
    /*
     * 元PUTと再照合lookupに別々の失敗応答を用意する
     */
    let shared = stop();
    let mut replies = creation(Some(500));
    replies.push(Reply::new(503, b"{}".to_vec()));
    let server = Server::new(replies).await;
    let report = into_report(runner(&server, &shared, Mode::Update, false)
        .await.run().await);
    /*
     * 主原因・最後段階・補助失敗とUnknown観測を分けて確認する
     */
    let page = &report.pages()[0];
    assert_eq!(page.stage, ProcessingStage::Reconcile);
    assert_eq!(page.details.primary.as_ref().unwrap().status, Some(500));
    assert_eq!(page.details.primary.as_ref().unwrap().stage,
        Some(ProcessingStage::PutSource));
    let auxiliary = &page.details.auxiliary[0];
    assert_eq!(auxiliary.operation, Some(DetailOperation::Lookup));
    assert_eq!(auxiliary.status, Some(503));
    assert_eq!(auxiliary.stage, Some(ProcessingStage::Reconcile));
    assert_eq!(page.recovery.as_ref().unwrap().observed,
        ObservedState::Unknown);
    assert_eq!(report.exit_code(), 1);
    private(&report);
}

///
/// 局所比較失敗へ結合したstatusなし先行事実を警告として残す。
///
/// # 概要
/// 旧adapterのwarning() Noneを利用せず安全な最終警告へ所有変換。
///
/// # 注記
/// 元の詳細操作・未知caller段階・Unknown観測を維持する。
///
/// # 戻り値
/// なし。statusなし消失や通信断の断定で失敗する。
///
#[tokio::test]
async fn local_failure_keeps_statusless_prior_evidence_and_unknown_stage() {
    let shared = stop();
    let input = plan(&[(b"a.md", b"a")], &shared).await;
    let evidence = no_status(&shared).await;
    let failure = ExecutorFailure::Local(LocalFailure::new(LocalKind::CompareIo,
        ProcessingStage::Reconcile, vec![evidence]));
    let mut batch = ExecutionBatch::new(&input);
    batch.record(0, PageStep::failed(failure,
        KnownPageFacts { old_id: Some(id(ID)), ..KnownPageFacts::default() },
        None)).unwrap();
    /*
     * HTTPではない主失敗と先行の単発HTTP観測を区別する
     */
    let report = into_report(batch);
    let page = &report.pages()[0];
    assert_eq!(page.details.primary.as_ref().unwrap().origin,
        FailureOrigin::Local);
    assert_eq!(page.details.primary.as_ref().unwrap().sent, None);
    assert_eq!(page.details.warnings.len(), 1);
    let warning = &page.details.warnings[0];
    assert_eq!(warning.response, ResponseCondition::ResponseUncertain);
    assert_eq!(warning.observed, ObservedState::Unknown);
    let detail = warning.detail.as_ref().unwrap();
    assert_eq!(detail.operation, Some(DetailOperation::ReadSource));
    assert_eq!(detail.stage, None);
    assert_eq!(detail.status, None);
    assert_eq!(report.summary().warnings, 1);
    assert_eq!(report.exit_code(), 1);
    private(&report);
}

///
/// 受信済み本文保管openの元causeを新HTTPや主原因へ変換しない。
///
/// # 概要
/// 実比較の保管失敗をReportへ通し元200/knownIDをcauseに保持する。
///
/// # 注記
/// fixtureの受信済みFileだけを削除し局所Ioとstored_causeを確認。
///
/// # 戻り値
/// なし。causeや段階の消失・HTTP200成功の補完で失敗する。
///
#[tokio::test]
async fn received_open_cause_survives_as_local_failure_detail() {
    let shared = stop();
    let input = Arc::new(plan(&[(b"a.md", b"a")], &shared).await);
    let received = body(b"a", &shared).await;
    std::fs::remove_file(location(&received.open().unwrap())).unwrap();
    let failure = compare_planned(Arc::clone(&input), 0, received,
        shared.clone()).await.unwrap_err();
    let mut batch = ExecutionBatch::new(&input);
    batch.record(0, PageStep::failed(failure, KnownPageFacts::default(), None))
        .unwrap();
    /*
     * 補助causeの元statusを残しても主結果は局所I/O失敗にする
     */
    let report = into_report(batch);
    let page = &report.pages()[0];
    assert!(matches!(page.outcome,
        PageOutcome::Failed { category: FailureCategory::Io, .. }));
    let primary = page.details.primary.as_ref().unwrap();
    assert_eq!(primary.origin, FailureOrigin::Local);
    assert_eq!(primary.kind, DetailKind::ReceivedOpen);
    let cause = primary.cause.as_ref().unwrap();
    assert_eq!(cause.operation, Some(DetailOperation::ReadSource));
    assert_eq!(cause.status, Some(200));
    assert_eq!(cause.known_id.as_ref().unwrap().as_str(), ID);
    assert!(report.render().diagnostics.contains("role=stored_cause"));
    assert_eq!(report.exit_code(), 1);
    private(&report);
}

///
/// 完了後の共有停止でも先行成功を残し全体詳細と起動警告を保持する。
///
/// # 概要
/// 全ページ完了後の全体停止も独立した実行失敗として報告する。
///
/// # 注記
/// ページ結果確定後に開始前Cancelledを記録しexit1を照合する。
///
/// # 戻り値
/// なし。先行成功の未実行化や停止分類消失で失敗する。
///
#[tokio::test]
async fn final_global_stop_preserves_completed_page_and_startup_warning() {
    /*
     * 成功確定後の全体取消を独立した失敗として記録する
     */
    let shared = stop();
    let input = plan(&[(b"a.md", b"a")], &shared).await;
    let mut batch = ExecutionBatch::new(&input);
    batch.record(0, PageStep::completed(Completion::Created,
        KnownPageFacts { new_id: Some(id(ID)), ..KnownPageFacts::default() }))
        .unwrap();
    batch.stop_before_page(ExecutorFailure::stopped(SourceError::Cancelled,
        ProcessingStage::Preflight));
    let mut report = into_report(batch);
    report.add_warnings(&[Warning::ConfigPermissions]);
    /*
     * 先行成功と全体取消の診断を同時に保持する
     */
    assert_eq!(report.summary().created, 1);
    assert_eq!(report.summary().not_executed, 0);
    assert_eq!(report.summary().warnings, 1);
    assert_eq!(report.exit_code(), 1);
    let output = report.render();
    assert!(output.diagnostics.contains("role=global"));
    assert!(output.diagnostics.contains("Cancelled"));
    private(&report);
}

///
/// 空計画・起動警告・入力設定失敗の終了分類を保つ。
///
/// # 注記
/// 空の実行結果はHTTPなしで変換し、その後の全体状態を比較する。
///
/// # 戻り値
/// なし。空計画を未実行失敗にした場合も失敗する。
///
#[tokio::test]
async fn empty_batch_keeps_zero_three_two_one_priority() {
    let shared = stop();
    let input = plan(&[], &shared).await;
    let mut report = into_report(ExecutionBatch::new(&input));
    assert_eq!(report.exit_code(), 0);
    report.add_warnings(&[Warning::CliSecret]);
    assert_eq!(report.exit_code(), 3);
    report.fail(crate::report::Failure::Input);
    assert_eq!(report.exit_code(), 2);
    report.fail(crate::report::Failure::Execution {
        category: FailureCategory::Cancelled, stage: ProcessingStage::Preflight,
    });
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.summary().warnings, 1);
}

///
/// 実行側で拒否された元payloadも計上せず復旧事実として保持する。
///
/// # 概要
/// 順序外/二重記録を拒否した後、Reportが全元結果を表示するか確認。
///
/// # 注記
/// 原順序の先行成功と未実行を保ち、拒否Recreatedを成功数へ足さない。
///
/// # 戻り値
/// なし。元payload消失や成功の二重計上で失敗する。
///
#[tokio::test]
async fn rejected_payloads_remain_unassigned_without_counting_success() {
    let shared = stop();
    let input = plan(&[(b"a.md", b"a"), (b"b.md", b"b")], &shared).await;
    let mut batch = ExecutionBatch::new(&input);
    /*
     * 内部不整合の拒否payloadにも確定IDを所有させる
     */
    assert!(batch.record(1, PageStep::completed(Completion::Recreated,
        KnownPageFacts { old_id: Some(id(ID)), new_id: Some(id(NEW)),
            old_revision: Some(7), old_delete_confirmed: true,
            old_lock_observed: Some(false) })).is_err());
    batch.record(0, PageStep::completed(Completion::Created,
        KnownPageFacts::default())).unwrap();
    assert!(batch.record(0, PageStep::completed(Completion::Updated,
        KnownPageFacts { old_id: Some(id(ID)), ..KnownPageFacts::default() }))
        .is_err());
    let report = into_report(batch);
    assert_eq!(report.summary().created, 1);
    assert_eq!(report.summary().recreated, 0);
    assert_eq!(report.summary().updated, 0);
    assert_eq!(report.summary().not_executed, 1);
    assert_eq!(report.exit_code(), 1);
    let output = report.render();
    assert_eq!(output.results.matches("unassigned_result:").count(), 2);
    assert!(output.results.contains("outcome=Recreated"));
    assert!(output.results.contains("old_delete_confirmed=true"));
    assert!(output.results.contains(NEW));
    private(&report);
}

///
/// DELETE通信状態不明で削除を確認しても警告と失敗の両方を表示する。
///
/// # 概要
/// 先行DELETEが主原因のままの停止をReportまで通す。
///
/// # 注記
/// 削除済みを未適用に補完せず、再作成0/exit1/警告1を確認する。
///
/// # 戻り値
/// なし。削除確認で成功扱いにしたり警告が消えた場合に失敗する。
///
#[tokio::test]
async fn uncertain_delete_reports_both_warning_and_failure() {
    /*
     * DELETE応答喪失の後で削除済みだけを確認する
     */
    let shared = stop();
    let mut replies = existing(None);
    replies.extend([metadata(true, false), Reply::new(404, Vec::new())]);
    let server = Server::new(replies).await;
    let report = into_report(runner(&server, &shared, Mode::Overwrite, false)
        .await.run().await);
    /*
     * 再作成せず元DELETE警告と主失敗の両方を保持する
     */
    let page = &report.pages()[0];
    assert!(matches!(page.outcome, PageOutcome::Failed { .. }));
    assert_eq!(page.details.warnings.len(), 1);
    assert_eq!(page.details.warnings[0].response,
        ResponseCondition::ResponseUncertain);
    assert_eq!(page.details.warnings[0].observed, ObservedState::Deleted);
    assert!(page.recovery.as_ref().unwrap().facts.old_delete_confirmed);
    assert_eq!(report.exit_code(), 1);
    assert_eq!(report.summary().warnings, 1);
    server.inspect(|requests| assert_eq!(requests.len(), 12));
    private(&report);
}

///
/// 未実行へ混入した復旧事実も変換で落とさず拒否payloadへ残す。
///
/// # 概要
/// 旧新ID/削除/lock/pathの観測を未実行へ載せ、記録側の拒否へ通す。
///
/// # 注記
/// 正常な空の未実行は復旧情報なし、異常な観測は全て保持と確認する。
///
/// # 戻り値
/// なし。未実行の元事実が消失した場合や正常完了になれば失敗する。
///
#[test]
fn not_executed_observations_survive_assignment_and_rejection() {
    let identity = crate::report::types::PageIdentity {
        path: crate::report::types::PagePath::new("/fixture/a").unwrap(),
        source: crate::report::types::SourceReference::new(
            crate::report::types::SourceKind::Zip, "a.md").unwrap() };
    /*
     * 正常な未実行の空事実は架空の復旧情報にしない
     */
    let empty = payload(KnownPageFacts::default(), PageExecution::NotExecuted,
        None, false, PageDetails::default(), ProcessingStage::Preflight);
    assert!(assigned(identity.clone(), empty).recovery.is_none());
    let mut details = PageDetails::default();
    details.observed = ObservedState::Deleted;
    details.lock_observed = Some(false);
    details.path_observed = Some(LookupResult::Found(id(NEW)));
    let value = payload(KnownPageFacts { old_id: Some(id(ID)),
        old_revision: Some(7), new_id: Some(id(NEW)),
        old_delete_confirmed: true, old_lock_observed: Some(true) },
        PageExecution::NotExecuted, None, false, details,
        ProcessingStage::Preflight);
    /*
     * 異常な未実行の全観測を記録拒否後も明示出力から確認する
     */
    let mut report = Report::planned(vec![identity.clone()]).unwrap();
    assert!(report.record_page(0, assigned(identity, value)).is_err());
    let output = report.render();
    for fact in [ID, NEW, "revision=Some(7)", "old_delete_confirmed=true",
        "old_lock_observed=Some(true)", "reconcile_lock_observed=Some(false)",
        "state=Found", "observed=Deleted"]
    { assert!(output.results.contains(fact), "fact={fact}"); }
    assert_eq!(report.summary().not_executed, 1);
    assert_eq!(report.summary().completed(), 0);
    assert_eq!(report.exit_code(), 1);
    private(&report);
}

///
/// 送信前open失敗をReportまで保持し再照合を始めない。
///
/// # 概要
/// 新規draftまで進め、消失した試験payloadのopenだけを失敗させる。
///
/// # 注記
/// 成立計画所有のaだけを削除し、b未実行と三要求を照合する。
///
/// # 戻り値
/// なし。元Io/未送信/確定ID/非露出が異なれば失敗する。
///
#[tokio::test]
async fn ms52_before_send_body_open_failure_keeps_report_and_no_recheck() {
    let shared = stop();
    let mut replies = creation(None);
    replies.pop();
    let server = Server::new(replies).await;
    let executor = runner(&server, &shared, Mode::Error, true).await;
    /*
     * Fileを閉じた後に試験所有payloadだけを消し送信準備を失敗させる
     */
    let file = executor.plan.open_body(0).unwrap();
    let path = location(&file);
    drop(file);
    std::fs::remove_file(path).unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await.unwrap();
    let report = into_report(batch);
    let page = &report.pages()[0];
    let primary = page.details.primary.as_ref().unwrap();
    assert_eq!(primary.operation, Some(DetailOperation::PutSource));
    assert_eq!(primary.stage, Some(ProcessingStage::PutSource));
    assert_eq!(primary.kind, DetailKind::Io);
    assert_eq!(primary.sent, Some(RequestState::NotStarted));
    assert_eq!(primary.status, None);
    /*
     * draftを保持し再照合・解除・後続を実行しない
     */
    assert_eq!(page.recovery.as_ref().unwrap().facts.draft_id,
        Some(id(ID)));
    assert!(page.details.auxiliary.is_empty());
    assert!(!page.details.cleanup_attempted);
    assert!(matches!(report.pages()[1].outcome, PageOutcome::NotExecuted));
    assert_eq!(report.exit_code(), 1);
    assert!(shared.checkpoint().is_ok());
    server.inspect(|requests| {
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].target.starts_with("/api/pages/id?path="));
        assert_eq!(requests[1].method, "POST");
        assert!(requests[1].target.starts_with("/api/pages?path="));
        assert_eq!(requests[2].method, "PUT");
        assert_eq!(requests[2].target, format!("/api/pages/{ID}/lock"));
    });
    private(&report);
}

///
/// peer部分受領切断を全量応答喪失と区別し安全停止する。
///
/// # 概要
/// 本文一byte後の無応答closeから単回再照合まで実経路を通す。
///
/// # 注記
/// アプリ受領量をwire量やworker引渡し量へ一般化しない。
///
/// # 戻り値
/// なし。元Transport/Unknown/未実行/要求数が異なれば失敗する。
///
#[tokio::test]
async fn ms52_partial_put_disconnect_preserves_uncertainty_and_stops() {
    let shared = stop();
    let mut replies = creation(None);
    replies.extend([identity(ID, false), metadata(false, false),
        Reply::new(200, b"different".to_vec())]);
    let server = Server::partial_put(replies, 4).await;
    /*
     * peerが部分受領したPUTの元失敗を再照合後も主原因に残す
     */
    let executor = runner(&server, &shared, Mode::Error, true).await;
    let batch = tokio::time::timeout(Duration::from_secs(5), executor.run())
        .await.unwrap();
    let report = into_report(batch);
    let page = &report.pages()[0];
    let primary = page.details.primary.as_ref().unwrap();
    assert_eq!(primary.operation, Some(DetailOperation::PutSource));
    assert_eq!(primary.stage, Some(ProcessingStage::PutSource));
    assert_eq!(primary.kind, DetailKind::Transport);
    assert_eq!(primary.sent, Some(RequestState::Started));
    assert_eq!(primary.status, None);
    assert_eq!(page.stage, ProcessingStage::Reconcile);
    assert_eq!(page.recovery.as_ref().unwrap().observed,
        ObservedState::Unknown);
    /*
     * 一byteの事実と単回GET列を別観測として照合する
     */
    let partial = server.partial().unwrap();
    assert_eq!(partial.ordinal, 4);
    assert_eq!(partial.received, 1);
    assert!(partial.received < partial.declared);
    server.inspect(|requests| {
        assert_eq!(requests.len(), 7);
        assert_eq!(requests[3].method, "PUT");
        assert_eq!(requests[3].target, format!("/api/pages/{ID}/source"));
        assert!(requests[4..].iter().all(|request| request.method == "GET"));
        assert!(requests[4].target.starts_with("/api/pages/id?path="));
        assert_eq!(requests[4].target, requests[0].target);
        assert_eq!(requests[5].target, format!("/api/pages/{ID}/meta"));
        assert_eq!(requests[6].target, format!("/api/pages/{ID}/source"));
    });
    assert!(!page.details.cleanup_attempted);
    assert!(matches!(report.pages()[1].outcome, PageOutcome::NotExecuted));
    assert_eq!(report.exit_code(), 1);
    assert!(shared.checkpoint().is_ok());
    private(&report);
}
