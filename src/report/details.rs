/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! HTTP/leaseの内部型へ依存せず元事実と復旧情報を保持する報告型
//!

use std::fmt;

use super::types::{FailureCategory, ObservedState, OperationWarning, PageId,
    PageOutcome, PageResult, ProcessingStage};

///
/// 単発Wiki操作と局所処理の区別。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FailureOrigin {
    /// 単発Wiki操作の観測。
    Wiki,
    /// HTTP応答へ偽装しない局所処理。
    Local,
}

///
/// 元の詳細操作を安全な有限値として保持する。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailOperation {
    /// client初期化。
    Initialize,
    /// path照会。
    Lookup,
    /// 本文取得。
    ReadSource,
    /// metadata取得。
    ReadMeta,
    /// draft作成。
    CreateDraft,
    /// lock取得。
    AcquireLock,
    /// lock延長。
    RenewLock,
    /// lock解除。
    ReleaseLock,
    /// 本文送信。
    PutSource,
    /// 旧ページ削除。
    DeletePage,
}

///
/// 原文を保持しない単発/局所の詳細分類。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailKind {
    /// context不一致。
    ContextMismatch,
    /// 要求準備失敗。
    Preparation,
    /// 認証情報不正。
    Authentication,
    /// 確定HTTP status。
    Http,
    /// 接続または受信の通信失敗。切断原因は断定しない。
    Transport,
    /// 応答形式不正。
    Protocol,
    /// 不正UTF-8。
    BodyUtf8,
    /// 禁止された置換文字。
    BodyReplacement,
    /// 局所I/O失敗。
    Io,
    /// worker失敗。
    Worker,
    /// 同じ期限への到達。
    Deadline,
    /// 共有取消。
    Cancelled,
    /// lock header不正。
    LockHeader,
    /// lock期限切れ。
    LockExpired,
    /// 使用不可lease。
    LeaseUnavailable,
    /// client/ID結合不一致。
    LeaseMismatch,
    /// 成立計画の位置不正。
    PlanPosition,
    /// 計画本文open失敗。
    PlanOpen,
    /// 受信保管open失敗。
    ReceivedOpen,
    /// 本文比較I/O失敗。
    CompareIo,
    /// 内部結果不整合。
    ResultInvariant,
    /// 観測した競合。
    Conflict,
}

///
/// 実送信bytesを推測しない要求開始状態。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestState {
    /// send開始前。
    NotStarted,
    /// send futureを開始した。
    Started,
}

///
/// 検証済みfront matter診断の有限分類。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticKind {
    /// 構文不正。
    Syntax,
    /// 意味検証不正。
    Validation,
    /// 未分類。
    Unknown,
}

///
/// 本文/property pathを含めない数値診断。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SafeDiagnostics {
    /// 有限分類。
    pub(crate) kind: DiagnosticKind,
    /// 元の検証済み行番号。
    pub(crate) line: Option<u64>,
    /// 元の検証済み桁番号。
    pub(crate) column: Option<u64>,
}

///
/// 主/補助/解除のどれにも使える秘密を含まない元事実。
///
#[derive(PartialEq, Eq)]
pub(crate) struct FailureDetail {
    /// Wikiと局所の出自。
    pub(crate) origin: FailureOrigin,
    /// Wikiの場合の詳細操作。局所ではNone。
    pub(crate) operation: Option<DetailOperation>,
    /// 元kindを失わない分類。
    pub(crate) kind: DetailKind,
    /// callerの明示段階。旧先行事実の未指定段階は補完しない。
    pub(crate) stage: Option<ProcessingStage>,
    /// 元send開始状態。局所ではNone。
    pub(crate) sent: Option<RequestState>,
    /// 元status。未観測を推測しない。
    pub(crate) status: Option<u16>,
    /// 元操作で確認済みのID。Debugでは表示しない。
    pub(crate) known_id: Option<PageId>,
    /// 任意の安全な診断。
    pub(crate) diagnostics: Option<SafeDiagnostics>,
    /// 受信保管open等の元cause。新しいHTTPを意味しない。
    pub(crate) cause: Option<Box<FailureDetail>>,
}

impl FailureDetail {
    ///
    /// 元kindと応答観測から最終表示用の大分類を選ぶ。
    ///
    /// # 概要
    /// 変更の不確定と局所障害を分け、送信前失敗を適用不明にしない。
    ///
    /// # 戻り値
    /// 元詳細を保持したまま利用する分類
    ///
    pub(crate) fn category(&self) -> FailureCategory {
        /*
         * 停止分類をHTTP statusより先に保つ
         */
        match self.kind {
            DetailKind::Deadline => return FailureCategory::Deadline,
            DetailKind::Cancelled => return FailureCategory::Cancelled,
            _ => {}
        }
        let mutation = matches!(self.operation,
            Some(DetailOperation::CreateDraft | DetailOperation::PutSource
                | DetailOperation::DeletePage));
        if mutation && self.sent == Some(RequestState::Started)
            && matches!(self.status, None | Some(500) | Some(200..=299))
        { return FailureCategory::Uncertain; }
        /*
         * 大分類だけで元protocol/I/O等を消さず詳細を併記する
         */
        match self.kind {
            /*
             * HTTP以外の局所・形式分類を維持する
             */
            DetailKind::Authentication => FailureCategory::Authentication,
            DetailKind::Conflict => FailureCategory::Conflict,
            DetailKind::Transport => FailureCategory::Transport,
            DetailKind::Protocol | DetailKind::LockHeader =>
                FailureCategory::Protocol,
            DetailKind::BodyUtf8 | DetailKind::BodyReplacement =>
                FailureCategory::InvalidBody,
            DetailKind::Io | DetailKind::PlanOpen | DetailKind::ReceivedOpen
                | DetailKind::CompareIo => FailureCategory::Io,
            DetailKind::Worker => FailureCategory::Worker,
            DetailKind::LockExpired | DetailKind::LeaseUnavailable =>
                FailureCategory::LockUnavailable,
            /*
             * 確定HTTP分類だけをstatusから選ぶ
             */
            DetailKind::Http => match self.status {
                Some(400) => FailureCategory::Rejected,
                Some(401) => FailureCategory::Authentication,
                Some(403) => FailureCategory::Permission,
                Some(409 | 423) => FailureCategory::Conflict,
                Some(status) => FailureCategory::Http(status),
                None => FailureCategory::Internal,
            },
            _ => FailureCategory::Internal,
        }
    }
}

impl fmt::Debug for FailureDetail {
    ///
    /// 識別子を隠し元の有限分類/段階だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// ID/本文/任意errorを含めない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailureDetail").field("origin", &self.origin)
            .field("operation", &self.operation).field("kind", &self.kind)
            .field("stage", &self.stage).field("sent", &self.sent)
            .field("status", &self.status)
            .field("diagnostics", &self.diagnostics)
            .field("cause", &self.cause).finish()
    }
}

///
/// path照会の履歴。現時点のpath状態とは区別する。
///
#[derive(Default)]
pub(crate) enum PathObservation {
    /// 未観測。
    #[default]
    Unknown,
    /// path404。
    Absent,
    /// path410。
    Deleted,
    /// 照会時点の有効ID。
    Found(
        /// 照会応答で確認したID。
        PageId,
    ),
}

impl fmt::Debug for PathObservation {
    ///
    /// 照会IDを隠し有限な観測状態だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// IDを含まない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "Unknown", Self::Absent => "Absent",
            Self::Deleted => "Deleted", Self::Found(_) => "Found",
        })
    }
}

///
/// 完全に確認した識別子と、未確認を区別する復旧事実。
///
#[derive(Default)]
pub(crate) struct RecoveryFacts {
    /// 元の確定ID。
    pub(crate) old_id: Option<PageId>,
    /// 元metadataのrevision。
    pub(crate) old_revision: Option<u64>,
    /// 今回のPOST等で確認した新ID。
    pub(crate) draft_id: Option<PageId>,
    /// falseは削除未確認。
    pub(crate) old_delete_confirmed: bool,
    /// 旧lock未観測はNone。
    pub(crate) old_lock_observed: Option<bool>,
    /// 最後の再照合対象のlock観測。自己保持の証明ではない。
    pub(crate) lock_observed: Option<bool>,
    /// 最後のpath照会履歴。現在状態の保証ではない。
    pub(crate) path_observed: PathObservation,
}

impl RecoveryFacts {
    ///
    /// 未実行結果が観測事実を持たないか確認する。
    ///
    /// # 戻り値
    /// 識別子・観測の全項目が未確認ならtrue
    ///
    pub(crate) fn is_empty(&self) -> bool {
        self.old_id.is_none() && self.old_revision.is_none()
            && self.draft_id.is_none() && !self.old_delete_confirmed
            && self.old_lock_observed.is_none() && self.lock_observed.is_none()
            && matches!(self.path_observed, PathObservation::Unknown)
    }
}

impl fmt::Debug for RecoveryFacts {
    ///
    /// IDとrevisionを隠し観測の有無だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 秘密と識別子を含めない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryFacts")
            .field("old_delete_confirmed", &self.old_delete_confirmed)
            .field("old_lock_observed", &self.old_lock_observed).finish()
    }
}

///
/// 適用確認後も後続を止める有限な規定理由。
///
#[derive(Debug)]
pub(crate) enum SafetyStop {
    /// 再作成PUTの応答不確定。
    RecreationUncertain,
}

///
/// 最終結果と独立した警告・元失敗・安全停止。
///
#[derive(Debug, Default)]
pub(crate) struct ResultDetails {
    /// 発生順の操作警告。最終Failedにも残す。
    pub(crate) warnings: Vec<OperationWarning>,
    /// 最終主失敗。
    pub(crate) primary: Option<FailureDetail>,
    /// 再照合等の補助失敗とadapter対象外の元事実。
    pub(crate) auxiliary: Vec<FailureDetail>,
    /// 独立した解除失敗。
    pub(crate) cleanup: Option<FailureDetail>,
    /// client解除を呼んだか。HTTP実送信とは別。
    pub(crate) cleanup_attempted: bool,
    /// 目的状態確認とは独立した停止理由。
    pub(crate) stop: Option<SafetyStop>,
}

///
/// 計画位置が確定できなかった元payloadを失わない受け手。
///
#[derive(Debug)]
pub(crate) struct UnassignedExecution {
    /// 元の適用結果。
    pub(crate) outcome: PageOutcome,
    /// 元の最後段階。
    pub(crate) stage: ProcessingStage,
    /// 旧新ID等の確定情報。
    pub(crate) facts: RecoveryFacts,
    /// 元callerの独立した観測。
    pub(crate) observed: ObservedState,
    /// 警告と主/補助/解除失敗。
    pub(crate) details: ResultDetails,
}

///
/// 二重記録や不整合で拒否された元結果を保持する。
///
#[derive(Debug)]
pub(crate) enum RejectedExecution {
    /// Reportへの不整合な記録要求。元ページを消さない。
    Page {
        /// 拒否された記録位置。
        index: usize,
        /// 破棄しない元のページ結果。
        page: PageResult,
    },
    /// Executor側で計画位置が確定しなかった結果。
    Unassigned(
        /// 計画位置へ結び付けない元payload。
        UnassignedExecution,
    ),
}
