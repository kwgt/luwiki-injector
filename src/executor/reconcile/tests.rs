/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実runのPUT異常・単回照合・停止と元事実保持を検証する
//!

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::cmd_args::resolve::Mode;
use crate::executor::prepare::tests::mock::{Reply, Server, TOKEN2, TOKEN3};
use crate::executor::tests::{location, plan, stop};
use crate::executor::types::{ExecutionPage, LocalKind, PageExecution};
use crate::source::StopContext;

use super::*;

const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
const TOKEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

thread_local! {
    // 各test threadだけの有限checkpoint同期。製品の設定へ公開しない。
    static CANCEL_AT: Cell<Option<usize>> = const { Cell::new(None) };
    static DEADLINE: Cell<bool> = const { Cell::new(false) };
    static REMOVE_BODY: Cell<bool> = const { Cell::new(false) };
}

///
/// 再照合の同期点で同じ停止境界へ取消または期限を注入する。
///
/// # 引数
/// * `stop` - 元PUTから同じcontext
///
/// # 戻り値
/// なし。直後の製品checkpointが停止を返す。
///
pub(super) fn before_checkpoint(stop: &StopContext) {
    if DEADLINE.replace(false) {
        // 2秒以下の専用fixture期限だけを待つ。製品時計は変更しない。
        std::thread::sleep(stop.deadline().saturating_duration_since(
            Instant::now()));
    }
    CANCEL_AT.with(|slot| match slot.get() {
        Some(0) => { slot.set(None); stop.cancel(); }
        Some(value) => slot.set(Some(value - 1)),
        None => {}
    });
}

///
/// 独立受信所有者の保管障害を比較前に一度だけ注入する。
///
/// # 引数
/// * `body` - この試験で受信済みのreadonly領域
///
/// # 戻り値
/// なし。元PUTと独立したopen失敗を検証する。
///
pub(super) fn before_compare(body: &crate::wiki_client::ReceivedBody) {
    if REMOVE_BODY.replace(false) {
        std::fs::remove_file(location(&body.open().unwrap())).unwrap();
    }
}

///
/// 有効IDまたは確定draft作成の応答を準備する。
///
/// # 引数
/// * `id` - dummy ID
/// * `created` - 作成201とdummy lockを付けるか
///
/// # 戻り値
/// 完全JSON応答
///
fn identity(id: &str, created: bool) -> Reply {
    let mut reply = Reply::new(if created { 201 } else { 200 },
        format!("{{\"id\":\"{id}\"}}").into_bytes());
    reply.lock = created;
    reply
}

///
/// currentまたは削除済みmetadataを作る。
///
/// # 引数
/// * `path` - 期待または競合fixture path
/// * `deleted` - 削除済み観測
///
/// # 戻り値
/// mandatoryrevisionを持つ完全metadata
///
fn metadata(path: &str, deleted: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"{kind}\",\
        \"value\":\"{path}\"}},\"deleted\":{deleted},\"locked\":false}},\
        \"revision_info\":{{\"revision\":7}}}}").into_bytes())
}

///
/// 新規または相違既存の元PUTまでの応答を用意する。
///
/// # 引数
/// * `existing` - update既存を通るか
/// * `status` - 元PUTのstatus。Noneはヘッダー前応答喪失。
///
/// # 戻り値
/// 実通常processorが一度PUTするscript
///
fn prefix(existing: bool, status: Option<u16>) -> Vec<Reply> {
    /*
     * 元PUTまでの確定応答と異常応答を別々に用意する
     */
    let mut replies = if existing {
        let mut locked = metadata("/fixture/a", false);
        locked.body = String::from_utf8(locked.body).unwrap()
            .replace("\"locked\":false", "\"locked\":true").into_bytes();
        vec![identity(ID, false), metadata("/fixture/a", false),
            Reply::new(200, b"before".to_vec()), Reply::locked(TOKEN),
            Reply::locked(TOKEN2), locked, identity(ID, false),
            Reply::new(200, b"before".to_vec()), Reply::locked(TOKEN3)]
    } else {
        vec![Reply::new(404, Vec::new()), identity(ID, true),
            Reply::locked(TOKEN2)]
    };
    replies.push(Reply { status, body: b"{}".to_vec(), lock: false,
        token: TOKEN });
    replies
}

///
/// 全件検証済み入力と同じ停止境界で通常Executorを作る。
///
/// # 引数
/// * `server` - このcase専用script
/// * `shared` - 元操作から共有するcontext
/// * `later` - 警告後の次ページも計画するか
///
/// # 戻り値
/// 単回実行する製品executor
///
async fn runner(server: &Server, shared: &StopContext, later: bool)
    -> Executor
{
    let config = crate::cmd_args::resolve::wiki_config_fixture(&server.url,
        true, shared.deadline());
    let entries: &[(&[u8], &[u8])] = if later {
        &[(b"a.md", b"target"), (b"b.md", b"later")]
    } else { &[(b"a.md", b"target")] };
    let mut executor = Executor::new(plan(entries, shared).await, &config,
        shared.clone()).unwrap();
    executor.mode = Mode::Update;
    executor
}

///
/// 正常な目的状態の再照合応答を追加する。
///
/// # 引数
/// * `replies` - 元操作を含むscript
///
/// # 戻り値
/// なし。ID→meta→本文の照合順を固定する。
///
fn applied(replies: &mut Vec<Reply>) {
    replies.extend([identity(ID, false), metadata("/fixture/a", false),
        Reply::new(200, b"target".to_vec())]);
}

///
/// 通常runの一件だけを取り出し余分な結果を拒否する。
///
/// # 引数
/// * `batch` - 実行済み製品結果
///
/// # 戻り値
/// 所有情報を失わないページ結果
///
fn only(batch: super::super::types::ExecutionBatch) -> ExecutionPage {
    let (mut pages, _, _, rejected) = batch.into_parts();
    assert_eq!(pages.len(), 1);
    assert!(rejected.is_empty());
    pages.remove(0)
}

///
/// 新規/updateの500/応答喪失は目的状態を確認して警告付き適用する。
///
/// # 概要
/// 元PUTだけ一度送り、lease再利用と全量本文保持を要求しない。
///
/// # 注記
/// 実runへ新規/既存とstatus有無の4caseを通し所有情報とURIを確認。
///
/// # 戻り値
/// なし。元事実消失または余分な変更要求で失敗する。
///
#[tokio::test]
async fn confirms_create_and_update_without_resending_or_releasing() {
    /*
     * 出自と元statusごとに製品runを通し警告/資源/要求を照合する
     */
    for existing in [false, true] {
        for status in [Some(500), None] {
            /*
             * 元異常を目的状態で確認し単回変更と秘匿を検査する
             */
            let shared = stop();
            let mut replies = prefix(existing, status);
            applied(&mut replies);
            let server = Server::new(replies).await;
            let page = only(runner(&server, &shared, false).await.run().await);
            let (_, known, stage, result, cleanup, attempted, details) =
                page.into_parts();
            let expected = if existing { Completion::Updated }
                else { Completion::Created };
            assert!(matches!(result, PageExecution::AppliedWithWarning(v)
                if v == expected));
            assert_eq!(stage, ProcessingStage::Reconcile);
            assert_eq!(details.observed, ObservedState::Applied);
            assert_eq!(details.warnings.len(), 1);
            assert_eq!(details.warnings[0].stage,
                Some(ProcessingStage::PutSource));
            let facts = details.warnings[0].facts();
            assert_eq!(facts.status(), status);
            assert_eq!(facts.operation(), WikiOperation::PutSource);
            assert_eq!(facts.known_id().unwrap().as_str(), ID);
            assert!(details.auxiliary.is_empty() && details.stop.is_none());
            assert!(!attempted && cleanup.is_none());
            assert_eq!(known.into_parts().2.is_some(), !existing);
            server.inspect(|requests| {
                assert_eq!(requests.len(), if existing { 13 } else { 7 });
                assert_eq!(requests.iter().filter(|r|
                    r.target.ends_with("/source") && r.method == "PUT")
                    .count(), 1);
                assert!(!requests.iter().any(|r| r.method == "DELETE"));
            });
            assert!(shared.checkpoint().is_ok());
        }
    }
}

///
/// 警告付き適用で通常の後続ページを処理する。
///
/// # 注記
/// 最初の新規PUT500確認後にbを新規作成し、結果を二重計上しない。
///
/// # 戻り値
/// なし。警告で通常継続を誤停止すれば失敗する。
///
#[tokio::test]
async fn applied_warning_allows_next_normal_page() {
    /*
     * 警告付き第一ページと通常の第二ページを同じ計画で実行する
     */
    let shared = stop();
    let mut replies = prefix(false, Some(500));
    applied(&mut replies);
    replies.extend([Reply::new(404, Vec::new()), identity(OTHER, true),
        Reply::locked(TOKEN2), Reply::new(204, Vec::new())]);
    let server = Server::new(replies).await;
    let (pages, first, global, _) = runner(&server, &shared, true).await
        .run().await.into_parts();
    assert!(first.is_none() && global.is_none());
    assert!(matches!(pages[0].execution(),
        PageExecution::AppliedWithWarning(Completion::Created)));
    assert!(matches!(pages[1].execution(),
        PageExecution::Completed(Completion::Created)));
    server.inspect(|requests| assert_eq!(requests.len(), 11));
}

///
/// 別ID・不存在・path差・削除・本文差では適用を補完しない。
///
/// # 概要
/// 各失敗時の必要最小GET数と元PUT/後続未実行を照合する。
///
/// # 注記
/// 同一script先頭に分岐別の観測を置き、不要なsource取得も拒否する。
///
/// # 戻り値
/// なし。差を未適用と因果推測した場合も失敗する。
///
#[tokio::test]
async fn mismatched_observations_stop_with_original_put_failure() {
    /*
     * 同じ元PUTへ異なる観測を返し停止位置と要求数を比較する
     */
    for case in 0..5 {
        /*
         * 分岐ごとの最短観測列を実行する
         */
        let shared = stop();
        let mut replies = prefix(false, Some(500));
        let (tail, extra, observed) = match case {
            0 => (vec![identity(OTHER, false)], 1, ObservedState::Unknown),
            1 => (vec![Reply::new(404, Vec::new())], 1,
                ObservedState::Unknown),
            2 => (vec![identity(ID, false), metadata("/other", false)], 2,
                ObservedState::Unknown),
            3 => (vec![identity(ID, false), metadata("/fixture/a", true)], 2,
                ObservedState::Deleted),
            _ => (vec![identity(ID, false), metadata("/fixture/a", false),
                Reply::new(200, b"other".to_vec())], 3,
                ObservedState::Unknown),
        };
        replies.extend(tail);
        let server = Server::new(replies).await;
        let (mut pages, first, _, _) = runner(&server, &shared, true).await
            .run().await.into_parts();
        assert_eq!(first, Some(0));
        assert!(matches!(pages[1].execution(), PageExecution::NotExecuted));
        let (_, _, stage, result, _, attempted, details) =
            pages.remove(0).into_parts();
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)
                && v.stage() == ProcessingStage::PutSource));
        assert_eq!(stage, ProcessingStage::Reconcile);
        assert_eq!(details.observed, observed);
        assert!(!attempted && details.auxiliary.is_empty());
        server.inspect(|requests| assert_eq!(requests.len(), 4 + extra));
    }
}

///
/// 再照合のGET失敗/不正UTF8は主PUTを置き換えない。
///
/// # 概要
/// 異常の位置と種別を変えて元PUT/補助原因を独立に照合する。
///
/// # 注記
/// lookup503、meta404、source410、不正UTF8の各補助失敗を所有確認する。
///
/// # 戻り値
/// なし。元status・補助caller段階・失敗分類が消えれば失敗する。
///
#[tokio::test]
async fn reconciliation_errors_remain_auxiliary_to_original_put() {
    /*
     * 各GET境界の異常を独立に注入し主原因と補助原因を照合する
     */
    for case in 0..4 {
        /*
         * 元PUTの後へ失敗するGETを一度だけ置く
         */
        let shared = stop();
        let mut replies = prefix(false, Some(500));
        if case > 0 { replies.push(identity(ID, false)); }
        if case > 1 { replies.push(metadata("/fixture/a", false)); }
        replies.push(match case {
            0 => Reply::new(503, Vec::new()),
            1 => Reply::new(404, Vec::new()),
            2 => Reply::new(410, Vec::new()),
            _ => Reply::new(200, vec![0xff]),
        });
        let server = Server::new(replies).await;
        let page = only(runner(&server, &shared, false).await.run().await);
        let (_, _, _, result, _, _, details) = page.into_parts();
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)));
        assert_eq!(details.observed, ObservedState::Unknown);
        assert_eq!(details.auxiliary.len(), 1);
        assert_eq!(details.auxiliary[0].stage(), ProcessingStage::Reconcile);
        let ExecutorFailure::Wiki(aux) = &details.auxiliary[0] else {
            panic!("expected GET failure");
        };
        if case == 3 { assert_eq!(aux.facts().kind(), FailureKind::BodyUtf8); }
        server.inspect(|requests| assert_eq!(requests.len(),
            5 + case.min(2)));
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// 再照合の各間隙で取消した後はGET/解除を開始しない。
///
/// # 概要
/// 段階間の取消を独立に注入し元PUTと停止理由を別に照合する。
///
/// # 注記
/// lookup前/meta前/source前/比較前/比較後で同じcontextを取消する。
///
/// # 戻り値
/// なし。停止後HTTPまたは主PUTの喪失で失敗する。
///
#[tokio::test]
async fn cancellation_in_every_reconciliation_gap_prevents_followups() {
    /*
     * 再照合の同期点ごとに同じ取消を通知し余分な要求を検出する
     */
    for gap in 0..5 {
        /*
         * 当該gapだけで取消し直後からHTTPを禁止する
         */
        let shared = stop();
        let mut replies = prefix(false, Some(500));
        applied(&mut replies);
        let server = Server::new(replies).await;
        let executor = runner(&server, &shared, false).await;
        CANCEL_AT.set(Some(gap));
        let (_, _, _, result, _, attempted, details) =
            only(executor.run().await).into_parts();
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)));
        assert!(matches!(&details.auxiliary[0], ExecutorFailure::Local(v)
            if v.kind() == LocalKind::Cancelled));
        assert_eq!(details.observed, ObservedState::Unknown);
        assert!(!attempted);
        server.inspect(|requests| assert_eq!(requests.len(),
            4 + gap.min(3)));
    }
}

///
/// 再照合前の同じ絶対期限到達で元失敗を保ちHTTP0にする。
///
/// # 注記
/// 2秒のfixture期限まで同期点で待ち、新たな猶予を追加しない。
///
/// # 戻り値
/// なし。元期限分類を失ったりGETを開始すれば失敗する。
///
#[tokio::test]
async fn deadline_before_reconciliation_preserves_original_failure() {
    let shared = StopContext::new(Instant::now() + Duration::from_secs(2));
    let server = Server::new(prefix(false, Some(500))).await;
    let executor = runner(&server, &shared, false).await;
    DEADLINE.set(true);
    let (_, _, _, result, _, attempted, details) =
        only(executor.run().await).into_parts();
    assert!(matches!(result, PageExecution::Failed(
        ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)));
    assert!(matches!(&details.auxiliary[0], ExecutorFailure::Local(v)
        if v.kind() == LocalKind::Deadline));
    assert!(!attempted);
    server.inspect(|requests| assert_eq!(requests.len(), 4));
}

///
/// 比較保管openのI/O失敗を元PUTとは独立して保持する。
///
/// # 注記
/// 再照合sourceを正常受信後にfixtureの保管だけを削除する。
///
/// # 戻り値
/// なし。局所障害が適用成功や新HTTP失敗へ化ければ失敗する。
///
#[tokio::test]
async fn received_storage_failure_is_local_and_preserves_put() {
    /*
     * 正常受信後の保管失敗だけを注入し元PUTと別所有を照合する
     */
    let shared = stop();
    let mut replies = prefix(false, Some(500));
    applied(&mut replies);
    let server = Server::new(replies).await;
    let executor = runner(&server, &shared, false).await;
    REMOVE_BODY.set(true);
    let (_, _, _, result, _, _, details) =
        only(executor.run().await).into_parts();
    assert!(matches!(result, PageExecution::Failed(
        ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)));
    assert!(matches!(&details.auxiliary[0], ExecutorFailure::Local(v)
        if v.kind() == LocalKind::ReceivedOpen));
    assert_eq!(details.observed, ObservedState::Unknown);
    assert!(shared.checkpoint().is_ok());
}

///
/// 適用警告/元statusなし事実のDebugは本文・ID・秘密を表示しない。
///
/// # 注記
/// ヘッダー前喪失を目的状態で確認し、owned出口と安全表示を確認する。
///
/// # 戻り値
/// なし。元warning喪失または外部値の露出で失敗する。
///
#[tokio::test]
async fn warning_move_keeps_no_status_facts_and_safe_debug() {
    let shared = stop();
    let mut replies = prefix(false, None);
    applied(&mut replies);
    let server = Server::new(replies).await;
    let page = only(runner(&server, &shared, false).await.run().await);
    let (_, _, _, _, _, _, details) = page.into_parts();
    let rendered = format!("{details:?} {:?}", details.warnings);
    for value in [ID, TOKEN, "target", "fixture", "Bearer"] {
        assert!(!rendered.contains(value));
    }
    assert!(details.warnings[0].facts().status().is_none());
    assert!(details.warnings[0].warning().is_none());
    assert_eq!(details.warnings[0].observed(), ObservedState::Applied);
}

///
/// 204観測後のupload終了I/O失敗も再照合して元kindを保つ。
///
/// # 概要
/// 送信workerが全fixture bytes引渡し後にだけIoを返すよう指定する。
///
/// # 注記
/// clientの正常statusを適用成功へ短絡せず、GET三件で確認する。
///
/// # 戻り値
/// なし。元kind/status消失または照合省略で失敗する。
///
#[tokio::test]
async fn success_status_with_upload_failure_requires_reconciliation() {
    /*
     * 送信終了失敗と正常statusを同時保持して三GET照合へ進める
     */
    let shared = stop();
    let mut replies = prefix(false, Some(204));
    applied(&mut replies);
    let server = Server::new(replies).await;
    let executor = runner(&server, &shared, false).await;
    crate::wiki_client::adapter::fail_upload_exit_fixture();
    let (_, _, _, result, _, attempted, details) =
        only(executor.run().await).into_parts();
    assert!(matches!(result,
        PageExecution::AppliedWithWarning(Completion::Created)));
    let facts = details.warnings[0].facts();
    assert_eq!(facts.status(), Some(204));
    assert_eq!(facts.kind(), FailureKind::Io);
    assert!(!attempted);
    server.inspect(|requests| assert_eq!(requests.len(), 7));
}

///
/// 再作成PUTは目的状態確認済みでも後続停止理由を残す。
///
/// # 概要
/// 実overwriteで旧削除/新POST後のPUT500を確認し次件HTTP0を照合。
///
/// # 注記
/// 同じcaseを一件計画/二件計画に通し最終ページの停止も保持する。
///
/// # 戻り値
/// なし。AppliedWithWarningをFailedへ潰すか後続へ進めば失敗する。
///
#[tokio::test]
async fn recreation_put_warning_keeps_independent_stop_even_on_last_page() {
    /*
     * 旧削除後のPUTだけ失敗させ確認後の独立停止を照合する
     */
    for later in [false, true] {
        /*
         * 正常削除後の新IDへだけ不確定PUTと照合を行う
         */
        let shared = stop();
        let mut replies = prefix(true, Some(204));
        // prefixの最後だけ旧DELETEとして利用し、その後に新draftを作る。
        replies.extend([identity(OTHER, true), Reply::locked(TOKEN2),
            Reply::new(500, b"{}".to_vec()), identity(OTHER, false),
            metadata("/fixture/a", false),
            Reply::new(200, b"target".to_vec())]);
        let server = Server::new(replies).await;
        let mut executor = runner(&server, &shared, later).await;
        executor.mode = Mode::Overwrite;
        let (mut pages, first, global, _) = executor.run().await.into_parts();
        assert!(first.is_none() && global.is_none());
        if later {
            assert!(matches!(pages[1].execution(), PageExecution::NotExecuted));
        }
        let (_, known, _, result, _, attempted, details) =
            pages.remove(0).into_parts();
        assert!(matches!(result,
            PageExecution::AppliedWithWarning(Completion::Recreated)));
        assert_eq!(details.stop, Some(StopReason::RecreationUncertain));
        assert_eq!(details.warnings.len(), 1);
        assert!(known.old_delete_confirmed());
        assert!(!attempted);
        server.inspect(|requests| assert_eq!(requests.len(), 16));
    }
}

///
/// 契約外の成功statusも成功へ短絡せず目的状態を確認する。
///
/// # 概要
/// 期待204と異なる200を元Http事実として残し、三GETで確認する。
///
/// # 注記
/// 新規/既存の双方で確定statusと適用操作を失わないことを確認。
///
/// # 戻り値
/// なし。200を無警告成功にしたり元statusを失えば失敗する。
///
#[tokio::test]
async fn unexpected_success_status_is_reconciled_with_warning() {
    /*
     * 異常な成功statusを同じIDの目的状態へ照合する
     */
    for existing in [false, true] {
        /*
         * 目的状態を確認しても元の契約外statusを警告へ保持する
         */
        let shared = stop();
        let mut replies = prefix(existing, Some(200));
        applied(&mut replies);
        let server = Server::new(replies).await;
        let (_, _, _, result, _, attempted, details) =
            only(runner(&server, &shared, false).await.run().await)
                .into_parts();
        assert!(matches!(result, PageExecution::AppliedWithWarning(_)));
        assert_eq!(details.warnings[0].facts().status(), Some(200));
        assert_eq!(details.warnings[0].facts().kind(), FailureKind::Http);
        assert_eq!(details.observed, ObservedState::Applied);
        assert!(!attempted);
        server.inspect(|requests| assert_eq!(requests.len(),
            if existing { 13 } else { 7 }));
    }
}

mod delete;
