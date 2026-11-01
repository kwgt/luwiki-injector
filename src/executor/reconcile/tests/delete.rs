/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実overwriteのDELETE確認・条件付き再作成・警告/停止の接続試験
//!

use crate::executor::normal::tests::pause;

use super::*;

const THIRD: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";

///
/// 相違旧ページのDELETEまで正常に進むscriptを用意する。
///
/// # 引数
/// * `reply` - 最後の旧DELETEに対する応答
///
/// # 戻り値
/// 旧IDの再照合/明示renewを含む十要求分の応答
///
fn deletion(reply: Reply) -> Vec<Reply> {
    let mut replies = prefix(true, Some(500));
    replies.pop();
    replies.push(reply);
    replies
}

///
/// 旧ID削除と有効path不存在の独立した応答を追加する。
///
/// # 引数
/// * `replies` - 元DELETEまでのscript
/// * `status` - path不存在の404または410
///
/// # 戻り値
/// なし。metaを先に返しID404を削除の代用にしない。
///
fn deleted(replies: &mut Vec<Reply>, status: u16) {
    replies.extend([metadata("/fixture/a", true),
        Reply::new(status, Vec::new())]);
}

///
/// 別の新IDへ通常のPOST/renew/PUTを追加する。
///
/// # 引数
/// * `replies` - 削除確認までのscript
///
/// # 戻り値
/// なし。旧leaseと異なる新draftの資源を返す。
///
fn recreated(replies: &mut Vec<Reply>) {
    replies.extend([identity(OTHER, true), Reply::locked(TOKEN2),
        Reply::new(204, Vec::new())]);
}

///
/// 同じ成立計画/contextでoverwriteを実行する所有者を用意する。
///
/// # 引数
/// * `server` - fixture専用socket
/// * `shared` - 元取得から同じ停止境界
/// * `later` - 未実行または継続を検証する次ページを含むか
///
/// # 戻り値
/// 未実行の製品executor
///
async fn overwrite(server: &Server, shared: &StopContext, later: bool)
    -> Executor
{
    let mut executor = runner(server, shared, later).await;
    executor.mode = Mode::Overwrite;
    executor
}

///
/// 同じ変更の再送・old PUT・自動回収がないことをURIで確認する。
///
/// # 概要
/// 旧lock取得POSTと新page POST、旧削除とlock解除を区別する。
///
/// # 引数
/// * `server` - 完全要求列の所有者
/// * `posts` - 新draft POSTの期待数
/// * `puts` - 新ID本文PUTの期待数
///
/// # 戻り値
/// なし。追加変更や自動復元があれば失敗する。
///
fn changes(server: &Server, posts: usize, puts: usize) {
    server.inspect(|requests| {
        /*
         * 変更URIを個別に数え、別操作で同じmethodが混ざらないようにする
         */
        assert_eq!(requests.iter().filter(|r| r.method == "POST"
            && r.target.starts_with("/api/pages?")).count(), posts);
        assert_eq!(requests.iter().filter(|r| r.method == "DELETE"
            && r.target == format!("/api/pages/{ID}")).count(), 1);
        assert_eq!(requests.iter().filter(|r| r.method == "PUT"
            && r.target == format!("/api/pages/{OTHER}/source")).count(), puts);
        assert!(!requests.iter().any(|r| r.method == "PUT"
            && r.target == format!("/api/pages/{ID}/source")));
        assert!(requests.iter().filter(|r| r.method == "DELETE")
            .all(|r| r.target == format!("/api/pages/{ID}")));
        assert!(requests.iter().all(|r| !r.target.contains("recursive=")
            && !r.target.contains("amend=")));
    });
}

///
/// 先行DELETE500警告の元事実とcaller段階を確認する。
///
/// # 引数
/// * `details` - 結果のowned補助情報
///
/// # 戻り値
/// なし。後続失敗で警告が消えた場合も失敗する。
///
fn delete_warning(details: &PageDetails) {
    let record = &details.warnings[0];
    assert_eq!(record.stage, Some(ProcessingStage::DeleteOld));
    assert_eq!(record.facts().operation(), WikiOperation::DeletePage);
    assert_eq!(record.facts().status(), Some(500));
    assert_eq!(record.facts().known_id().unwrap().as_str(), ID);
    assert_eq!(record.observed(), ObservedState::Deleted);
}

///
/// 完全DELETE500と両方の削除確認後だけ警告付き再作成を完了する。
///
/// # 概要
/// path404/410の双方と先行警告を持つ次件継続を実runで照合する。
///
/// # 注記
/// 最初のページの旧新IDと、次ページの正常作成を同時に確認する。
///
/// # 戻り値
/// なし。無警告成功や余分な旧変更で失敗する。
///
#[tokio::test]
async fn complete_500_recreates_only_after_both_observations() {
    for absence in [404, 410] {
        /*
         * 削除確認→再作成→別ページの通常作成を一つの計画へ通す
         */
        let shared = stop();
        let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
        deleted(&mut replies, absence);
        recreated(&mut replies);
        replies.extend([Reply::new(404, Vec::new()), identity(THIRD, true),
            Reply::locked(TOKEN2), Reply::new(204, Vec::new())]);
        let server = Server::new(replies).await;
        let (mut pages, first, global, rejected) =
            overwrite(&server, &shared, true).await.run().await.into_parts();
        assert!(first.is_none() && global.is_none() && rejected.is_empty());
        assert!(matches!(pages[1].execution(),
            PageExecution::Completed(Completion::Created)));
        let (_, known, stage, result, cleanup, attempted, details) =
            pages.remove(0).into_parts();
        assert!(matches!(result,
            PageExecution::AppliedWithWarning(Completion::Recreated)));
        assert_eq!(stage, ProcessingStage::Finished);
        assert_eq!(details.observed, ObservedState::Applied);
        assert!(details.stop.is_none() && details.auxiliary.is_empty());
        assert!(!attempted && cleanup.is_none());
        delete_warning(&details);
        let (old, revision, new, confirmed, _) = known.into_parts();
        assert_eq!(old.unwrap().as_str(), ID);
        assert_eq!(new.unwrap().as_str(), OTHER);
        assert_eq!(revision, Some(7));
        assert!(confirmed);
        changes(&server, 2, 1);
        server.inspect(|requests| assert_eq!(requests.len(), 19));
    }
}

///
/// 再作成の各段階で失敗しても先行DELETE警告と復旧情報を保持する。
///
/// # 概要
/// POST拒否/喪失/不正201、renew失敗、PUT拒否/照合失敗を独立検証。
///
/// # 注記
/// 同じ旧情報を保ち、後続ページが未実行で新IDの確定範囲も確認する。
///
/// # 戻り値
/// なし。先行警告/主原因消失やdraft回収で失敗する。
///
#[tokio::test]
async fn recreation_failures_retain_delete_warning_and_old_identity() {
    for case in 0..7 {
        /*
         * 削除確認済みの同じ起点から別の後続操作だけを失敗させる
         */
        let shared = stop();
        let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
        deleted(&mut replies, 404);
        let (stage, total, new_known, puts) = match case {
            /*
             * POST段階の拒否・喪失・不正応答を分離する
             */
            0 => {
                replies.push(Reply::new(403, b"{}".to_vec()));
                (ProcessingStage::CreateDraft, 13, false, 0)
            }
            1 => {
                replies.push(Reply { status: None, body: Vec::new(),
                    lock: false, token: TOKEN });
                (ProcessingStage::CreateDraft, 13, false, 0)
            }
            2 => {
                // 完全JSONのIDだけ確定し、不正lockではPUTへ進めない。
                let mut reply = identity(OTHER, true);
                reply.lock = false;
                replies.push(reply);
                (ProcessingStage::CreateDraft, 13, true, 0)
            }
            3 => {
                let mut reply = identity(OTHER, true);
                reply.body = b"{\"id\":\"partial".to_vec();
                replies.push(reply);
                (ProcessingStage::CreateDraft, 13, false, 0)
            }
            /*
             * 作成後の延長・本文PUT・照合失敗を分離する
             */
            4 => {
                replies.extend([identity(OTHER, true),
                    Reply::new(500, b"{}".to_vec())]);
                (ProcessingStage::PutSource, 14, true, 0)
            }
            _ => {
                replies.extend([identity(OTHER, true), Reply::locked(TOKEN2),
                    Reply::new(if case == 5 { 400 } else { 500 },
                        b"{}".to_vec())]);
                if case == 6 { replies.push(Reply::new(503, b"{}".to_vec())); }
                (ProcessingStage::PutSource,
                    if case == 5 { 15 } else { 16 }, true, 1)
            }
        };
        let server = Server::new(replies).await;
        let (mut pages, first, _, rejected) =
            overwrite(&server, &shared, true).await.run().await.into_parts();
        assert_eq!(first, Some(0));
        assert!(rejected.is_empty());
        assert!(matches!(pages[1].execution(), PageExecution::NotExecuted));
        let (_, known, last, result, cleanup, attempted, details) =
            pages.remove(0).into_parts();
        assert!(matches!(&result, PageExecution::Failed(v)
            if v.stage() == stage));
        assert_eq!(last, if case == 6 { ProcessingStage::Reconcile }
            else { stage });
        assert!(!attempted && cleanup.is_none());
        delete_warning(&details);
        assert_eq!(details.warnings.len(), 1);
        let (old, revision, new, confirmed, _) = known.into_parts();
        assert_eq!(old.unwrap().as_str(), ID);
        assert_eq!(revision, Some(7));
        assert!(confirmed);
        assert_eq!(new.is_some(), new_known);
        if let Some(new) = new { assert_eq!(new.as_str(), OTHER); }
        changes(&server, 1, puts);
        server.inspect(|requests| assert_eq!(requests.len(), total));
    }
}

///
/// DELETE/再作成PUTの両異常を発生順に残し目的状態でも停止する。
///
/// # 概要
/// 元DELETEと新PUTのstatus有無を組み合わせ、次件を開始しない。
///
/// # 注記
/// 再作成の目的状態は確認するが最終ページの場合もStopを保持する。
///
/// # 戻り値
/// なし。警告順消失や継続許可の緩和で失敗する。
///
#[tokio::test]
async fn two_mutation_incidents_remain_ordered_and_stop_after_confirmation() {
    for status in [Some(500), None] {
        for later in [false, true] {
            /*
             * 正常削除確認後、新IDの異常PUTだけを目的状態へ照合する
             */
            let shared = stop();
            let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
            deleted(&mut replies, 404);
            replies.extend([identity(OTHER, true), Reply::locked(TOKEN2),
                Reply { status, body: b"{}".to_vec(), lock: false,
                    token: TOKEN }, identity(OTHER, false),
                metadata("/fixture/a", false),
                Reply::new(200, b"target".to_vec())]);
            let server = Server::new(replies).await;
            let (mut pages, first, _, _) =
                overwrite(&server, &shared, later).await.run().await
                    .into_parts();
            assert!(first.is_none());
            if later {
                assert!(matches!(pages[1].execution(),
                    PageExecution::NotExecuted));
            }
            let (_, known, _, result, _, attempted, details) =
                pages.remove(0).into_parts();
            assert!(matches!(result,
                PageExecution::AppliedWithWarning(Completion::Recreated)));
            assert_eq!(details.stop, Some(StopReason::RecreationUncertain));
            assert_eq!(details.warnings.len(), 2);
            assert!(matches!(&details.path_observed,
                Some(LookupResult::Found(id)) if id.as_str() == OTHER));
            let display = format!("{details:?} {:?}", details.warnings);
            for value in [ID, OTHER, TOKEN, "target", "before"] {
                assert!(!display.contains(value));
            }
            delete_warning(&details);
            assert_eq!(details.warnings[1].stage,
                Some(ProcessingStage::PutSource));
            assert_eq!(details.warnings[1].facts().status(), status);
            assert_eq!(details.warnings[1].facts().known_id().unwrap().as_str(),
                OTHER);
            assert_eq!(details.warnings[1].observed(), ObservedState::Applied);
            assert!(known.old_delete_confirmed() && !attempted);
            changes(&server, 1, 1);
            server.inspect(|requests| assert_eq!(requests.len(), 18));
        }
    }
}

///
/// DELETE通信状態不明/不完全500/不正成功は削除確認済みでも停止する。
///
/// # 概要
/// HTTP status受信と完全応答を区別し元kind/statusと観測を保持する。
///
/// # 注記
/// ヘッダー前喪失・不正200・不正500本文・500本文切断を独立確認。
///
/// # 戻り値
/// なし。削除確認だけで新POSTを開始した場合に失敗する。
///
#[tokio::test]
async fn uncertain_delete_never_recreates_even_when_deletion_is_observed() {
    for case in 0..4 {
        /*
         * どの異常も削除は確認できるが完全500だけの許可を与えない
         */
        let shared = stop();
        let reply = match case {
            0 => Reply { status: None, body: Vec::new(), lock: false,
                token: TOKEN },
            1 => Reply::new(200, b"{}".to_vec()),
            2 => Reply::new(500, b"{".to_vec()),
            _ => Reply::new(500, b"{}".to_vec()),
        };
        let status = reply.status;
        let mut replies = deletion(reply);
        deleted(&mut replies, 410);
        let server = if case == 3 { Server::truncated(replies, 10).await }
            else { Server::new(replies).await };
        let page = only(overwrite(&server, &shared, false).await.run().await);
        let (_, known, last, result, _, attempted, details) = page.into_parts();
        let PageExecution::Failed(ExecutorFailure::Wiki(original)) = result
            else { panic!("original DELETE"); };
        assert_eq!(original.stage(), ProcessingStage::DeleteOld);
        assert_eq!(original.facts().status(), status);
        if case >= 2 { assert_ne!(original.facts().kind(), FailureKind::Http); }
        if case == 3 {
            assert_eq!(original.facts().kind(), FailureKind::Transport);
        }
        assert_eq!(last, ProcessingStage::Reconcile);
        assert_eq!(details.observed, ObservedState::Deleted);
        assert!(known.old_delete_confirmed() && !attempted);
        assert!(details.auxiliary.is_empty());
        changes(&server, 0, 0);
        server.inspect(|requests| assert_eq!(requests.len(), 12));
    }
}

///
/// metadataとpathが両条件を満たさない場合は再作成しない。
///
/// # 概要
/// 旧ID不存在/現存/移動と、有効path再利用を異なる観測として残す。
///
/// # 注記
/// 途中の不成立では不要な後続GETを送らず他者IDを新IDに補完しない。
///
/// # 戻り値
/// なし。片方の404や別IDから削除・空pathを推測すれば失敗する。
///
#[tokio::test]
async fn incomplete_or_conflicting_delete_observations_never_create() {
    for case in 0..5 {
        /*
         * 条件不成立または旧meta不在を別scriptへ分離する
         */
        let shared = stop();
        let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
        let (extra, confirmed) = match case {
            0 => { replies.push(Reply::new(404, b"{}".to_vec())); (1, false) }
            1 => { replies.push(metadata("/fixture/a", false)); (1, false) }
            2 => { replies.push(metadata("/other", true)); (1, true) }
            _ => {
                replies.extend([metadata("/fixture/a", true),
                    identity(if case == 3 { OTHER } else { ID }, false)]);
                (2, true)
            }
        };
        let server = Server::new(replies).await;
        let page = only(overwrite(&server, &shared, false).await.run().await);
        let (_, known, _, result, _, attempted, details) = page.into_parts();
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v)) if v.facts().status() == Some(500)));
        assert_eq!(known.old_delete_confirmed(), confirmed);
        assert!(known.into_parts().2.is_none());
        if case >= 3 {
            assert!(matches!(details.path_observed,
                Some(LookupResult::Found(_))));
        } else { assert!(details.path_observed.is_none()); }
        assert!(!attempted);
        changes(&server, 0, 0);
        server.inspect(|requests| assert_eq!(requests.len(), 10 + extra));
    }
}

///
/// 再照合GET失敗でも旧削除観測と主DELETEを別々に維持する。
///
/// # 概要
/// 失敗位置に応じて確認できた旧削除と補助原因を区別する。
///
/// # 注記
/// meta503とlookup503を別に注入し、成功した部分観測だけ残す。
///
/// # 戻り値
/// なし。補助GETが主失敗を上書きした場合も失敗する。
///
#[tokio::test]
async fn delete_reconciliation_failure_keeps_partial_facts() {
    /*
     * 旧meta成功の有無によって削除確認が異なることを確かめる
     */
    for after_meta in [false, true] {
        /*
         * meta観測の有無を変えて主DELETEと補助GETを別に保持する
         */
        let shared = stop();
        let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
        if after_meta { replies.push(metadata("/fixture/a", true)); }
        replies.push(Reply::new(503, b"{}".to_vec()));
        let server = Server::new(replies).await;
        let page = only(overwrite(&server, &shared, false).await.run().await);
        let (_, known, _, result, _, attempted, details) = page.into_parts();
        assert_eq!(known.old_delete_confirmed(), after_meta);
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v)) if v.stage() == ProcessingStage::DeleteOld
                && v.facts().status() == Some(500)));
        assert_eq!(details.auxiliary.len(), 1);
        assert_eq!(details.auxiliary[0].stage(), ProcessingStage::Reconcile);
        assert!(!attempted);
        changes(&server, 0, 0);
        server.inspect(|requests| assert_eq!(requests.len(),
            if after_meta { 12 } else { 11 }));
    }
}

///
/// DELETE再照合の取消は部分観測を残し全追加HTTPを止める。
///
/// # 概要
/// meta前/lookup前/lookup後に取消して最終Reportまで元事実を照合する。
///
/// # 注記
/// 同じ停止flagで元DELETE500・補助Cancelled・後続未実行を保持する。
///
/// # 戻り値
/// なし。取消後にPOST/GET/解除した場合に失敗する。
///
#[tokio::test]
async fn cancel_in_delete_reconciliation_prevents_any_followup() {
    for gap in 0..3 {
        /*
         * 同期checkpointごとに取消し成功した部分観測だけを残す
         */
        let shared = stop();
        let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
        deleted(&mut replies, 404);
        let server = Server::new(replies).await;
        let executor = overwrite(&server, &shared, true).await;
        CANCEL_AT.set(Some(gap));
        let report = crate::executor::report::into_report(executor.run().await);
        /*
         * 所有変換後も主DELETEと補助取消・後続未実行を独立に保持する
         */
        let page = &report.pages()[0];
        let primary = page.details.primary.as_ref().unwrap();
        assert_eq!(primary.kind, crate::report::details::DetailKind::Http);
        assert_eq!(primary.operation,
            Some(crate::report::details::DetailOperation::DeletePage));
        assert_eq!(primary.sent,
            Some(crate::report::details::RequestState::Started));
        assert_eq!(primary.status, Some(500));
        assert_eq!(primary.stage, Some(ProcessingStage::DeleteOld));
        assert_eq!(page.details.auxiliary[0].kind,
            crate::report::details::DetailKind::Cancelled);
        assert_eq!(page.details.auxiliary[0].stage,
            Some(ProcessingStage::Reconcile));
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.summary().first_failure, Some(0));
        assert_eq!(report.summary().completed(), 0);
        assert_eq!(report.summary().failed, 1);
        assert_eq!(report.summary().not_executed, 1);
        assert_eq!(report.summary().warnings, usize::from(gap > 0));
        assert!(!page.details.cleanup_attempted);
        assert!(page.details.cleanup.is_none());
        /*
         * 成功した観測だけを復旧事実へ移し、未確認を補完しない
         */
        let recovery = page.recovery.as_ref().unwrap();
        assert_eq!(recovery.facts.old_id.as_ref().unwrap().as_str(), ID);
        assert_eq!(recovery.facts.old_revision, Some(7));
        assert!(recovery.facts.draft_id.is_none());
        assert_eq!(recovery.facts.old_delete_confirmed, gap > 0);
        assert_eq!(recovery.facts.old_lock_observed, Some(gap == 0));
        assert_eq!(recovery.observed, if gap == 0 { ObservedState::Unknown }
            else { ObservedState::Deleted });
        assert_eq!(recovery.facts.lock_observed,
            if gap == 0 { None } else { Some(false) });
        match &recovery.facts.path_observed {
            crate::report::details::PathObservation::Unknown =>
                assert!(gap < 2),
            crate::report::details::PathObservation::Absent =>
                assert_eq!(gap, 2),
            _ => panic!("未観測のpath状態"),
        }
        changes(&server, 0, 0);
        server.inspect(|requests| assert_eq!(requests.len(), 10 + gap));
    }
}

///
/// DELETE500後の同一期限到達で再照合や再作成を開始しない。
///
/// # 注記
/// 元DELETE完了後に2秒の同じdeadlineを待ち、GET数0を確認する。
///
/// # 戻り値
/// なし。新しい猶予や異なる停止原因への変換で失敗する。
///
#[tokio::test]
async fn deadline_after_delete_stops_before_any_reconciliation_get() {
    let shared = StopContext::new(Instant::now() + Duration::from_secs(2));
    let server = Server::new(deletion(Reply::new(500, b"{}".to_vec()))).await;
    let executor = overwrite(&server, &shared, false).await;
    DEADLINE.set(true);
    let (_, known, _, _, _, attempted, details) =
        only(executor.run().await).into_parts();
    assert!(matches!(&details.auxiliary[0], ExecutorFailure::Local(v)
        if v.kind() == LocalKind::Deadline));
    assert!(!known.old_delete_confirmed() && !attempted);
    changes(&server, 0, 0);
    server.inspect(|requests| assert_eq!(requests.len(), 10));
}

///
/// 再作成直前の取消でも確定済みDELETE警告を失わない。
///
/// # 概要
/// 実通常processorの既存pauseで照合後/新POST前にだけ停止する。
///
/// # 注記
/// actorは共有flagの取消と再開通知だけを行い実Wikiへ接続しない。
///
/// # 戻り値
/// なし。新POSTや警告消失で失敗する。
///
#[tokio::test]
async fn stop_before_recreation_keeps_delete_warning_and_no_new_post() {
    /*
     * 完了した削除照合と新POSTとの間でだけ取消する
     */
    let shared = stop();
    let mut replies = deletion(Reply::new(500, b"{}".to_vec()));
    deleted(&mut replies, 404);
    let server = Server::new(replies).await;
    let executor = overwrite(&server, &shared, true).await;
    let (ready, resume) = pause(ProcessingStage::CreateDraft, 0);
    let actor = async {
        ready.await.unwrap();
        shared.cancel();
        resume.send(()).unwrap();
    };
    let (batch, ()) = tokio::join!(executor.run(), actor);
    let (mut pages, first, _, _) = batch.into_parts();
    assert_eq!(first, Some(0));
    assert!(matches!(pages[1].execution(), PageExecution::NotExecuted));
    let (_, known, stage, result, _, attempted, details) =
        pages.remove(0).into_parts();
    assert!(matches!(result, PageExecution::Failed(ExecutorFailure::Local(v))
        if v.kind() == LocalKind::Cancelled
            && v.stage() == ProcessingStage::CreateDraft));
    assert_eq!(stage, ProcessingStage::CreateDraft);
    assert!(known.old_delete_confirmed() && !attempted);
    delete_warning(&details);
    changes(&server, 0, 0);
    server.inspect(|requests| assert_eq!(requests.len(), 12));
}

///
/// 初回draft POST異常では照合・再送・自動回収せず停止する。
///
/// # 概要
/// 元POSTの500/喪失と不正201の確定ID範囲を明示して確認する。
///
/// # 注記
/// path不存在の後にPOST異常を一度返し、全追加GET/PUT/DELETE0を照合。
///
/// # 戻り値
/// なし。応答候補IDを確定したり再送した場合に失敗する。
///
#[tokio::test]
async fn initial_post_anomalies_stop_without_draft_discovery_or_retry() {
    for case in 0..4 {
        /*
         * 不正201では完全JSONのIDだけを保持し候補を補完しない
         */
        let shared = stop();
        let reply = match case {
            0 => Reply::new(500, b"{}".to_vec()),
            1 => Reply { status: None, body: Vec::new(), lock: false,
                token: TOKEN },
            2 => identity(ID, false),
            _ => Reply::new(201, b"{\"id\":\"partial".to_vec()),
        };
        let mut reply = reply;
        if case == 2 { reply.status = Some(201); }
        let server = Server::new(vec![Reply::new(404, Vec::new()), reply])
            .await;
        let (_, known, _, result, _, attempted, details) =
            only(runner(&server, &shared, false).await.run().await)
                .into_parts();
        assert!(matches!(result, PageExecution::Failed(
            ExecutorFailure::Wiki(v))
            if v.stage() == ProcessingStage::CreateDraft
                && v.facts().operation() == WikiOperation::CreateDraft));
        assert_eq!(known.into_parts().2.is_some(), case == 2);
        assert!(!attempted && details.auxiliary.is_empty());
        server.inspect(|requests| assert_eq!(requests.len(), 2));
    }
}

///
/// 競合時のcleanup失敗は元Conflictと独立した解除失敗で残る。
///
/// # 概要
/// lock後の他者変更を検出し、唯一の解除が500または喪失となる。
///
/// # 注記
/// 未保持tokenの再利用や追加解除なし、後続未実行を実runで検証する。
///
/// # 戻り値
/// なし。解除エラーで主原因を置換したり再送すれば失敗する。
///
#[tokio::test]
async fn failed_cleanup_never_replaces_conflict_or_retries_unlock() {
    for status in [Some(500), None] {
        /*
         * 事前beforeと再取得targetを相違にし、解除一回だけ失敗させる
         */
        let shared = stop();
        let mut replies = prefix(true, Some(500));
        replies.truncate(8);
        replies[7] = Reply::new(200, b"target".to_vec());
        replies.push(Reply { status, body: b"{}".to_vec(), lock: false,
            token: TOKEN });
        let server = Server::new(replies).await;
        let (mut pages, first, _, _) =
            overwrite(&server, &shared, true).await.run().await.into_parts();
        assert_eq!(first, Some(0));
        assert!(matches!(pages[1].execution(), PageExecution::NotExecuted));
        let (_, _, stage, result, cleanup, attempted, _) =
            pages.remove(0).into_parts();
        assert!(matches!(result,
            PageExecution::Failed(ExecutorFailure::Local(v))
            if v.kind() == LocalKind::Conflict
                && v.stage() == ProcessingStage::Recheck));
        let cleanup = cleanup.expect("cleanup facts");
        assert_eq!(cleanup.stage(), ProcessingStage::ReleaseLock);
        assert_eq!(cleanup.facts().status(), status);
        assert_eq!(cleanup.facts().operation(), WikiOperation::ReleaseLock);
        assert!(attempted);
        assert_eq!(stage, ProcessingStage::ReleaseLock);
        server.inspect(|requests| {
            assert_eq!(requests.len(), 9);
            assert_eq!(requests[8].target, format!("/api/pages/{ID}/lock"));
            assert!(!requests.iter().any(|r|
                r.target == format!("/api/pages/{ID}")));
        });
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// cleanup直前の取消は解除を開始せず主原因と補助停止を別に残す。
///
/// # 概要
/// 保持確実/非draft根拠のあるConflictでReleaseLock境界だけ取消する。
///
/// # 注記
/// cleanup_attempted=falseと補助Cancelled、主Conflictを同時確認する。
///
/// # 戻り値
/// なし。取消後HTTPや補助停止消失で失敗する。
///
#[tokio::test]
async fn cancel_before_cleanup_keeps_primary_and_unsent_stop_separate() {
    let shared = stop();
    let mut replies = prefix(true, Some(500));
    replies.truncate(8);
    replies[7] = Reply::new(200, b"target".to_vec());
    let server = Server::new(replies).await;
    let executor = overwrite(&server, &shared, false).await;
    let (ready, resume) = pause(ProcessingStage::ReleaseLock, 0);
    let actor = async {
        ready.await.unwrap();
        shared.cancel();
        resume.send(()).unwrap();
    };
    /*
     * 実通常processorが解除を要求する直前にだけ共有取消を通知する
     */
    let (batch, ()) = tokio::join!(executor.run(), actor);
    let (_, _, stage, result, cleanup, attempted, details) =
        only(batch).into_parts();
    assert!(matches!(result, PageExecution::Failed(ExecutorFailure::Local(v))
        if v.kind() == LocalKind::Conflict
            && v.stage() == ProcessingStage::Recheck));
    assert_eq!(stage, ProcessingStage::ReleaseLock);
    assert!(!attempted && cleanup.is_none());
    assert!(matches!(&details.auxiliary[0], ExecutorFailure::Local(v)
        if v.kind() == LocalKind::Cancelled
            && v.stage() == ProcessingStage::ReleaseLock));
    server.inspect(|requests| assert_eq!(requests.len(), 8));
}

///
/// 変更前GET/lock異常は単発の元分類で停止し追加操作をしない。
///
/// # 概要
/// meta失敗、acquire/renewの500や不正header、lock後GET失敗を確認。
///
/// # 注記
/// tokenの不足を補完せず、未確認/nonusableなleaseを解除しない。
///
/// # 戻り値
/// なし。GET/lock再送や自動解除、段階/元operation消失で失敗する。
///
#[tokio::test]
async fn read_and_lock_failures_stop_without_implicit_recovery() {
    for case in 0..6 {
        /*
         * 変更前の異なる境界だけを失敗させ余分な要求を検出する
         */
        let shared = stop();
        let (count, operation, stage, malformed) = match case {
            0 => (2, WikiOperation::ReadMeta, ProcessingStage::ReadExisting,
                false),
            1 => (4, WikiOperation::AcquireLock, ProcessingStage::AcquireLock,
                false),
            2 => (5, WikiOperation::RenewLock, ProcessingStage::Recheck, false),
            3 => (4, WikiOperation::AcquireLock, ProcessingStage::AcquireLock,
                true),
            4 => (5, WikiOperation::RenewLock, ProcessingStage::Recheck, true),
            _ => (6, WikiOperation::ReadMeta, ProcessingStage::Recheck, false),
        };
        let mut replies = prefix(true, Some(500));
        replies.truncate(count);
        replies[count - 1] = if malformed { Reply::new(204, Vec::new()) }
            else { Reply::new(500, b"{}".to_vec()) };
        let server = Server::new(replies).await;
        let (_, _, _, result, cleanup, attempted, _) =
            only(overwrite(&server, &shared, false).await.run().await)
                .into_parts();
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else { panic!("original read/lock failure"); };
        assert_eq!(failure.stage(), stage);
        assert_eq!(failure.facts().operation(), operation);
        assert_eq!(failure.facts().status(),
            Some(if malformed { 204 } else { 500 }));
        assert!(!attempted && cleanup.is_none());
        server.inspect(|requests| assert_eq!(requests.len(), count));
        assert!(shared.checkpoint().is_ok());
    }
}
