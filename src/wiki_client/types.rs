/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 本文・秘密値を表示経路へ渡さないWiki単発結果と失敗分類
//!

use std::fmt;

use crate::report::types::PageId;

///
/// 単発REST操作。変更操作の進行判断は含めない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WikiOperation {
    Initialize, Lookup, ReadSource, ReadMeta, CreateDraft, AcquireLock,
    RenewLock, ReleaseLock, PutSource, DeletePage,
}

///
/// 外部messageを保持しないWiki境界の失敗。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    ContextMismatch, Preparation, Authentication, Http, Transport, Protocol,
    BodyUtf8, BodyReplacement, Io, Worker, Deadline, Cancelled,
    LockHeader, LockExpired, LeaseUnavailable, LeaseMismatch,
}

///
/// send futureを開始したか。実送信byte数を推測しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendState { NotStarted, Started }

///
/// HTTPの観測境界。副作用の適用有無を意味しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureBoundary {
    BeforeSend, ResponseUncertain, ResponseReceived,
}

///
/// error応答から選択するfront matterの有限分類。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontMatterKind { Syntax, Validation, Unknown }

///
/// 本文・任意message・property pathを持たない診断。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Diagnostics {
    pub(crate) kind: FrontMatterKind,
    pub(crate) line: Option<u64>,
    pub(crate) column: Option<u64>,
}

///
/// 操作と確定事実だけを持ち、認証・本文・生エラーを保持しない失敗。
///
pub(crate) struct WikiFailure {
    operation: WikiOperation,
    kind: FailureKind,
    sent: SendState,
    status: Option<u16>,
    known_id: Option<PageId>,
    diagnostics: Option<Diagnostics>,
}

impl WikiFailure {
    ///
    /// 公開可能な情報だけで失敗を作る。
    ///
    /// # 引数
    /// * `operation` - 単発の操作
    /// * `kind` - 固定分類
    /// * `sent` - send開始の事実
    /// * `status` - 確定したHTTP status
    /// * `id` - 呼出し時点から既知の照会ID
    ///
    /// # 戻り値
    /// 秘密値を持たない失敗
    ///
    pub(super) fn new(operation: WikiOperation, kind: FailureKind,
        sent: SendState, status: Option<u16>, id: Option<&PageId>) -> Self
    {
        Self { operation, kind, sent, status, known_id: id.cloned(),
            diagnostics: None }
    }

    ///
    /// 有限型・数値だけの診断を付ける。
    ///
    /// # 引数
    /// * `diagnostics` - 検証済み診断
    ///
    /// # 戻り値
    /// 診断を保持した失敗
    ///
    pub(super) fn with_diagnostics(mut self,
        diagnostics: Option<Diagnostics>) -> Self
    {
        self.diagnostics = diagnostics;
        self
    }

    ///
    /// 固定失敗分類を取得する。
    ///
    /// # 戻り値
    /// 値を含めない分類
    ///
    pub(crate) fn kind(&self) -> FailureKind { self.kind }

    ///
    /// 失敗した操作を取得する。
    ///
    /// # 戻り値
    /// 単発の操作
    ///
    pub(crate) fn operation(&self) -> WikiOperation { self.operation }

    ///
    /// send開始状態を取得する。
    ///
    /// # 戻り値
    /// 送信開始前または開始後
    ///
    pub(crate) fn sent(&self) -> SendState { self.sent }

    ///
    /// 送信開始とstatus観測から失敗境界を導出する。
    ///
    /// # 戻り値
    /// HTTP観測の三分類。受信完了・適用有無は推測しない。
    ///
    pub(crate) fn boundary(&self) -> FailureBoundary {
        match (self.sent, self.status) {
            (SendState::NotStarted, _) => FailureBoundary::BeforeSend,
            (SendState::Started, None) => FailureBoundary::ResponseUncertain,
            (SendState::Started, Some(_)) => FailureBoundary::ResponseReceived,
        }
    }

    ///
    /// 確定HTTP statusを取得する。
    ///
    /// # 戻り値
    /// 応答未確定ならNone
    ///
    pub(crate) fn status(&self) -> Option<u16> { self.status }

    ///
    /// 呼出し時から既知のIDを取得する。
    ///
    /// # 戻り値
    /// 確定した照会ID
    ///
    pub(crate) fn known_id(&self) -> Option<&PageId> { self.known_id.as_ref() }

    ///
    /// 有限診断情報を取得する。
    ///
    /// # 戻り値
    /// 本文を含めない診断
    ///
    pub(crate) fn diagnostics(&self) -> Option<Diagnostics> {
        self.diagnostics
    }
}

impl fmt::Debug for WikiFailure {
    ///
    /// 識別子・本文・秘密値を出さず失敗事実を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WikiFailure")
            .field("operation", &self.operation).field("kind", &self.kind)
            .field("sent", &self.sent).field("status", &self.status)
            .field("diagnostics", &self.diagnostics).finish()
    }
}

impl fmt::Display for WikiFailure {
    ///
    /// 安全なDebug分類だけを診断へ渡す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for WikiFailure {}

///
/// 初回path照会だけの正常結果。
///
pub(crate) enum LookupResult { Found(PageId), NotFound, Deleted }

impl fmt::Debug for LookupResult {
    ///
    /// 取得IDを表示せず状態だけを返す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 有限状態
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Found(_) => "Found", Self::NotFound => "NotFound",
            Self::Deleted => "Deleted",
        })
    }
}

///
/// metadataのpath状態。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind { Current, LastDeleted }

///
/// 応答pathを収集しない期待pathとの関係。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathRelation { MatchesExpected, Different }

///
/// 照会IDと有限metadata。応答にid欄を要求しない。
///
pub(crate) struct PageMetadata {
    pub(super) id: PageId,
    pub(super) path_kind: PathKind,
    pub(super) relation: PathRelation,
    pub(super) deleted: bool,
    pub(super) locked: bool,
    pub(super) revision: u64,
}

impl PageMetadata {
    ///
    /// 照会に使用したIDを取得する。
    ///
    /// # 戻り値
    /// 既知のID
    ///
    pub(crate) fn id(&self) -> &PageId { &self.id }

    ///
    /// pathの有限種別を取得する。
    ///
    /// # 戻り値
    /// currentまたはlast_deleted
    ///
    pub(crate) fn path_kind(&self) -> PathKind { self.path_kind }

    ///
    /// 期待pathとの関係を取得する。
    ///
    /// # 戻り値
    /// 一致または相違
    ///
    pub(crate) fn path_relation(&self) -> PathRelation { self.relation }

    ///
    /// 削除状態を取得する。
    ///
    /// # 戻り値
    /// サーバーから確認した状態
    ///
    pub(crate) fn deleted(&self) -> bool { self.deleted }

    ///
    /// ロック状態を取得する。
    ///
    /// # 戻り値
    /// サーバーから確認した状態
    ///
    pub(crate) fn locked(&self) -> bool { self.locked }

    ///
    /// revisionを取得する。
    ///
    /// # 戻り値
    /// 検査済み整数
    ///
    pub(crate) fn revision(&self) -> u64 { self.revision }
}

impl fmt::Debug for PageMetadata {
    ///
    /// IDやpathを出さず有限状態だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PageMetadata")
            .field("path_kind", &self.path_kind)
            .field("relation", &self.relation).field("deleted", &self.deleted)
            .field("locked", &self.locked).field("revision", &self.revision)
            .finish()
    }
}
