/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! loopback HTTPと同期点でWiki読取・受信・取消・回収を検証する
//!

mod mock;
mod series;
pub(in crate::wiki_client) mod writes;

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cmd_args::resolve::wiki_config_fixture;
use crate::report::types::{PageId, PagePath};
use crate::source::{SourceError, StopContext};

use self::mock::{Reply, Server};
use super::*;
use super::receive::Received;
use super::types::WikiOperation;
use super::types::{FailureKind, PathKind, PathRelation, SendState};

const ID: &str = "00000000000000000000000000";

thread_local! {
    static ACTION: RefCell<Option<WorkerAction>> =
        const { RefCell::new(None) };
    static METRICS: RefCell<Option<Arc<Metrics>>> =
        const { RefCell::new(None) };
    static PIECE_LIMIT: RefCell<usize> =
        const { RefCell::new(super::receive::WORK_CHUNK) };
    static RECEIVE_STARTED: RefCell<Option<tokio::sync::oneshot::Sender<()>>> =
        const { RefCell::new(None) };
}

///
/// testだけの確定分割幅を返す。
///
/// # 戻り値
/// 製品作業上限以内のpiece幅（bytes）
///
pub(super) fn piece_limit() -> usize {
    PIECE_LIMIT.with(|limit| *limit.borrow())
        .clamp(1, super::receive::WORK_CHUNK)
}

///
/// 確定header後の受信入口を試験同期点として通知する。
///
/// # 戻り値
/// なし。このthreadで登録した通知を一度だけ消費する。
///
pub(super) fn notify_receive_started() {
    RECEIVE_STARTED.with(|slot| {
        if let Some(sender) = slot.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
}

///
/// worker境界だけで使う失敗・所有権注入。
///
pub(super) enum WorkerAction {
    Io, Panic,
    Hold {
        ready: tokio::sync::oneshot::Sender<PathBuf>,
        release: std::sync::mpsc::Receiver<()>,
    },
}

///
/// 内部作業pieceだけを観察する指標。絶対RSS測定ではない。
///
#[derive(Default)]
pub(super) struct Metrics {
    max_piece: AtomicUsize,
    capacity: AtomicUsize,
}

///
/// workerへ一度だけ注入を移動する。
///
/// # 戻り値
/// threadからworkerへ移せる試験操作
///
pub(super) fn take_worker_action() -> Option<WorkerAction> {
    ACTION.with(|action| action.borrow_mut().take())
}

///
/// producerの作業量だけを試験内で記録する。
///
/// # 引数
/// * `length` - 分割後pieceの長さ
/// * `capacity` - channel容量
///
/// # 戻り値
/// なし。設定されたmetricsだけを更新する。
///
pub(super) fn observe_piece(length: usize, capacity: usize) {
    METRICS.with(|metrics| {
        if let Some(metrics) = metrics.borrow().as_ref() {
            metrics.max_piece.fetch_max(length, Ordering::Relaxed);
            metrics.capacity.store(capacity, Ordering::Relaxed);
        }
    });
}

///
/// 同期worker開始時の通常失敗とJoinErrorを注入する。
///
/// # 注記
/// Ioは保存処理前の型付き分類注入であり、実ディスク故障の再現ではない。
///
/// # 引数
/// * `action` - この受信だけの試験操作
///
/// # 戻り値
/// 通常なら成功、I/O注入なら固定失敗
///
pub(super) fn before_work(action: &Option<WorkerAction>)
    -> Result<(), FailureKind>
{
    match action {
        Some(WorkerAction::Io) => Err(FailureKind::Io),
        Some(WorkerAction::Panic) => panic!("injected worker failure"),
        _ => Ok(()),
    }
}

///
/// workerが結果を所有する同期点で、future破棄を観察する。
///
/// # 引数
/// * `result` - worker所有の結果
/// * `action` - この受信だけの試験操作
/// * `stop` - 共有context
///
/// # 戻り値
/// なし。設定された同期点だけを一度実行する。
///
pub(super) fn after_work(result: &Result<Received, FailureKind>,
    action: Option<WorkerAction>, stop: &StopContext)
{
    if let Some(WorkerAction::Hold { ready, release }) = action {
        if let Ok(Received::Body(body)) = result {
            let file = body.open().unwrap();
            let path = fs::read_link(format!("/proc/self/fd/{}",
                file.as_raw_fd())).unwrap();
            let _ = ready.send(path);
            let _ = release.recv_timeout(Duration::from_secs(10));
            let _ = stop.checkpoint();
        }
    }
}

///
/// 同一期限の製品設定fixtureとclientを作る。
///
/// # 引数
/// * `server` - loopback対象
/// * `bearer` - ダミー認証方式
///
/// # 戻り値
/// clientと共有context
///
fn client(server: &Server, bearer: bool) -> (WikiClient, StopContext) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let config = wiki_config_fixture(&server.url(), bearer, deadline);
    let stop = StopContext::new(deadline);
    (WikiClient::new(&config, stop.clone()).unwrap(), stop)
}

///
/// lookup成功JSONのfixtureを作る。
///
/// # 戻り値
/// 固定IDの応答
///
fn found() -> Reply {
    Reply::new(200, format!(r#"{{"id":"{ID}"}}"#).into_bytes())
}

///
/// bodyのreadonly handleを全読込する。
///
/// # 引数
/// * `body` - 検証成功した受信lease
///
/// # 戻り値
/// fixture照合用bytes
///
fn contents(body: &ReceivedBody) -> Vec<u8> {
    let mut bytes = Vec::new();
    body.open().unwrap().read_to_end(&mut bytes).unwrap();
    bytes
}

///
/// callback等を使わずloopbackへの要求数を待つ。
///
/// # 引数
/// * `server` - 観測対象
/// * `count` - 必要な要求数
///
/// # 戻り値
/// なし。timeoutまでに要求が来たことを確認する。
///
async fn requested(server: &Server, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.count() < count {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
}

///
/// Wiki専用Basic/Bearerとencoded queryを製品clientで確認する。
///
/// # 概要
/// pathの#/?/&/+を構文として使わず、decoded queryが元pathと一致する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 実環境の資格情報を使わず、ダミーheaderをserver内だけで照合する。
///
#[tokio::test]
async fn uses_wiki_authentication_and_escaped_query() {
    /*
     * 二つの認証方式を独立設定で確認する
     */
    for bearer in [false, true] {
        let server = Server::new(vec![found()]);
        let (client, _) = client(&server, bearer);
        let path = PagePath::new("/日本語#?&+").unwrap();
        assert!(matches!(client.lookup(&path).await.unwrap(),
            LookupResult::Found(id) if id.as_str() == ID));
        let request = server.requests().remove(0);
        let expected = if bearer { "Bearer dummy-wiki-secret" }
            else { "Basic ZHVtbXktdXNlcjpkdW1teS1wYXNzd29yZA==" };
        assert!(request.authorization == expected, "dummy auth mismatch");
        let url = url::Url::parse(&format!("http://fixture{}", request.target))
            .unwrap();
        assert_eq!(url.path(), "/api/pages/id");
        let pairs: Vec<_> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0.as_ref(), "path");
        assert_eq!(pairs[0].1.as_ref(), path.as_str());
    }
}

///
/// 異なる期限と既停止contextは送信前に拒否する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// client生成だけでHTTP0を確認し、後から長い期限へ延長できない。
///
#[tokio::test]
async fn rejects_mismatched_or_stopped_context_before_http() {
    let server = Server::new(vec![found()]);
    let deadline = Instant::now() + Duration::from_secs(60);
    let config = wiki_config_fixture(&server.url(), true, deadline);
    let wrong = StopContext::new(deadline + Duration::from_secs(1));
    assert_eq!(WikiClient::new(&config, wrong).unwrap_err().kind(),
        FailureKind::ContextMismatch);
    let stop = StopContext::new(deadline);
    stop.cancel();
    assert_eq!(WikiClient::new(&config, stop).unwrap_err().kind(),
        FailureKind::Cancelled);
    assert_eq!(server.count(), 0);
}

///
/// 初回lookup以外の404/410を不存在成功へ写さない。
///
/// # 概要
/// 同じstatusを異なる操作へ渡し、既知IDと確定statusを保持する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// source/metaの404/410は失敗、lookupだけは正常状態とする。
///
#[tokio::test]
async fn distinguishes_lookup_absence_from_other_operation_failures() {
    let server = Server::new(vec![
        Reply::new(404, b"reason".to_vec()), Reply::new(410, Vec::new()),
        Reply::new(404, Vec::new()), Reply::new(410, b"invalid JSON".to_vec()),
    ]);
    let (client, stop) = client(&server, true);
    let path = PagePath::new("/a").unwrap();
    let id = PageId::new(ID).unwrap();
    assert!(matches!(client.lookup(&path).await.unwrap(),
        LookupResult::NotFound));
    assert!(matches!(client.lookup(&path).await.unwrap(),
        LookupResult::Deleted));
    /*
     * 失敗結果でもcaller既知IDとstatusを捨てない
     */
    let source = client.read_source(&id).await.unwrap_err();
    let meta = client.read_meta(&id, &path).await.unwrap_err();
    assert_eq!(source.status(), Some(404));
    assert_eq!(meta.status(), Some(410));
    assert_eq!(source.known_id().unwrap().as_str(), ID);
    assert_eq!(meta.known_id().unwrap().as_str(), ID);
    assert_eq!(source.sent(), SendState::Started);
    assert!(stop.checkpoint().is_ok());
}

///
/// GETの3xxを追従せず、500失敗後の明示GETも許す。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// Locationは同じloopbackへ向け、追従0・自動再送0を要求数で確認する。
///
#[tokio::test]
async fn rejects_redirect_without_retry_and_allows_explicit_next_read() {
    let server = Server::new(vec![Reply::redirect("/moved"), found(),
        Reply::new(500, b"{malformed".to_vec()), found()]);
    let (client, stop) = client(&server, true);
    let path = PagePath::new("/a").unwrap();
    let redirect = client.lookup(&path).await.unwrap_err();
    assert_eq!(redirect.status(), Some(302));
    assert_eq!(server.count(), 1);
    assert!(client.lookup(&path).await.is_ok());
    let error = client.lookup(&path).await.unwrap_err();
    assert_eq!(error.status(), Some(500));
    assert!(stop.checkpoint().is_ok());
    assert!(client.lookup(&path).await.is_ok());
    assert_eq!(server.count(), 4);
}

///
/// nested metadataにid欄を要求せず、false flagと相違pathを保持する。
///
/// # 概要
/// metadata応答の有限fieldとcaller既知IDを独立に照合する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// decoded Unicode表記を変更せず、DifferentをExecutor判断用に返す。
///
#[tokio::test]
async fn reads_metadata_with_caller_id_and_path_relation() {
    /*
     * caller既知IDと応答の相違pathを独立に照合する
     */
    let body = br#"{"page_info":{"path":{
        "kind":"last_deleted","value":"/other"},
        "deleted":true,"locked":false},"revision_info":{"revision":7}}"#;
    let server = Server::new(vec![Reply::new(200, body.to_vec())]);
    let (client, _) = client(&server, true);
    let id = PageId::new(ID).unwrap();
    let meta = client.read_meta(&id, &PagePath::new("/wanted").unwrap())
        .await.unwrap();
    assert_eq!(meta.id().as_str(), ID);
    assert_eq!(meta.path_kind(), PathKind::LastDeleted);
    assert_eq!(meta.path_relation(), PathRelation::Different);
    assert!(meta.deleted());
    assert!(!meta.locked());
    assert_eq!(meta.revision(), 7);
    assert_eq!(server.requests()[0].target,
        format!("/api/pages/{ID}/meta"));
}

///
/// 入力上限を超える既存本文でも内部pieceは有限にし、readonlyで保持する。
///
/// # 概要
/// 64KiB境界の多byte列・BOM/CRLF/末尾を保ち、lease破棄で回収する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// metricsは作業pieceだけ。transportやtest fixtureを含むRSS測定ではない。
///
#[tokio::test]
async fn receives_large_exact_body_without_input_cap_and_reclaims_lease() {
    let mut expected = b"\xef\xbb\xbf".to_vec();
    expected.resize(64 * 1024 - 1, b'a');
    expected.extend_from_slice("あ".as_bytes());
    expected.resize(2 * 1024 * 1024, b'b');
    expected.extend_from_slice(b"\r\nlast\n");
    let server = Server::new(vec![Reply::new(200, expected.clone())]);
    let (client, _) = client(&server, true);
    let metrics = Arc::new(Metrics::default());
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    let body = client.read_source(&PageId::new(ID).unwrap()).await.unwrap();
    METRICS.with(|slot| *slot.borrow_mut() = None);
    /*
     * 元bytesとreadonly性、作業量と最終回収を個別に観測する
     */
    assert_eq!(contents(&body), expected);
    assert_eq!(body.byte_len(), expected.len() as u64);
    assert!(metrics.max_piece.load(Ordering::Relaxed) <= 64 * 1024);
    assert!(metrics.max_piece.load(Ordering::Relaxed) > 0);
    assert_eq!(metrics.capacity.load(Ordering::Relaxed), 1);
    let mut file = body.open().unwrap();
    let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .unwrap();
    assert!(file.write_all(b"mutation").is_err());
    drop(file);
    drop(body);
    assert!(!path.parent().unwrap().exists());
}

///
/// 空本文を許し、不正UTF-8・置換文字・切断列を拒否する。
///
/// # 概要
/// 正常空本文と三つの不正本文を同じclientへ渡し後続照会を確認する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 受信失敗でglobal取消をせず、確定HTTP200とIDを保持する。
///
#[tokio::test]
async fn validates_empty_and_invalid_source_without_global_cancel() {
    let server = Server::new(vec![
        Reply::new(200, Vec::new()), Reply::new(200, b"\xff".to_vec()),
        Reply::new(200, b"\xef\xbf\xbd".to_vec()),
        Reply::new(200, b"\xe3\x81".to_vec()), found(),
    ]);
    let (client, stop) = client(&server, true);
    let id = PageId::new(ID).unwrap();
    assert_eq!(client.read_source(&id).await.unwrap().byte_len(), 0);
    /*
     * 本文拒否とtransport拒否を分けて固定分類へ写す
     */
    for kind in [FailureKind::BodyUtf8, FailureKind::BodyReplacement,
        FailureKind::BodyUtf8]
    {
        let error = client.read_source(&id).await.unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.status(), Some(200));
        assert_eq!(error.known_id().unwrap().as_str(), ID);
        assert!(stop.checkpoint().is_ok());
    }
    assert!(client.lookup(&PagePath::new("/a").unwrap()).await.is_ok());
}

///
/// worker境界の型付きI/O分類・panicをglobal取消へ変換しない。
///
/// # 概要
/// 独立に注入した二分類の失敗について、statusと後続照会を確認する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 各失敗後に同じcontextで明示GETし、残期限内の再照会を阻害しない。
/// I/Oは保存処理前の分類注入であり、実保存I/O失敗の証明ではない。
///
#[tokio::test]
async fn worker_failures_allow_subsequent_explicit_http() {
    /*
     * local失敗とJoinErrorを独立の受信に注入する
     */
    for action in [WorkerAction::Io, WorkerAction::Panic] {
        let kind = if matches!(action, WorkerAction::Io) { FailureKind::Io }
            else { FailureKind::Worker };
        let server = Server::new(vec![
            Reply::new(200, b"body".to_vec()), found(),
        ]);
        let (client, stop) = client(&server, true);
        ACTION.with(|slot| *slot.borrow_mut() = Some(action));
        let error = client.read_source(&PageId::new(ID).unwrap())
            .await.unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.status(), Some(200));
        assert_eq!(error.known_id().unwrap().as_str(), ID);
        assert!(stop.checkpoint().is_ok());
        assert!(client.lookup(&PagePath::new("/a").unwrap()).await.is_ok());
        assert_eq!(server.count(), 2);
    }
}

///
/// HTTP bodyの途中切断をchannelの正常EOFと混同しない。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// JSON自体が完結しても宣言量不足なら成功にせず、statusを保持する。
///
#[tokio::test]
async fn truncated_transport_never_becomes_normal_json_eof() {
    let server = Server::new(vec![found().truncated(), found()]);
    let (client, stop) = client(&server, true);
    let path = PagePath::new("/a").unwrap();
    let error = client.lookup(&path).await.unwrap_err();
    assert_eq!(error.kind(), FailureKind::Transport);
    assert_eq!(error.status(), Some(200));
    assert!(stop.checkpoint().is_ok());
    assert!(client.lookup(&path).await.is_ok());
}

///
/// permitを受信終了まで保持し、待機中取消で次HTTPを始めない。
///
/// # 概要
/// 一つ目のbodyを止め、二つ目のlookupが送信しないことを観察する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 同じclientを使い、取消後は後続要求も0のまま維持する。
///
#[tokio::test]
async fn permit_is_held_through_receive_and_cancelled_waiter_sends_nothing() {
    let (reply, release) = Reply::new(200, b"body".to_vec()).held();
    let server = Server::new(vec![reply, found()]);
    let (client, stop) = client(&server, true);
    let client = Arc::new(client);
    let first_client = Arc::clone(&client);
    let first = tokio::spawn(async move {
        first_client.read_source(&PageId::new(ID).unwrap()).await
    });
    requested(&server, 1).await;
    /*
     * 二件目をpermit待機に置いてから共有取消を通知する
     */
    let next_client = Arc::clone(&client);
    let next = tokio::spawn(async move {
        next_client.lookup(&PagePath::new("/a").unwrap()).await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.count(), 1);
    stop.cancel();
    assert_eq!(next.await.unwrap().unwrap_err().sent(), SendState::NotStarted);
    assert_eq!(first.await.unwrap().unwrap_err().kind(),
        FailureKind::Cancelled);
    let _ = release.send(());
    assert_eq!(server.count(), 1);
    assert!(client.lookup(&PagePath::new("/a").unwrap()).await.is_err());
    assert_eq!(server.count(), 1);
}

///
/// header受信後の期限到達はstatusを保持し追加HTTPしない。
///
/// # 概要
/// header後の同期点を確認してから期限終了と後続HTTP禁止を検査する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// body受信を止めたloopbackから、同じ絶対期限で終了する。
///
#[tokio::test]
async fn deadline_during_receive_keeps_status_and_stops_next_http() {
    /*
     * 十分な期限内にfixtureを生成し、bodyだけを明示同期で保持する
     */
    let (reply, release) = Reply::new(200, b"body".to_vec()).held();
    let server = Server::new(vec![reply]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let config = wiki_config_fixture(&server.url(), true, deadline);
    let stop = StopContext::new(deadline);
    let client = WikiClient::new(&config, stop.clone()).unwrap();
    let client = Arc::new(client);
    let (ready, started) = tokio::sync::oneshot::channel();
    RECEIVE_STARTED.with(|slot| *slot.borrow_mut() = Some(ready));
    let reader = Arc::clone(&client);
    let task = tokio::spawn(async move {
        reader.read_source(&PageId::new(ID).unwrap()).await
    });
    /*
     * header確定後の入口に到達したことを期限結果と別に検査する
     */
    tokio::time::timeout(Duration::from_secs(2), started)
        .await.unwrap().unwrap();
    let error = task.await.unwrap().unwrap_err();
    let _ = release.send(());
    assert_eq!(error.kind(), FailureKind::Deadline);
    assert_eq!(error.status(), Some(200));
    assert!(client.lookup(&PagePath::new("/a").unwrap()).await.is_err());
    assert_eq!(server.count(), 1);
}

///
/// poll済みfuture破棄ではglobal取消し、worker所有後に回収する。
///
/// # 概要
/// 受信済みbodyをworker同期点に保持し、abortと回収を分けて観察する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// worker終了前のpath・permit所有と、解除後の回収・permit返却を確認する。
///
#[tokio::test]
async fn dropped_read_cancels_and_preserves_worker_owned_cleanup() {
    let server = Server::new(vec![Reply::new(200, b"body".to_vec())]);
    let (client, stop) = client(&server, true);
    let client = Arc::new(client);
    let (ready, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    ACTION.with(|slot| *slot.borrow_mut() = Some(WorkerAction::Hold {
        ready, release: wait,
    }));
    let reader = Arc::clone(&client);
    let task = tokio::spawn(async move {
        reader.read_source(&PageId::new(ID).unwrap()).await
    });
    let path = tokio::time::timeout(Duration::from_secs(2), started)
        .await.unwrap().unwrap();
    /*
     * workerが所有するfileを先に消さず、取消だけを通知する
     */
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(stop.checkpoint(), Err(SourceError::Cancelled));
    assert!(path.exists());
    assert_eq!(client.available_permits(), 0);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.parent().unwrap().exists()
            || client.available_permits() != 1
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
    assert_eq!(client.available_permits(), 1);
}

///
/// client・結果・失敗表示から本文と認証・任意error情報を除外する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// ダミーsentinelを含むerror bodyを受け、statusと有限診断だけを照合する。
///
#[tokio::test]
async fn displays_and_error_diagnostics_do_not_expose_response_or_auth() {
    /*
     * 有限診断だけを選択し、表示のsentinel非露出を確認する
     */
    let error = br#"{"kind":"front_matter","detail":{"type":"syntax","line":2,
        "column":3,"message":"private-body","property_path":"private-path"}}"#;
    let server = Server::new(vec![Reply::new(400, error.to_vec())]);
    let (client, stop) = client(&server, true);
    let failure = client.lookup(&PagePath::new("/private-name").unwrap())
        .await.unwrap_err();
    assert_eq!(failure.status(), Some(400));
    assert_eq!(failure.diagnostics().unwrap().line, Some(2));
    assert!(stop.checkpoint().is_ok());
    let rendered = format!("{client:?} {failure:?} {failure}");
    for secret in ["private-body", "private-path", "private-name",
        "dummy-wiki-secret", "dummy-password", "Authorization"]
    {
        assert!(!rendered.contains(secret));
    }
}

///
/// 401/403はサーバーのread認可結果として受け、statusと既知IDを保つ。
///
/// # 概要
/// opaque Bearerからscopeを推測せず、三読取操作を単発GETで照会する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 拒否後も同じcontextで明示lookup可能。実scope付与の検証は実Wikiで行う。
///
#[tokio::test]
async fn read_authorization_denials_preserve_facts_without_global_cancel() {
    /*
     * 同じダミーtokenでも認可判断は応答statusとして扱う
     */
    for status in [401, 403] {
        let server = Server::new(vec![
            Reply::new(status, b"{}".to_vec()),
            Reply::new(status, b"{}".to_vec()),
            Reply::new(status, b"{}".to_vec()), found(),
        ]);
        let (client, stop) = client(&server, true);
        let id = PageId::new(ID).unwrap();
        let path = PagePath::new("/a").unwrap();
        let lookup = client.lookup(&path).await.unwrap_err();
        let source = client.read_source(&id).await.unwrap_err();
        let meta = client.read_meta(&id, &path).await.unwrap_err();
        /*
         * 送信開始・確定status・caller既知IDを操作ごとに照合する
         */
        for (failure, operation, known) in [
            (&lookup, WikiOperation::Lookup, false),
            (&source, WikiOperation::ReadSource, true),
            (&meta, WikiOperation::ReadMeta, true),
        ] {
            assert_eq!(failure.operation(), operation);
            assert_eq!(failure.kind(), FailureKind::Http);
            assert_eq!(failure.status(), Some(status));
            assert_eq!(failure.sent(), SendState::Started);
            assert_eq!(failure.known_id().map(PageId::as_str),
                if known { Some(ID) } else { None });
        }
        assert!(stop.checkpoint().is_ok());
        assert!(client.lookup(&path).await.is_ok());
        assert_eq!(server.count(), 4);
        /*
         * GETとAPI targetの順序を照合し、ダミー認証は表示しない
         */
        let requests = server.requests();
        assert!(requests.iter().all(|request| request.method == "GET"));
        assert_eq!(requests.iter().map(|request| request.target.clone())
            .collect::<Vec<_>>(), vec![
                "/api/pages/id?path=%2Fa".to_owned(),
                format!("/api/pages/{ID}/source"),
                format!("/api/pages/{ID}/meta"),
                "/api/pages/id?path=%2Fa".to_owned(),
            ]);
        assert!(requests.iter().all(|request|
            request.authorization == "Bearer dummy-wiki-secret"));
    }
}

///
/// 巨大未知値と深いJSONを製品HTTP・有界channel・parserへ通す。
///
/// # 概要
/// 巨大key/string/number、未知depthとmetadataの有限選択抽出を確認する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 通常64KiB作業幅で受信する。fixtureのwrite境界はTCP再結合され得る。
/// metricsはproducerの内部pieceだけであり、RSS上限の検証ではない。
///
#[tokio::test]
async fn receives_large_json_over_http_without_collecting_unknown_values() {
    /*
     * test fixtureだけは全量生成し、製品の逐次処理経路と区別する
     */
    let depth = 4096;
    let key = "k".repeat(1024 * 1024);
    let text = "s".repeat(1024 * 1024);
    let number = "9".repeat(1024 * 1024);
    let unknown = format!(r#""{key}":"{text}","number":1e{number},
        "nested":{}0{},"other":{{"id":"invalid","path":null}}"#,
        "[".repeat(depth), "]".repeat(depth));
    let lookup = format!(r#"{{{unknown},"id":"{ID}"}}"#);
    let metadata = format!(r#"{{{unknown},"page_info":{{"path":{{
        "kind":"current","value":"/\u0061"}},"deleted":false,"locked":true}},
        "revision_info":{{"revision":8}}}}"#);
    let server = Server::new(vec![
        Reply::new(200, lookup.into_bytes()),
        Reply::new(200, metadata.into_bytes()),
        Reply::new(200, br#"{"id":null}"#.to_vec()), found(),
    ]);
    let (client, stop) = client(&server, true);
    let path = PagePath::new("/a").unwrap();
    let metrics = Arc::new(Metrics::default());
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    /*
     * 3MiB超の応答でも必要値だけを有限結果として公開する
     */
    assert!(matches!(client.lookup(&path).await.unwrap(),
        LookupResult::Found(id) if id.as_str() == ID));
    let metadata = client.read_meta(&PageId::new(ID).unwrap(), &path)
        .await.unwrap();
    METRICS.with(|slot| *slot.borrow_mut() = None);
    assert_eq!(metadata.id().as_str(), ID);
    assert_eq!(metadata.path_relation(), PathRelation::MatchesExpected);
    assert_eq!(metadata.revision(), 8);
    assert!(metrics.max_piece.load(Ordering::Relaxed) > 0);
    assert!(metrics.max_piece.load(Ordering::Relaxed) <= 64 * 1024);
    assert_eq!(metrics.capacity.load(Ordering::Relaxed), 1);
    /*
     * parserの通常拒否後にも残期限内の明示GETを許す
     */
    let error = client.lookup(&path).await.unwrap_err();
    assert_eq!(error.kind(), FailureKind::Protocol);
    assert_eq!(error.status(), Some(200));
    assert!(stop.checkpoint().is_ok());
    assert!(client.lookup(&path).await.is_ok());
    assert_eq!(server.count(), 4);
}

///
/// 製品HTTPからlexerまでの接続を1byte確定pieceで検証する。
///
/// # 概要
/// raw UTF-8、escape、surrogate pairと必要keyを全境界で分割する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// test専用producer幅だけを1byteにし、製品の64KiB設定は変更しない。
/// 正常metadataと単独surrogate拒否後の明示GETを同じcontextで確認する。
///
#[tokio::test]
async fn parses_unicode_and_rejects_bad_escape_across_one_byte_pieces() {
    /*
     * 必要fieldと未知値へ多byte列と各escapeを用意する
     */
    let metadata = r#"{"page_info":{"path":{"kind":"current",
        "value":"/あ\ud83d\ude00"},"deleted":false,"locked":false},
        "revision_info":{"revision":1},"unknown":"\u0061\n\\\""}"#;
    let malformed = format!(r#"{{"id":"{ID}","unknown":"\ud800"}}"#);
    let server = Server::new(vec![
        Reply::new(200, metadata.as_bytes().to_vec()),
        Reply::new(200, malformed.into_bytes()), found(),
    ]);
    let (client, stop) = client(&server, true);
    let metrics = Arc::new(Metrics::default());
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    PIECE_LIMIT.with(|slot| *slot.borrow_mut() = 1);
    /*
     * 正常値と不正escapeの両方を同じ確定分割経路へ通す
     */
    let path = PagePath::new("/あ😀").unwrap();
    let metadata = client.read_meta(&PageId::new(ID).unwrap(), &path)
        .await.unwrap();
    assert_eq!(metadata.path_relation(), PathRelation::MatchesExpected);
    let error = client.lookup(&path).await.unwrap_err();
    assert_eq!(error.kind(), FailureKind::Protocol);
    assert_eq!(error.status(), Some(200));
    assert!(stop.checkpoint().is_ok());
    assert!(client.lookup(&path).await.is_ok());
    /*
     * 最大piece=1と有界容量を確認してthread内fixtureを戻す
     */
    PIECE_LIMIT.with(|slot| *slot.borrow_mut() = super::receive::WORK_CHUNK);
    METRICS.with(|slot| *slot.borrow_mut() = None);
    assert_eq!(metrics.max_piece.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.capacity.load(Ordering::Relaxed), 1);
    assert_eq!(server.count(), 3);
}

///
/// 本文の多bytecarryと元bytes保持を1byte確定pieceで検証する。
///
/// # 概要
/// BOM・raw多byte・LF/CRLF・末尾を保ち、置換文字と未完列を拒否する。
///
/// # 戻り値
/// なし。本文bytes・失敗事実・後続GET・内部piece幅を検査する。
///
/// # 注記
/// test専用producer幅だけを1byteにし、製品の64KiB設定は変更しない。
/// 各本文拒否後も同じcontextで明示GETできることを確認する。
///
#[tokio::test]
async fn preserves_and_validates_source_across_one_byte_pieces() {
    /*
     * 変換しない正常bytesと、多byte途中・置換文字の拒否fixtureを作る
     */
    let expected = "\u{feff}あ😀\nline\r\nlast".as_bytes();
    let server = Server::new(vec![
        Reply::new(200, expected.to_vec()),
        Reply::new(200, b"\xef\xbf\xbd".to_vec()), found(),
        Reply::new(200, b"\xe3\x81".to_vec()), found(),
    ]);
    let (client, stop) = client(&server, true);
    let id = PageId::new(ID).unwrap();
    let path = PagePath::new("/a").unwrap();
    let metrics = Arc::new(Metrics::default());
    METRICS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&metrics)));
    PIECE_LIMIT.with(|slot| *slot.borrow_mut() = 1);
    /*
     * carryを必ず使う分割経路で保管成功と元bytes・実量を確認する
     */
    let body = client.read_source(&id).await.unwrap();
    assert_eq!(contents(&body), expected);
    assert_eq!(body.byte_len(), expected.len() as u64);
    drop(body);
    /*
     * 拒否後も確定statusと既知IDを保ち、共有取消なしで照会を続ける
     */
    for kind in [FailureKind::BodyReplacement, FailureKind::BodyUtf8] {
        let error = client.read_source(&id).await.unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.status(), Some(200));
        assert_eq!(error.sent(), SendState::Started);
        assert_eq!(error.known_id().unwrap().as_str(), ID);
        assert!(stop.checkpoint().is_ok());
        assert!(client.lookup(&path).await.is_ok());
    }
    /*
     * 確定piece幅を照合し、thread内fixtureを製品既定幅へ戻す
     */
    PIECE_LIMIT.with(|slot| *slot.borrow_mut() = super::receive::WORK_CHUNK);
    METRICS.with(|slot| *slot.borrow_mut() = None);
    assert_eq!(metrics.max_piece.load(Ordering::Relaxed), 1);
    assert_eq!(metrics.capacity.load(Ordering::Relaxed), 1);
    assert_eq!(server.count(), 5);
}
