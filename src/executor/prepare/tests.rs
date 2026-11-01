/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実WikiClientを使う初回分岐と元失敗・操作間停止の接続試験
//!

pub(in crate::executor) mod mock;

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::executor::tests::{location, plan, stop};
use crate::executor::types::{ExecutionBatch, PageExecution};
use crate::source::SourceError;
use crate::wiki_client::types::FailureKind;

use self::mock::{Reply, Server, TOKEN2};
use super::*;

// fixtureのULIDであり、実token/実ページIDを読まない。
const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const TOKEN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

thread_local! {
    // 各試験threadの次のprepareだけが所有し、余った操作も返却時にdrop。
    static ACTION: RefCell<Option<Action>> = const { RefCell::new(None) };
}

///
/// 同じcontextの段階間でだけ停止や保管失敗を注入する。
///
pub(super) struct Action {
    /// 対象となるcaller段階。
    stage: ProcessingStage,
    /// 同段階の先行境界を何回通すか。
    skip: usize,
    /// 一回だけ消費する試験操作。
    kind: ActionKind,
}

///
/// 製品設定に存在しない試験内の同期操作。
///
enum ActionKind {
    /// 同じcontextを取消する。
    Cancel,
    /// 再起算しない同じdeadlineまで待つ。
    WaitDeadline,
    /// 完了通知後に外側futureを破棄できる同期点。
    Pause {
        /// 境界へ到達した通知。
        started: tokio::sync::oneshot::Sender<()>,
        /// 同じ試験だけの再開通知。
        resume: tokio::sync::oneshot::Receiver<()>,
    },
    /// fixtureのreadonly本文だけを削除する。
    RemoveFile(PathBuf),
}

///
/// 呼出試験threadの操作を一度だけprepareへmoveする。
///
/// # 戻り値
/// 次のprepare以外へ漏れない同期操作
///
pub(super) fn take_action() -> Option<Action> {
    ACTION.with(|action| action.borrow_mut().take())
}

///
/// 明示操作直前の同期点で一度だけ試験操作を消費する。
///
/// # 概要
/// TLSをawait越しに借りず、prepareが所有する操作だけを使う。
///
/// # 引数
/// * `stop` - 同じ停止context
/// * `stage` - 次のcaller段階
/// * `action` - このprepareだけの操作
///
/// # 戻り値
/// なし。直後の製品checkpointが停止を検出する。
///
pub(super) async fn before_stage(stop: &StopContext, stage: ProcessingStage,
    action: &mut Option<Action>)
{
    if !action.as_ref().is_some_and(|action| action.stage == stage) { return; }
    if let Some(action) = action.as_mut() {
        if action.skip > 0 {
            action.skip -= 1;
            return;
        }
    }
    /*
     * 操作を借用からmoveして同期/取消をこの一件へ閉じる
     */
    match action.take().unwrap().kind {
        ActionKind::Cancel => stop.cancel(),
        ActionKind::WaitDeadline => { let _ = stop.wait_stopped().await; }
        ActionKind::Pause { started, resume } => {
            let _ = started.send(());
            let _ = resume.await;
        }
        ActionKind::RemoveFile(path) => std::fs::remove_file(path).unwrap(),
    }
}

///
/// 正常PUT返却済みの確定完了直後へ取消を注入する。
///
/// # 引数
/// * `stop` - 同じ境界
/// * `action` - このprepareが所有する操作
///
/// # 戻り値
/// なし。確定Completedを失敗へ戻す検査はしない。
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
/// 同期操作を次の一件へだけ設定する。
///
/// # 引数
/// * `stage` - 対象段階
/// * `skip` - 同段階の先行境界数
/// * `kind` - fixture操作
///
/// # 戻り値
/// なし。グローバルな時刻/取消状態は変更しない。
///
fn action(stage: ProcessingStage, skip: usize, kind: ActionKind) {
    ACTION.with(|value| {
        *value.borrow_mut() = Some(Action { stage, skip, kind });
    });
}

///
/// 全件検証済み計画と実WikiClientを同じ停止から準備する。
///
/// # 概要
/// モード変更はcfg(test)の私有fixtureだけに閉じ製品設定入口を変えない。
///
/// # 引数
/// * `server` - 独立loopback server
/// * `shared` - 同じcontext
/// * `mode` - 検証済みModeの列挙値
/// * `bytes` - fixture本文
///
/// # 戻り値
/// 成立計画を所有するexecutor
///
async fn executor(server: &Server, shared: &StopContext, mode: Mode,
    bytes: &[u8]) -> Executor
{
    let input = plan(&[(b"a.md", bytes)], shared).await;
    let config = crate::cmd_args::resolve::wiki_config_fixture(&server.url,
        true, shared.deadline());
    let mut executor = Executor::new(input, &config, shared.clone()).unwrap();
    executor.mode = mode;
    executor
}

///
/// 正常lookupまたはPOSTの完全ID応答を作る。
///
/// # 引数
/// * `status` - 200または201
///
/// # 戻り値
/// POSTだけdummy lock付きの正常応答
///
fn identity(status: u16) -> Reply {
    let mut reply = Reply::new(status, format!("{{\"id\":\"{ID}\"}}")
        .into_bytes());
    reply.lock = status == 201;
    reply
}

///
/// 有限metadataの正常/競合状態を指定する。
///
/// # 引数
/// * `path` - dummy期待pathまたは相違path
/// * `deleted` - 削除済みを観測させるか
/// * `locked` - 既存lockフラグ
///
/// # 戻り値
/// 完全metadata応答
///
fn metadata(path: &str, deleted: bool, locked: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"{kind}\",\
        \"value\":\"{path}\"}},\"deleted\":{deleted},\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}").into_bytes())
}

///
/// Finishedを既存batchへmoveして識別情報と元結果を得る。
///
/// # 引数
/// * `progress` - 実prepare結果
/// * `executor` - 同じ成立計画
///
/// # 戻り値
/// 判明情報、主結果、最後の段階
///
fn finished(progress: PageProgress, executor: &Executor)
    -> (KnownPageFacts, PageExecution, ProcessingStage)
{
    let PageProgress::Finished(step) = progress else { panic!("unfinished") };
    let mut batch = ExecutionBatch::new(&executor.plan);
    batch.record(0, step).unwrap();
    let (mut pages, _, _, rejected) = batch.into_parts();
    assert!(rejected.is_empty());
    let (_, known, stage, execution, cleanup, attempted, _details) =
        pages.remove(0).into_parts();
    assert!(!attempted);
    assert!(cleanup.is_none());
    (known, execution, stage)
}

///
/// 完全要求のmethod/targetを独立期待列へ照合する。
///
/// # 引数
/// * `server` - 専用server
/// * `methods` - 正しい要求順
///
/// # 戻り値
/// なし。隠れた再送/解放/再照会を拒否する。
///
fn methods(server: &Server, methods: &[&str]) {
    server.inspect(|requests| {
        let actual: Vec<_> = requests.iter().map(|v| v.method.as_str())
            .collect();
        assert_eq!(actual, methods);
        assert!(requests.iter().all(|v| !v.target.contains("amend=")
            && !v.target.contains("recursive=")));
    });
}

///
/// 全モードの初回404/410だけがPOST→PUTで新規作成する。
///
/// # 概要
/// 完全PUT本文と新IDを確認し、空本文/BOM/CRLF/多byteを加工しない。
///
/// # 注記
/// 3モード×2不存在status、空/多byte本文を各専用serverで照合する。
///
/// # 戻り値
/// なし。要求順や本文/newIDが異なれば失敗する。
///
#[tokio::test]
async fn all_modes_create_from_lookup_absence_with_exact_bytes() {
    for mode_index in 0..3 {
        for absence in [404, 410] {
            /*
             * 各caseの独立した計画/client/scriptを作る
             */
            for bytes in [b"".as_slice(), "\u{feff}日本語\r\n".as_bytes()] {
                let shared = stop();
                let server = Server::new(vec![Reply::new(absence, Vec::new()),
                    identity(201), Reply::locked(TOKEN2),
                    Reply::new(204, Vec::new())]).await;
                let mode = match mode_index {
                    0 => Mode::Error, 1 => Mode::Update, _ => Mode::Overwrite,
                };
                let executor = executor(&server, &shared, mode, bytes).await;
                let (known, result, stage) = finished(prepare(&executor, 0)
                    .await, &executor);
                assert!(matches!(result,
                    PageExecution::Completed(Completion::Created)));
                assert_eq!(stage, ProcessingStage::Finished);
                assert_eq!(known.into_parts().2.unwrap().as_str(), ID);
                methods(&server, &["GET", "POST", "PUT", "PUT"]);
                /*
                 * mockが受信した全Content-Length bytesを独立に確認する
                 */
                server.inspect(|requests| {
                    assert!(requests[0].target.contains("path=%2Ffixture%2Fa"));
                    assert!(requests[1].body.is_empty());
                    assert_eq!(requests[3].body.as_slice(), bytes);
                    assert_eq!(requests[3].target,
                        format!("/api/pages/{ID}/source"));
                    assert!(requests[3].lock_auth);
                    assert_eq!(requests[2].lock_version, Some(1));
                    assert_eq!(requests[3].lock_version, Some(2));
                });
                assert!(shared.checkpoint().is_ok());
            }
        }
    }
}

///
/// error既存は本文/metaを読まずlookupだけで拒否する。
///
/// # 注記
/// 入力本文が同一でも判断前に取得しないことを要求列へ照合する。
///
/// # 戻り値
/// なし。余分な読取/変更または既存ID消失で失敗する。
///
#[tokio::test]
async fn existing_error_rejects_without_reading_body_or_metadata() {
    let shared = stop();
    let server = Server::new(vec![identity(200)]).await;
    let executor = executor(&server, &shared, Mode::Error, b"same").await;
    let (known, result, stage) = finished(prepare(&executor, 0).await,
        &executor);
    assert!(matches!(result, PageExecution::Failed(ExecutorFailure::Local(v))
        if v.kind() == LocalKind::Conflict));
    assert_eq!(stage, ProcessingStage::Lookup);
    assert_eq!(known.into_parts().0.unwrap().as_str(), ID);
    methods(&server, &["GET"]);
    assert!(shared.checkpoint().is_ok());
}

///
/// lockedフラグに依存せず同一本文は変更せずスキップする。
///
/// # 概要
/// 両モードとlock状態を分け、正常meta/sourceの完全bytesを比較する。
///
/// # 注記
/// 成功は実prepareと実WikiClientの結果でfixture processorではない。
///
/// # 戻り値
/// なし。同一skipでlock/変更を送れば失敗する。
///
#[tokio::test]
async fn identical_body_skips_without_lock_or_changes() {
    for overwrite in [false, true] {
        for locked in [false, true] {
            let shared = stop();
            let bytes = b"\xef\xbb\xbfsame\r\n";
            let server = Server::new(vec![identity(200),
                metadata("/fixture/a", false, locked),
                Reply::new(200, bytes.to_vec())]).await;
            let mode = if overwrite { Mode::Overwrite } else { Mode::Update };
            let executor = executor(&server, &shared, mode, bytes).await;
            /*
             * 元識別情報と同一skipを確認し三読取以外がないことを照合する
             */
            let (known, result, _) = finished(prepare(&executor, 0).await,
                &executor);
            assert!(matches!(result,
                PageExecution::Completed(Completion::SkippedIdentical)));
            let (old, revision, new, _, _) = known.into_parts();
            assert_eq!(old.unwrap().as_str(), ID);
            assert_eq!(revision, Some(7));
            assert!(new.is_none());
            methods(&server, &["GET", "GET", "GET"]);
        }
    }
}

///
/// 相違は未完snapshotとしてreadonly本文と旧ID/revisionを返す。
///
/// # 概要
/// 同量bytes差と異サイズを両モードで試し、寿命/秘匿も照合する。
///
/// # 注記
/// G4未実装をCompleted/架空Conflictへ変換しない。
///
/// # 戻り値
/// なし。相違の成功補完/所有喪失/余分なHTTPで失敗する。
///
#[tokio::test]
async fn different_body_returns_owned_continuation_without_mutation() {
    for overwrite in [false, true] {
        for bytes in [b"ab".as_slice(), b"longer".as_slice()] {
            let shared = stop();
            let server = Server::new(vec![identity(200),
                metadata("/fixture/a", false, false),
                Reply::new(200, bytes.to_vec())]).await;
            let mode = if overwrite { Mode::Overwrite } else { Mode::Update };
            let executor = executor(&server, &shared, mode, b"aa").await;
            let progress = prepare(&executor, 0).await;
            /*
             * 未完状態の表示とmove後の事前本文所有を独立確認する
             */
            let display = format!("{progress:?}");
            for sentinel in [ID, TOKEN, "/fixture", "dummy-wiki-secret"] {
                assert!(!display.contains(sentinel));
            }
            let PageProgress::ExistingDifferent(snapshot) = progress else {
                panic!("owned continuation");
            };
            let (id, revision, body) = snapshot
                .into_parts_for(&executor.plan, 0).unwrap();
            assert_eq!(revision, 7);
            assert_eq!(id.as_str(), ID);
            let path = location(&body.open().unwrap());
            let mut actual = Vec::new();
            body.open().unwrap().read_to_end(&mut actual).unwrap();
            assert_eq!(actual, bytes);
            assert!(body.open().unwrap().write_all(b"mutation").is_err());
            methods(&server, &["GET", "GET", "GET"]);
            drop(body);
            assert!(!path.parent().unwrap().exists());
        }
    }
}

///
/// 初回lookupの不正/喪失を不存在にせず元失敗で停止する。
///
/// # 概要
/// status観測あり/なしと不完IDを分け、余分なPOSTを拒否する。
///
/// # 注記
/// 元operation/status/knownIDとcaller Lookupの接続を確認する。
///
/// # 戻り値
/// なし。元失敗の喪失または再照会で失敗する。
///
#[tokio::test]
async fn lookup_failures_never_become_absence_or_retry() {
    for reply in [Reply::new(500, b"{}".to_vec()),
        Reply { status: None, body: Vec::new(), lock: false, token: TOKEN },
        Reply::new(200, b"{\"id\":\"01AR".to_vec())]
    {
        let expected = reply.status;
        let shared = stop();
        let server = Server::new(vec![reply]).await;
        let executor = executor(&server, &shared, Mode::Update, b"x").await;
        /*
         * HTTP観測事実とcaller段階を投影せず確認する
         */
        let (known, result, stage) = finished(prepare(&executor, 0).await,
            &executor);
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else {
            panic!("original lookup failure");
        };
        assert_eq!(failure.facts().operation(), WikiOperation::Lookup);
        assert_eq!(failure.facts().status(), expected);
        assert_eq!(stage, ProcessingStage::Lookup);
        assert!(known.into_parts().2.is_none());
        methods(&server, &["GET"]);
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// meta/sourceの404/410は初回不存在として扱わない。
///
/// # 概要
/// 既存IDとsource段階の旧revisionを保持し変更要求なしで停止する。
///
/// # 注記
/// 各APIとstatusの組を独立scriptで確認する。
///
/// # 戻り値
/// なし。後続POSTや元operation喪失で失敗する。
///
#[tokio::test]
async fn later_absence_status_keeps_existing_identity_and_operation() {
    for source in [false, true] {
        for status in [404, 410] {
            let shared = stop();
            let mut replies = vec![identity(200)];
            if source { replies.push(metadata("/fixture/a", false, false)); }
            replies.push(Reply::new(status, b"{}".to_vec()));
            let server = Server::new(replies).await;
            let executor = executor(&server, &shared, Mode::Update, b"x").await;
            /*
             * caller段階と元APIを保ち初回404/410と区別する
             */
            let (known, result, stage) = finished(prepare(&executor, 0).await,
                &executor);
            let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
                else { panic!("original read failure") };
            let operation = if source { WikiOperation::ReadSource }
                else { WikiOperation::ReadMeta };
            assert_eq!(failure.facts().operation(), operation);
            assert_eq!(failure.facts().status(), Some(status));
            assert_eq!(stage, ProcessingStage::ReadExisting);
            let (old, revision, _, _, _) = known.into_parts();
            assert_eq!(old.unwrap().as_str(), ID);
            assert_eq!(revision, if source { Some(7) } else { None });
            let expected = if source { vec!["GET", "GET", "GET"] }
                else { vec!["GET", "GET"] };
            methods(&server, &expected);
        }
    }
}

///
/// metadata状態の不一致と本文不正を同一スキップにしない。
///
/// # 概要
/// 観測競合は局所Conflict、不正応答は元Wiki kindへ分離する。
///
/// # 注記
/// 正常な相違path/削除と不正UTF-8/U+FFFDを代表境界として確認する。
///
/// # 戻り値
/// なし。無効応答を完了へ補完すれば失敗する。
///
#[tokio::test]
async fn invalid_metadata_state_or_body_never_skips() {
    for deleted in [false, true] {
        let shared = stop();
        let path = if deleted { "/fixture/a" } else { "/other" };
        let server = Server::new(vec![identity(200),
            metadata(path, deleted, false)]).await;
        let executor = executor(&server, &shared, Mode::Update, b"x").await;
        let (_, result, _) = finished(prepare(&executor, 0).await, &executor);
        assert!(matches!(result,
            PageExecution::Failed(ExecutorFailure::Local(v))
                if v.kind() == LocalKind::Conflict));
        methods(&server, &["GET", "GET"]);
    }
    /*
     * 検査済みの元kindと既知IDを保持して不正本文を停止する
     */
    for bytes in [b"\xff".as_slice(), "\u{fffd}".as_bytes()] {
        let shared = stop();
        let server = Server::new(vec![identity(200),
            metadata("/fixture/a", false, false),
            Reply::new(200, bytes.to_vec())]).await;
        let executor = executor(&server, &shared, Mode::Update, b"x").await;
        let (_, result, _) = finished(prepare(&executor, 0).await, &executor);
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else {
            panic!("body validation failure");
        };
        assert!(matches!(failure.facts().kind(),
            FailureKind::BodyUtf8 | FailureKind::BodyReplacement));
        assert_eq!(failure.facts().known_id().unwrap().as_str(), ID);
        methods(&server, &["GET", "GET", "GET"]);
    }
}

///
/// POST header不正でも完全確定IDを残し部分JSONでは補わない。
///
/// # 概要
/// 完全JSONの後と未完JSONの前を分け、PUT/解除/再POST0を確認する。
///
/// # 注記
/// lock header欠落の同じ201でも確定IDの範囲は異なる。
///
/// # 戻り値
/// なし。候補IDの補完や確定ID喪失で失敗する。
///
#[tokio::test]
async fn failed_post_keeps_only_complete_created_identity() {
    for complete in [false, true] {
        let shared = stop();
        let mut reply = identity(201);
        reply.lock = false;
        if !complete { reply.body.pop(); }
        let server = Server::new(vec![Reply::new(404, Vec::new()), reply])
            .await;
        let executor = executor(&server, &shared, Mode::Error, b"x").await;
        /*
         * 元201/ID/caller段階を保ち後続要求がないことを確認する
         */
        let (known, result, stage) = finished(prepare(&executor, 0).await,
            &executor);
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else {
            panic!("post failure");
        };
        assert_eq!(stage, ProcessingStage::CreateDraft);
        assert_eq!(failure.facts().status(), Some(201));
        assert_eq!(failure.facts().known_id().is_some(), complete);
        assert_eq!(known.into_parts().2.is_some(), complete);
        methods(&server, &["GET", "POST"]);
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// PUT失敗でも確定newIDを保ちdraftを自動回収しない。
///
/// # 概要
/// 正常送信後のstatusあり/なしを代表境界として照合する。
///
/// # 注記
/// 400/403は照合せず、500/喪失の照合失敗でも元PUTを保持する。
///
/// # 戻り値
/// なし。newID喪失や余分な解除/DELETEで失敗する。
///
#[tokio::test]
async fn failed_put_retains_new_id_without_draft_cleanup() {
    for status in [Some(400), Some(403), Some(500), None] {
        let shared = stop();
        let reply = Reply { status, body: b"{}".to_vec(), lock: false,
            token: TOKEN };
        let mut replies = vec![Reply::new(404, Vec::new()), identity(201),
            Reply::locked(TOKEN2), reply];
        let rechecking = status == Some(500) || status.is_none();
        if rechecking { replies.push(Reply::new(503, Vec::new())); }
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, Mode::Error, b"x").await;
        /*
         * 元PUT観測と完全POSTのnewIDを別々に確認する
         */
        let (known, result, stage) = finished(prepare(&executor, 0).await,
            &executor);
        let PageExecution::Failed(ExecutorFailure::Wiki(failure)) = result
            else {
            panic!("put failure");
        };
        assert_eq!(stage, if rechecking { ProcessingStage::Reconcile }
            else { ProcessingStage::PutSource });
        assert_eq!(failure.facts().status(), status);
        assert_eq!(failure.facts().operation(), WikiOperation::PutSource);
        assert_eq!(known.into_parts().2.unwrap().as_str(), ID);
        methods(&server, if rechecking {
            &["GET", "POST", "PUT", "PUT", "GET"]
        } else { &["GET", "POST", "PUT", "PUT"] });
        assert!(shared.checkpoint().is_ok());
    }
}

///
/// 操作間取消で次のHTTP/比較を開始せず判明情報を残す。
///
/// # 概要
/// caller段階の同期点で取消し、先行要求数を明示して確認する。
///
/// # 注記
/// 初回前/POST前/PUT前/meta後/source後を一件ごとに独立確認する。
///
/// # 戻り値
/// なし。停止後要求や判明ID消失で失敗する。
///
#[tokio::test]
async fn shared_cancel_stops_each_unlocked_operation_gap() {
    for (stage, skip, existing, count) in [
        (ProcessingStage::Lookup, 0, false, 0),
        (ProcessingStage::CreateDraft, 0, false, 1),
        (ProcessingStage::PutSource, 0, false, 2),
        (ProcessingStage::ReadExisting, 1, true, 2),
        (ProcessingStage::ReadExisting, 2, true, 3),
    ] {
        let shared = stop();
        let replies = if existing { vec![identity(200),
            metadata("/fixture/a", false, false),
            Reply::new(200, b"x".to_vec())]
        } else { vec![Reply::new(404, Vec::new()), identity(201)] };
        let server = Server::new(replies).await;
        let executor = executor(&server, &shared, Mode::Update, b"x").await;
        action(stage, skip, ActionKind::Cancel);
        /*
         * 同期点取消の元kindと要求数を照合し先行IDだけを残す
         */
        let (known, result, actual_stage) = finished(prepare(&executor, 0)
            .await, &executor);
        assert!(matches!(result,
            PageExecution::Failed(ExecutorFailure::Local(v))
                if v.kind() == LocalKind::Cancelled));
        assert_eq!(actual_stage, stage);
        let new = known.into_parts().2;
        assert_eq!(new.is_some(), stage == ProcessingStage::PutSource);
        server.inspect(|requests| assert_eq!(requests.len(), count));
    }
}

///
/// 同じ期限へ到達したPOST後はPUTを送らず確定IDを保つ。
///
/// # 概要
/// deadlineを再起算せず段階同期点で到達を待つ。
///
/// # 注記
/// 試験時刻の5分仮定やsleep幅の推測に依存しない。
///
/// # 戻り値
/// なし。停止分類/newID/要求数が異なれば失敗する。
///
#[tokio::test]
async fn same_deadline_after_post_prevents_put_and_keeps_identity() {
    let shared = StopContext::new(Instant::now() + Duration::from_secs(2));
    let server = Server::new(vec![Reply::new(404, Vec::new()),
        identity(201)]).await;
    let executor = executor(&server, &shared, Mode::Error, b"x").await;
    action(ProcessingStage::PutSource, 0, ActionKind::WaitDeadline);
    let (known, result, _) = finished(prepare(&executor, 0).await, &executor);
    assert!(matches!(result, PageExecution::Failed(ExecutorFailure::Local(v))
        if v.kind() == LocalKind::Deadline));
    assert_eq!(known.into_parts().2.unwrap().as_str(), ID);
    methods(&server, &["GET", "POST"]);
}

///
/// 比較保管open失敗をHTTPや同一へ偽装せず元causeを保持する。
///
/// # 注記
/// source受信後にfixture計画Fileだけを削除して実比較入口を呼ぶ。
///
/// # 戻り値
/// なし。局所失敗/caller段階/取消非発火が異なれば失敗する。
///
#[tokio::test]
async fn comparison_open_failure_is_local_and_keeps_old_facts() {
    let shared = stop();
    let server = Server::new(vec![identity(200),
        metadata("/fixture/a", false, false), Reply::new(200, b"x".to_vec())])
        .await;
    let executor = executor(&server, &shared, Mode::Update, b"x").await;
    let path = location(&executor.plan.open_body(0).unwrap());
    /*
     * source受信後の比較入口でだけ計画保管のI/O失敗を注入する
     */
    action(ProcessingStage::ReadExisting, 2, ActionKind::RemoveFile(path));
    let (known, result, stage) = finished(prepare(&executor, 0).await,
        &executor);
    assert!(matches!(result, PageExecution::Failed(ExecutorFailure::Local(v))
        if v.kind() == LocalKind::PlanOpen));
    assert_eq!(stage, ProcessingStage::ReadExisting);
    assert_eq!(known.into_parts().1, Some(7));
    methods(&server, &["GET", "GET", "GET"]);
    assert!(shared.checkpoint().is_ok());
}

///
/// 開始済みprepareの実dropは取消しdraftの追加操作をしない。
///
/// # 概要
/// POST後/PUT前の同期点到達後だけ外側futureを破棄する。
///
/// # 注記
/// TLS操作はprepareがtake済みで、別タスク/試験への影響を残さない。
///
/// # 戻り値
/// なし。取消未共有/解除/再POSTで失敗する。
///
#[tokio::test]
async fn dropped_prepare_cancels_without_draft_cleanup() {
    let shared = stop();
    let observer = shared.clone();
    let server = Server::new(vec![Reply::new(404, Vec::new()),
        identity(201)]).await;
    let executor = executor(&server, &shared, Mode::Error, b"x").await;
    let (started, ready) = tokio::sync::oneshot::channel();
    let (_resume, release) = tokio::sync::oneshot::channel();
    action(ProcessingStage::PutSource, 0,
        ActionKind::Pause { started, resume: release });
    let task = tokio::spawn(async move { prepare(&executor, 0).await });
    /*
     * POST確定後の同期点を待ち実futureを破棄する
     */
    tokio::time::timeout(Duration::from_secs(2), ready).await.unwrap().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(observer.checkpoint(), Err(SourceError::Cancelled));
    methods(&server, &["GET", "POST"]);
}

///
/// 正常PUT返却済みの完了を直後の取消で未実行へ戻さない。
///
/// # 注記
/// 返却後の同期取消を入れ、結果と共有停止を別々に確認する。
///
/// # 戻り値
/// なし。確定Createdが失われれば失敗する。
///
#[tokio::test]
async fn completed_put_survives_stop_before_next_page() {
    let shared = stop();
    let server = Server::new(vec![Reply::new(404, Vec::new()), identity(201),
        Reply::locked(TOKEN2), Reply::new(204, Vec::new())]).await;
    let executor = executor(&server, &shared, Mode::Error, b"x").await;
    action(ProcessingStage::Finished, 0, ActionKind::Cancel);
    let (known, result, _) = finished(prepare(&executor, 0).await, &executor);
    assert!(matches!(result, PageExecution::Completed(Completion::Created)));
    assert_eq!(known.into_parts().2.unwrap().as_str(), ID);
    assert_eq!(shared.checkpoint(), Err(SourceError::Cancelled));
    methods(&server, &["GET", "POST", "PUT", "PUT"]);
}
