/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 単発変更のHTTP契約・資源失効・確定情報保持・非露出試験
//!

use std::cell::{Cell, RefCell};
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use crate::cmd_args::resolve::RunLimits;
use crate::projection::{self, ProjectionPlan};
use crate::report::types::{PageId, PagePath};
use crate::source::{EntryKind, SourceSnapshot, StopContext};
use crate::validation::validate_snapshot;

use crate::wiki_client::lock::{Origin, ReleaseEffect};
use crate::wiki_client::types::{FailureKind, SendState};

use super::{ID, Reply, Server, client};

const TOKEN: &str = "00000000000000000000000001";
const NEW_TOKEN: &str = "00000000000000000000000002";

thread_local! {
    static JSON_STOP: Cell<bool> = const { Cell::new(false) };
    static PUBLISH: RefCell<Option<Publish>> = const { RefCell::new(None) };
}

///
/// 公開直前の共有取消または時計進行。
///
enum Publish { Cancel, Expire }

///
/// JSON workerへ試験停止を一度だけ移動する。
///
/// # 戻り値
/// 完全JSON後の取消注入指定
///
pub(in crate::wiki_client) fn take_json_stop() -> bool {
    JSON_STOP.with(|stop| stop.replace(false))
}

///
/// JSON worker内で停止指定を設定する。
///
/// # 引数
/// * `stop` - 呼出しthreadから移動した指定
///
/// # 戻り値
/// なし
///
pub(in crate::wiki_client) fn set_json_stop(stop: bool) {
    JSON_STOP.with(|slot| slot.set(stop));
}

///
/// 全JSON構造と正常EOFの検証後にだけ取消を注入する。
///
/// # 引数
/// * `stop` - 同じ共有取消
///
/// # 戻り値
/// なし。部分IDを確定とする注入ではない。
///
pub(in crate::wiki_client) fn after_json_complete(stop: &StopContext) {
    if take_json_stop() { stop.cancel(); }
}

///
/// transport公開前の停止または期限切れを決定的に注入する。
///
/// # 引数
/// * `stop` - 同じ共有取消
///
/// # 戻り値
/// なし
///
pub(in crate::wiki_client) fn before_publish(stop: &StopContext) {
    match PUBLISH.with(|slot| slot.borrow_mut().take()) {
        Some(Publish::Cancel) => stop.cancel(),
        Some(Publish::Expire) => crate::wiki_client::lock::tests::set_now(
            Some("2999-01-01T00:00:00Z")),
        None => {}
    }
}

///
/// 実検証経路でreadonly送信fixtureの計画を作る。
///
/// # 概要
/// 取得済みfixtureと検証上限を準備する。
/// 同じ停止境界で全件検証と計画確定を行う。
///
/// # 引数
/// * `bytes` - ダミーUTF-8本文
///
/// # 戻り値
/// 全件検証済みの計画所有者
///
pub(in crate::wiki_client) async fn planned(bytes: &[u8])
    -> Arc<ProjectionPlan>
{
    /*
     * 取得済みfixtureと検証上限を準備する
     */
    let snapshot = SourceSnapshot::validation_root_fixture(&[
        (b"page.md", EntryKind::File, bytes),
    ]).unwrap();
    let limits = RunLimits {
        max_pages: 10, max_page_bytes: 2_000_000, max_zip_entries: 100,
        max_zip_compressed_bytes: 2_000_000,
        max_zip_uncompressed_bytes: 2_000_000, max_git_bytes: 3_000_000,
        max_processing_seconds: 60, max_http_concurrency: 1,
    };
    /*
     * 同じ停止境界で全件検証と計画確定を行う
     */
    let stop = StopContext::new(Instant::now() + Duration::from_secs(60));
    let input = validate_snapshot(snapshot, "/".into(), &limits,
        stop.clone()).await.unwrap();
    Arc::new(projection::finalize(input, stop).await.unwrap())
}

///
/// 正常lock header付きfixtureを作る。
///
/// # 引数
/// * `status` - 正常status
/// * `token` - ダミーtoken
/// * `body` - ダミー応答
///
/// # 戻り値
/// 正常header fixture
///
fn locked(status: u16, token: &str, body: &[u8]) -> Reply {
    Reply::new(status, body.to_vec()).header("X-Page-Lock",
        &format!("expire=2999-01-01T00:00:00Z token={token}"))
}

///
/// 固定fixture IDを作る。
///
/// # 戻り値
/// 検証済みID
///
fn id() -> PageId { PageId::new(ID).unwrap() }

///
/// 本文なしPOSTと作成ID/lockを認証方式ごとに確認する。
///
/// # 概要
/// 認証方式ごとに正常ドラフト応答を準備する。
/// 本文なしPOSTと値としてescapeしたpathを照合する。
///
/// # 注記
/// queryを値としてdecodeし、本文/tokenをassertの診断へ出さない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn creates_draft_without_body_and_preserves_checked_identity() {
    for bearer in [false, true] {
        /*
         * 認証方式ごとに正常ドラフト応答を準備する
         */
        let response = format!("{{\"id\":\"{ID}\",\"unknown\":[1]}}");
        let server = Server::new(vec![locked(201, TOKEN, response.as_bytes())]);
        let (wiki, _) = client(&server, bearer);
        let path = PagePath::new("/日本語/子").unwrap();
        let draft = wiki.create_draft(&path).await.unwrap();
        assert!(draft.id().as_str() == ID);
        let (_, lease) = draft.into_parts();
        assert_eq!(lease.origin(), Origin::CreatedDraft);
        /*
         * 本文なしPOSTと値としてescapeしたpathを照合する
         */
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert!(requests[0].body.is_empty());
        assert!(requests[0].lock.is_empty());
        let target = server.url().join(&requests[0].target).unwrap();
        assert!(target.query_pairs().any(|(key, value)|
            key == "path" && value == path.as_str()));
    }
}

///
/// header不正でも全JSON確定IDを保持し、部分/不正JSONは補わない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 正常IDと不正末尾/重複ID/欠落IDを区別して比較する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn keeps_created_id_on_bad_header_but_rejects_incomplete_json() {
    let valid = format!("{{\"id\":\"{ID}\"}}");
    for complete in [true, false] {
        let body = if complete { valid.clone() } else { valid.clone() + "x" };
        let server = Server::new(vec![Reply::new(201, body.into_bytes())]);
        let (wiki, stop) = client(&server, true);
        let failure = wiki.create_draft(&PagePath::new("/page").unwrap())
            .await.unwrap_err();
        assert_eq!(failure.status(), Some(201));
        assert_eq!(failure.sent(), SendState::Started);
        assert_eq!(failure.known_id().is_some(), complete);
        assert!(stop.checkpoint().is_ok());
        assert_eq!(server.count(), 1);
    }
    /*
     * 必須項目不正を有効tokenで救済しない
     */
    for body in ["{}".to_owned(), "{\"id\":\"invalid\"}".to_owned(),
        format!("{{\"id\":\"{ID}\",\"id\":\"{ID}\"}}")]
    {
        let server = Server::new(vec![locked(201, TOKEN, body.as_bytes())]);
        let (wiki, _) = client(&server, false);
        let failure = wiki.create_draft(&PagePath::new("/page").unwrap())
            .await.unwrap_err();
        assert!(failure.known_id().is_none());
        assert_eq!(failure.status(), Some(201));
    }
}

///
/// 完全JSON/正常EOF後の取消でも確定IDと201を失わない。
///
/// # 注記
/// workerへ停止注入を移動し、後続HTTPを発行しないことを確認。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn retains_post_identity_when_stopped_after_complete_json() {
    let body = format!("{{\"id\":\"{ID}\"}}");
    let server = Server::new(vec![locked(201, TOKEN, body.as_bytes())]);
    let (wiki, stop) = client(&server, true);
    JSON_STOP.with(|slot| slot.set(true));
    let failure = wiki.create_draft(&PagePath::new("/page").unwrap())
        .await.unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Cancelled);
    assert_eq!(failure.status(), Some(201));
    assert!(failure.known_id().is_some_and(|id| id.as_str() == ID));
    assert!(stop.checkpoint().is_err());
    assert!(wiki.acquire_lock(&id()).await.is_err());
    assert_eq!(server.count(), 1);
}

///
/// 延長で新tokenだけに切り替わり、取得由来の解除作用を推測しない。
///
/// # 概要
/// 取得と延長で異なるtokenを返す三操作を準備する。
/// 旧tokenと新tokenの送信先と追加HTTPなしを照合する。
///
/// # 注記
/// 三単発操作の呼出しだけを確認し、Executor系列とは扱わない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn renews_with_new_token_and_releases_unknown_page_kind() {
    /*
     * 取得と延長で異なるtokenを返す三操作を準備する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        locked(204, NEW_TOKEN, b""), Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    wiki.renew_lock(&mut lease).await.unwrap();
    assert_eq!(lease.origin(), Origin::Acquired);
    let effect = wiki.release_lock(&id(), &mut lease).await.unwrap();
    assert_eq!(effect, ReleaseEffect::ReleasedWithPossibleDraftDeletion);
    assert!(!lease.usable());
    assert!(wiki.delete_page(&id(), &mut lease).await.is_err());
    /*
     * 旧tokenと新tokenの送信先と追加HTTPなしを照合する
     */
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].lock.is_empty());
    assert!(requests[1].lock == format!("token={TOKEN}"));
    assert!(requests[2].lock == format!("token={NEW_TOKEN}"));
    assert_eq!(requests[2].method, "DELETE");
}

///
/// 失敗した延長の旧tokenを復帰させずHTTPの確定事実を保持する。
///
/// # 概要
/// 拒否・不正header・途中切断の延長応答を準備する。
/// 確定事実と旧lease使用不可・再送なしを照合する。
///
/// # 注記
/// 403/404/409/500/不正正常header/同token/通信断を独立に試す。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn invalidates_started_renewal_on_every_failed_response() {
    /*
     * 拒否・不正header・途中切断の延長応答を準備する
     */
    let replies = vec![Reply::new(403, b"{}".to_vec()),
        Reply::new(404, b"{}".to_vec()), Reply::new(409, b"{}".to_vec()),
        Reply::new(500, b"{}".to_vec()), Reply::new(204, Vec::new()),
        locked(204, TOKEN, b""), Reply::new(500, b"{}".to_vec()).truncated()];
    for reply in replies {
        /*
         * 各単発延長を開始して失敗事実を受け取る
         */
        let server = Server::new(vec![locked(204, TOKEN, b""), reply]);
        let (wiki, stop) = client(&server, true);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let failure = wiki.renew_lock(&mut lease).await.unwrap_err();
        /*
         * 確定事実と旧lease使用不可・再送なしを照合する
         */
        assert_eq!(failure.sent(), SendState::Started);
        assert!(failure.status().is_some());
        assert!(failure.known_id().is_some());
        assert!(!lease.usable());
        assert!(stop.checkpoint().is_ok());
        assert!(wiki.release_lock(&id(), &mut lease).await.is_err());
        assert_eq!(server.count(), 2);
    }
}

///
/// client/IDの誤結合と停止では送信を開始しない。
///
/// # 概要
/// 同じ接続先の独立clientと取得leaseを準備する。
/// 対象ID不一致と共有clone・停止後HTTPなしを照合する。
///
/// # 注記
/// cloneは同じ内部client、別newは同じURLでも別clientとして区別する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rejects_wrong_client_id_and_boundary_before_send() {
    /*
     * 同じ接続先の独立clientと取得leaseを準備する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(204, Vec::new())]);
    let (wiki, stop) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let (other, _) = client(&server, true);
    let failure = other.delete_page(&id(), &mut lease).await.unwrap_err();
    assert_eq!(failure.kind(), FailureKind::LeaseMismatch);
    assert_eq!(failure.sent(), SendState::NotStarted);
    /*
     * 対象ID不一致と共有clone・停止後HTTPなしを照合する
     */
    let wrong = PageId::new("00000000000000000000000003").unwrap();
    assert!(wiki.delete_page(&wrong, &mut lease).await.is_err());
    assert!(lease.usable());
    wiki.clone().delete_page(&id(), &mut lease).await.unwrap();
    stop.cancel();
    assert!(wiki.acquire_lock(&id()).await.is_err());
    assert_eq!(server.count(), 2);
    let requests = server.requests();
    assert_eq!(requests[1].method, "DELETE");
    assert!(!requests[1].target.contains('?'));
}

///
/// readonly本文のbytesとPUT400保持条件/204失効を照合する。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 空/多byte/BOM/CRLF/LF/末尾を変換せず、status以外を診断へ出さない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn streams_exact_bytes_and_retains_only_complete_put_400() {
    for status in [204, 400, 403, 500] {
        /*
         * statusごとにreadonly本文と一byte送信を準備する
         */
        let response = if status == 204 { Vec::new() } else { b"{}".to_vec() };
        let server = Server::new(vec![locked(204, TOKEN, b""),
            Reply::new(status, response)]);
        let (wiki, stop) = client(&server, true);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let bytes = "\u{feff}日本語\r\nline\n末尾".as_bytes();
        let plan = planned(bytes).await;
        crate::wiki_client::upload::tests::set_action(
            crate::wiki_client::upload::tests::Action::Width(1));
        let result = wiki.put_source(&id(), &mut lease,
            plan.upload_body(0).unwrap()).await;
        /*
         * 成功・lease保持と送信本文の完全bytesを照合する
         */
        assert_eq!(result.is_ok(), status == 204);
        if let Err(failure) = result {
            assert_eq!(failure.status(), Some(status));
            assert!(failure.known_id().is_some());
        }
        assert_eq!(lease.usable(), status == 400);
        assert!(stop.checkpoint().is_ok());
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].body == bytes);
        assert_eq!(requests[1].length, bytes.len());
        assert_eq!(requests[1].content_type, "text/markdown");
        assert!(requests[1].lock == format!("token={TOKEN}"));
        assert!(!requests[1].target.contains('?'));
    }
    /*
     * 空本文でも変換せず正常PUTを完了する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, false);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let plan = planned(b"").await;
    wiki.put_source(&id(), &mut lease, plan.upload_body(0).unwrap())
        .await.unwrap();
    assert!(server.requests()[1].body.is_empty());
}

///
/// 不正/途切れた400を資源保持の証拠として採用しない。
///
/// # 注記
/// error JSON構造またはHTTP EOFが不完全なfixtureを区別する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rejects_malformed_or_truncated_put_400_without_restoring_lease() {
    for reply in [Reply::new(400, b"invalid".to_vec()),
        Reply::new(400, b"{}".to_vec()).truncated()]
    {
        let server = Server::new(vec![locked(204, TOKEN, b""), reply]);
        let (wiki, _) = client(&server, true);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let plan = planned(b"body").await;
        let failure = wiki.put_source(&id(), &mut lease,
            plan.upload_body(0).unwrap()).await.unwrap_err();
        assert_eq!(failure.status(), Some(400));
        assert!(!lease.usable());
    }
}

///
/// 作成由来の解除作用と公開前の期限/取消停止を正確に返す。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// lease公開前の時計進行と完全JSON後取消を分けて注入する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn distinguishes_draft_deletion_and_stops_expired_publication() {
    let body = format!("{{\"id\":\"{ID}\"}}");
    let server = Server::new(vec![locked(201, TOKEN, body.as_bytes()),
        Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, true);
    let (id, mut lease) = wiki.create_draft(&PagePath::new("/draft").unwrap())
        .await.unwrap().into_parts();
    assert_eq!(wiki.release_lock(&id, &mut lease).await.unwrap(),
        ReleaseEffect::DraftDeleted);
    assert!(!lease.usable());
    /*
     * header受信時有効でも公開前に期限切れなら成功leaseを返さない
     */
    let server = Server::new(vec![locked(201, TOKEN, body.as_bytes())]);
    let (wiki, _) = client(&server, true);
    PUBLISH.with(|slot| *slot.borrow_mut() = Some(Publish::Expire));
    let failure = wiki.create_draft(&PagePath::new("/draft").unwrap())
        .await.unwrap_err();
    crate::wiki_client::lock::tests::set_now(None);
    assert_eq!(failure.kind(), FailureKind::LockExpired);
    assert_eq!(failure.status(), Some(201));
    assert!(failure.known_id().is_some());
}

///
/// 204後の停止でもstatus/IDを保持し、leaseは再使用させない。
///
/// # 注記
/// DELETEの確定応答後に共有取消を注入し、追加HTTP0を確認する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn keeps_delete_204_facts_and_invalidates_lease_after_stop() {
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    PUBLISH.with(|slot| *slot.borrow_mut() = Some(Publish::Cancel));
    let failure = wiki.delete_page(&id(), &mut lease).await.unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Cancelled);
    assert_eq!(failure.status(), Some(204));
    assert!(failure.known_id().is_some());
    assert!(!lease.usable());
    assert!(wiki.release_lock(&id(), &mut lease).await.is_err());
    assert_eq!(server.count(), 2);
}

///
/// 全本文未引渡しの早期204を成功扱いせず確定statusを保持する。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 実I/O前の同期点でworkerを止め、body完了を時間推測で判定しない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rejects_early_204_until_full_stream_delivery_is_observed() {
    use crate::wiki_client::upload::tests::{Action, set_action};
    use crate::wiki_client::upload::tests::set_join_notice;

    /*
     * 早期204とworker同期点・計画所有を準備する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(204, Vec::new()).early()]);
    let (wiki, _) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let plan = planned(b"body").await;
    let body = plan.upload_body(0).unwrap();
    drop(plan);
    let (ready, prepared) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (joining, observed) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        set_action(Action::Pause { ready, release: gate });
        set_join_notice(joining);
        let result = wiki.put_source(&id(), &mut lease, body).await;
        (result, lease)
    });
    /*
     * worker準備とjoin到達を確認して停止点を解除する
     */
    let path = tokio::time::timeout(Duration::from_secs(2), prepared)
        .await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), observed)
        .await.unwrap().unwrap();
    assert!(path.exists());
    release.send(()).unwrap();
    /*
     * 未引渡しを失敗としlease失効・領域回収を照合する
     */
    let (result, lease) = task.await.unwrap();
    let failure = result.unwrap_err();
    assert_eq!(failure.kind(), FailureKind::Protocol);
    assert_eq!(failure.status(), Some(204));
    assert!(failure.known_id().is_some());
    assert!(!lease.usable());
    assert!(!path.parent().unwrap().exists());
    assert_eq!(server.count(), 2);
}

///
/// PUT future実破棄は共通取消を通知しworker所有領域を先に消さない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// worker内同期点で停止し、領域/permitの保持と終了後回収を分けて確認。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn dropped_put_keeps_plan_and_permit_until_worker_exits() {
    use crate::wiki_client::upload::tests::{Action, set_action};

    /*
     * worker停止点と計画・permitの観察者を準備する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(204, Vec::new()).early()]);
    let (wiki, stop) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let observer = wiki.clone();
    let plan = planned(b"body").await;
    let body = plan.upload_body(0).unwrap();
    drop(plan);
    let (ready, prepared) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let task = tokio::spawn(async move {
        set_action(Action::Pause { ready, release: gate });
        wiki.put_source(&id(), &mut lease, body).await
    });
    /*
     * futureを実破棄して取消とworker所有を照合する
     */
    let path = tokio::time::timeout(Duration::from_secs(2), prepared)
        .await.unwrap().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(stop.checkpoint().is_err());
    assert!(path.exists());
    assert_eq!(observer.available_permits(), 0);
    let count = server.count();
    assert!(observer.acquire_lock(&id()).await.is_err());
    assert_eq!(server.count(), count);
    /*
     * workerを終了させて領域とpermitの回収を待つ
     */
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.parent().unwrap().exists()
            || observer.available_permits() != 1
        { tokio::task::yield_now().await; }
    }).await.unwrap();
}

///
/// upload worker失敗の分類とIDを失わず秘密を表示へ出さない。
///
/// # 概要
/// 取得済みleaseとダミー本文へI/O失敗を注入する。
/// 安全な表示へ認証・token・本文が出ないことを照合する。
///
/// # 注記
/// worker境界のIo注入を使い実ディスク障害の証拠とは扱わない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn preserves_upload_io_classification_and_hides_secrets() {
    use crate::wiki_client::upload::tests::{Action, set_action};

    /*
     * 取得済みleaseとダミー本文へI/O失敗を注入する
     */
    let server = Server::new(vec![locked(204, TOKEN, b"")]);
    let (wiki, stop) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let plan = planned(b"private-dummy-body").await;
    set_action(Action::Io);
    let failure = wiki.put_source(&id(), &mut lease,
        plan.upload_body(0).unwrap()).await.unwrap_err();
    /*
     * 元分類・既知ID・通常失敗の取消非発火を照合する
     */
    assert_eq!(failure.kind(), FailureKind::Io);
    assert!(failure.known_id().is_some());
    assert!(stop.checkpoint().is_ok());
    /*
     * 安全な表示へ認証・token・本文が出ないことを照合する
     */
    let display = format!("{failure:?} {failure} {lease:?} {wiki:?}");
    for secret in [TOKEN, "private-dummy-body", "expire=", "token=",
        "wiki-bearer-secret", "wiki-password-secret"]
    { assert!(!display.contains(secret)); }
}

///
/// 受信時期限切れ/重複headerでは取得204を成功leaseにしない。
///
/// # 概要
/// 重複headerと期限切れheaderを別fixtureで準備する。
/// 各取得失敗のstatus・IDと通常取消非発火を照合する。
///
/// # 注記
/// statusと既知IDは保持し、通常拒否で共通取消を発火しない。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rejects_expired_and_duplicate_acquire_headers() {
    /*
     * 重複headerと期限切れheaderを別fixtureで準備する
     */
    let duplicate = locked(204, TOKEN, b"").header("X-Page-Lock",
        &format!("expire=2999-01-01T00:00:00Z token={NEW_TOKEN}"));
    let expired = Reply::new(204, Vec::new()).header("X-Page-Lock",
        &format!("expire=2000-01-01T00:00:00Z token={TOKEN}"));
    for (reply, kind) in [(duplicate, FailureKind::LockHeader),
        (expired, FailureKind::LockExpired)]
    {
        /*
         * 各取得失敗のstatus・IDと通常取消非発火を照合する
         */
        let server = Server::new(vec![reply]);
        let (wiki, stop) = client(&server, true);
        let failure = wiki.acquire_lock(&id()).await.unwrap_err();
        assert_eq!(failure.kind(), kind);
        assert_eq!(failure.status(), Some(204));
        assert!(failure.known_id().is_some());
        assert!(stop.checkpoint().is_ok());
        assert_eq!(server.count(), 1);
    }
}

///
/// permit待機中の時計進行と共有取消を送信直前に拒否する。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 一度pollして待機を確定させてから境界を変え、送信数を照合する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rechecks_expiry_and_shared_stop_after_waiting_for_permit() {
    for cancelled in [false, true] {
        let server = Server::new(vec![locked(204, TOKEN, b"")]);
        let (wiki, stop) = client(&server, true);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let held = wiki.transport.hold_permit().await;
        let mut renewal = Box::pin(wiki.renew_lock(&mut lease));
        poll_fn(|context| {
            assert!(matches!(renewal.as_mut().poll(context), Poll::Pending));
            Poll::Ready(())
        }).await;
        /*
         * 未送信の資源を不確定資源へ変更しない
         */
        if cancelled { stop.cancel(); }
        else { crate::wiki_client::lock::tests::set_now(
            Some("2999-01-01T00:00:00Z")); }
        drop(held);
        let failure = renewal.await.unwrap_err();
        crate::wiki_client::lock::tests::set_now(None);
        assert_eq!(failure.sent(), SendState::NotStarted);
        assert_eq!(failure.status(), None);
        assert!(lease.usable());
        assert_eq!(server.count(), 1);
    }
}

///
/// 開始済み延長のfuture破棄でも旧tokenは復帰しない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// error body受信入口でfutureを破棄し共有取消/HTTP0/permit回収を確認。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn dropped_started_renewal_keeps_old_token_unavailable() {
    let (held, release) = Reply::new(500, b"{}".to_vec()).held();
    let server = Server::new(vec![locked(204, TOKEN, b""), held]);
    let (wiki, stop) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let (sender, noticed) = tokio::sync::oneshot::channel();
    super::RECEIVE_STARTED.with(|slot| *slot.borrow_mut() = Some(sender));
    let mut future = Box::pin(wiki.renew_lock(&mut lease));
    tokio::select! {
        _ = &mut future => panic!("fixture body must remain held"),
        result = noticed => { assert!(result.is_ok()); }
    }
    /*
     * 排他的借用の解放前に状態は既に使用不可へ変わっている
     */
    drop(future);
    assert!(!lease.usable());
    assert!(stop.checkpoint().is_err());
    assert!(wiki.release_lock(&id(), &mut lease).await.is_err());
    assert_eq!(server.count(), 2);
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(2), async {
        while wiki.available_permits() != 1 { tokio::task::yield_now().await; }
    }).await.unwrap();
}

///
/// 409/423/3xxを単発失敗とし再送/redirect/自動解放を行わない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// lock取得409とDELETE拒否を別fixtureで試し確定status/IDを保持する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn keeps_conflict_locked_and_redirect_responses_without_followups() {
    let server = Server::new(vec![Reply::new(409, b"{}".to_vec())]);
    let (wiki, _) = client(&server, true);
    let failure = wiki.acquire_lock(&id()).await.unwrap_err();
    assert_eq!(failure.status(), Some(409));
    assert!(failure.known_id().is_some());
    assert_eq!(server.count(), 1);
    /*
     * 拒否後に旧資源を保持したと推測せず、追従要求もしない
     */
    for reply in [Reply::new(423, b"{}".to_vec()), Reply::redirect("/other")]
    {
        let server = Server::new(vec![locked(204, TOKEN, b""), reply]);
        let (wiki, stop) = client(&server, false);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let failure = wiki.delete_page(&id(), &mut lease).await.unwrap_err();
        assert!(matches!(failure.status(), Some(423 | 302)));
        assert!(failure.known_id().is_some());
        assert!(!lease.usable());
        assert!(stop.checkpoint().is_ok());
        assert_eq!(server.count(), 2);
    }
}

///
/// 204/完全400後のjoin中停止でleaseを使用可能へ戻さない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 全byteをStreamへ渡したworkerを終了直前に止め、取消後に終了させる。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rechecks_shared_stop_after_put_worker_join() {
    for status in [204, 400] {
        use crate::wiki_client::upload::tests::{Action, set_action};
        use crate::wiki_client::upload::tests::set_join_notice;

        /*
         * 全本文受取後の応答とworker終了直前の同期点を準備する
         */
        let server = Server::new(vec![locked(204, TOKEN, b""),
            Reply::new(status, if status == 400 { b"{}".to_vec() }
                else { Vec::new() })]);
        let (wiki, stop) = client(&server, true);
        let mut lease = wiki.acquire_lock(&id()).await.unwrap();
        let observer = wiki.clone();
        let plan = planned(b"body").await;
        let body = plan.upload_body(0).unwrap();
        drop(plan);
        let (ready, paused) = tokio::sync::oneshot::channel();
        let (joining, joined) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            set_action(Action::PauseExit { ready, release: gate });
            set_join_notice(joining);
            let result = wiki.put_source(&id(), &mut lease, body).await;
            (result, lease)
        });
        let path = tokio::time::timeout(Duration::from_secs(2), paused)
            .await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), joined)
            .await.unwrap().unwrap();
        /*
         * 204/完全400検証後のjoinも同じ共有境界で終える
         */
        stop.cancel();
        assert!(path.exists());
        assert_eq!(observer.available_permits(), 0);
        release.send(()).unwrap();
        let (result, lease) = task.await.unwrap();
        let failure = result.unwrap_err();
        assert_eq!(failure.kind(), FailureKind::Cancelled);
        assert_eq!(failure.status(), Some(status));
        assert!(!lease.usable());
        assert!(!path.parent().unwrap().exists());
        assert!(observer.acquire_lock(&id()).await.is_err());
        assert_eq!(server.count(), 2);
    }
}

///
/// 完全400でもupload worker失敗なら保持へ戻さない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 全本文受取後の400と終了I/O失敗を併せ、statusとIDを保持する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[tokio::test]
async fn rejects_put_400_retention_after_upload_worker_failure() {
    use crate::wiki_client::upload::tests::{Action, set_action};

    /*
     * 400受信と独立したworker終了失敗を注入する
     */
    let server = Server::new(vec![locked(204, TOKEN, b""),
        Reply::new(400, b"{}".to_vec())]);
    let (wiki, stop) = client(&server, true);
    let mut lease = wiki.acquire_lock(&id()).await.unwrap();
    let plan = planned(b"body").await;
    set_action(Action::ExitIo);
    let failure = wiki.put_source(&id(), &mut lease,
        plan.upload_body(0).unwrap()).await.unwrap_err();
    /*
     * HTTP事実を保持し、資源保持と共有取消を発生させない
     */
    assert_eq!(failure.kind(), FailureKind::Io);
    assert_eq!(failure.status(), Some(400));
    assert_eq!(failure.sent(), SendState::Started);
    assert!(failure.known_id().is_some());
    assert!(!lease.usable());
    assert!(stop.checkpoint().is_ok());
    assert_eq!(server.count(), 2);
}
