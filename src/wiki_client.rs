/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! Wikiの読取・変更操作と逐次I/O・安全な単発失敗の公開境界
//!

pub(crate) mod adapter;
mod json;
mod lock;
mod receive;
mod store;
mod transport;
mod upload;
pub(crate) mod types;

pub(crate) use lock::{DraftCreated, LockLease, Origin, ReleaseEffect};
pub(crate) use store::ReceivedBody;
pub(crate) use types::{LookupResult, PageMetadata, WikiFailure};

use std::sync::Arc;

use crate::cmd_args::ResolvedRunConfig;
use crate::projection::PlannedBody;
use crate::report::types::{PageId, PagePath};
use crate::source::{SourceError, StopContext};

use self::json::JsonValue;
use self::receive::{Job, Received};
use self::transport::{Change, ChangeReply, Transport};
use self::types::{FailureKind, SendState, WikiOperation};

///
/// Executor判断・自動再照合を行わない単発Wiki client。
///
#[derive(Clone)]
pub(crate) struct WikiClient { transport: Arc<Transport> }

impl std::fmt::Debug for WikiClient {
    ///
    /// 接続先・認証・request内部値を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 固定型名だけの表示
    ///
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>)
        -> std::fmt::Result
    {
        formatter.write_str("WikiClient")
    }
}

impl WikiClient {
    ///
    /// Wiki設定と同じ絶対期限の共有contextから作る。
    ///
    /// # 引数
    /// * `config` - 採用済みtarget・Wiki認証・上限
    /// * `stop` - 取得・検証・計画と同じcontextのclone
    ///
    /// # 戻り値
    /// 単発Wiki client。不一致期限は送信前に拒否する。
    ///
    pub(crate) fn new(config: &ResolvedRunConfig, stop: StopContext)
        -> Result<Self, WikiFailure>
    {
        Ok(Self { transport: Arc::new(Transport::new(config, stop)?) })
    }

    ///
    /// 検証済みpathの有効IDを初回照会する。
    ///
    /// # 引数
    /// * `path` - 絶対Wiki path
    ///
    /// # 戻り値
    /// ID・不存在・削除済み、または安全な失敗
    ///
    pub(crate) async fn lookup(&self, path: &PagePath)
        -> Result<LookupResult, WikiFailure>
    {
        /*
         * encoded照会を一度だけ行い、初回不存在と削除済みを区別する
         */
        let operation = WikiOperation::Lookup;
        let mut url = self.transport.url(&["api", "pages", "id"])
            .map_err(|kind| local_failure(operation, kind, None))?;
        url.query_pairs_mut().append_pair("path", path.as_str());
        let reply = self.transport.get(url, operation, None, Job::Id).await?;
        match (reply.status, reply.received) {
            (200, Received::Json(JsonValue::Id(id))) =>
                Ok(LookupResult::Found(id)),
            (404, Received::Empty) => Ok(LookupResult::NotFound),
            (410, Received::Empty) => Ok(LookupResult::Deleted),
            _ => Err(received_failure(operation, reply.status, None)),
        }
    }

    ///
    /// caller既知のIDの本文を独立readonly保管へ受け取る。
    ///
    /// # 引数
    /// * `id` - 検証済み照会ID
    ///
    /// # 戻り値
    /// 全本文検証済みlease、または安全な失敗
    ///
    pub(crate) async fn read_source(&self, id: &PageId)
        -> Result<ReceivedBody, WikiFailure>
    {
        /*
         * caller既知IDを送信と独立保管の成功・失敗へ結び付ける
         */
        let operation = WikiOperation::ReadSource;
        let url = self.transport.url(&["api", "pages", id.as_str(), "source"])
            .map_err(|kind| local_failure(operation, kind, Some(id)))?;
        let reply = self.transport.get(url, operation, Some(id),
            Job::Body(id.clone())).await?;
        match reply.received {
            Received::Body(body) => Ok(body),
            _ => Err(received_failure(operation, reply.status, Some(id))),
        }
    }

    ///
    /// metadataを逐次検査し、期待pathとの関係を事実として返す。
    ///
    /// # 引数
    /// * `id` - caller既知のID。応答id欄を要求しない。
    /// * `expected_path` - callerが検証した期待path
    ///
    /// # 戻り値
    /// 有限metadata、または安全な失敗
    ///
    pub(crate) async fn read_meta(&self, id: &PageId, expected_path: &PagePath)
        -> Result<PageMetadata, WikiFailure>
    {
        let operation = WikiOperation::ReadMeta;
        let url = self.transport.url(&["api", "pages", id.as_str(), "meta"])
            .map_err(|kind| local_failure(operation, kind, Some(id)))?;
        let reply = self.transport.get(url, operation, Some(id),
            Job::Metadata { id: id.clone(), expected: expected_path.clone() })
            .await?;
        match reply.received {
            Received::Json(JsonValue::Metadata(meta)) => Ok(meta),
            _ => Err(received_failure(operation, reply.status, Some(id))),
        }
    }

    ///
    /// 本文なしPOSTを一度だけ送り、確定IDと秘匿lockを返す。
    ///
    /// # 概要
    /// 完全JSONの確定IDと有効lockだけを作成結果へ結び付ける。
    ///
    /// # 引数
    /// * `path` - 検証済み絶対path
    ///
    /// # 戻り値
    /// 正常201の作成結果、または既知IDを保持した安全な失敗
    ///
    pub(crate) async fn create_draft(&self, path: &PagePath)
        -> Result<DraftCreated, WikiFailure>
    {
        let operation = WikiOperation::CreateDraft;
        let mut url = self.transport.url(&["api", "pages"])
            .map_err(|kind| local_failure(operation, kind, None))?;
        url.query_pairs_mut().append_pair("path", path.as_str());
        let reply = self.transport.change(Change {
            operation, method: reqwest::Method::POST, url, id: None,
            success: 201, body: None,
        }, None).await?;
        /*
         * 完全JSONのIDだけを資源へ結び付け、公開時期限も検査する
         */
        let Received::Json(JsonValue::Id(id)) = reply.received else {
            return Err(received_failure(operation, reply.status, None));
        };
        let data = reply.data.ok_or_else(||
            received_failure(operation, reply.status, Some(&id)))?;
        check(&self.transport.stop).map_err(|kind| WikiFailure::new(operation,
            kind, SendState::Started, Some(reply.status), Some(&id)))?;
        let lease = LockLease::new(data, &self.transport.identity, id.clone(),
            Origin::CreatedDraft).map_err(|kind| WikiFailure::new(operation,
                kind, SendState::Started, Some(reply.status), Some(&id)))?;
        Ok(DraftCreated { id, lease })
    }

    ///
    /// ページ種別を推測せず単発lockを取得する。
    ///
    /// # 引数
    /// * `id` - 確定対象ID
    ///
    /// # 戻り値
    /// 取得由来の秘匿資源、または安全な失敗
    ///
    pub(crate) async fn acquire_lock(&self, id: &PageId)
        -> Result<LockLease, WikiFailure>
    {
        let operation = WikiOperation::AcquireLock;
        let reply = self.lock_request(id, operation, reqwest::Method::POST,
            None).await?;
        let data = reply.data.ok_or_else(||
            received_failure(operation, reply.status, Some(id)))?;
        check(&self.transport.stop).map_err(|kind| WikiFailure::new(operation,
            kind, SendState::Started, Some(reply.status), Some(id)))?;
        LockLease::new(data, &self.transport.identity, id.clone(),
            Origin::Acquired).map_err(|kind| WikiFailure::new(operation,
                kind, SendState::Started, Some(reply.status), Some(id)))
    }

    ///
    /// await中排他的に借用し、正常新tokenだけへ切り替える。
    ///
    /// # 引数
    /// * `lease` - 同じclientの使用可能な資源
    ///
    /// # 戻り値
    /// 延長成功、または旧tokenを再使用しない安全な失敗
    ///
    pub(crate) async fn renew_lock(&self, lease: &mut LockLease)
        -> Result<(), WikiFailure>
    {
        let operation = WikiOperation::RenewLock;
        let id = lease.id().clone();
        let reply = self.lock_request(&id, operation, reqwest::Method::PUT,
            Some(lease)).await?;
        let data = reply.data.ok_or_else(||
            received_failure(operation, reply.status, Some(&id)))?;
        check(&self.transport.stop).and_then(|_| lease.renew(data))
            .map_err(|kind| WikiFailure::new(operation, kind,
                SendState::Started, Some(reply.status), Some(&id)))
    }

    ///
    /// 明示解除を一度だけ送り、draft削除の作用を隠さない。
    ///
    /// # 引数
    /// * `id` - 対象ID
    /// * `lease` - 対象へ結び付いた使用可能資源
    ///
    /// # 戻り値
    /// 確定解除作用、または安全な失敗
    ///
    pub(crate) async fn release_lock(&self, id: &PageId,
        lease: &mut LockLease) -> Result<ReleaseEffect, WikiFailure>
    {
        let origin = lease.origin();
        self.lock_request(id, WikiOperation::ReleaseLock,
            reqwest::Method::DELETE, Some(lease)).await?;
        Ok(match origin {
            Origin::CreatedDraft => ReleaseEffect::DraftDeleted,
            Origin::Acquired =>
                ReleaseEffect::ReleasedWithPossibleDraftDeletion,
        })
    }

    ///
    /// 検証済みreadonly本文bytesを変換せず単発PUTする。
    ///
    /// # 引数
    /// * `id` - 対象ID
    /// * `lease` - 同じ対象の資源
    /// * `body` - 成立済み計画だけから生成した本文所有者
    ///
    /// # 戻り値
    /// 正常204、または安全な失敗。amendは省略(false)。
    ///
    pub(crate) async fn put_source(&self, id: &PageId, lease: &mut LockLease,
        body: PlannedBody) -> Result<(), WikiFailure>
    {
        let operation = WikiOperation::PutSource;
        let url = self.transport.url(&["api", "pages", id.as_str(), "source"])
            .map_err(|kind| local_failure(operation, kind, Some(id)))?;
        self.transport.change(Change { operation, method: reqwest::Method::PUT,
            url, id: Some(id), success: 204, body: Some(body) }, Some(lease))
            .await.map(|_| ())
    }

    ///
    /// 配下へ作用させず単発DELETEする。
    ///
    /// # 引数
    /// * `id` - 対象ID
    /// * `lease` - 同じ対象の資源
    ///
    /// # 戻り値
    /// 正常204、または安全な失敗。recursiveは省略(false)。
    ///
    pub(crate) async fn delete_page(&self, id: &PageId,
        lease: &mut LockLease) -> Result<(), WikiFailure>
    {
        let operation = WikiOperation::DeletePage;
        let url = self.transport.url(&["api", "pages", id.as_str()])
            .map_err(|kind| local_failure(operation, kind, Some(id)))?;
        self.transport.change(Change {
            operation, method: reqwest::Method::DELETE, url, id: Some(id),
            success: 204, body: None,
        }, Some(lease)).await.map(|_| ())
    }

    ///
    /// lock endpointだけの内部固定要求を組み立てる。
    ///
    /// # 引数
    /// * `id` - 対象ID
    /// * `operation` - 取得/延長/解除
    /// * `method` - 固定HTTP method
    /// * `lease` - 延長/解除時の秘匿資源
    ///
    /// # 戻り値
    /// 単発正常204または安全な失敗
    ///
    async fn lock_request(&self, id: &PageId, operation: WikiOperation,
        method: reqwest::Method, lease: Option<&mut LockLease>)
        -> Result<ChangeReply, WikiFailure>
    {
        let url = self.transport.url(&["api", "pages", id.as_str(), "lock"])
            .map_err(|kind| local_failure(operation, kind, Some(id)))?;
        self.transport.change(Change { operation, method, url, id: Some(id),
            success: 204, body: None }, lease).await
    }

    ///
    /// test内だけでworkerとtransportのpermit所有を確認する。
    ///
    /// # 戻り値
    /// 利用可能なpermit数
    ///
    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.transport.available_permits()
    }
}

///
/// contextの停止をWikiの固定分類へ写す。
///
/// # 引数
/// * `stop` - 共有絶対期限と取消
///
/// # 戻り値
/// 境界内なら成功
///
fn check(stop: &StopContext) -> Result<(), FailureKind> {
    stop.checkpoint().map_err(|error| match error {
        SourceError::Deadline => FailureKind::Deadline,
        _ => FailureKind::Cancelled,
    })
}

///
/// URL準備や防御的拒否を値なしの失敗へ写す。
///
/// # 引数
/// * `operation` - 単発操作
/// * `kind` - 固定分類
/// * `id` - caller既知ID
///
/// # 戻り値
/// 安全な失敗
///
fn local_failure(operation: WikiOperation, kind: FailureKind,
    id: Option<&PageId>) -> WikiFailure
{
    WikiFailure::new(operation, kind, SendState::NotStarted, None, id)
}

///
/// 防御的なreply不整合でも受信済みstatusと既知IDを失わない。
///
/// # 引数
/// * `operation` - 単発操作
/// * `status` - 受信済みHTTP status
/// * `id` - caller既知のID
///
/// # 戻り値
/// 送信開始後の安全な失敗
///
fn received_failure(operation: WikiOperation, status: u16,
    id: Option<&PageId>) -> WikiFailure
{
    WikiFailure::new(operation, FailureKind::Protocol, SendState::Started,
        Some(status), id)
}

#[cfg(test)]
mod tests;
