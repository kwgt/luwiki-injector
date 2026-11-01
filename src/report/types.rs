/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 秘密値と本文を持たない報告用の識別子・結果型
//!

use std::fmt;

use super::details::{RecoveryFacts, ResultDetails};

///
/// 報告へ渡す識別情報や状態の不正。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReportError {
    InvalidIdentity,
    InvalidId,
    DuplicatePage,
    InvalidIndex,
    AlreadyRecorded,
    InconsistentRecovery,
    InconsistentStage,
    /// 成立計画と異なるページへの記録。
    InconsistentIdentity,
    /// 最終適用結果と独立警告列の矛盾。
    InconsistentWarning,
}

///
/// 絶対Wikiページパスを保持する識別子。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PagePath(String);

impl PagePath {
    ///
    /// 制御文字や危険要素のないページパスを構築する。
    ///
    /// # 引数
    /// * `value` - 絶対Wikiパス
    ///
    /// # 戻り値
    /// 安全な識別子、または値を含めないエラー
    ///
    pub(crate) fn new(value: &str) -> Result<Self, ReportError> {
        let relative = value.strip_prefix('/')
            .ok_or(ReportError::InvalidIdentity)?;
        if value != "/" { validate_relative(relative)?; }
        Ok(Self(value.to_owned()))
    }

    ///
    /// 検証済みページパスを取得する。
    ///
    /// # 戻り値
    /// 絶対ページパス
    ///
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

///
/// 入力参照の方式。URLと資格情報は保持しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind { Zip, LocalGit, HttpsGit, GitHub }

///
/// 入力方式と入力root相対のentry名だけを保持する。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceReference {
    kind: SourceKind,
    entry: String,
}

impl SourceReference {
    ///
    /// URLや本文ではなく相対entry名から入力参照を作る。
    ///
    /// # 引数
    /// * `kind` - 入力方式
    /// * `entry` - 検証済み入力root相対名
    ///
    /// # 戻り値
    /// 安全な入力識別子
    ///
    pub(crate) fn new(kind: SourceKind, entry: &str)
        -> Result<Self, ReportError>
    {
        validate_relative(entry)?;
        if entry.starts_with('/') || entry.contains("://") {
            return Err(ReportError::InvalidIdentity);
        }
        Ok(Self { kind, entry: entry.to_owned() })
    }

    ///
    /// 入力方式とentry名を取得する。
    ///
    /// # 戻り値
    /// 資格情報を含まない入力参照
    ///
    pub(crate) fn parts(&self) -> (SourceKind, &str) {
        (self.kind, &self.entry)
    }
}

///
/// ページ結果へ関連付ける二つの安全な識別子。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageIdentity {
    pub(crate) path: PagePath,
    pub(crate) source: SourceReference,
}

///
/// サーバーのULID形式のページID。ロックtokenを受け付けない。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageId(String);

impl PageId {
    ///
    /// REST APIのページID欄だけからIDを構築する。
    ///
    /// # 引数
    /// * `value` - ULID形式の確定ページID
    ///
    /// # 戻り値
    /// ID、または入力値を含めないエラー
    ///
    pub(crate) fn new(value: &str) -> Result<Self, ReportError> {
        let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        if value.len() != 26 || !matches!(value.as_bytes()[0], b'0'..=b'7')
            || !value.bytes()
                .all(|v| alphabet.contains(&v.to_ascii_uppercase()))
        {
            return Err(ReportError::InvalidId);
        }
        Ok(Self(value.to_owned()))
    }

    ///
    /// 確定したページIDを取得する。
    ///
    /// # 戻り値
    /// ページID文字列
    ///
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}

///
/// ページ処理の最後に確定した段階。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessingStage {
    Preflight, Lookup, ReadExisting, AcquireLock, Recheck, DeleteOld,
    CreateDraft, PutSource, Reconcile, ReleaseLock, Finished,
}

///
/// 適用された操作。警告付き適用でも操作種別を失わない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action { Create, Update, Recreate }

///
/// 原因を断定せず公開する失敗分類。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureCategory {
    Authentication, Permission, Conflict, Rejected, Http(u16), Transport,
    Uncertain, NotApplied, RecreationFailed, Deadline, ExecutionUnavailable,
    Logging,
    /// 共有取消による停止。
    Cancelled,
    /// 応答またはlock headerの形式不正。
    Protocol,
    /// 受信本文のUTF-8等の検証失敗。
    InvalidBody,
    /// 保管・比較等の局所I/O失敗。
    Io,
    /// worker処理の失敗。
    Worker,
    /// 所有結果等の内部不整合。
    Internal,
    /// 期限切れ等により使用できないlock。
    LockUnavailable,
}

///
/// 再照合で確定した状態。Unknownは未確定を表す。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedState { Applied, NotApplied, Deleted, Present, Unknown }

///
/// 操作警告のHTTP応答観測。statusなしで切断原因を断定しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseCondition {
    /// 観測済みのHTTP status。
    Http(u16),
    /// status未観測。元のkindと送信状態は詳細に保持する。
    ResponseUncertain,
}

///
/// API要求の種類。生の要求やヘッダーを保持しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation { Lookup, Post, Put, Delete, Lock, Unlock }

///
/// 事実だけを保持する操作警告。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OperationWarning {
    /// 観測した操作の表示分類。
    pub(crate) operation: Operation,
    /// HTTP応答の観測状態。
    pub(crate) response: ResponseCondition,
    /// callerが独立に確認した状態。
    pub(crate) observed: ObservedState,
    /// 元の詳細操作・kind・送信/応答の事実。合成警告では未指定も許す。
    pub(crate) detail: Option<super::details::FailureDetail>,
}

///
/// 排他的なページ別結果。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageOutcome {
    Created,
    SkippedIdentical,
    Updated,
    Recreated,
    /// 累積操作警告を独立列へ保持した適用済み結果。
    AppliedWithWarning {
        /// 最終的に適用した操作。
        action: Action,
    },
    Failed { category: FailureCategory, stage: ProcessingStage },
    NotExecuted,
}

///
/// 利用者へ示す手動復旧方針。自動再送は指示しない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryAdvice {
    InspectBeforeRetry, RecreateManually, InspectDraft, ResolveConflict,
}

///
/// 確定識別情報と段階だけを保持する復旧情報。
///
pub(crate) struct RecoveryInfo {
    /// 成立計画と同じ安全な識別情報。
    pub(crate) identity: PageIdentity,
    /// 旧新ID・revision・削除/lock/path観測を所有する。
    pub(crate) facts: RecoveryFacts,
    /// 元原因の段階。最後の試行段階とは別。
    pub(crate) stage: ProcessingStage,
    /// callerが確認した状態。
    pub(crate) observed: ObservedState,
    /// 手動確認が必要な場合だけの有限方針。
    pub(crate) advice: Option<RecoveryAdvice>,
}

impl fmt::Debug for RecoveryInfo {
    ///
    /// 識別子を隠し有限状態だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 秘密・本文・ID/pathを含まない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryInfo").field("stage", &self.stage)
            .field("observed", &self.observed).finish()
    }
}

///
/// 結果・累積警告・主/補助/解除失敗を一体で所有するページ記録。
///
pub(crate) struct PageResult {
    /// 成立計画の識別子。
    pub(crate) identity: PageIdentity,
    /// 排他的なページ適用結果。
    pub(crate) outcome: PageOutcome,
    /// 最後に試行した段階。
    pub(crate) stage: ProcessingStage,
    /// 正常結果の識別事実も失わない復旧/確認情報。
    pub(crate) recovery: Option<RecoveryInfo>,
    /// ページ結果と独立した警告・失敗・安全停止。
    pub(crate) details: ResultDetails,
}

impl fmt::Debug for PageResult {
    ///
    /// 識別情報を隠し適用結果と有限段階だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 本文・path・IDを含まない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageResult").field("outcome", &self.outcome)
            .field("stage", &self.stage).field("details", &self.details)
            .finish()
    }
}

///
/// 表示可能なパス識別子の禁止要素を検証する。
///
/// # 引数
/// * `value` - 相対部分
///
/// # 戻り値
/// 禁止要素がなければ成功
///
fn validate_relative(value: &str) -> Result<(), ReportError> {
    if value.is_empty() || value.contains('\\') || value.contains('\u{fffd}')
        || value.chars().any(|v| v.is_whitespace() || v.is_control())
        || value.split('/').any(|v| matches!(v, "" | "." | ".."))
    {
        return Err(ReportError::InvalidIdentity);
    }
    Ok(())
}
