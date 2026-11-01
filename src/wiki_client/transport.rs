/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! Wiki専用認証・URL・単発送信と受信終了までのHTTP permit
//!

use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::cmd_args::resolve::WikiAuth;
use crate::cmd_args::ResolvedRunConfig;
use crate::projection::PlannedBody;
use crate::report::types::PageId;
use crate::source::{CancelOnDrop, StopContext};

use super::json::JsonValue;
use super::lock::{self, LockData, LockLease};
use super::receive::{self, Job, Received};
use super::types::{FailureKind, SendState, WikiFailure, WikiOperation};
use super::upload::Upload;

///
/// 認証headerと接続先を表示しないWiki専用transport。
///
pub(super) struct Transport {
    client: reqwest::Client,
    base: Url,
    authorization: HeaderValue,
    permit: Arc<Semaphore>,
    pub(super) stop: StopContext,
    pub(super) identity: Arc<()>,
}

impl Transport {
    ///
    /// 検証済み設定からWiki専用clientを作る。
    ///
    /// # 概要
    /// context不一致を送信前に拒否し、GitHub tokenを取得しない。
    ///
    /// # 引数
    /// * `config` - 採用済みtarget・Wiki認証・期限・上限
    /// * `stop` - 設定と同じ期限の共有context
    ///
    /// # 戻り値
    /// 単発transportまたは安全な送信前失敗
    ///
    pub(super) fn new(config: &ResolvedRunConfig, stop: StopContext)
        -> Result<Self, WikiFailure>
    {
        let fail = |kind| WikiFailure::new(WikiOperation::Initialize, kind,
            SendState::NotStarted, None, None);
        /*
         * 期限を延長せず、初期版の直列permitを設定上限内に置く
         */
        if config.deadline() != stop.deadline() {
            return Err(fail(FailureKind::ContextMismatch));
        }
        super::check(&stop).map_err(fail)?;
        if config.settings().limits.max_http_concurrency == 0 {
            return Err(fail(FailureKind::Preparation));
        }
        let client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|_| fail(FailureKind::Preparation))?;
        /*
         * request構築だけで認証headerを検査し、値を型付き失敗へ移さない
         */
        let base = config.settings().target.clone();
        let request = match config.auth() {
            WikiAuth::Bearer(token) =>
                client.get(base.clone()).bearer_auth(token.expose()),
            WikiAuth::Basic { user, password } =>
                client.get(base.clone()).basic_auth(user.expose(),
                    Some(password.expose())),
        }.build().map_err(|_| fail(FailureKind::Authentication))?;
        let mut authorization = request.headers().get(AUTHORIZATION)
            .cloned().ok_or_else(|| fail(FailureKind::Authentication))?;
        authorization.set_sensitive(true);
        Ok(Self { client, base, authorization,
            permit: Arc::new(Semaphore::new(1)), stop, identity: Arc::new(()) })
    }

    ///
    /// 固定API segmentからURLを組み立てる。
    ///
    /// # 引数
    /// * `segments` - path/id/source等の値
    ///
    /// # 戻り値
    /// 同じ接続先のescaped URL
    ///
    pub(super) fn url(&self, segments: &[&str]) -> Result<Url, FailureKind> {
        let mut url = self.base.clone();
        url.path_segments_mut().map_err(|_| FailureKind::Preparation)?
            .clear().extend(segments);
        Ok(url)
    }

    ///
    /// 単発GETと全受信を実行し、通常失敗時はguardを解除する。
    ///
    /// # 引数
    /// * `url` - 自分で組み立てた同じoriginのURL
    /// * `operation` - 単発読取操作
    /// * `id` - caller既知のID
    /// * `job` - 正常200での受信目的
    ///
    /// # 戻り値
    /// 確定応答、または安全な失敗
    ///
    pub(super) async fn get(&self, url: Url, operation: WikiOperation,
        id: Option<&PageId>, job: Job) -> Result<Reply, WikiFailure>
    {
        let mut guard = CancelOnDrop::new(self.stop.clone());
        let result = self.get_inner(url, operation, id, job).await;
        guard.disarm();
        result
    }

    ///
    /// permit・送信・受信の全待機を同じ停止境界へ接続する。
    ///
    /// # 概要
    /// 確定statusを失わず、HTTP/error受信を終えるまでpermitを保持する。
    ///
    /// # 引数
    /// * `url` - 値としてescapedされたURL
    /// * `operation` - 単発の操作
    /// * `id` - caller既知のID
    /// * `job` - 200応答の受信目的
    ///
    /// # 戻り値
    /// 全受信済み応答または型付き失敗
    ///
    async fn get_inner(&self, url: Url, operation: WikiOperation,
        id: Option<&PageId>, job: Job) -> Result<Reply, WikiFailure>
    {
        let failure = |kind, sent, status|
            WikiFailure::new(operation, kind, sent, status, id);
        /*
         * permit待機後とsend開始直前の双方で停止を拒否する
         */
        super::check(&self.stop).map_err(|kind|
            failure(kind, SendState::NotStarted, None))?;
        let permit = crate::measurement::permit(Arc::new(tokio::select! {
            permit = self.permit.clone().acquire_owned() =>
                permit.map_err(|_| failure(FailureKind::Preparation,
                    SendState::NotStarted, None))?,
            _ = self.stop.wait_stopped() => return Err(failure(
                stopped(&self.stop), SendState::NotStarted, None,
            )),
        }));
        let request = self.client.get(url)
            .header(AUTHORIZATION, self.authorization.clone()).build()
            .map_err(|_| failure(FailureKind::Preparation,
                SendState::NotStarted, None))?;
        super::check(&self.stop).map_err(|kind|
            failure(kind, SendState::NotStarted, None))?;
        /*
         * selectがexecuteを初回pollしなかった停止を未送信として保つ
         */
        let request_id = crate::measurement::id();
        let mut request_scope = None;
        let started = AtomicBool::new(false);
        let mut execute = Box::pin(self.client.execute(request));
        let response = tokio::select! {
            response = poll_fn(|context| {
                if !started.load(Ordering::Acquire) {
                    /*
                     * 初回poll直前の停止を検査し、HTTP開始の観測区間を作る
                     */
                    if let Err(kind) = super::check(&self.stop) {
                        return Poll::Ready(Err(failure(kind,
                            SendState::NotStarted, None)));
                    }
                    request_scope = Some(crate::measurement::Span::new(
                        crate::measurement::Event::HttpStart,
                        crate::measurement::Event::HttpEnd, request_id,
                        self.stop.trace_id(), crate::measurement::Role::Other));
                    started.store(true, Ordering::Release);
                }
                execute.as_mut().poll(context)
                    .map(|result| result.map_err(|_| failure(
                        FailureKind::Transport, SendState::Started, None,
                    )))
            }) => response?,
            _ = self.stop.wait_stopped() => return Err(failure(
                stopped(&self.stop),
                if started.load(Ordering::Acquire) { SendState::Started }
                else { SendState::NotStarted }, None,
            )),
        };
        /*
         * statusと照会IDを保持し、error bodyも逐次処理する
         */
        let status = response.status().as_u16();
        super::check(&self.stop).map_err(|kind|
            failure(kind, SendState::Started, Some(status)))?;
        let lookup_missing = operation == WikiOperation::Lookup
            && matches!(status, 404 | 410);
        let success = status == 200 || lookup_missing;
        let purpose = if status == 200 { job }
            else if lookup_missing { Job::Discard } else { Job::Error };
        let _request_scope = request_scope;
        let received = receive::run(response, purpose, self.stop.clone(),
            crate::measurement::share(&permit)).await;
        super::check(&self.stop).map_err(|kind|
            failure(kind, SendState::Started, Some(status)))?;
        /*
         * 不正error bodyでも確定したstatusを捨てない
         */
        match (success, received) {
            (true, Ok(received)) => Ok(Reply { status, received }),
            (true, Err(kind)) => Err(failure(kind, SendState::Started,
                Some(status))),
            (false, Ok(received)) => Err(failure(FailureKind::Http,
                SendState::Started, Some(status))
                .with_diagnostics(receive::diagnostics(received))),
            (false, Err(kind)) => Err(failure(kind, SendState::Started,
                Some(status))),
        }
    }

    ///
    /// 変更単発操作を実行し、通常返却では共有取消guardを解除する。
    ///
    /// # 引数
    /// * `change` - 内部で組み立てた固定操作
    /// * `lease` - await中排他的に借用する資源
    ///
    /// # 戻り値
    /// 完全応答または秘密を持たない失敗
    ///
    pub(super) async fn change(&self, change: Change<'_>,
        lease: Option<&mut LockLease>) -> Result<ChangeReply, WikiFailure>
    {
        let mut guard = CancelOnDrop::new(self.stop.clone());
        let result = self.change_inner(change, lease).await;
        guard.disarm();
        result
    }

    ///
    /// 準備・permit・upload workerを同じ期限内で所有する。
    ///
    /// # 概要
    /// worker終了と最終停止確認を終えてからlease保持を確定する。
    ///
    /// # 引数
    /// * `change` - 値としてescapedされた操作
    /// * `lease` - 要求に結び付いた秘匿資源
    ///
    /// # 戻り値
    /// worker終了確認済みの単発結果
    ///
    async fn change_inner(&self, mut change: Change<'_>,
        mut lease: Option<&mut LockLease>) -> Result<ChangeReply, WikiFailure>
    {
        let fail = |kind| WikiFailure::new(change.operation, kind,
            SendState::NotStarted, None, change.id);
        super::check(&self.stop).map_err(fail)?;
        if let Some(lease) = lease.as_deref() {
            let id = change.id.ok_or_else(|| fail(FailureKind::Preparation))?;
            lease.check(&self.identity, id).map_err(fail)?;
        }
        /*
         * permitはuploadと受信workerへ共有し、実I/O終了前に返さない
         */
        let permit = crate::measurement::permit(Arc::new(tokio::select! {
            result = self.permit.clone().acquire_owned() =>
                result.map_err(|_| fail(FailureKind::Preparation))?,
            _ = self.stop.wait_stopped() =>
                return Err(fail(stopped(&self.stop))),
        }));
        super::check(&self.stop).map_err(fail)?;
        let mut request = self.client.request(change.method.clone(),
            change.url.clone())
            .header(AUTHORIZATION, self.authorization.clone());
        if let Some(lease) = lease.as_deref() {
            request = request.header("x-lock-authentication",
                lease.authentication().map_err(fail)?);
        }
        let mut upload = None;
        /*
         * 本文準備をworkerに委ね、File/planをrequestへ裸で渡さない
         */
        if let Some(body) = change.body.take() {
            let length = body.byte_len();
            let (owner, body) = Upload::prepare(body, self.stop.clone(),
                crate::measurement::share(&permit)).await.map_err(fail)?;
            upload = Some(owner);
            request = request.header(CONTENT_TYPE, "text/markdown")
                .header(CONTENT_LENGTH, length).body(body);
        }
        let result = match request.build() {
            Ok(request) => self.exchange(request, &change, lease.as_deref_mut(),
                crate::measurement::share(&permit)).await,
            Err(_) => Err(fail(FailureKind::Preparation)),
        };
        /*
         * 応答拒否でもworkerを終了させ、引渡し量とwire送信を混同しない
         */
        if let Some(mut upload) = upload {
            let complete = upload.complete();
            #[cfg(test)]
            super::upload::tests::notify_join();
            let joined = upload.join().await;
            if let Err(kind) = super::check(&self.stop) {
                let (sent, status, id) = match &result {
                    Ok(reply) => (SendState::Started, Some(reply.status),
                        change.id),
                    Err(failure) => (failure.sent(), failure.status(),
                        failure.known_id()),
                };
                return Err(WikiFailure::new(change.operation, kind, sent,
                    status, id));
            }
            if let Ok(reply) = &result {
                let kind = joined.err().or_else(||
                    (!complete).then_some(FailureKind::Protocol));
                if let Some(kind) = kind {
                    return Err(WikiFailure::new(change.operation, kind,
                        SendState::Started, Some(reply.status), change.id));
                }
            } else if let (Err(failure), Err(kind)) = (&result, joined) {
                if matches!(failure.kind(), FailureKind::Transport
                    | FailureKind::Worker | FailureKind::Protocol)
                    || (failure.kind() == FailureKind::Http
                        && failure.status() == Some(400))
                {
                    return Err(WikiFailure::new(change.operation, kind,
                        failure.sent(), failure.status(), failure.known_id()));
                }
            }
        }
        if let Ok(reply) = &result {
            let known = match &reply.received {
                Received::Json(JsonValue::Id(id)) => Some(id),
                _ => change.id,
            };
            super::check(&self.stop).map_err(|kind|
                WikiFailure::new(change.operation, kind, SendState::Started,
                    Some(reply.status), known))?;
        }
        /*
         * 完全400でもworker終了後の停止・期限確認まで保持へ戻さない
         */
        if let Err(failure) = &result {
            if change.operation == WikiOperation::PutSource
                && failure.kind() == FailureKind::Http
                && failure.status() == Some(400)
            {
                if let Some(lease) = lease.as_deref_mut() {
                    lease.retain(&self.stop).map_err(|kind|
                        WikiFailure::new(change.operation, kind,
                            failure.sent(), failure.status(),
                            failure.known_id()))?;
                }
            }
        }
        result
    }

    ///
    /// 初回pollで資源を使用不可とし、確定応答の事実を保持する。
    ///
    /// # 概要
    /// headerと完全受信を検査し、POST確定IDと応答statusを失わない。
    ///
    /// # 引数
    /// * `request` - 構築済み単発request
    /// * `change` - 正常statusと受信目的
    /// * `lease` - 排他的な秘匿資源
    /// * `permit` - 受信workerまで所有するpermit
    ///
    /// # 戻り値
    /// 完全検証済み応答または確定事実を持つ失敗
    ///
    async fn exchange(&self, request: reqwest::Request, change: &Change<'_>,
        mut lease: Option<&mut LockLease>, permit: Arc<OwnedSemaphorePermit>)
        -> Result<ChangeReply, WikiFailure>
    {
        let failure = |kind, sent, status, id|
            WikiFailure::new(change.operation, kind, sent, status, id);
        let request_id = crate::measurement::id();
        let mut request_scope = None;
        let started = AtomicBool::new(false);
        let mut execute = Box::pin(self.client.execute(request));
        /*
         * 未pollならNotStartedを維持し、初回poll直前にも期限を検査する
         */
        let response = tokio::select! {
            result = poll_fn(|context| {
                if !started.load(Ordering::Acquire) {
                    /*
                     * 停止境界とleaseを検査し、変更要求の資源を使用開始にする
                     */
                    let ready = super::check(&self.stop).and_then(|_| {
                        if let Some(lease) = lease.as_deref_mut() {
                            let id = change.id
                                .ok_or(FailureKind::Preparation)?;
                            lease.check(&self.identity, id)?;
                            lease.begin();
                        }
                        Ok(())
                    });
                    /*
                     * 検査失敗は未開始で返し、成功時だけHTTP開始を記録する
                     */
                    if let Err(kind) = ready {
                        return Poll::Ready(Err(failure(kind,
                            SendState::NotStarted, None, change.id)));
                    }
                    request_scope = Some(crate::measurement::Span::new(
                        crate::measurement::Event::HttpStart,
                        crate::measurement::Event::HttpEnd, request_id,
                        self.stop.trace_id(), crate::measurement::Role::Other));
                    started.store(true, Ordering::Release);
                }
                execute.as_mut().poll(context).map(|result|
                    result.map_err(|_| failure(FailureKind::Transport,
                        SendState::Started, None, change.id)))
            }) => result?,
            _ = self.stop.wait_stopped() => return Err(failure(
                stopped(&self.stop),
                if started.load(Ordering::Acquire) { SendState::Started }
                else { SendState::NotStarted }, None, change.id,
            )),
        };
        let status = response.status().as_u16();
        let success = status == change.success;
        let expects_lock = matches!(change.operation,
            WikiOperation::CreateDraft | WikiOperation::AcquireLock
                | WikiOperation::RenewLock);
        let data = if success && expects_lock {
            Some(lock::parse(response.headers()))
        } else { None };
        /*
         * header不正でもPOSTの全JSONを読み、確定IDを失わない
         */
        let job = if !success { Job::Error }
            else if change.operation == WikiOperation::CreateDraft {
                Job::CreatedId
            }
            else { Job::Discard };
        let _request_scope = request_scope;
        let received = receive::run(response, job, self.stop.clone(), permit)
            .await;
        let known_id = match &received {
            Ok(Received::Json(JsonValue::Id(id))) if success =>
                Some(id.clone()),
            _ => change.id.cloned(),
        };
        let known = known_id.as_ref();
        #[cfg(test)]
        super::tests::writes::before_publish(&self.stop);
        super::check(&self.stop).map_err(|kind|
            failure(kind, SendState::Started, Some(status), known))?;
        /*
         * 完全error受信の事実を返し、資源保持の確定はworker終了後へ残す
         */
        if !success {
            return match received {
                Ok(received) => Err(failure(FailureKind::Http,
                    SendState::Started, Some(status), known)
                    .with_diagnostics(receive::diagnostics(received))),
                Err(kind) => Err(failure(kind, SendState::Started,
                    Some(status), known)),
            };
        }
        let data = data.transpose().map_err(|kind|
            failure(kind, SendState::Started, Some(status), known))?;
        let received = received.map_err(|kind|
            failure(kind, SendState::Started, Some(status), known))?;
        if let Some(data) = &data {
            data.valid().map_err(|kind|
                failure(kind, SendState::Started, Some(status), known))?;
        }
        Ok(ChangeReply { status, received, data })
    }

    ///
    /// worker終了前後のpermit所有を試験内だけで観察する。
    ///
    /// # 戻り値
    /// 利用可能なpermit数
    ///
    #[cfg(test)]
    pub(super) fn available_permits(&self) -> usize {
        self.permit.available_permits()
    }

    ///
    /// 試験だけでpermit待ちの境界を作る。
    ///
    /// # 戻り値
    /// 試験が明示解放するpermit
    ///
    #[cfg(test)]
    pub(super) async fn hold_permit(&self) -> OwnedSemaphorePermit {
        self.permit.clone().acquire_owned().await.unwrap()
    }
}

///
/// 受信処理が完了した単発応答。
///
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) received: Received,
}

///
/// 内部の固定変更操作。任意外部URLやtokenを公開しない。
///
pub(super) struct Change<'a> {
    pub(super) operation: WikiOperation,
    pub(super) method: reqwest::Method,
    pub(super) url: Url,
    pub(super) id: Option<&'a PageId>,
    pub(super) success: u16,
    pub(super) body: Option<PlannedBody>,
}

///
/// 正常契約検証後にだけ上位へ渡す必要値。
///
pub(super) struct ChangeReply {
    pub(super) status: u16,
    pub(super) received: Received,
    pub(super) data: Option<LockData>,
}

///
/// 待機で到達した共有停止を返す。
///
/// # 引数
/// * `stop` - 既に停止通知を受けたcontext
///
/// # 戻り値
/// 固定停止分類
///
fn stopped(stop: &StopContext) -> FailureKind {
    super::check(stop).err().unwrap_or(FailureKind::Cancelled)
}
