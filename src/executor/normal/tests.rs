/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実通常processorのrenew・再照合・解放gate・部分成功の接続試験
//!

use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::executor::prepare::tests::mock::{Reply, Server, TOKEN2, TOKEN3};
use crate::executor::tests::{plan, stop};
use crate::executor::types::PageExecution;
use crate::source::SourceError;
use crate::wiki_client::types::{FailureKind, WikiOperation};

use super::*;

// 試験だけのID/token。実資格情報を読まない。
const OLD: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
const TOKEN1: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

thread_local! {
    // process開始時にtakeし、その一件が所有する。
    static ACTION: RefCell<Option<Action>> = const { RefCell::new(None) };
}

///
/// 次の通常一件だけに閉じる段階同期操作。
///
pub(super) struct Action {
    /// 対象caller段階。
    stage: ProcessingStage,
    /// 同じ段階の先行checkpoint数。
    skip: usize,
    /// 一度だけ消費する操作。
    kind: ActionKind,
}

///
/// 製品設定にない同期試験操作。
///
enum ActionKind {
    /// 同じcontextへ取消を通知する。
    Cancel,
    /// 固定deadlineへ到達するまで待つ。
    Deadline,
    /// 実future破棄の同期点。
    Pause {
        /// 境界到達を呼出側へ通知する。
        started: tokio::sync::oneshot::Sender<()>,
        /// この一件だけの再開通知。
        resume: tokio::sync::oneshot::Receiver<()>,
    },
}

///
/// TLS操作をawait前に一件だけへmoveする。
///
/// # 戻り値
/// 他ページへ残さない同期操作
///
pub(super) fn take_action() -> Option<Action> {
    ACTION.with(|value| value.borrow_mut().take())
}

///
/// caller境界で同期操作を一度だけ実行する。
///
/// # 概要
/// TLS借用をawait越しに保持せず次件にも残さない。
///
/// # 引数
/// * `stop` - 同じ境界
/// * `stage` - 次のcaller段階
/// * `action` - この一件の所有操作
///
/// # 戻り値
/// なし。直後の製品checkpointが元停止を検出する。
///
pub(super) async fn before_stage(stop: &StopContext, stage: ProcessingStage,
    action: &mut Option<Action>)
{
    if !action.as_ref().is_some_and(|v| v.stage == stage) { return; }
    if let Some(action) = action.as_mut() {
        if action.skip > 0 { action.skip -= 1; return; }
    }
    /*
     * 同期操作を所有してからのみawaitする
     */
    match action.take().unwrap().kind {
        ActionKind::Cancel => stop.cancel(),
        ActionKind::Deadline => { let _ = stop.wait_stopped().await; }
        ActionKind::Pause { started, resume } => {
            let _ = started.send(());
            let _ = resume.await;
        }
    }
}

///
/// 正常更新返却後の取消を注入する。
///
/// # 引数
/// * `stop` - 同じ境界
/// * `action` - この一件の操作
///
/// # 戻り値
/// なし。確定完了を失敗へ戻さない。
///
pub(super) fn after_completion(stop: &StopContext,
    action: &mut Option<Action>)
{
    if action.as_ref().is_some_and(|v| v.stage == ProcessingStage::Finished) {
        if let ActionKind::Cancel = action.take().unwrap().kind {
            stop.cancel();
        }
    }
}

///
/// 同期操作を次の一件へ設定する。
///
/// # 引数
/// * `stage` - 対象caller段階
/// * `skip` - 先行checkpoint数
/// * `kind` - 同期操作
///
/// # 戻り値
/// なし。global clockを変更しない。
///
fn action(stage: ProcessingStage, skip: usize, kind: ActionKind) {
    ACTION.with(|value| {
        *value.borrow_mut() = Some(Action { stage, skip, kind });
    });
}

///
/// 実Wiki試験の次の一件だけを既存同期境界で停止する。
///
/// # 引数
/// * `stage` - 停止するcaller段階
/// * `skip` - 同じ段階の先行checkpoint数
///
/// # 戻り値
/// 到達通知と再開送信端。製品へ公開しない。
///
pub(in crate::executor) fn pause(stage: ProcessingStage, skip: usize)
    -> (tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>)
{
    let (started, ready) = tokio::sync::oneshot::channel();
    let (resume, release) = tokio::sync::oneshot::channel();
    action(stage, skip, ActionKind::Pause { started, resume: release });
    (ready, resume)
}

///
/// dummy ID応答またはdraft作成を組み立てる。
///
/// # 引数
/// * `id` - dummyの確定ID
/// * `created` - draft POST応答か
///
/// # 戻り値
/// 完全JSONと必要なdummy lock
///
fn identity(id: &str, created: bool) -> Reply {
    let mut reply = Reply::new(if created { 201 } else { 200 },
        format!("{{\"id\":\"{id}\"}}").into_bytes());
    reply.lock = created;
    reply
}

///
/// 期待pathのmandatoryrevisionとlock観測を返す。
///
/// # 引数
/// * `path` - dummy path
/// * `locked` - 有効lock観測
///
/// # 戻り値
/// 完全な正常metadata
///
fn meta(path: &str, locked: bool) -> Reply {
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"current\",\
        \"value\":\"{path}\"}},\"deleted\":false,\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}").into_bytes())
}

///
/// G3初回相違からlock/最初のrenewまでのscriptを作る。
///
/// # 戻り値
/// 同じ旧ID/本文と独立新tokenの応答列
///
fn initial() -> Vec<Reply> {
    vec![identity(OLD, false), meta("/fixture/a", false),
        Reply::new(200, b"before".to_vec()), Reply::locked(TOKEN1),
        Reply::locked(TOKEN2)]
}

///
/// 正常再照合を追加する。
///
/// # 引数
/// * `replies` - 同じ一件のscript
/// * `bytes` - 再取得した本文
///
/// # 戻り値
/// なし。meta先行/ID照会/sourceの順を保つ。
///
fn recheck_script(replies: &mut Vec<Reply>, bytes: &[u8]) {
    replies.extend([meta("/fixture/a", true), identity(OLD, false),
        Reply::new(200, bytes.to_vec())]);
}

///
/// 実計画/clientから通常runの所有者を作る。
///
/// # 引数
/// * `server` - 独立loopback script
/// * `shared` - 同じ期限/取消
/// * `overwrite` - fixtureの検証済みMode選択
/// * `entries` - 全件検証を通すfixture
///
/// # 戻り値
/// 単回消費するexecutor
///
async fn executor(server: &Server, shared: &StopContext, overwrite: bool,
    entries: &[(&[u8], &[u8])]) -> Executor
{
    let config = crate::cmd_args::resolve::wiki_config_fixture(&server.url,
        true, shared.deadline());
    let mut executor = Executor::new(plan(entries, shared).await, &config,
        shared.clone()).unwrap();
    executor.mode = if overwrite { Mode::Overwrite } else { Mode::Update };
    executor
}

///
/// 余分な要求を含め全method列を照合する。
///
/// # 引数
/// * `server` - 専用server
/// * `expected` - 必要な明示要求列
///
/// # 戻り値
/// なし。再照会/解放/再試行が余分なら失敗する。
///
fn methods(server: &Server, expected: &[&str]) {
    server.inspect(|requests| {
        let actual: Vec<_> = requests.iter().map(|v| v.method.as_str())
            .collect();
        assert_eq!(actual, expected);
    });
}

///
/// update/overwrite正常系列と新旧ID/token/完全bytesを確認する。
///
/// # 概要
/// 実runの正常結果と有限観測をmove出口から照合する。
///
/// # 注記
/// 固定bufferのmockは全PUT bytes受信後にだけ204を返す。
///
/// # 戻り値
/// なし。旧/newの混同や必要renewの欠落で失敗する。
///
#[tokio::test]
async fn normal_modes_renew_recheck_and_preserve_exact_outcomes() {
    for overwrite in [false, true] {
        let shared = stop();
        let mut replies = initial();
        recheck_script(&mut replies, b"before");
        replies.extend([Reply::locked(TOKEN3), Reply::new(204, Vec::new())]);
        if overwrite {
            replies.extend([identity(NEW, true), Reply::locked(TOKEN2),
                Reply::new(204, Vec::new())]);
        }
        let server = Server::new(replies).await;
        let bytes = "\u{feff}更新\r\n".as_bytes();
        let executor = executor(&server, &shared, overwrite,
            &[(b"a.md", bytes)]).await;
        let (mut pages, failure, global, rejected) = executor.run().await
            .into_parts();
        assert!(failure.is_none() && global.is_none() && rejected.is_empty());
        /*
         * 完了種別と旧削除確認/lock観測・newIDを別々に確認する
         */
        let (_, known, stage, outcome, cleanup, attempted, _details) =
            pages.remove(0).into_parts();
        let expected = if overwrite { Completion::Recreated }
            else { Completion::Updated };
        assert!(matches!(outcome, PageExecution::Completed(v)
            if v == expected));
        assert_eq!(stage, ProcessingStage::Finished);
        assert!(!attempted && cleanup.is_none());
        let (old, revision, new, deleted, locked) = known.into_parts();
        assert_eq!(old.unwrap().as_str(), OLD);
        assert_eq!(revision, Some(7));
        assert_eq!(deleted, overwrite);
        assert_eq!(locked, Some(true));
        assert_eq!(new.as_ref().map(|id| id.as_str()),
            if overwrite { Some(NEW) } else { None });
        server.inspect(|requests| {
            assert_eq!(requests[5].target, format!("/api/pages/{OLD}/meta"));
            assert!(requests[6].target.starts_with("/api/pages/id?"));
            assert_eq!(requests[4].lock_version, Some(1));
            assert_eq!(requests[8].lock_version, Some(2));
            assert_eq!(requests[9].lock_version, Some(3));
            let put = requests.last().unwrap();
            assert_eq!(put.body.as_slice(), bytes);
            assert_eq!(put.lock_version, if overwrite { Some(2) }
                else { Some(3) });
            assert!(!put.target.contains("amend="));
            assert!(!requests[9].target.contains("recursive="));
        });
        let expected = if overwrite {
            vec!["GET", "GET", "GET", "POST", "PUT", "GET", "GET", "GET",
                "PUT", "DELETE", "POST", "PUT", "PUT"]
        } else {
            vec!["GET", "GET", "GET", "POST", "PUT", "GET", "GET", "GET",
                "PUT", "PUT"]
        };
        methods(&server, &expected);
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// lock後の入力同一化も他者変更として停止し確実な旧lockを解放する。
///
/// # 概要
/// update/overwriteの正常cleanupと失敗cleanupを主Conflictから分離する。
///
/// # 注記
/// main failureはRecheck、最後の試行はReleaseLockを保持する。
///
/// # 戻り値
/// なし。競合の成功補完/主原因上書きで失敗する。
///
#[tokio::test]
async fn changed_body_keeps_conflict_and_independent_cleanup() {
    for overwrite in [false, true] {
        /*
         * 両モードで入力同一化した他者変更を競合として照合する
         */
        for failed_cleanup in [false, true] {
            let shared = stop();
            let mut replies = initial();
            recheck_script(&mut replies, b"target");
            replies.push(Reply::new(if failed_cleanup { 500 } else { 204 },
                if failed_cleanup { b"{}".to_vec() } else { Vec::new() }));
            let server = Server::new(replies).await;
            let executor = executor(&server, &shared, overwrite,
                &[(b"a.md", b"target")]).await;
            let (mut pages, first, global, _) = executor.run().await
                .into_parts();
            assert_eq!(first, Some(0));
            assert!(global.is_none());
            let (_, known, last, outcome, cleanup, attempted, _details) =
                pages.remove(0).into_parts();
            /*
             * 解放成否を変えても主原因と確認情報を上書きしない
             */
            assert!(matches!(outcome, PageExecution::Failed(error)
                if matches!(&error, ExecutorFailure::Local(v)
                    if v.kind() == LocalKind::Conflict)
                    && error.stage() == ProcessingStage::Recheck));
            assert_eq!(last, ProcessingStage::ReleaseLock);
            assert!(attempted);
            assert_eq!(known.old_lock_observed(), Some(true));
            assert_eq!(cleanup.is_some(), failed_cleanup);
            if let Some(cleanup) = cleanup {
                assert_eq!(cleanup.stage(), ProcessingStage::ReleaseLock);
                assert_eq!(cleanup.facts().status(), Some(500));
            }
            methods(&server, &["GET", "GET", "GET", "POST", "PUT", "GET", "GET",
                "GET", "DELETE"]);
            server.inspect(|requests| {
                let deletes: Vec<_> = requests.iter().filter(|request|
                    request.method == "DELETE").collect();
                assert_eq!(deletes.len(), 1);
                assert_eq!(deletes[0].target,
                    format!("/api/pages/{OLD}/lock"));
                assert_eq!(deletes[0].lock_version, Some(2));
                assert!(!requests.iter().any(|request|
                    request.target == format!("/api/pages/{OLD}")));
            });
            assert!(shared.checkpoint().is_ok());
        }
    }
}

///
/// acquire409/423は元失敗と旧情報を残し後続操作を開始しない。
///
/// # 概要
/// 実prepareの相違から取得拒否へ進め、資源なしの停止を確認する。
///
/// # 注記
/// 単発client分類だけでなく実runの要求数/cleanup0を照合する。
///
/// # 戻り値
/// なし。元status/段階喪失または余分なHTTPで失敗する。
///
#[tokio::test]
async fn acquire_conflict_or_locked_stops_without_followup_or_cleanup() {
    for status in [409, 423] {
        let shared = stop();
        let mut replies = initial();
        replies.truncate(3);
        replies.push(Reply::new(status, b"{}".to_vec()));
        let server = Server::new(replies).await;
        let runner = executor(&server, &shared, false,
            &[(b"a.md", b"target")]).await;
        let (mut pages, first, global, rejected) = runner.run().await
            .into_parts();
        /*
         * 取得拒否の元事実と未生成資源の観測を別々に確認する
         */
        assert_eq!(first, Some(0));
        assert!(global.is_none() && rejected.is_empty());
        let (_, known, last, result, cleanup, attempted, _details) =
            pages.remove(0)
            .into_parts();
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else { panic!("acquire failure") };
        assert_eq!(last, ProcessingStage::AcquireLock);
        assert_eq!(failure.stage(), ProcessingStage::AcquireLock);
        assert_eq!(failure.facts().operation(), WikiOperation::AcquireLock);
        assert_eq!(failure.facts().kind(), FailureKind::Http);
        assert_eq!(failure.facts().status(), Some(status));
        assert_eq!(failure.facts().known_id().unwrap().as_str(), OLD);
        let (old, revision, new, deleted, locked) = known.into_parts();
        assert_eq!(old.unwrap().as_str(), OLD);
        assert_eq!(revision, Some(7));
        assert!(new.is_none() && !deleted && locked.is_none());
        assert!(!attempted && cleanup.is_none());
        methods(&server, &["GET", "GET", "GET", "POST"]);
        server.inspect(|requests| {
            assert_eq!(requests[3].target, format!("/api/pages/{OLD}/lock"));
        });
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// overwriteのlock後path変更は旧ページを削除せず旧lockだけ解放する。
///
/// # 概要
/// 非draft/保持確認済みの観測Conflictを主原因として保つ。
///
/// # 注記
/// meta先行で停止し余分なpath/source再照会や再作成をしない。
///
/// # 戻り値
/// なし。path不一致を削除へ進めると失敗する。
///
#[tokio::test]
async fn overwrite_path_change_releases_only_confirmed_old_lock() {
    let shared = stop();
    let mut replies = initial();
    replies.extend([meta("/other", true), Reply::new(204, Vec::new())]);
    let server = Server::new(replies).await;
    let runner = executor(&server, &shared, true, &[(b"a.md", b"target")])
        .await;
    let (mut pages, first, global, rejected) = runner.run().await.into_parts();
    /*
     * 主Conflictと最後の解除試行を分け、元の旧情報を保持する
     */
    assert_eq!(first, Some(0));
    assert!(global.is_none() && rejected.is_empty());
    let (_, known, last, result, cleanup, attempted, _details) =
        pages.remove(0).into_parts();
    let PageExecution::Failed(ExecutorFailure::Local(failure)) = result else {
        panic!("path conflict");
    };
    assert_eq!(failure.kind(), LocalKind::Conflict);
    assert_eq!(failure.stage(), ProcessingStage::Recheck);
    assert_eq!(last, ProcessingStage::ReleaseLock);
    assert!(attempted && cleanup.is_none());
    let (old, revision, new, deleted, locked) = known.into_parts();
    assert_eq!(old.unwrap().as_str(), OLD);
    assert_eq!(revision, Some(7));
    assert!(new.is_none() && !deleted);
    assert_eq!(locked, Some(true));
    methods(&server, &["GET", "GET", "GET", "POST", "PUT", "GET", "DELETE"]);
    server.inspect(|requests| {
        assert_eq!(requests[5].target, format!("/api/pages/{OLD}/meta"));
        assert_eq!(requests[6].target, format!("/api/pages/{OLD}/lock"));
        assert_eq!(requests[6].lock_version, Some(2));
        assert!(!requests.iter().any(|request|
            request.target == format!("/api/pages/{OLD}")));
    });
    assert!(shared.checkpoint().is_ok());
}

///
/// locked=falseとdraft不足は保持/非draftgateを独立に落とす。
///
/// # 概要
/// currentだけを根拠にdraftへDELETE lockを送らない。
///
/// # 注記
/// mandatoryrevisionなしと正常metaのunlocked観測を分ける。
///
/// # 戻り値
/// なし。根拠なし解放/変更で失敗する。
///
#[tokio::test]
async fn unlocked_or_draft_metadata_prevents_write_and_cleanup() {
    for draft in [false, true] {
        let shared = stop();
        let mut replies = initial();
        replies.push(if draft { Reply::new(200, b"{\"page_info\":{\"path\":\
            {\"kind\":\"current\",\"value\":\"/fixture/a\"},\"deleted\":false,\
            \"locked\":true}}".to_vec()) } else { meta("/fixture/a", false) });
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, false,
            &[(b"a.md", b"target")]).await;
        let (mut pages, _, _, _) = executor.run().await.into_parts();
        let (_, known, _, result, cleanup, attempted, _details) =
            pages.remove(0)
            .into_parts();
        /*
         * 非draft確認とlock観測は別々の事実として照合する
         */
        assert!(!attempted && cleanup.is_none());
        if draft {
            assert!(matches!(result,
                PageExecution::Failed(ExecutorFailure::Wiki(_))));
            assert_eq!(known.old_lock_observed(), None);
        } else {
            assert!(matches!(result,
                PageExecution::Failed(ExecutorFailure::Local(v))
                if v.kind() == LocalKind::Conflict));
            assert_eq!(known.old_lock_observed(), Some(false));
        }
        methods(&server, &["GET", "GET", "GET", "POST", "PUT", "GET"]);
    }
}

///
/// meta確認後のID変更は旧leaseだけを安全に解放する。
///
/// # 概要
/// ID変更を含む応答列を実行し、確認済み旧ページだけの解放を検査する。
///
/// # 注記
/// 新IDへ操作せず、入力一致を確かめる追加sourceも取得しない。
///
/// # 戻り値
/// なし。別IDへの変更要求で失敗する。
///
#[tokio::test]
async fn changed_lookup_identity_releases_only_confirmed_old_page() {
    /*
     * meta確認後に別IDを返す応答列でexecutorを構築する
     */
    let shared = stop();
    let mut replies = initial();
    replies.extend([meta("/fixture/a", true), identity(NEW, false),
        Reply::new(204, Vec::new())]);
    let server = Server::new(replies).await;
    let executor = executor(&server, &shared, false,
        &[(b"a.md", b"target")]).await;
    /*
     * 失敗結果と、旧ID・確認済みlock版だけを使った解放を検査する
     */
    let batch = executor.run().await;
    assert!(matches!(batch.pages()[0].execution(), PageExecution::Failed(_)));
    server.inspect(|requests| {
        assert_eq!(requests.len(), 8);
        assert_eq!(requests[7].target, format!("/api/pages/{OLD}/lock"));
        assert_eq!(requests[7].lock_version, Some(2));
    });
}

///
/// 通常GET失敗は元主原因を保ち確認済みgateだけで解放する。
///
/// # 概要
/// 非draft未確認のmeta失敗と確認済みlookup/source失敗を分ける。
///
/// # 注記
/// 通常GET失敗で共有取消を発火せず必要な明示解放だけを確認する。
///
/// # 戻り値
/// なし。gateなし解放または元GET失敗喪失で失敗する。
///
#[tokio::test]
async fn read_failure_cleanup_requires_prior_non_draft_confirmation() {
    for failure_point in 0..3 {
        let shared = stop();
        let mut replies = initial();
        if failure_point >= 1 { replies.push(meta("/fixture/a", true)); }
        if failure_point == 2 { replies.push(identity(OLD, false)); }
        replies.push(Reply::new(500, b"{}".to_vec()));
        if failure_point >= 1 { replies.push(Reply::new(204, Vec::new())); }
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, false,
            &[(b"a.md", b"target")]).await;
        let (mut pages, _, global, _) = executor.run().await.into_parts();
        /*
         * 元operationを保持しmeta確認後だけ解放試行を記録する
         */
        let (_, known, _, result, cleanup, attempted, _details) =
            pages.remove(0)
            .into_parts();
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else { panic!("read failure") };
        let operation = match failure_point {
            0 => WikiOperation::ReadMeta, 1 => WikiOperation::Lookup,
            _ => WikiOperation::ReadSource,
        };
        assert_eq!(failure.facts().operation(), operation);
        assert_eq!(failure.facts().status(), Some(500));
        assert_eq!(failure.stage(), ProcessingStage::Recheck);
        assert_eq!(attempted, failure_point >= 1);
        assert!(cleanup.is_none() && global.is_none());
        assert_eq!(known.old_lock_observed(),
            if failure_point == 0 { None } else { Some(true) });
        server.inspect(|requests| assert_eq!(requests.len(),
            if failure_point == 0 { 6 } else { 7 + failure_point }));
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// renew失敗では旧tokenでprotected操作/解放へ進まない。
///
/// # 概要
/// 初回renewと変更直前renewの送信後拒否を分ける。
///
/// # 注記
/// 同じheader tokenの再返却も新token成功とみなさない。
///
/// # 戻り値
/// なし。失敗renew後の旧token再使用で失敗する。
///
#[tokio::test]
async fn failed_renewal_never_restores_old_token_for_changes() {
    for before_write in [false, true] {
        for malformed in [false, true] {
            let shared = stop();
            let mut replies = initial();
            let stage = if before_write { ProcessingStage::PutSource }
                else { ProcessingStage::Recheck };
            let bad = if malformed { Reply::locked(if before_write { TOKEN2 }
                else { TOKEN1 }) } else { Reply::new(403, b"{}".to_vec()) };
            if before_write {
                recheck_script(&mut replies, b"before");
                replies.push(bad);
            } else { *replies.last_mut().unwrap() = bad; }
            let server = Server::new(replies).await;
            let executor = executor(&server, &shared, false,
                &[(b"a.md", b"target")]).await;
            let (mut pages, _, _, _) = executor.run().await.into_parts();
            let (_, _, _, result, cleanup, attempted, _details) =
                pages.remove(0)
                .into_parts();
            /*
             * caller phaseと元RenewLock/状態を保持し追加要求を拒否する
             */
            let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
                else { panic!("renew failure") };
            assert_eq!(failure.stage(), stage);
            assert_eq!(failure.facts().operation(), WikiOperation::RenewLock);
            if malformed { assert_eq!(failure.facts().kind(),
                FailureKind::LockHeader); }
            assert!(!attempted && cleanup.is_none());
            server.inspect(|requests| assert_eq!(requests.len(),
                if before_write { 9 } else { 5 }));
        }
    }
}

///
/// 完全400だけの保持と403/500/喪失の不確実状態を区別する。
///
/// # 概要
/// clientが返すusable状態に従い解放有無を決める。
///
/// # 注記
/// 他statusだけから保存/保持を断定しない。
///
/// # 戻り値
/// なし。保持不確実なtokenで解放を送れば失敗する。
///
#[tokio::test]
async fn put_rejection_cleanup_follows_client_lease_state() {
    for status in [Some(400), Some(403), Some(500), None] {
        let shared = stop();
        let mut replies = initial();
        recheck_script(&mut replies, b"before");
        replies.push(Reply::locked(TOKEN3));
        replies.push(Reply { status, body: b"{}".to_vec(), lock: false,
            token: TOKEN1 });
        if status == Some(400) { replies.push(Reply::new(204, Vec::new())); }
        if status == Some(500) || status.is_none() {
            replies.push(Reply::new(503, Vec::new()));
        }
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, false,
            &[(b"a.md", b"target")]).await;
        let (mut pages, _, _, _) = executor.run().await.into_parts();
        let (_, _, _, result, _, attempted, _details) =
            pages.remove(0).into_parts();
        /*
         * 元PUT statusは保持し400以外の解除要求0を確認する
         */
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else { panic!("put failure") };
        assert_eq!(failure.facts().status(), status);
        assert_eq!(attempted, status == Some(400));
        server.inspect(|requests| assert_eq!(requests.len(),
            if status == Some(403) { 10 } else { 11 }));
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// DELETE成功後の再作成失敗にも全旧情報と新ID範囲を保持する。
///
/// # 概要
/// POST/新lease renew/新PUT失敗を一件ずつ独立に確認する。
///
/// # 注記
/// 旧削除確認はstatus推測ではなく正常DELETE返却の事実。
///
/// # 戻り値
/// なし。旧削除/ID喪失またはdraft cleanupで失敗する。
///
#[tokio::test]
async fn recreation_failures_keep_confirmed_delete_and_new_identity() {
    for failure_point in 0..3 {
        let shared = stop();
        let mut replies = initial();
        recheck_script(&mut replies, b"before");
        replies.extend([Reply::locked(TOKEN3), Reply::new(204, Vec::new())]);
        if failure_point == 0 { replies.push(Reply::new(409, b"{}".to_vec())); }
        else {
            replies.push(identity(NEW, true));
            replies.push(if failure_point == 1 {
                Reply::new(500, b"{}".to_vec())
            }
                else { Reply::locked(TOKEN2) });
            if failure_point == 2 {
                replies.push(Reply::new(403, b"{}".to_vec()));
            }
        }
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, true,
            &[(b"a.md", b"target")]).await;
        let (mut pages, _, _, _) = executor.run().await.into_parts();
        let (_, known, _, result, cleanup, attempted, _details) =
            pages.remove(0)
            .into_parts();
        /*
         * 同じKnownの旧/newと削除確認を失敗結果のままmove照合する
         */
        assert!(matches!(result, PageExecution::Failed(_)));
        assert!(!attempted && cleanup.is_none());
        let (old, revision, new, deleted, locked) = known.into_parts();
        assert_eq!(old.unwrap().as_str(), OLD);
        assert_eq!((revision, deleted, locked), (Some(7), true, Some(true)));
        assert_eq!(new.as_ref().map(|v| v.as_str()),
            if failure_point == 0 { None } else { Some(NEW) });
        server.inspect(|requests| assert_eq!(requests.len(),
            11 + failure_point));
    }
}

///
/// DELETE不確定の照合失敗はPOSTせず元DELETEと未確認を保持する。
///
/// # 概要
/// 元DELETE失敗と未確認flagを保ち、照合失敗後の変更0を確認する。
///
/// # 注記
/// 500/応答喪失のstatusあり/なしを独立に照合する。
///
/// # 戻り値
/// なし。未知削除を確定または未削除へ補完すれば失敗する。
///
#[tokio::test]
async fn failed_delete_stops_without_confirmed_deletion_or_recreation() {
    /*
     * status観測あり/なしの未知削除を別scriptで確認する
     */
    for status in [Some(500), None] {
        /*
         * 元DELETE異常と一回の照合失敗を専用scriptで返す
         */
        let shared = stop();
        let mut replies = initial();
        recheck_script(&mut replies, b"before");
        replies.extend([Reply::locked(TOKEN3), Reply { status,
            body: b"{}".to_vec(), lock: false, token: TOKEN1 }]);
        replies.push(Reply::new(503, b"{}".to_vec()));
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, true,
            &[(b"a.md", b"target")]).await;
        let (mut pages, _, _, _) = executor.run().await.into_parts();
        let (_, known, _, result, _, attempted, _details) =
            pages.remove(0).into_parts();
        assert!(!known.old_delete_confirmed() && !attempted);
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else { panic!("delete failure") };
        assert_eq!(failure.facts().status(), status);
        assert_eq!(failure.stage(), ProcessingStage::DeleteOld);
        server.inspect(|requests| assert_eq!(requests.len(), 11));
    }
}

///
/// 操作間取消/期限で解放を含む追加HTTPを開始しない。
///
/// # 概要
/// acquire前/再照合前/変更前/DELETE後を同期して確認する。
///
/// # 注記
/// fixed deadlineを再起算せず新POST前の到達を待つ。
///
/// # 戻り値
/// なし。停止後のrequestまたは削除確認喪失で失敗する。
///
#[tokio::test]
async fn common_stop_prevents_followup_and_cleanup_http() {
    for (stage, skip, overwrite, count, deadline) in [
        (ProcessingStage::AcquireLock, 0, false, 3, false),
        (ProcessingStage::Recheck, 0, false, 4, false),
        (ProcessingStage::PutSource, 0, false, 8, false),
        (ProcessingStage::CreateDraft, 0, true, 10, false),
        (ProcessingStage::CreateDraft, 0, true, 10, true),
    ] {
        let shared = if deadline { StopContext::new(Instant::now()
            + Duration::from_secs(2)) } else { stop() };
        let mut replies = initial();
        recheck_script(&mut replies, b"before");
        replies.extend([Reply::locked(TOKEN3), Reply::new(204, Vec::new())]);
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, overwrite,
            &[(b"a.md", b"target")]).await;
        action(stage, skip, if deadline { ActionKind::Deadline }
            else { ActionKind::Cancel });
        let (mut pages, _, _, _) = executor.run().await.into_parts();
        /*
         * 停止時は解放関数を呼ばず既に確定した削除だけ保持する
         */
        let (_, known, _, result, _, attempted, _details) =
            pages.remove(0).into_parts();
        let expected = if deadline { LocalKind::Deadline }
            else { LocalKind::Cancelled };
        assert!(matches!(result,
                PageExecution::Failed(ExecutorFailure::Local(v))
            if v.kind() == expected));
        assert!(!attempted);
        assert_eq!(known.old_delete_confirmed(),
            stage == ProcessingStage::CreateDraft);
        server.inspect(|requests| assert_eq!(requests.len(), count));
    }
}

///
/// 実runの複数結果は最初の失敗後を未実行に保つ。
///
/// # 概要
/// 正常新規→同一skip→初回失敗の実HTTP列を所有して照合する。
///
/// # 注記
/// fakeProcessorを使わず新規にもArenewが入ることを確認する。
///
/// # 戻り値
/// なし。先行成功喪失または後続実行で失敗する。
///
#[tokio::test]
async fn multiple_pages_keep_successes_and_later_not_executed() {
    /*
     * 実一件処理を順序driverへ接続し先行結果と未実行を照合する
     */
    let shared = stop();
    let server = Server::new(vec![Reply::new(404, Vec::new()),
        identity(NEW, true), Reply::locked(TOKEN2), Reply::new(204, Vec::new()),
        identity(OLD, false), meta("/fixture/b", false),
        Reply::new(200, b"same".to_vec()), Reply::new(500, b"{}".to_vec())])
        .await;
    let executor = executor(&server, &shared, false, &[(b"d.md", b"d"),
        (b"c.md", b"c"), (b"b.md", b"same"), (b"a.md", b"new")]).await;
    let (pages, first, _, _) = executor.run().await.into_parts();
    assert_eq!(first, Some(2));
    assert!(matches!(pages[0].execution(),
        PageExecution::Completed(Completion::Created)));
    assert!(matches!(pages[1].execution(),
        PageExecution::Completed(Completion::SkippedIdentical)));
    assert!(matches!(pages[2].execution(), PageExecution::Failed(_)));
    assert!(matches!(pages[3].execution(), PageExecution::NotExecuted));
    methods(&server, &["GET", "POST", "PUT", "PUT", "GET", "GET", "GET",
        "GET"]);
}

///
/// 別executor計画のsnapshotはHTTP前に拒否する。
///
/// # 概要
/// 同じ入力の別計画へsnapshotを渡し、追加HTTPなしの拒否を検査する。
///
/// # 注記
/// 同じ内容でもArc生成口が異なることを実構築で確認する。
///
/// # 戻り値
/// なし。別contextの旧IDへlock要求すれば失敗する。
///
#[tokio::test]
async fn foreign_snapshot_is_rejected_before_lock() {
    /*
     * 最初の計画で既存本文を確認したsnapshotを取得する
     */
    let shared = stop();
    let server = Server::new(vec![identity(OLD, false),
        meta("/fixture/a", false),
        Reply::new(200, b"before".to_vec())]).await;
    let first = executor(&server, &shared, false, &[(b"a.md", b"target")])
        .await;
    let PageProgress::ExistingDifferent(snapshot) = prepare(&first, 0).await
        else { panic!("snapshot") };
    /*
     * 別計画へ渡したsnapshotの拒否と、lock要求がないことを確認する
     */
    let second = executor(&server, &shared, false, &[(b"a.md", b"target")])
        .await;
    let step = change(&second, 0, snapshot, None).await;
    assert!(step.is_failed());
    methods(&server, &["GET", "GET", "GET"]);
    assert!(shared.checkpoint().is_ok());
}

///
/// 実run futureはSendで開始後dropを共有取消として扱う。
///
/// # 概要
/// 正常renew/meta前の同期点でのみabortし、追加解放を発行しない。
///
/// # 注記
/// process開始時takeされたActionは別ページ/試験へ残らない。
///
/// # 戻り値
/// なし。Send境界/取消共有/HTTP停止が異なれば失敗する。
///
#[tokio::test]
async fn dropped_run_is_send_and_cancels_without_extra_release() {
    let shared = stop();
    let observer = shared.clone();
    let server = Server::new(initial()).await;
    let executor = executor(&server, &shared, false,
        &[(b"a.md", b"target")]).await;
    let (started, ready) = tokio::sync::oneshot::channel();
    let (_resume, release) = tokio::sync::oneshot::channel();
    action(ProcessingStage::Recheck, 1,
        ActionKind::Pause { started, resume: release });
    let task = tokio::spawn(executor.run());
    /*
     * 開始/renew終了を同期し実futureだけを破棄する
     */
    tokio::time::timeout(Duration::from_secs(2), ready).await.unwrap().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(observer.checkpoint(), Err(SourceError::Cancelled));
    methods(&server, &["GET", "GET", "GET", "POST", "PUT"]);
}

///
/// 更新完了直後の停止と空計画HTTP0を実runで確認する。
///
/// # 概要
/// 同じdriverの確定先行結果を停止で未実行へ戻さない。
///
/// # 注記
/// 未使用Scriptが空計画では一件も呼ばれないことも確認する。
///
/// # 戻り値
/// なし。完了消失/空計画HTTPで失敗する。
///
#[tokio::test]
async fn completed_update_survives_stop_and_empty_run_sends_nothing() {
    /*
     * 完了直後の停止と次の独立した空計画を区別して確認する
     */
    let shared = stop();
    let mut replies = initial();
    recheck_script(&mut replies, b"before");
    replies.extend([Reply::locked(TOKEN3), Reply::new(204, Vec::new())]);
    let server = Server::new(replies).await;
    let executor = executor(&server, &shared, false,
        &[(b"a.md", b"target"), (b"b.md", b"later")]).await;
    action(ProcessingStage::Finished, 0, ActionKind::Cancel);
    let batch = executor.run().await;
    assert!(matches!(batch.pages()[0].execution(),
        PageExecution::Completed(Completion::Updated)));
    assert!(matches!(batch.pages()[1].execution(), PageExecution::NotExecuted));
    let shared = stop();
    let server = Server::new(Vec::new()).await;
    let executor = self::executor(&server, &shared, false, &[]).await;
    assert!(executor.run().await.pages().is_empty());
    methods(&server, &[]);
}
