/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 最終Reportへ投影せず実行結果と元の失敗事実を所有する受け手
//!

use std::fmt;

use crate::projection::ProjectionPlan;
use crate::report::types::{ObservedState, PageId, PageIdentity,
    ProcessingStage};
use crate::source::SourceError;
use crate::wiki_client::adapter::{ExecutionFailure, OperationEvidence};
use crate::wiki_client::{LookupResult, WikiFailure};

///
/// 外部応答を偽装しない実行側の有限な局所失敗分類。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalKind {
    /// 設定と共有contextの絶対期限が異なる。
    ContextMismatch,
    /// 成立計画に存在しない位置を要求した。
    PlanPosition,
    /// 成立計画のreadonly本文を開けなかった。
    PlanOpen,
    /// 受信済み本文のreadonly保管を開けなかった。
    ReceivedOpen,
    /// 比較中の読込または実量整合検査に失敗した。
    CompareIo,
    /// blocking workerが正常に終了しなかった。
    Worker,
    /// 同じ絶対期限へ到達した。
    Deadline,
    /// 共有取消へ到達した。
    Cancelled,
    /// 二重記録または結果の段階整合に違反した。
    ResultInvariant,
    /// 既存拒否またはページ/lock期待状態の不整合を確認した。
    Conflict,
}

///
/// 本文と秘密を含まない局所失敗と順序付き先行事実。
///
pub(crate) struct LocalFailure {
    /// 固定の局所分類。
    kind: LocalKind,
    /// 呼出側が明示した主失敗段階。
    stage: ProcessingStage,
    /// status未観測も失わない先行操作事実。
    evidence: Vec<OperationEvidence>,
    /// 既存保管openが返した元事実。新規HTTPを意味しない。
    received_open_cause: Option<WikiFailure>,
}

impl LocalFailure {
    ///
    /// 固定分類とcaller段階から局所失敗を作る。
    ///
    /// # 引数
    /// * `kind` - ReceivedOpen以外の局所分類
    /// * `stage` - callerが確定した段階
    /// * `evidence` - 複製せず所有する先行事実
    ///
    /// # 戻り値
    /// 元HTTP失敗を持たない局所失敗
    ///
    pub(crate) fn new(kind: LocalKind, stage: ProcessingStage,
        evidence: Vec<OperationEvidence>) -> Self
    {
        let kind = if kind == LocalKind::ReceivedOpen {
            LocalKind::ResultInvariant
        } else { kind };
        Self { kind, stage, evidence, received_open_cause: None }
    }

    ///
    /// 受信済み保管openの元事実をそのまま保持する。
    ///
    /// # 引数
    /// * `cause` - 実際のReceivedBody.open失敗
    /// * `stage` - caller段階
    ///
    /// # 戻り値
    /// 新規HTTPへ数えない保管失敗
    ///
    pub(super) fn received_open(cause: WikiFailure,
        stage: ProcessingStage) -> Self
    {
        Self { kind: LocalKind::ReceivedOpen, stage, evidence: Vec::new(),
            received_open_cause: Some(cause) }
    }

    ///
    /// 安全な局所分類を取得する。
    ///
    /// # 戻り値
    /// 値を含まない分類
    ///
    pub(crate) fn kind(&self) -> LocalKind { self.kind }

    ///
    /// 主失敗のcaller段階を取得する。
    ///
    /// # 戻り値
    /// 原因段階
    ///
    pub(crate) fn stage(&self) -> ProcessingStage { self.stage }

    ///
    /// 元の先行操作事実を参照する。
    ///
    /// # 戻り値
    /// statusなしを含む発生順の列
    ///
    pub(crate) fn evidence(&self) -> &[OperationEvidence] { &self.evidence }

    ///
    /// 受信済み保管openの補助事実を参照する。
    ///
    /// # 戻り値
    /// 保管openの場合だけ元事実
    ///
    pub(crate) fn received_open_cause(&self) -> Option<&WikiFailure> {
        self.received_open_cause.as_ref()
    }
}

impl fmt::Debug for LocalFailure {
    ///
    /// 元IDや本文を表示せず局所分類と段階だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalFailure").field("kind", &self.kind)
            .field("stage", &self.stage)
            .field("evidence_count", &self.evidence.len()).finish()
    }
}

///
/// 局所失敗と元の単発Wiki失敗を混同しない所有型。
///
#[derive(Debug)]
pub(crate) enum ExecutorFailure {
    /// 元の単発失敗・caller段階・先行事実。
    Wiki(ExecutionFailure),
    /// HTTP失敗を作り直さない局所失敗。
    Local(LocalFailure),
}

impl ExecutorFailure {
    ///
    /// evidenceなしの局所失敗を作る。
    ///
    /// # 引数
    /// * `kind` - 値を含まない分類
    /// * `stage` - caller段階
    ///
    /// # 戻り値
    /// 局所失敗
    ///
    pub(super) fn local(kind: LocalKind, stage: ProcessingStage) -> Self {
        Self::Local(LocalFailure::new(kind, stage, Vec::new()))
    }

    ///
    /// Sourceの停止だけを局所境界へ写す。
    ///
    /// # 引数
    /// * `error` - checkpointの有限失敗
    /// * `stage` - caller段階
    ///
    /// # 戻り値
    /// 元の期限/取消を保持する局所失敗
    ///
    pub(super) fn stopped(error: SourceError,
        stage: ProcessingStage) -> Self
    {
        let kind = match error {
            SourceError::Deadline => LocalKind::Deadline,
            SourceError::Cancelled => LocalKind::Cancelled,
            _ => LocalKind::ResultInvariant,
        };
        Self::local(kind, stage)
    }

    ///
    /// 主失敗の段階を取得する。
    ///
    /// # 戻り値
    /// 単発operationから推測しないcaller段階
    ///
    pub(crate) fn stage(&self) -> ProcessingStage {
        match self {
            Self::Wiki(failure) => failure.stage(),
            Self::Local(failure) => failure.stage(),
        }
    }

    ///
    /// 先行操作事実を投影せず取得する。
    ///
    /// # 戻り値
    /// statusなしも維持した発生順の列
    ///
    pub(crate) fn evidence(&self) -> &[OperationEvidence] {
        match self {
            Self::Wiki(failure) => failure.warnings(),
            Self::Local(failure) => failure.evidence(),
        }
    }

    ///
    /// callerの先行事実を元失敗・causeを保ってmove結合する。
    ///
    /// # 概要
    /// 局所失敗へ順序列を結合し、Wikiの場合は両所有を返して消さない。
    ///
    /// # 引数
    /// * `prior` - この失敗より前に発生した元事実
    ///
    /// # 戻り値
    /// 局所失敗は結合済み。Wikiは元失敗と未結合列を両方返す。
    ///
    pub(crate) fn with_prior_evidence(self,
        mut prior: Vec<OperationEvidence>)
        -> Result<Self, (Self, Vec<OperationEvidence>)>
    {
        /*
         * 局所失敗は補助causeを残し先行順の列を結合する
         */
        match self {
            Self::Local(mut local) => {
                prior.append(&mut local.evidence);
                local.evidence = prior;
                Ok(Self::Local(local))
            }
            Self::Wiki(failure) => {
                // Wiki列はadapter生成前に結合し、ここでは両所有を返す。
                Err((Self::Wiki(failure), prior))
            }
        }
    }
}

impl fmt::Display for ExecutorFailure {
    ///
    /// 安全なDebugと同じ有限表示を返す。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 秘密と本文を含まない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

///
/// 通常処理が確定させる排他的な完了種別。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// 新規本文を作成した。
    Created,
    /// bytes一致で変更を省略した。
    SkippedIdentical,
    /// 既存本文を更新した。
    Updated,
    /// 旧削除後に新IDで再作成した。
    Recreated,
}

///
/// 判明した識別情報だけを持ち、候補IDを補わない。
///
#[derive(Default)]
pub(crate) struct KnownPageFacts {
    /// 元の確定ID。
    pub(super) old_id: Option<PageId>,
    /// 元metadataの確定revision。
    pub(super) old_revision: Option<u64>,
    /// 完全POST等から確認した新ID。
    pub(super) new_id: Option<PageId>,
    /// 正常DELETEまたは旧ID metadataによる削除確認。falseは未確認。
    pub(super) old_delete_confirmed: bool,
    /// lock後metadataの観測。Noneは未観測でfalseに補完しない。
    pub(super) old_lock_observed: Option<bool>,
}

impl KnownPageFacts {
    ///
    /// 正常DELETEまたは旧ID照合で削除を確認したかを取得する。
    ///
    /// # 戻り値
    /// falseは未確認で、未削除の証明ではない。
    ///
    pub(crate) fn old_delete_confirmed(&self) -> bool {
        self.old_delete_confirmed
    }

    ///
    /// lock後metadataのlocked観測を取得する。
    ///
    /// # 戻り値
    /// Noneは未観測でfalseに補完しない。
    ///
    pub(crate) fn old_lock_observed(&self) -> Option<bool> {
        self.old_lock_observed
    }

    ///
    /// 識別情報を最終受け手へmoveする。
    ///
    /// # 戻り値
    /// 旧ID・旧revision・新ID・削除確認・lock観測。未確認を補完しない。
    ///
    pub(crate) fn into_parts(self)
        -> (Option<PageId>, Option<u64>, Option<PageId>, bool, Option<bool>)
    {
        (self.old_id, self.old_revision, self.new_id, self.old_delete_confirmed,
            self.old_lock_observed)
    }
}

///
/// 成功と失敗を失わず未実行とも区別する結果。
///
#[derive(Debug)]
pub(crate) enum PageExecution {
    /// 通常処理の確定完了。
    Completed(Completion),
    /// 目的状態を確認したが元操作の異常を保持する完了。
    AppliedWithWarning(Completion),
    /// 元事実を所有する主失敗。
    Failed(ExecutorFailure),
    /// このページの処理を開始していない。
    NotExecuted,
}

///
/// 適用結果とは独立した次ページの進行可否。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageFlow {
    /// 規定の通常処理を続けられる。
    Continue,
    /// 目的状態が確認できても後続を開始しない。
    Stop,
}

///
/// 応答不確定な再作成を目的状態確認だけで継続させない理由。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// 再作成PUTの応答状態が確定しなかった。
    RecreationUncertain,
}

///
/// 元の操作事実とcaller段階を組にして所有する警告記録。
///
#[derive(Debug)]
pub(crate) struct OperationRecord {
    /// callerが確定した段階。旧adapter列の未記録段階は補完しない。
    pub(crate) stage: Option<ProcessingStage>,
    /// 詳細操作・既知ID・statusなしも残る元の事実。
    pub(crate) evidence: OperationEvidence,
}

impl OperationRecord {
    ///
    /// 元の単発失敗事実を参照する。
    ///
    /// # 戻り値
    /// 詳細操作・kind・status・確定ID
    ///
    pub(crate) fn facts(&self) -> &WikiFailure { self.evidence.facts() }

    ///
    /// callerが確認した状態を取得する。
    ///
    /// # 戻り値
    /// 応答コードから補完しない観測
    ///
    pub(crate) fn observed(&self) -> ObservedState {
        self.evidence.observed()
    }

    ///
    /// 確定statusだけを既存Report警告へ写す。
    ///
    /// # 戻り値
    /// statusなしはNoneでも元事実は本記録に残る。
    ///
    pub(crate) fn warning(&self)
        -> Option<crate::report::types::OperationWarning>
    {
        self.evidence.warning()
    }
}

///
/// 元操作と再照合の観測を結果とは独立して所有する。
///
pub(crate) struct PageDetails {
    /// 元操作の異常と独立した観測を持つ発生順の警告。
    pub(crate) warnings: Vec<OperationRecord>,
    /// adapterの警告対象外でも消してはならない元失敗。
    pub(crate) retained: Vec<ExecutorFailure>,
    /// 主原因を置き換えない再照合の補助失敗。
    pub(crate) auxiliary: Vec<ExecutorFailure>,
    /// 最後まで確認した適用状態。初期値は未確認。
    pub(crate) observed: ObservedState,
    /// 再照合対象IDのlock観測。新draftと旧ページを混ぜない。
    pub(crate) lock_observed: Option<bool>,
    /// 最後に成功したpath照会の履歴。再作成後の現在状態ではない。
    pub(crate) path_observed: Option<LookupResult>,
    /// 再照合の最後の試行段階。HTTP実送信とは別。
    pub(crate) last_stage: Option<ProcessingStage>,
    /// 適用確認後も後続を止める独立した理由。
    pub(crate) stop: Option<StopReason>,
}

impl Default for PageDetails {
    ///
    /// 未確認・警告なしの受け手を準備する。
    ///
    /// # 戻り値
    /// 元事実を補完しない初期状態
    ///
    fn default() -> Self {
        Self { warnings: Vec::new(), retained: Vec::new(),
            auxiliary: Vec::new(), observed: ObservedState::Unknown,
            lock_observed: None, path_observed: None,
            last_stage: None, stop: None }
    }
}

impl fmt::Debug for PageDetails {
    ///
    /// 事実の件数と有限状態だけを表示する。
    ///
    /// # 引数
    /// * `f` - 出力先
    ///
    /// # 戻り値
    /// 本文・識別子・秘密を含まない表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageDetails").field("observed", &self.observed)
            .field("warnings", &self.warnings.len())
            .field("retained", &self.retained.len())
            .field("auxiliary", &self.auxiliary.len())
            .field("stop", &self.stop).finish()
    }
}

///
/// processorが一度だけ順序driverへ渡すページ処理結果。
///
pub(super) struct PageStep {
    /// 確定結果または主失敗。
    execution: PageExecution,
    /// 秘密を含まない判明情報。
    known: KnownPageFacts,
    /// 独立した解除失敗。通常処理では最大一回。
    cleanup: Option<ExecutionFailure>,
    /// client解除関数を呼んだ事実。HTTP実送信とは別。
    cleanup_attempted: bool,
    /// 再照合の観測・警告・停止理由。
    details: PageDetails,
}

impl PageStep {
    ///
    /// 主原因を保って解放前停止等の補助失敗を記録する。
    ///
    /// # 引数
    /// * `failure` - 別操作の前に確認した有限失敗
    ///
    /// # 戻り値
    /// 元結果と補助原因を別所有する一件
    ///
    pub(super) fn with_auxiliary(mut self, failure: ExecutorFailure) -> Self {
        self.details.last_stage = Some(failure.stage());
        self.details.auxiliary.push(failure);
        self
    }

    ///
    /// 再作成前の警告と部分観測を後続結果へ発生順で引き継ぐ。
    ///
    /// # 概要
    /// 後続失敗/取消でも先行DELETE事実を消さず成功操作は一度数える。
    ///
    /// # 引数
    /// * `prior` - 再作成前までの所有済み警告・観測
    ///
    /// # 戻り値
    /// 元の主/解除失敗と先行警告を保持する結果
    ///
    pub(super) fn with_prior_details(mut self, mut prior: PageDetails)
        -> Self
    {
        /*
         * 先行DELETEと後続PUTの事実を順序を変えず結合する
         */
        prior.warnings.append(&mut self.details.warnings);
        prior.retained.append(&mut self.details.retained);
        prior.auxiliary.append(&mut self.details.auxiliary);
        self.details.warnings = prior.warnings;
        self.details.retained = prior.retained;
        self.details.auxiliary = prior.auxiliary;
        if self.details.path_observed.is_none() {
            self.details.path_observed = prior.path_observed;
        }
        self.details.stop = self.details.stop.or(prior.stop);
        /*
         * 正常再作成も先行警告があれば警告付き適用とする
         */
        if let PageExecution::Completed(completion) = &self.execution {
            if !self.details.warnings.is_empty()
                || !self.details.retained.is_empty()
            {
                self.details.observed = ObservedState::Applied;
                self.execution = PageExecution::AppliedWithWarning(*completion);
            }
        } else if self.details.last_stage.is_none() {
            self.details.observed = prior.observed;
        }
        self
    }

    ///
    /// 再照合の所有情報を失わず一件へ結び付ける。
    ///
    /// # 引数
    /// * `details` - 元警告・補助失敗・観測・停止理由
    ///
    /// # 戻り値
    /// 独立した再照合情報を持つ結果
    ///
    pub(super) fn with_details(mut self, details: PageDetails) -> Self {
        self.details = details;
        self
    }

    ///
    /// 目的状態確認済みの適用を警告付き結果へ変える。
    ///
    /// # 引数
    /// * `completion` - 確認した適用操作
    /// * `known` - 確定識別情報
    ///
    /// # 戻り値
    /// 警告はwith_detailsで別所有する結果
    ///
    pub(super) fn applied(completion: Completion,
        known: KnownPageFacts) -> Self
    {
        let mut step = Self::completed(completion, known);
        step.execution = PageExecution::AppliedWithWarning(completion);
        step
    }

    ///
    /// 主失敗と独立停止理由の双方から後続開始を判断する。
    ///
    /// # 戻り値
    /// 後続を開始できるか
    ///
    pub(super) fn flow(&self) -> PageFlow {
        if self.is_failed() || self.details.stop.is_some() { PageFlow::Stop }
        else { PageFlow::Continue }
    }

    ///
    /// 主失敗を保ち、解除関数呼出の成否を独立に加える。
    ///
    /// # 引数
    /// * `result` - client解除呼出の結果。BeforeSendも試行に数える。
    ///
    /// # 戻り値
    /// HTTP実送信と別の試行確認を持つ元失敗
    ///
    pub(super) fn with_cleanup(mut self, result: Result<(), ExecutionFailure>)
        -> Self
    {
        self.cleanup_attempted = true;
        self.cleanup = result.err();
        self
    }

    ///
    /// 結果が主失敗かを参照し、所有情報は移動しない。
    ///
    /// # 戻り値
    /// 主失敗ならtrue
    ///
    pub(super) fn is_failed(&self) -> bool {
        matches!(&self.execution, PageExecution::Failed(_))
    }
    ///
    /// 確定完了を一度の記録へまとめる。
    ///
    /// # 引数
    /// * `completion` - 実処理が確定した種別
    /// * `known` - 判明情報
    ///
    /// # 戻り値
    /// 完了結果
    ///
    pub(super) fn completed(completion: Completion,
        known: KnownPageFacts) -> Self
    {
        Self { execution: PageExecution::Completed(completion), known,
            cleanup: None, cleanup_attempted: false,
            details: PageDetails::default() }
    }

    ///
    /// 主失敗と解除失敗を別所有で記録へまとめる。
    ///
    /// # 引数
    /// * `failure` - 元段階を保持する主失敗
    /// * `known` - 判明情報
    /// * `cleanup` - 明示解除の失敗。元段階はReleaseLock
    ///
    /// # 戻り値
    /// 主失敗を上書きしない結果
    ///
    pub(super) fn failed(failure: ExecutorFailure, known: KnownPageFacts,
        cleanup: Option<ExecutionFailure>) -> Self
    {
        let cleanup_attempted = cleanup.is_some();
        Self { execution: PageExecution::Failed(failure), known, cleanup,
            cleanup_attempted, details: PageDetails::default() }
    }
}

///
/// 計画の識別子と主失敗・後始末失敗を分離したページ記録。
///
pub(crate) struct ExecutionPage {
    /// 検証済み計画の識別子。
    identity: PageIdentity,
    /// 判明したIDと旧revision。
    known: KnownPageFacts,
    /// 最後の試行段階。主失敗の段階とは別。
    last_stage: ProcessingStage,
    /// 完了・主失敗・未実行。
    execution: PageExecution,
    /// 独立した明示解放の失敗。
    cleanup_failure: Option<ExecutionFailure>,
    /// 成否/実送信と区別した解除関数の呼出確認。
    cleanup_attempted: bool,
    /// 再照合の観測・警告・停止理由。
    details: PageDetails,
}

impl ExecutionPage {
    ///
    /// 結果を最終受け手へmoveする。
    ///
    /// # 戻り値
    /// 識別子・判明情報・最後段階・主結果・解除失敗/試行・再照合情報
    ///
    pub(crate) fn into_parts(self) -> (PageIdentity, KnownPageFacts,
        ProcessingStage, PageExecution, Option<ExecutionFailure>, bool,
        PageDetails)
    {
        (self.identity, self.known, self.last_stage, self.execution,
            self.cleanup_failure, self.cleanup_attempted, self.details)
    }

    ///
    /// 投影前の結果を参照する。
    ///
    /// # 戻り値
    /// 元の完了または失敗
    ///
    pub(crate) fn execution(&self) -> &PageExecution { &self.execution }
}

impl fmt::Debug for ExecutionPage {
    ///
    /// 識別情報を表示せず段階と安全な結果だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionPage").field("stage", &self.last_stage)
            .field("execution", &self.execution)
            .field("cleanup_failure", &self.cleanup_failure).finish()
    }
}

///
/// 確定計画順の先行結果と最初の失敗・後続未実行を所有する。
///
pub(crate) struct ExecutionBatch {
    /// 並べ替えないページ列。
    pages: Vec<ExecutionPage>,
    /// 主失敗が発生した最初の位置。
    first_failure: Option<usize>,
    /// 主失敗と区別した安全停止位置。
    safety_stop: Option<usize>,
    /// ページ開始前の共有停止または内部整合失敗。
    pre_page_failure: Option<ExecutorFailure>,
    /// 次に一度だけ記録可能な計画位置。
    next: usize,
    /// 内部整合で拒否した元payload。既存結果を上書きしない。
    rejected_steps: Vec<PageStep>,
}

impl ExecutionBatch {
    ///
    /// 全件検証済み計画だけから未実行列を準備する。
    ///
    /// # 引数
    /// * `plan` - 確定計画
    ///
    /// # 戻り値
    /// HTTPも本文読込も行わない結果受け手
    ///
    pub(super) fn new(plan: &ProjectionPlan) -> Self {
        let pages = plan.pages().iter().map(|page| ExecutionPage {
            identity: page.identity().clone(), known: KnownPageFacts::default(),
            last_stage: ProcessingStage::Preflight,
            execution: PageExecution::NotExecuted, cleanup_failure: None,
            cleanup_attempted: false, details: PageDetails::default(),
        }).collect();
        Self { pages, first_failure: None, safety_stop: None,
            pre_page_failure: None, next: 0,
            rejected_steps: Vec::new() }
    }

    ///
    /// 順序と二重記録・解除段階を検査して一件だけ記録する。
    ///
    /// # 概要
    /// 不正結果は既存の確定結果を上書きせず拒否する。
    ///
    /// # 引数
    /// * `index` - 次の計画位置
    /// * `step` - 一度だけmoveする結果
    ///
    /// # 戻り値
    /// 記録成功、または安全な内部整合失敗
    ///
    pub(super) fn record(&mut self, index: usize, step: PageStep)
        -> Result<(), ExecutorFailure>
    {
        /*
         * 主失敗後の続行・二重記録・無効な解除段階を拒否する
         */
        if index != self.next || index >= self.pages.len()
            || self.first_failure.is_some() || self.pre_page_failure.is_some()
            || self.safety_stop.is_some()
            || matches!(&step.execution, PageExecution::NotExecuted)
            || step.cleanup.as_ref().is_some_and(|failure|
                failure.stage() != ProcessingStage::ReleaseLock)
            || (matches!(&step.execution,
                PageExecution::AppliedWithWarning(_))
                && (step.details.observed != ObservedState::Applied
                    || (step.details.warnings.is_empty()
                        && step.details.retained.is_empty())))
            || (step.details.stop.is_some() && !matches!(&step.execution,
                PageExecution::AppliedWithWarning(_)))
            || (step.cleanup.is_some() && !step.cleanup_attempted)
            || (step.cleanup_attempted
                && !matches!(&step.execution, PageExecution::Failed(_)))
        {
            self.rejected_steps.push(step);
            return Err(ExecutorFailure::local(LocalKind::ResultInvariant,
                ProcessingStage::Preflight));
        }
        /*
         * 主原因と最後の解除試行を分けて所有情報を確定する
         */
        let stage = match &step.execution {
            PageExecution::Completed(_) | PageExecution::AppliedWithWarning(_)
                => ProcessingStage::Finished,
            PageExecution::Failed(failure) => failure.stage(),
            PageExecution::NotExecuted => unreachable!("checked result"),
        };
        if matches!(&step.execution, PageExecution::Failed(_)) {
            self.first_failure = Some(index);
        }
        if step.details.stop.is_some() { self.safety_stop = Some(index); }
        let page = &mut self.pages[index];
        page.last_stage = if step.cleanup_attempted {
            ProcessingStage::ReleaseLock
        } else { step.details.last_stage.unwrap_or(stage) };
        page.execution = step.execution;
        page.known = step.known;
        page.cleanup_failure = step.cleanup;
        page.cleanup_attempted = step.cleanup_attempted;
        page.details = step.details;
        self.next += 1;
        Ok(())
    }

    ///
    /// 次ページを開始しない停止を先行結果と共に保持する。
    ///
    /// # 引数
    /// * `failure` - ページ開始前の停止/内部失敗
    ///
    /// # 戻り値
    /// なし。後続ページは未実行を保つ。
    ///
    pub(super) fn stop_before_page(&mut self, failure: ExecutorFailure) {
        if self.pre_page_failure.is_none() {
            self.pre_page_failure = Some(failure);
        }
    }

    ///
    /// 計画順の結果を参照する。
    ///
    /// # 戻り値
    /// 先行結果と後続未実行
    ///
    pub(crate) fn pages(&self) -> &[ExecutionPage] { &self.pages }

    ///
    /// 全結果を最終受け手へ複製せず渡す。
    ///
    /// # 戻り値
    /// ページ列・最初の主失敗位置・開始前失敗・拒否した元payload
    ///
    pub(crate) fn into_parts(self)
        -> (Vec<ExecutionPage>, Option<usize>, Option<ExecutorFailure>,
            Vec<(KnownPageFacts, PageExecution, Option<ExecutionFailure>,
                bool, PageDetails)>)
    {
        let rejected = self.rejected_steps.into_iter().map(|step|
            (step.known, step.execution, step.cleanup, step.cleanup_attempted,
                step.details))
            .collect();
        (self.pages, self.first_failure, self.pre_page_failure, rejected)
    }
}

impl fmt::Debug for ExecutionBatch {
    ///
    /// 識別子を表示せず件数と安全な開始前失敗だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionBatch").field("page_count", &self.pages.len())
            .field("pre_page_failure", &self.pre_page_failure).finish()
    }
}
