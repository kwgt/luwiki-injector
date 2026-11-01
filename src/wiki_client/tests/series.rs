/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 明示単発系列と実行側adapterへの安全な事実引渡し試験
//!

use std::fs;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::cmd_args::resolve::{Warning, wiki_config_fixture};
use crate::report::types::{ObservedState, Operation, OperationWarning, PageId,
    PagePath, ProcessingStage, ResponseCondition};
use crate::source::StopContext;
use crate::wiki_client::adapter::{ExecutionFailure, execution_failure,
    operation_evidence};
use crate::wiki_client::lock::{Origin, ReleaseEffect};
use crate::wiki_client::types::{FailureBoundary, FailureKind, LookupResult,
    PathKind, SendState, WikiFailure, WikiOperation};
use crate::wiki_client::{LockLease, WikiClient};

use super::writes::planned;
use super::{ID, Reply, Server, client, contents};

const NEW_ID: &str = "00000000000000000000000003";
const TOKEN: &str = "00000000000000000000000001";
const NEW_TOKEN: &str = "00000000000000000000000002";

///
/// 最終payload全体をmove受領し起動警告を別に所有する試験受け手。
///
#[derive(Debug)]
struct Receiver { startup: Vec<Warning>, failure: ExecutionFailure }

///
/// 正常ロック応答を作る。
///
/// # 引数
/// * `status` - 正常status
/// * `token` - 試験専用token
/// * `body` - 試験専用JSON
///
/// # 戻り値
/// RFC3339期限と有効headerを持つfixture
///
fn locked(status: u16, token: &str, body: Vec<u8>) -> Reply {
    Reply::new(status, body).header("X-Page-Lock",
        &format!("expire=2999-01-01T00:00:00Z token={token}"))
}

///
/// 旧IDと異なる新IDのドラフト応答を作る。
///
/// # 戻り値
/// 新tokenを持つ201応答
///
fn created() -> Reply {
    locked(201, NEW_TOKEN, format!(r#"{{"id":"{NEW_ID}"}}"#).into_bytes())
}

///
/// currentまたはlast_deleted metadataを作る。
///
/// # 引数
/// * `deleted` - 旧IDについて観測させる削除状態
///
/// # 戻り値
/// 必須項目を持つmetadata応答
///
fn metadata(deleted: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!(r#"{{"page_info":{{"path":{{"kind":"{kind}",
        "value":"/a"}},"deleted":{deleted},"locked":false}},
        "revision_info":{{"revision":4}}}}"#).into_bytes())
}

///
/// 記録済み要求の件数・method・targetを全件照合する。
///
/// # 引数
/// * `server` - loopback記録
/// * `expected` - callerが明示した順序
///
/// # 戻り値
/// なし。隠れた再送・照会・解除があれば失敗する。
///
fn requests(server: &Server, expected: &[(&str, String)]) {
    let actual = server.requests().into_iter().map(|request|
        (request.method, request.target)).collect::<Vec<_>>();
    let expected = expected.iter().map(|(method, target)|
        ((*method).to_owned(), target.clone())).collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

///
/// 送信前停止を失敗の境界・分類とともに照合する。
///
/// # 引数
/// * `failure` - 後続単発操作の失敗
/// * `kind` - 同じcontextの停止原因
///
/// # 戻り値
/// なし。観測statusを生成しないことを確認する。
///
fn stopped(failure: WikiFailure, kind: FailureKind) {
    assert_eq!(failure.boundary(), FailureBoundary::BeforeSend);
    assert_eq!(failure.sent(), SendState::NotStarted);
    assert_eq!(failure.kind(), kind);
    assert_eq!(failure.status(), None);
}

///
/// 新規POSTから同じ新IDへreadonly本文PUTを明示する。
///
/// # 概要
/// 実計画のbytes・新token・資源失効と追加要求なしを確認する。
///
/// # 戻り値
/// なし。全要求と実byte列を照合する。
///
/// # 注記
/// BOM・多byte・LF/CRLF・末尾を変換せず、amendを省略する。
///
#[tokio::test]
async fn explicit_create_then_put_preserves_planned_bytes() {
    let bytes = "\u{feff}あ\nprivate-body\r\nlast".as_bytes();
    let plan = planned(bytes).await;
    let server = Server::new(vec![created(), Reply::new(204, Vec::new())]);
    let (wiki, stop) = client(&server, true);
    let path = PagePath::new("/a").unwrap();
    let (id, mut lease) = wiki.create_draft(&path).await.unwrap().into_parts();
    assert_eq!(id.as_str(), NEW_ID);
    assert_eq!(lease.origin(), Origin::CreatedDraft);
    /*
     * callerが新IDの本文PUTを明示し、成功後の旧資源を再使用しない
     */
    let owner = std::sync::Arc::downgrade(&plan);
    let body = plan.upload_body(0).unwrap();
    drop(plan);
    assert!(owner.upgrade().is_some());
    wiki.put_source(&id, &mut lease, body).await.unwrap();
    assert!(owner.upgrade().is_none());
    assert!(!lease.usable());
    assert!(stop.checkpoint().is_ok());
    let observed = server.requests();
    assert!(observed[1].body == bytes);
    assert!(observed[1].lock == format!("token={NEW_TOKEN}"));
    assert_eq!(observed[1].length, bytes.len());
    assert_eq!(observed[1].content_type, "text/markdown");
    requests(&server, &[("POST", "/api/pages?path=%2Fa".into()),
        ("PUT", format!("/api/pages/{NEW_ID}/source"))]);
}

///
/// ロック前後の独立本文を再取得し、callerが明示解除する。
///
/// # 概要
/// 異なる本文を同時所有して観測し、clientがPUT判断しないことを確認する。
///
/// # 戻り値
/// なし。要求列・由来・解除作用・保管回収を検査する。
///
/// # 注記
/// procのfileリンクは試験内だけで参照し、診断へ出さない。
///
#[tokio::test]
async fn explicit_lock_recheck_keeps_independent_bodies_and_cleanup() {
    let server = Server::new(vec![Reply::new(200, b"before".to_vec()),
        locked(204, TOKEN, Vec::new()),
        Reply::new(200, b"after".to_vec()), metadata(false),
        Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, false);
    let id = PageId::new(ID).unwrap();
    let path = PagePath::new("/a").unwrap();
    let before = wiki.read_source(&id).await.unwrap();
    let mut lease = wiki.acquire_lock(&id).await.unwrap();
    let after = wiki.read_source(&id).await.unwrap();
    assert_eq!(lease.origin(), Origin::Acquired);
    assert!(contents(&before) == b"before");
    assert!(contents(&after) == b"after");
    /*
     * 二つの独立readonly領域の所有とdrop後回収を観測する
     */
    let mut areas = Vec::new();
    for body in [&before, &after] {
        let file = body.open().unwrap();
        assert!(file.metadata().unwrap().is_file());
        let stored = fs::read_link(format!("/proc/self/fd/{}",
            file.as_raw_fd())).unwrap();
        assert!(stored.exists());
        areas.push(stored.parent().unwrap().to_path_buf());
        assert!(!format!("{body:?}").contains(&stored.display().to_string()));
    }
    assert!(areas[0] != areas[1]);
    assert!(!wiki.read_meta(&id, &path).await.unwrap().deleted());
    assert_eq!(wiki.release_lock(&id, &mut lease).await.unwrap(),
        ReleaseEffect::ReleasedWithPossibleDraftDeletion);
    drop(before);
    assert!(!areas[0].exists());
    assert!(areas[1].exists());
    drop(after);
    assert!(!areas[1].exists());
    requests(&server, &[("GET", format!("/api/pages/{ID}/source")),
        ("POST", format!("/api/pages/{ID}/lock")),
        ("GET", format!("/api/pages/{ID}/source")),
        ("GET", format!("/api/pages/{ID}/meta")),
        ("DELETE", format!("/api/pages/{ID}/lock"))]);
}

///
/// DELETE後の新POSTとPUTが旧ID・旧leaseを流用しない。
///
/// # 概要
/// 旧lease再使用の送信前拒否と新tokenの単発PUTを確認する。
///
/// # 戻り値
/// なし。非再帰DELETEと全要求列を照合する。
///
/// # 注記
/// 再作成の継続可否はこの試験のcallerが明示する。
///
#[tokio::test]
async fn explicit_delete_recreate_uses_new_identity_and_token() {
    let plan = planned(b"new-body").await;
    let server = Server::new(vec![locked(204, TOKEN, Vec::new()),
        Reply::new(204, Vec::new()), created(), Reply::new(204, Vec::new())]);
    let (wiki, _) = client(&server, true);
    let old = PageId::new(ID).unwrap();
    let mut lease = wiki.acquire_lock(&old).await.unwrap();
    wiki.delete_page(&old, &mut lease).await.unwrap();
    assert!(!lease.usable());
    /*
     * 旧lease拒否はHTTPを増やさず、新POSTから別資源を取得する
     */
    let rejected = wiki.delete_page(&old, &mut lease).await.unwrap_err();
    assert_eq!(rejected.boundary(), FailureBoundary::BeforeSend);
    assert_eq!(server.count(), 2);
    let (new, mut fresh) = wiki.create_draft(&PagePath::new("/a").unwrap())
        .await.unwrap().into_parts();
    assert!(new.as_str() != old.as_str());
    wiki.put_source(&new, &mut fresh, plan.upload_body(0).unwrap())
        .await.unwrap();
    assert!(!fresh.usable());
    assert!(server.requests()[3].lock == format!("token={NEW_TOKEN}"));
    requests(&server, &[("POST", format!("/api/pages/{ID}/lock")),
        ("DELETE", format!("/api/pages/{ID}")),
        ("POST", "/api/pages?path=%2Fa".into()),
        ("PUT", format!("/api/pages/{NEW_ID}/source"))]);
}

///
/// DELETE500後の独立削除観測と最終POST失敗をpayloadに同時保持する。
///
/// # 概要
/// 本番adapterから試験受け手へmoveし、先行事実の消失を検査する。
///
/// # 戻り値
/// なし。旧ID・status・段階・起動警告分離を照合する。
///
/// # 注記
/// 実Reportの型・renderer・集計への統合試験ではない。
///
#[tokio::test]
async fn delete_warning_survives_recreation_failure_and_receiver_move() {
    let server = Server::new(vec![locked(204, TOKEN, Vec::new()),
        Reply::new(500, b"{}".to_vec()), Reply::new(410, Vec::new()),
        metadata(true), Reply::new(409, b"{}".to_vec())]);
    let (wiki, stop) = client(&server, true);
    let old = PageId::new(ID).unwrap();
    let path = PagePath::new("/a").unwrap();
    let mut lease = wiki.acquire_lock(&old).await.unwrap();
    let deletion = wiki.delete_page(&old, &mut lease).await.unwrap_err();
    assert!(stop.checkpoint().is_ok());
    assert!(!lease.usable());
    assert!(matches!(wiki.lookup(&path).await.unwrap(), LookupResult::Deleted));
    let meta = wiki.read_meta(&old, &path).await.unwrap();
    assert!(meta.deleted());
    assert_eq!(meta.path_kind(), PathKind::LastDeleted);
    /*
     * callerの削除観測を添えて、最終失敗と先行事実をmoveする
     */
    let evidence = operation_evidence(deletion, ObservedState::Deleted)
        .unwrap();
    let failure = wiki.create_draft(&path).await.unwrap_err();
    let receiver = Receiver { startup: vec![Warning::ConfigPermissions],
        failure: execution_failure(failure, ProcessingStage::CreateDraft,
            vec![evidence]) };
    assert_eq!(receiver.startup, vec![Warning::ConfigPermissions]);
    assert_eq!(receiver.failure.stage(), ProcessingStage::CreateDraft);
    assert_eq!(receiver.failure.facts().kind(), FailureKind::Http);
    assert_eq!(receiver.failure.facts().status(), Some(409));
    assert_eq!(receiver.failure.facts().operation(),
        WikiOperation::CreateDraft);
    assert_eq!(receiver.failure.warnings().len(), 1);
    let prior = &receiver.failure.warnings()[0];
    assert_eq!(prior.facts().known_id().unwrap().as_str(), ID);
    assert_eq!(prior.facts().status(), Some(500));
    assert_eq!(prior.warning(), Some(OperationWarning {
        operation: Operation::Delete, response: ResponseCondition::Http(500),
        observed: ObservedState::Deleted, detail: None }));
    assert!(stop.checkpoint().is_ok());
    requests(&server, &[("POST", format!("/api/pages/{ID}/lock")),
        ("DELETE", format!("/api/pages/{ID}")),
        ("GET", "/api/pages/id?path=%2Fa".into()),
        ("GET", format!("/api/pages/{ID}/meta")),
        ("POST", "/api/pages?path=%2Fa".into())]);
}

///
/// statusなしTransportとstatusあり失敗の発生順を受け手まで保持する。
///
/// # 概要
/// HTTPから得た二つの先行事実を最終POST失敗と一緒にmoveする。
///
/// # 戻り値
/// なし。元kind・send・ID・観測・順序と表示非露出を検査する。
///
/// # 注記
/// statusなしをDisconnectedへ推測変換せず、起動警告は別列に残す。
///
#[tokio::test]
async fn no_status_evidence_keeps_order_without_disconnected_inference() {
    let raw = br#"{"message":"private-error","kind":"unknown"}"#;
    let server = Server::new(vec![
        Reply::new(200, Vec::new()).without_headers(),
        Reply::new(500, raw.to_vec()),
        Reply::new(201, format!(r#"{{"id":"{NEW_ID}"}}"#).into_bytes())
            .header("X-Page-Lock", "private-header"),
    ]);
    let (wiki, stop) = client(&server, false);
    let id = PageId::new(ID).unwrap();
    let first = wiki.read_source(&id).await.unwrap_err();
    assert_eq!(first.boundary(), FailureBoundary::ResponseUncertain);
    assert!(stop.checkpoint().is_ok());
    let second = wiki.read_meta(&id, &PagePath::new("/a").unwrap())
        .await.unwrap_err();
    let evidence = vec![operation_evidence(first, ObservedState::Unknown)
        .unwrap(), operation_evidence(second, ObservedState::Unknown).unwrap()];
    /*
     * 完全JSONの新確定IDは最終header不正でも保持し、元列をmoveする
     */
    let final_failure = wiki.create_draft(&PagePath::new("/a").unwrap())
        .await.unwrap_err();
    let receiver = Receiver { startup: vec![Warning::CliSecret],
        failure: execution_failure(final_failure, ProcessingStage::CreateDraft,
            evidence) };
    assert_eq!(receiver.startup, vec![Warning::CliSecret]);
    assert_eq!(receiver.failure.warnings().len(), 2);
    let first = &receiver.failure.warnings()[0];
    assert_eq!(first.facts().kind(), FailureKind::Transport);
    assert_eq!(first.facts().sent(), SendState::Started);
    assert_eq!(first.facts().status(), None);
    assert_eq!(first.facts().known_id().unwrap().as_str(), ID);
    assert_eq!(first.facts().operation(), WikiOperation::ReadSource);
    assert_eq!(first.observed(), ObservedState::Unknown);
    assert_eq!(first.warning(), None);
    let second = &receiver.failure.warnings()[1];
    assert_eq!(second.facts().operation(), WikiOperation::ReadMeta);
    assert_eq!(second.facts().status(), Some(500));
    assert_eq!(second.facts().known_id().unwrap().as_str(), ID);
    assert_eq!(second.observed(), ObservedState::Unknown);
    assert_eq!(second.warning().unwrap().response,
        ResponseCondition::Http(500));
    assert_eq!(receiver.failure.facts().kind(), FailureKind::LockHeader);
    assert_eq!(receiver.failure.facts().status(), Some(201));
    assert_eq!(receiver.failure.facts().known_id().unwrap().as_str(), NEW_ID);
    let display = format!("{receiver:?} {} {first:?} {:?} {wiki:?}",
        receiver.failure, second.warning());
    for private in [ID, NEW_ID, TOKEN, NEW_TOKEN, "private-header",
        "private-error", "dummy-password", "dummy-wiki-secret",
        "Authorization", "private-body"]
    {
        assert!(!display.contains(private));
    }
    requests(&server, &[("GET", format!("/api/pages/{ID}/source")),
        ("GET", format!("/api/pages/{ID}/meta")),
        ("POST", "/api/pages?path=%2Fa".into())]);
}

///
/// DELETE500後のpresentまたはunknown観測だけではPOSTを追加しない。
///
/// # 概要
/// 明示照会の成功・失敗だけをcallerが観測し、失敗へ先行事実を添える。
///
/// # 戻り値
/// なし。HTTP列とcaller指定状態の無変換保持を確認する。
///
/// # 注記
/// Executorの再作成継続条件は実装せず、試験callerもPOSTを呼ばない。
///
#[tokio::test]
async fn present_or_unknown_lookup_does_not_start_implicit_recreation() {
    for present in [true, false] {
        let lookup = if present { super::found() }
            else { Reply::new(200, b"{}".to_vec()) };
        let server = Server::new(vec![locked(204, TOKEN, Vec::new()),
            Reply::new(500, b"{}".to_vec()), lookup,
            Reply::new(500, b"{}".to_vec())]);
        let (wiki, stop) = client(&server, true);
        let id = PageId::new(ID).unwrap();
        let path = PagePath::new("/a").unwrap();
        let mut lease = wiki.acquire_lock(&id).await.unwrap();
        let deletion = wiki.delete_page(&id, &mut lease).await.unwrap_err();
        /*
         * 別の明示GETの結果だけからcallerの観測値を決める
         */
        let observed = match wiki.lookup(&path).await {
            Ok(LookupResult::Found(found)) if found.as_str() == ID =>
                ObservedState::Present,
            Err(error) => {
                assert_eq!(error.kind(), FailureKind::Protocol);
                ObservedState::Unknown
            }
            _ => panic!("unexpected fixture observation"),
        };
        let evidence = operation_evidence(deletion, observed).unwrap();
        let failure = wiki.read_meta(&id, &path).await.unwrap_err();
        let payload = execution_failure(failure, ProcessingStage::Reconcile,
            vec![evidence]);
        assert_eq!(payload.warnings()[0].observed(), observed);
        assert!(stop.checkpoint().is_ok());
        requests(&server, &[("POST", format!("/api/pages/{ID}/lock")),
            ("DELETE", format!("/api/pages/{ID}")),
            ("GET", "/api/pages/id?path=%2Fa".into()),
            ("GET", format!("/api/pages/{ID}/meta"))]);
    }
}

///
/// 変更拒否をHTTP観測として分類し、適用有無を補完しない。
///
/// # 概要
/// 変更POSTの代表拒否statusからUnknownのまま安全adapterを通す。
///
/// # 戻り値
/// なし。status・kind・境界・観測無変換と再送なしを検査する。
///
/// # 注記
/// 4xxでもNotAppliedへ変換しない。302も自動追従しない。
///
#[tokio::test]
async fn mutation_http_status_is_observation_without_application_inference() {
    for status in [401, 403, 404, 410, 409, 423, 500, 302] {
        /*
         * 代表statusごとに単発POST拒否を準備する
         */
        let server = Server::new(vec![Reply::new(status, b"{}".to_vec())]);
        let (wiki, stop) = client(&server, true);
        let failure = wiki.create_draft(&PagePath::new("/a").unwrap())
            .await.unwrap_err();
        assert_eq!(failure.boundary(), FailureBoundary::ResponseReceived);
        assert_eq!(failure.kind(), FailureKind::Http);
        assert_eq!(failure.status(), Some(status));
        assert!(failure.known_id().is_none());
        /*
         * Unknownの観測をadapterへ渡し推測と再送なしを照合する
         */
        let evidence = operation_evidence(failure, ObservedState::Unknown)
            .unwrap();
        assert_eq!(evidence.warning().unwrap().observed,
            ObservedState::Unknown);
        assert!(stop.checkpoint().is_ok());
        requests(&server, &[("POST", "/api/pages?path=%2Fa".into())]);
    }
}

///
/// 正常status後の不正本文・JSON・途中切断も受信観測分類を維持する。
///
/// # 概要
/// 異なる失敗kindを最終実行payloadへ写し、statusと既知IDを保持する。
///
/// # 戻り値
/// なし。入力不正へ写さず安全な診断だけを維持する。
///
/// # 注記
/// 不完POSTの抽出候補IDを復旧IDにせず、全HTTP EOFを必須とする。
///
#[tokio::test]
async fn malformed_and_truncated_responses_keep_boundary_and_original_kind() {
    let server = Server::new(vec![Reply::new(200, vec![0xff]),
        Reply::new(200, b"\xef\xbf\xbd".to_vec()),
        Reply::new(200, b"{}".to_vec()),
        Reply::new(500, b"private-error".to_vec()),
        Reply::new(200, b"body".to_vec()).truncated(),
        locked(201, NEW_TOKEN,
            format!(r#"{{"id":"{NEW_ID}","x":["#).into_bytes()),
    ]);
    let (wiki, stop) = client(&server, true);
    let id = PageId::new(ID).unwrap();
    /*
     * status後の本文・metadata・error拒否を同一contextで観測する
     */
    let failures = [wiki.read_source(&id).await.unwrap_err(),
        wiki.read_source(&id).await.unwrap_err(),
        wiki.read_meta(&id, &PagePath::new("/a").unwrap()).await.unwrap_err(),
        wiki.read_source(&id).await.unwrap_err(),
        wiki.read_source(&id).await.unwrap_err()];
    for (failure, kind, status) in failures.into_iter().zip([
        (FailureKind::BodyUtf8, 200), (FailureKind::BodyReplacement, 200),
        (FailureKind::Protocol, 200), (FailureKind::Protocol, 500),
        (FailureKind::Transport, 200),
    ]).map(|(failure, (kind, status))| (failure, kind, status)) {
        let payload = execution_failure(failure, ProcessingStage::Recheck,
            Vec::new());
        assert_eq!(payload.stage(), ProcessingStage::Recheck);
        assert_eq!(payload.facts().boundary(),
            FailureBoundary::ResponseReceived);
        assert_eq!(payload.facts().kind(), kind);
        assert_eq!(payload.facts().status(), Some(status));
        assert_eq!(payload.facts().known_id().unwrap().as_str(), ID);
        assert!(!format!("{payload:?}").contains("private-error"));
    }
    /*
     * 不完JSONの候補IDは公開せず、statusだけを確定事実に残す
     */
    let failure = wiki.create_draft(&PagePath::new("/a").unwrap())
        .await.unwrap_err();
    assert_eq!(failure.boundary(), FailureBoundary::ResponseReceived);
    assert_eq!(failure.status(), Some(201));
    assert_eq!(failure.kind(), FailureKind::Protocol);
    assert!(failure.known_id().is_none());
    assert!(stop.checkpoint().is_ok());
    requests(&server, &[("GET", format!("/api/pages/{ID}/source")),
        ("GET", format!("/api/pages/{ID}/source")),
        ("GET", format!("/api/pages/{ID}/meta")),
        ("GET", format!("/api/pages/{ID}/source")),
        ("GET", format!("/api/pages/{ID}/source")),
        ("POST", "/api/pages?path=%2Fa".into())]);
}

///
/// callerが明示する単発系列の試験手順。
///
#[derive(Clone, Copy)]
enum Step {
    Create, Put, Source, Lock, Meta, Release, Delete, Delete500, Lookup,
}

///
/// 手順に一致するfixtureを作る。
///
/// # 引数
/// * `step` - callerの次の単発操作
///
/// # 戻り値
/// 試験用応答。clientの継続判断ではない。
///
fn response(step: Step) -> Reply {
    match step {
        Step::Create => created(),
        Step::Lock => locked(204, TOKEN, Vec::new()),
        Step::Source => Reply::new(200, b"body".to_vec()),
        Step::Meta => metadata(true),
        Step::Delete500 => Reply::new(500, b"{}".to_vec()),
        Step::Lookup => Reply::new(410, Vec::new()),
        _ => Reply::new(204, Vec::new()),
    }
}

///
/// callerの指定した系列prefixだけを実行する。
///
/// # 概要
/// 成功資源はcallerが所有し、DELETE500も通常返却として受け取る。
///
/// # 引数
/// * `wiki` - 共通contextのclient
/// * `steps` - 実行するprefix
/// * `plan` - readonly検証済み計画
///
/// # 戻り値
/// 現在の確定IDと最後にcallerが所有するlease
///
async fn prefix(wiki: &WikiClient, steps: &[Step],
    plan: &std::sync::Arc<crate::projection::ProjectionPlan>)
    -> (PageId, LockLease)
{
    let path = PagePath::new("/a").unwrap();
    let mut id = PageId::new(ID).unwrap();
    let mut lease = None;
    /*
     * 失敗による暗黙の継続を設けず、指定した単発操作だけを順に呼ぶ
     */
    for step in steps {
        match step {
            Step::Create => {
                let (created, fresh) = wiki.create_draft(&path)
                    .await.unwrap().into_parts();
                id = created;
                lease = Some(fresh);
            }
            Step::Lock => lease = Some(wiki.acquire_lock(&id).await.unwrap()),
            Step::Source => { drop(wiki.read_source(&id).await.unwrap()); }
            Step::Meta => { let _ = wiki.read_meta(&id, &path).await.unwrap(); }
            Step::Lookup => {
                assert!(matches!(wiki.lookup(&path).await.unwrap(),
                    LookupResult::Deleted));
            }
            Step::Delete => {
                wiki.delete_page(&id, lease.as_mut().unwrap()).await.unwrap();
            }
            Step::Delete500 => {
                let failure = wiki.delete_page(&id, lease.as_mut().unwrap())
                    .await.unwrap_err();
                assert_eq!(failure.status(), Some(500));
            }
            Step::Put => {
                wiki.put_source(&id, lease.as_mut().unwrap(),
                    plan.upload_body(0).unwrap()).await.unwrap();
            }
            Step::Release => {
                wiki.release_lock(&id, lease.as_mut().unwrap()).await.unwrap();
            }
        }
    }
    // 初回sourceだけのprefixは呼出側で別に検査する。
    (id, lease.expect("prefix contains lock or creation"))
}

///
/// 停止後の明示lookup・作成・本文PUT・解放をすべて送信前に拒否する。
///
/// # 概要
/// lease使用不可のprefixでも停止原因が優先され、HTTPを増やさない。
///
/// # 引数
/// * `wiki` - 同じ期限・共有取消のclient
/// * `id` - prefix最後の確定ID
/// * `lease` - caller所有の資源
/// * `plan` - 独立readonly計画
/// * `kind` - 停止原因
///
/// # 戻り値
/// なし。すべての要求がBeforeSendであることを確認する。
///
async fn assert_stopped_followups(wiki: &WikiClient, id: &PageId,
    lease: &mut LockLease,
    plan: &std::sync::Arc<crate::projection::ProjectionPlan>, kind: FailureKind)
{
    let path = PagePath::new("/a").unwrap();
    /*
     * 同じclientへの明示後続も期限・取消の境界を越えない
     */
    stopped(wiki.lookup(&path).await.unwrap_err(), kind);
    stopped(wiki.create_draft(&path).await.unwrap_err(), kind);
    stopped(wiki.put_source(id, lease, plan.upload_body(0).unwrap())
        .await.unwrap_err(), kind);
    stopped(wiki.release_lock(id, lease).await.unwrap_err(), kind);
    stopped(wiki.read_source(id).await.unwrap_err(), kind);
    stopped(wiki.read_meta(id, &path).await.unwrap_err(), kind);
}

///
/// 四系列の各操作間で同一期限・共有取消後の追加HTTPを禁止する。
///
/// # 概要
/// 未到達prefixだけを実行し、次の操作を始める前に停止を注入する。
///
/// # 戻り値
/// なし。cancel/deadline双方についてprefixの要求数を維持する。
///
/// # 注記
/// phase用の期限を作らず、設定とStopContextに同一絶対期限を渡す。
/// 初回source直後はleaseがないためGET/POST/lockの停止を直接検査する。
///
#[tokio::test]
async fn every_series_gap_stops_followups_on_shared_cancel_or_deadline() {
    let plan = planned(b"body").await;
    let series: &[&[Step]] = &[
        &[Step::Create, Step::Put],
        &[Step::Source, Step::Lock, Step::Source, Step::Meta, Step::Release],
        &[Step::Lock, Step::Delete, Step::Create, Step::Put],
        &[Step::Lock, Step::Delete500, Step::Lookup, Step::Meta, Step::Create],
    ];
    /*
     * 各系列の全操作間を独立fixtureで検査し、共有contextを使い続ける
     */
    for steps in series {
        for gap in 1..steps.len() {
            for cancel in [true, false] {
                let server = Server::new(steps.iter().copied()
                    .map(response).collect());
                let deadline = Instant::now() + Duration::from_secs(2);
                let config = wiki_config_fixture(&server.url(), true, deadline);
                let stop = StopContext::new(deadline);
                let wiki = WikiClient::new(&config, stop.clone()).unwrap();
                let no_lease = matches!(steps[0], Step::Source) && gap == 1;
                let state = if no_lease {
                    drop(wiki.read_source(&PageId::new(ID).unwrap())
                        .await.unwrap());
                    None
                } else { Some(prefix(&wiki, &steps[..gap], &plan).await) };
                assert!(stop.checkpoint().is_ok());
                assert_eq!(server.count(), gap);
                /*
                 * 同じ絶対期限の経過または共有取消で次要求を停止する
                 */
                let kind = if cancel {
                    stop.cancel();
                    FailureKind::Cancelled
                } else {
                    tokio::time::sleep_until(deadline.into()).await;
                    FailureKind::Deadline
                };
                if let Some((id, mut lease)) = state {
                    assert_stopped_followups(&wiki, &id, &mut lease,
                        &plan, kind).await;
                } else {
                    let path = PagePath::new("/a").unwrap();
                    let id = PageId::new(ID).unwrap();
                    stopped(wiki.lookup(&path).await.unwrap_err(), kind);
                    stopped(wiki.create_draft(&path).await.unwrap_err(), kind);
                    stopped(wiki.acquire_lock(&id).await.unwrap_err(), kind);
                    stopped(wiki.read_source(&id).await.unwrap_err(), kind);
                }
                assert_eq!(server.count(), gap);
                assert_eq!(wiki.available_permits(), 1);
            }
        }
    }
}

///
/// adapterの有限全分類と操作投影で元事実を維持する。
///
/// # 概要
/// 全kindを実行側へmoveし、操作とstageが同義という仮定を排除する。
///
/// # 戻り値
/// なし。分類・観測・stageと警告対象外の境界を照合する。
///
/// # 注記
/// 型契約の単体試験であり、HTTP系列の代替証拠にはしない。
///
#[test]
fn adapter_preserves_all_failure_kinds_and_explicit_stages() {
    /*
     * 入力不正へ潰さず、すべての固定分類を実行側に残す
     */
    for kind in [FailureKind::ContextMismatch, FailureKind::Preparation,
        FailureKind::Authentication, FailureKind::Http, FailureKind::Transport,
        FailureKind::Protocol, FailureKind::BodyUtf8,
        FailureKind::BodyReplacement, FailureKind::Io, FailureKind::Worker,
        FailureKind::Deadline, FailureKind::Cancelled, FailureKind::LockHeader,
        FailureKind::LockExpired, FailureKind::LeaseUnavailable,
        FailureKind::LeaseMismatch]
    {
        let failure = WikiFailure::new(WikiOperation::ReadSource, kind,
            SendState::Started, Some(200), None);
        let payload = execution_failure(failure, ProcessingStage::Reconcile,
            Vec::new());
        assert_eq!(payload.facts().kind(), kind);
        assert_eq!(payload.facts().operation(), WikiOperation::ReadSource);
        assert_eq!(payload.stage(), ProcessingStage::Reconcile);
    }
    /*
     * 詳細操作を残したまま既存警告型への純粋な投影だけを検査する
     */
    for (source, target) in [
        (WikiOperation::Lookup, Operation::Lookup),
        (WikiOperation::ReadSource, Operation::Lookup),
        (WikiOperation::ReadMeta, Operation::Lookup),
        (WikiOperation::CreateDraft, Operation::Post),
        (WikiOperation::PutSource, Operation::Put),
        (WikiOperation::DeletePage, Operation::Delete),
        (WikiOperation::AcquireLock, Operation::Lock),
        (WikiOperation::RenewLock, Operation::Lock),
        (WikiOperation::ReleaseLock, Operation::Unlock),
    ] {
        let failure = WikiFailure::new(source, FailureKind::Http,
            SendState::Started, Some(500), None);
        let evidence = operation_evidence(failure, ObservedState::Unknown)
            .unwrap();
        assert_eq!(evidence.facts().operation(), source);
        assert_eq!(evidence.warning().unwrap().operation, target);
        assert_eq!(evidence.observed(), ObservedState::Unknown);
    }
    /*
     * 送信前と初期化の事実は操作警告に補完しない
     */
    for (operation, sent, status) in [
        (WikiOperation::Initialize, SendState::Started, Some(500)),
        (WikiOperation::ReadSource, SendState::NotStarted, None),
        (WikiOperation::ReadSource, SendState::Started, None),
    ] {
        let failure = WikiFailure::new(operation, FailureKind::Cancelled,
            sent, status, None);
        assert!(operation_evidence(failure, ObservedState::Unknown).is_none());
    }
}

///
/// DELETE応答の受信中停止でstatus・旧IDを保ちleaseを失効する。
///
/// # 概要
/// header受信入口の同期後に共通取消または同一期限へ到達させる。
///
/// # 戻り値
/// なし。worker/permit回収後も追加HTTPを開始しない。
///
/// # 注記
/// 完全error受信前の500はHttpへ潰さず、停止分類とstatusを併記する。
///
#[tokio::test]
async fn stopping_during_delete_receive_preserves_status_id_and_reclaims() {
    for cancel in [true, false] {
        let (reply, release) = Reply::new(500, b"{}".to_vec()).held();
        let server = Server::new(vec![locked(204, TOKEN, Vec::new()), reply]);
        let deadline = Instant::now() + Duration::from_secs(3);
        let config = wiki_config_fixture(&server.url(), true, deadline);
        let stop = StopContext::new(deadline);
        let wiki = WikiClient::new(&config, stop.clone()).unwrap();
        let observer = wiki.clone();
        let id = PageId::new(ID).unwrap();
        let mut lease = wiki.acquire_lock(&id).await.unwrap();
        let (ready, started) = tokio::sync::oneshot::channel();
        /*
         * task内で受信同期を登録し、header観測を期限推測と分離する
         */
        let task = tokio::spawn(async move {
            super::RECEIVE_STARTED.with(|slot|
                *slot.borrow_mut() = Some(ready));
            let result = wiki.delete_page(&id, &mut lease).await;
            (result, lease)
        });
        tokio::time::timeout(Duration::from_secs(2), started)
            .await.unwrap().unwrap();
        if cancel { stop.cancel(); }
        let (result, lease) = task.await.unwrap();
        let _ = release.send(());
        /*
         * 停止原因と確定HTTP事実を分離し、実行側へ安全に移す
         */
        let payload = execution_failure(result.unwrap_err(),
            ProcessingStage::DeleteOld, Vec::new());
        let kind = if cancel { FailureKind::Cancelled }
            else { FailureKind::Deadline };
        assert_eq!(payload.facts().kind(), kind);
        assert_eq!(payload.facts().boundary(),
            FailureBoundary::ResponseReceived);
        assert_eq!(payload.facts().status(), Some(500));
        assert_eq!(payload.facts().known_id().unwrap().as_str(), ID);
        assert!(!lease.usable());
        assert_eq!(observer.available_permits(), 1);
        stopped(observer.lookup(&PagePath::new("/a").unwrap())
            .await.unwrap_err(), kind);
        requests(&server, &[("POST", format!("/api/pages/{ID}/lock")),
            ("DELETE", format!("/api/pages/{ID}"))]);
    }
}

///
/// ロック後再取得futureの実dropだけは共有取消とworker回収を継続する。
///
/// # 概要
/// 通常500返却と異なる実abortを、worker所有の領域・permitで観測する。
///
/// # 戻り値
/// なし。回収前後の追加HTTP0と最終保管領域消失を照合する。
///
/// # 注記
/// proc由来の保管pathは試験内だけで扱い、診断に渡さない。
///
#[tokio::test]
async fn dropping_recheck_future_cancels_series_and_defers_owned_cleanup() {
    let server = Server::new(vec![locked(204, TOKEN, Vec::new()),
        Reply::new(200, b"private-body".to_vec())]);
    let (wiki, stop) = client(&server, true);
    let id = PageId::new(ID).unwrap();
    let mut lease = wiki.acquire_lock(&id).await.unwrap();
    let observer = wiki.clone();
    let (ready, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    /*
     * workerへ結果所有後の同期点を移し、受信futureを実際に破棄する
     */
    let task = tokio::spawn(async move {
        super::ACTION.with(|slot| *slot.borrow_mut() =
            Some(super::WorkerAction::Hold { ready, release: wait }));
        wiki.read_source(&PageId::new(ID).unwrap()).await
    });
    let path = tokio::time::timeout(Duration::from_secs(2), started)
        .await.unwrap().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(stop.checkpoint().is_err());
    assert!(path.exists());
    assert_eq!(observer.available_permits(), 0);
    stopped(observer.release_lock(&id, &mut lease).await.unwrap_err(),
        FailureKind::Cancelled);
    assert_eq!(server.count(), 2);
    /*
     * 所有workerの終了後にだけ領域とpermitが回収される
     */
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.parent().unwrap().exists()
            || observer.available_permits() != 1
        { tokio::task::yield_now().await; }
    }).await.unwrap();
    stopped(observer.lookup(&PagePath::new("/a").unwrap())
        .await.unwrap_err(), FailureKind::Cancelled);
    requests(&server, &[("POST", format!("/api/pages/{ID}/lock")),
        ("GET", format!("/api/pages/{ID}/source"))]);
}
