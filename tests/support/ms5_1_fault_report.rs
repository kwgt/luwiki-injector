/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51故障試験の出力を閉じた語彙へ限定して照合する
//!

use anyhow::{Result, anyhow, ensure};

use super::real_cli_http::valid_id;
use super::real_cli_report::Captured;

///
/// 元の単発操作を保持する有限詳細の期待値。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DetailExpectation {
    /// WikiまたはLocal。
    pub(crate) origin: String,
    /// 元の操作。未指定はNoneのまま比較する。
    pub(crate) operation: Option<String>,
    /// 元の有限分類。
    pub(crate) kind: String,
    /// 元の段階。未指定は補完しない。
    pub(crate) stage: Option<String>,
    /// NotStartedまたはStarted。未指定は補完しない。
    pub(crate) sent: Option<String>,
    /// 観測済みstatus。未観測はNone。
    pub(crate) status: Option<u16>,
    /// 別の明示行で出力された確認済みID。
    pub(crate) known_id: Option<String>,
    /// 有限診断分類と行・桁。
    pub(crate) diagnostics: Option<(String, Option<u64>, Option<u64>)>,
    /// 保管失敗等の元cause。
    pub(crate) cause: Option<Box<DetailExpectation>>,
}

impl DetailExpectation {
    ///
    /// Wiki操作の完全な元事実を期待する。
    ///
    /// # 引数
    /// * `operation` - 元操作
    /// * `kind` - 元分類
    /// * `stage` - 明示段階
    /// * `sent` - send開始状態
    /// * `status` - 観測status
    ///
    /// # 戻り値
    /// ID・診断・causeの追加指定が可能な期待値
    ///
    pub(crate) fn wiki(operation: &str, kind: &str, stage: Option<&str>,
        sent: Option<&str>, status: Option<u16>) -> Self
    {
        Self { origin: "Wiki".into(), operation: Some(operation.into()),
            kind: kind.into(), stage: stage.map(str::to_owned),
            sent: sent.map(str::to_owned), status, known_id: None,
            diagnostics: None, cause: None }
    }

    ///
    /// HTTPを伴わない局所処理の期待値を作る。
    ///
    /// # 引数
    /// * `kind` - 元分類
    /// * `stage` - 明示段階
    ///
    /// # 戻り値
    /// operation・sent・statusが明示Noneの期待値
    ///
    pub(crate) fn local(kind: &str, stage: Option<&str>) -> Self {
        Self { origin: "Local".into(), operation: None, kind: kind.into(),
            stage: stage.map(str::to_owned), sent: None, status: None,
            known_id: None, diagnostics: None, cause: None }
    }
}

///
/// 発生順に比較する操作警告。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WarningExpectation {
    /// 表示操作。
    pub(crate) operation: String,
    /// Noneは明示ResponseUncertain。
    pub(crate) response: Option<u16>,
    /// 再照合した有限状態。
    pub(crate) observed: String,
    /// 元詳細。存在しないことも独立して比較する。
    pub(crate) detail: Option<DetailExpectation>,
}

impl WarningExpectation {
    ///
    /// 元詳細付きの操作警告を作る。
    ///
    /// # 引数
    /// * `operation` - 表示操作
    /// * `response` - HTTP応答または明示未観測
    /// * `observed` - 再照合状態
    /// * `detail` - 元の単発詳細
    ///
    /// # 戻り値
    /// 発生順を変更せず比較できる警告
    ///
    pub(crate) fn new(operation: &str, response: Option<u16>,
        observed: &str, detail: DetailExpectation) -> Self
    {
        Self { operation: operation.into(), response,
            observed: observed.into(), detail: Some(detail) }
    }
}

///
/// cleanup呼出と安全停止を独立して比較する期待値。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ExecutionExpectation {
    /// client解除関数を呼んだ事実。
    pub(crate) cleanup_attempted: bool,
    /// trueは明示RecreationUncertain。
    pub(crate) stop: bool,
}

///
/// 未観測を補完しない旧新ID・revision・削除等の復旧期待値。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RecoveryExpectation {
    /// 確認済み旧ID。
    pub(crate) old_id: Option<String>,
    /// 確認済み旧revision。
    pub(crate) revision: Option<u64>,
    /// 確認済み作成先ID。
    pub(crate) draft_id: Option<String>,
    /// 旧削除が確定した事実。
    pub(crate) deleted: bool,
    /// 旧lockの観測。
    pub(crate) old_lock: Option<bool>,
    /// 最後の再照合lock観測。
    pub(crate) lock: Option<bool>,
    /// 過去のpath照会状態。
    pub(crate) path_state: String,
    /// path照会時だけのID。
    pub(crate) path_id: Option<String>,
    /// 主原因の段階。
    pub(crate) stage: String,
    /// 再照合状態。
    pub(crate) observed: String,
    /// 固定手動確認方針の有限名。未出力も比較する。
    pub(crate) advice: Option<String>,
}

impl RecoveryExpectation {
    ///
    /// 基本の復旧期待値を作る。
    ///
    /// # 引数
    /// * `old_id` - 確認済み旧ID
    /// * `revision` - 旧revision
    /// * `draft_id` - 確認済み新ID
    /// * `deleted` - 旧削除確認
    /// * `stage` - 原因段階
    /// * `observed` - 再照合状態
    ///
    /// # 戻り値
    /// lock/path/adviceは未観測として始まる期待値
    ///
    pub(crate) fn new(old_id: Option<&str>, revision: Option<u64>,
        draft_id: Option<&str>, deleted: bool, stage: &str,
        observed: &str) -> Self
    {
        Self { old_id: old_id.map(str::to_owned), revision,
            draft_id: draft_id.map(str::to_owned), deleted, old_lock: None,
            lock: None, path_state: "Unknown".into(), path_id: None,
            stage: stage.into(), observed: observed.into(), advice: None }
    }
}

///
/// 一ページの完全な結果期待値。識別子を診断へ表示しない。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PageExpectation {
    /// 内部照合だけに使う既知path。
    pub(crate) path: String,
    /// 内部照合だけに使う入力参照。
    pub(crate) source: String,
    /// action/categoryも含む製品の正規有限表示。
    pub(crate) outcome: String,
    /// 最後段階。
    pub(crate) stage: String,
    /// 発生順の操作警告。
    pub(crate) warnings: Vec<WarningExpectation>,
    /// 主失敗。
    pub(crate) primary: Option<DetailExpectation>,
    /// 発生順の補助失敗。
    pub(crate) auxiliary: Vec<DetailExpectation>,
    /// 独立した解除失敗。
    pub(crate) cleanup: Option<DetailExpectation>,
    /// 解除試行・停止行の有無と内容。
    pub(crate) execution: Option<ExecutionExpectation>,
    /// 復旧行の有無と全事実。
    pub(crate) recovery: Option<RecoveryExpectation>,
}

impl PageExpectation {
    ///
    /// 詳細なしの一ページ期待値を作る。
    ///
    /// # 引数
    /// * `path` - 期待path
    /// * `source` - 期待入力参照
    /// * `outcome` - 正規結果表示
    /// * `stage` - 最後段階
    ///
    /// # 戻り値
    /// 追加の詳細行はすべて未出力とした期待値
    ///
    pub(crate) fn new(path: &str, source: &str, outcome: &str,
        stage: &str) -> Self
    {
        Self { path: path.into(), source: source.into(),
            outcome: outcome.into(), stage: stage.into(), warnings: vec![],
            primary: None, auxiliary: vec![], cleanup: None,
            execution: None, recovery: None }
    }
}

///
/// 終了・計画・全七集計と全ページの完全期待値。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FaultExpectation {
    /// 実終了と表示終了の一致を要求する0～3。
    pub(crate) exit: i32,
    /// 計画成立の明示bool。
    pub(crate) planned: bool,
    /// created/skipped/updated/recreated/warnings/failed/not_executed。
    pub(crate) counts: [u64; 7],
    /// 計画順のページ結果。
    pub(crate) pages: Vec<PageExpectation>,
    /// 明示された最初の主失敗path。
    pub(crate) first_failure: Option<String>,
    /// 起動時の固定警告文。操作警告と別計上する。
    pub(crate) startup_warnings: Vec<String>,
    /// ページへ属さない固定全体診断。
    pub(crate) global_errors: Vec<String>,
    /// ページ主原因と分離した全体停止の有限詳細。
    pub(crate) global_details: Vec<DetailExpectation>,
}

impl FaultExpectation {
    ///
    /// 終了・計画・全七集計の期待値を作る。
    ///
    /// # 引数
    /// * `exit` - 製品終了コード
    /// * `planned` - 計画成立
    /// * `counts` - 定義順の七集計
    ///
    /// # 戻り値
    /// ページ・起動警告・全体診断なしの期待値
    ///
    pub(crate) fn new(exit: i32, planned: bool, counts: [u64; 7]) -> Self {
        Self { exit, planned, counts, pages: vec![], first_failure: None,
            startup_warnings: vec![], global_errors: vec![],
            global_details: vec![] }
    }

    ///
    /// 計画順を保ってページ期待値を追加する。
    ///
    /// # 引数
    /// * `page` - 完全ページ期待値
    ///
    /// # 戻り値
    /// ページ追加後の期待値
    ///
    pub(crate) fn page(mut self, page: PageExpectation) -> Self {
        self.pages.push(page);
        self
    }
}

///
/// 原文を保持せず、有限検証を通った報告だけを所有する。
///
pub(crate) struct FaultReport {
    /// 解析済みの完全事実。外部には原文を返さない。
    facts: FaultExpectation,
}

impl FaultReport {
    ///
    /// 実終了と全行をstrictに解析する。
    ///
    /// # 概要
    /// 欠落・重複・未知語彙・余分な行は固定エラーで拒否する。
    ///
    /// # 引数
    /// * `captured` - 表示しない子出力
    ///
    /// # 戻り値
    /// 検証済み有限報告。失敗にも原文を含めない。
    ///
    pub(crate) fn parse(captured: &Captured) -> Result<Self> {
        /*
         * 実終了・先頭三行を正規形として検証する
         */
        for stream in [&captured.stdout, &captured.stderr] {
            ensure!(!stream.chars().any(|v|
                v == '\u{fffd}' || (v.is_control() && v != '\n'))
                && (stream.is_empty() || stream.ends_with('\n')),
                "fault invalid stream framing");
        }
        let exit = captured.code.filter(|v| (0..=3).contains(v))
            .ok_or_else(|| anyhow!("fault report exit"))?;
        let mut lines = captured.stdout.lines();
        let header = lines.next().ok_or_else(|| anyhow!("fault header"))?;
        let planned = if header == format!(
            "result: exit={exit} plan=established") { true }
            else if header == format!(
                "result: exit={exit} plan=not-established") { false }
            else { return Err(anyhow!("fault header")); };
        let completed = number(lines.next().and_then(|v|
            v.strip_prefix("completed=")))?;
        let counts = summary(lines.next().unwrap_or(""))?;
        let total = counts[..4].iter().try_fold(0u64, |sum, count|
            sum.checked_add(*count)).ok_or_else(||
                anyhow!("fault completed overflow"))?;
        ensure!(completed == total,
            "fault completed count");
        let mut parser = Parser::new(FaultExpectation::new(exit, planned,
            counts));
        /*
         * 出力先を分けて全行を解析し、欠落・件数も照合する
         */
        for line in lines { parser.result_line(line)?; }
        for line in captured.stderr.lines() { parser.diagnostic_line(line)?; }
        parser.finish()?;
        Ok(Self { facts: parser.facts })
    }

    ///
    /// 各caseの独立期待と全有限事実を完全比較する。
    ///
    /// # 引数
    /// * `expected` - case側で組み立てた全事実
    ///
    /// # 戻り値
    /// 全件一致。差分や原文はエラーへ展開しない。
    ///
    pub(crate) fn expect(&self, expected: &FaultExpectation) -> Result<()> {
        ensure!(&self.facts == expected, "fault report expectation mismatch");
        Ok(())
    }

    ///
    /// 解析済みページを内部照合用に参照する。
    ///
    /// # 引数
    /// * `path` - 既知case path
    ///
    /// # 戻り値
    /// 有限事実。識別子の表示には使わない。
    ///
    pub(crate) fn page(&self, path: &str) -> Result<&PageExpectation> {
        self.facts.pages.iter().find(|v| v.path == path)
            .ok_or_else(|| anyhow!("fault expected page missing"))
    }

    ///
    /// path/source/本文を含めない有限記録を作る。
    ///
    /// # 概要
    /// 成否にかかわらずcase照合の前に観測を記録できる。
    ///
    /// # 戻り値
    /// 閉じた語彙と数値だけで構成した診断
    ///
    pub(crate) fn safe_record(&self) -> String {
        /*
         * 集計と有限結果を固定page番号へ写す
         */
        let mut record = format!(
            "MS51 fault exit={} plan={} counts={:?}", self.facts.exit,
            if self.facts.planned { "established" } else {
                "not-established" }, self.facts.counts);
        for (index, page) in self.facts.pages.iter().enumerate() {
            record.push_str(&format!(" page=P{:02} outcome={} stage={}",
                index + 1, page.outcome, page.stage));
            for warning in &page.warnings {
                record.push_str(&format!(
                    " warning={} response={:?} observed={}",
                    warning.operation, warning.response, warning.observed));
                if let Some(detail) = &warning.detail {
                    append_detail(&mut record, "warning", detail);
                }
            }
            /*
             * 主・補助・解除の区別と停止・復旧事実を保つ
             */
            if let Some(detail) = &page.primary {
                append_detail(&mut record, "primary", detail);
            }
            for detail in &page.auxiliary {
                append_detail(&mut record, "auxiliary", detail);
            }
            if let Some(detail) = &page.cleanup {
                append_detail(&mut record, "cleanup", detail);
            }
            if let Some(value) = &page.execution {
                record.push_str(&format!(
                    " cleanup_attempted={} safety_stop={}",
                    value.cleanup_attempted, if value.stop {
                        "RecreationUncertain" } else { "None" }));
            }
            if let Some(value) = &page.recovery {
                record.push_str(&format!(
                    " old_id={:?} revision={:?} new_id={:?} deleted={} \
                    old_lock={:?} lock={:?} path_state={} path_id={:?} \
                    recovery_stage={} observed={} advice={:?}",
                    value.old_id, value.revision, value.draft_id,
                    value.deleted, value.old_lock, value.lock,
                    value.path_state, value.path_id, value.stage,
                    value.observed, value.advice));
            }
        }
        /*
         * 主失敗位置と起動・全体診断も原pathなしで残す
         */
        let failure = self.facts.first_failure.as_ref().and_then(|path|
            self.facts.pages.iter().position(|page| &page.path == path));
        record.push_str(&format!(" first_failure={:?}",
            failure.map(|index| index + 1)));
        for warning in &self.facts.startup_warnings {
            record.push_str(if warning == STARTUP_WARNINGS[0] {
                " startup_warning=CliSecret" } else {
                " startup_warning=ConfigPermissions" });
        }
        for error in &self.facts.global_errors {
            record.push_str(&format!(" global_error={error}"));
        }
        for detail in &self.facts.global_details {
            append_detail(&mut record, "global", detail);
        }
        record
    }
}

///
/// 行の一意性と関連行の整合性を追跡する内部状態。
///
struct Parser {
    /// 出力を持たない解析済み事実。
    facts: FaultExpectation,
    /// 各ページのpath履歴・復旧状態行の既読。
    recovery_seen: Vec<[bool; 2]>,
    /// stderrのページ主失敗行の既読。
    failure_seen: Vec<bool>,
    /// stderrのページ安全停止行の既読。
    stop_seen: Vec<bool>,
    /// 明示ID行。roleとIDだけを内部保持する。
    operation_ids: Vec<Vec<(String, String)>>,
    /// 次に照合すべきstored_causeの列。
    causes: Vec<DetailExpectation>,
    /// stored_causeが属するページ。
    cause_page: Option<usize>,
    /// optionalな補助ログの既読。
    log_seen: bool,
    /// 未実行のみを知らせる固定全体診断の既読。
    incomplete_seen: bool,
}

impl Parser {
    ///
    /// 先頭集計の解析後に追跡状態を作る。
    ///
    /// # 引数
    /// * `facts` - 先頭三行の事実
    ///
    /// # 戻り値
    /// ページ未読の内部状態
    ///
    fn new(facts: FaultExpectation) -> Self {
        Self { facts, recovery_seen: vec![], failure_seen: vec![],
            stop_seen: vec![], operation_ids: vec![], causes: vec![],
            cause_page: None, log_seen: false, incomplete_seen: false }
    }

    ///
    /// 既存ページの内部位置だけを返す。
    ///
    /// # 引数
    /// * `path` - 未表示の出力path
    ///
    /// # 戻り値
    /// 一意なページ位置
    ///
    fn index(&self, path: &str) -> Result<usize> {
        self.facts.pages.iter().position(|v| v.path == path)
            .ok_or_else(|| anyhow!("fault unknown page"))
    }

    ///
    /// stdoutの関連行を直前のページだけへ結び付ける。
    ///
    /// # 引数
    /// * `path` - 表示しない関連行のpath
    ///
    /// # 戻り値
    /// 製品のページ単位の表示順に一致するページ位置
    ///
    fn current_index(&self, path: &str) -> Result<usize> {
        let index = self.index(path)?;
        ensure!(index + 1 == self.facts.pages.len(),
            "fault result page order");
        Ok(index)
    }

    ///
    /// stdoutの各行を一つの既知レコードとして解析する。
    ///
    /// # 概要
    /// 既知行でもfield追加や関連行の重複は受け付けない。
    ///
    /// # 引数
    /// * `line` - 表示しない結果行
    ///
    /// # 戻り値
    /// 行の完全解析結果
    ///
    fn result_line(&mut self, line: &str) -> Result<()> {
        /*
         * ページ本行と先頭の主失敗位置を登録する
         */
        if let Some(path) = line.strip_prefix("first_failure=") {
            ensure!(self.facts.first_failure.is_none()
                && self.facts.pages.is_empty() && path_valid(path),
                "fault duplicate first failure");
            self.facts.first_failure = Some(path.into());
        } else if line.starts_with("page=") {
            self.add_page(line)?;
        } else if let Some(value) = line.strip_prefix("recovery: ") {
            self.add_recovery(value)?;
        } else if let Some(value) = line.strip_prefix("path_history: ") {
            self.add_history(value)?;
        } else if let Some(value) = line.strip_prefix("recovery_state: ") {
            let values = fields(value, &["path", "stage", "observed"])?;
            let index = self.current_index(values[0])?;
            ensure!(self.recovery_seen[index] == [true, false],
                "fault recovery state order");
            let recovery = self.facts.pages[index].recovery.as_mut()
                .ok_or_else(|| anyhow!("fault recovery state order"))?;
            recovery.stage = stage(values[1])?;
            recovery.observed = observed(values[2])?;
            self.recovery_seen[index][1] = true;
        /*
         * 手動確認文・解除/停止・元操作IDを別レコードで検査する
         */
        } else if let Some(value) = line.strip_prefix("manual_action: ") {
            let advice = advice(value)?;
            let recovery = self.facts.pages.last_mut()
                .and_then(|v| v.recovery.as_mut())
                .ok_or_else(|| anyhow!("fault advice order"))?;
            ensure!(recovery.advice.is_none(), "fault duplicate advice");
            recovery.advice = Some(advice);
        } else if let Some(value) = line.strip_prefix("execution_facts: ") {
            let values = fields(value, &["path", "cleanup_attempted", "stop"])?;
            let index = self.current_index(values[0])?;
            let stop = match values[2] {
                "None" => false, "Some(RecreationUncertain)" => true,
                _ => return Err(anyhow!("fault unknown stop")),
            };
            let page = &mut self.facts.pages[index];
            ensure!(page.execution.is_none(), "fault duplicate execution");
            page.execution = Some(ExecutionExpectation {
                cleanup_attempted: boolean(values[1])?, stop });
        } else if let Some(value) = line.strip_prefix("operation_fact: ") {
            let values = fields(value, &["path", "role", "id"])?;
            let index = self.current_index(values[0])?;
            let role = finite(values[1], &ROLES)?;
            ensure!(valid_id(values[2]), "fault operation ID");
            self.operation_ids[index].push((role, values[2].into()));
        } else { return Err(anyhow!("fault unknown result line")); }
        Ok(())
    }

    ///
    /// 計画順のページ本行を追加する。
    ///
    /// # 概要
    /// 末尾段階を先に分離し、結果内部の段階と混同しない。
    ///
    /// # 引数
    /// * `line` - 非表示のページ本行
    ///
    /// # 戻り値
    /// 一意で正規形のページ登録結果
    ///
    fn add_page(&mut self, line: &str) -> Result<()> {
        /*
         * 固定境界を一度ずつ分け、識別子を内部検証する
         */
        let (head, last) = line.rsplit_once(" stage=")
            .ok_or_else(|| anyhow!("fault page stage"))?;
        let (identity, result) = head.split_once(" outcome=")
            .ok_or_else(|| anyhow!("fault page outcome"))?;
        let values = fields(identity, &["page", "source"])?;
        ensure!(path_valid(values[0]) && source_valid(values[1]),
            "fault page identity");
        ensure!(self.index(values[0]).is_err(), "fault duplicate page");
        let page = PageExpectation::new(values[0], values[1],
            &outcome(result)?, &stage(last)?);
        /*
         * 関連行の一意性追跡もページと同じ順序で追加する
         */
        self.facts.pages.push(page);
        self.recovery_seen.push([false, false]);
        self.failure_seen.push(false);
        self.stop_seen.push(false);
        self.operation_ids.push(vec![]);
        Ok(())
    }

    ///
    /// 旧新ID・revision・削除・lockの全fieldを解析する。
    ///
    /// # 概要
    /// Noneの明示を必須とし、不在fieldと混同しない。
    ///
    /// # 引数
    /// * `value` - 復旧行の非表示payload
    ///
    /// # 戻り値
    /// 完全な復旧事実の登録結果
    ///
    fn add_recovery(&mut self, value: &str) -> Result<()> {
        /*
         * 正規順序の全fieldを読み、明示Noneを維持する
         */
        let values = fields(value, &["path", "old_id", "revision",
            "draft_id", "old_delete_confirmed", "old_lock_observed",
            "reconcile_lock_observed"])?;
        let index = self.current_index(values[0])?;
        let page = &mut self.facts.pages[index];
        ensure!(page.recovery.is_none(), "fault duplicate recovery");
        let mut recovery = RecoveryExpectation::new(None, None, None,
            boolean(values[4])?, "", "");
        recovery.old_id = optional_id(values[1])?;
        recovery.revision = optional(values[2], |v| number(Some(v)))?;
        recovery.draft_id = optional_id(values[3])?;
        recovery.old_lock = optional(values[5], boolean)?;
        recovery.lock = optional(values[6], boolean)?;
        page.recovery = Some(recovery);
        Ok(())
    }

    ///
    /// path履歴の有限状態と対応するIDを登録する。
    ///
    /// # 概要
    /// FoundのみIDが必要で、それ以外は明示Noneを要求する。
    ///
    /// # 引数
    /// * `value` - 非表示の履歴payload
    ///
    /// # 戻り値
    /// 一意なpath履歴の登録結果
    ///
    fn add_history(&mut self, value: &str) -> Result<()> {
        /*
         * 履歴状態とIDの対応を検査する
         */
        let values = fields(value, &["path", "state", "id"])?;
        let index = self.current_index(values[0])?;
        ensure!(!self.recovery_seen[index][0], "fault duplicate history");
        let state = finite(values[1], &["Unknown", "Absent", "Deleted",
            "Found"])?;
        let id = optional_id(values[2])?;
        ensure!((state == "Found") == id.is_some(), "fault history ID");
        let recovery = self.facts.pages[index].recovery.as_mut()
            .ok_or_else(|| anyhow!("fault history order"))?;
        recovery.path_state = state;
        recovery.path_id = id;
        self.recovery_seen[index][0] = true;
        Ok(())
    }

    ///
    /// stderrを警告・各失敗role・固定診断へ分離する。
    ///
    /// # 概要
    /// 生ログや未知診断は通さず、補助ログも集計と照合する。
    ///
    /// # 引数
    /// * `line` - 表示しない診断行
    ///
    /// # 戻り値
    /// 診断の完全解析結果
    ///
    fn diagnostic_line(&mut self, line: &str) -> Result<()> {
        /*
         * causeの再帰表示は直前詳細のcauseとだけ照合する
         */
        if line.starts_with("detail: ") { return self.add_detail(line); }
        ensure!(self.causes.is_empty(), "fault missing stored cause");
        if let Some(value) = line.strip_prefix("warning: page=") {
            self.add_warning(value)?;
        } else if let Some(value) = line.strip_prefix("warning: ") {
            let value = finite(value, &STARTUP_WARNINGS)?;
            ensure!(!self.facts.startup_warnings.contains(&value),
                "fault duplicate startup warning");
            self.facts.startup_warnings.push(value);
        } else if let Some(value) = line.strip_prefix("error: page=") {
            self.add_page_error(value)?;
        /*
         * 未実行診断と補助集計ログはページ結果から独立検証する
         */
        } else if line == "error: 未実行のページが残っています" {
            ensure!(!self.incomplete_seen, "fault duplicate incomplete");
            self.incomplete_seen = true;
        } else if let Some(value) = line.strip_prefix("error: ") {
            let error = global_error(value)?;
            ensure!(!self.facts.global_errors.contains(&error),
                "fault duplicate global diagnosis");
            self.facts.global_errors.push(error);
        } else if line.starts_with("[INFO] report ") {
            ensure!(!self.log_seen, "fault duplicate log");
            let counts = self.facts.counts;
            ensure!(line == format!(
                "[INFO] report exit={} warnings={} failed={} not_executed={}",
                self.facts.exit, counts[4], counts[5], counts[6]),
                "fault log counts");
            self.log_seen = true;
        } else { return Err(anyhow!("fault unknown diagnostic line")); }
        Ok(())
    }

    ///
    /// 元詳細と別に操作警告の有限観測を追加する。
    ///
    /// # 概要
    /// statusなしをResponseUncertainのまま保持する。
    ///
    /// # 引数
    /// * `value` - page以後の非表示payload
    ///
    /// # 戻り値
    /// 発生順での警告登録結果
    ///
    fn add_warning(&mut self, value: &str) -> Result<()> {
        /*
         * 固定fieldから一意な操作と応答観測を読む
         */
        let values = fields(&format!("page={value}"),
            &["page", "operation", "response", "observed"])?
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        let index = self.index(&values[0])?;
        let response = if values[2] == "ResponseUncertain" { None }
            else { Some(http_value(&values[2])?) };
        self.facts.pages[index].warnings.push(WarningExpectation {
            operation: finite(&values[1], &["Lookup", "Post", "Put",
                "Delete", "Lock", "Unlock"])? , response,
            observed: observed(&values[3])?, detail: None });
        Ok(())
    }

    ///
    /// 主失敗と安全停止の固定ページ診断を照合する。
    ///
    /// # 概要
    /// FailedとAppliedWithWarningの安全停止を混同しない。
    ///
    /// # 引数
    /// * `value` - page以後の非表示payload
    ///
    /// # 戻り値
    /// 一意な診断とページ結果の一致
    ///
    fn add_page_error(&mut self, value: &str) -> Result<()> {
        /*
         * stop診断を失敗件数と分けて照合する
         */
        if let Some(path) = value.strip_suffix(
            " safety_stop=RecreationUncertain")
        {
            let index = self.index(path)?;
            ensure!(!self.stop_seen[index] && self.facts.pages[index]
                .execution.as_ref().is_some_and(|v| v.stop),
                "fault safety stop mismatch");
            self.stop_seen[index] = true;
            return Ok(());
        }
        /*
         * 主失敗のcategoryと原因stageをページ結果へ対応させる
         */
        let (path, failure) = value.split_once(" category=")
            .ok_or_else(|| anyhow!("fault page error"))?;
        let index = self.index(path)?;
        let (kind, at) = failure.split_once(" stage=")
            .ok_or_else(|| anyhow!("fault page error stage"))?;
        let kind = category(kind)?;
        let at = stage(at)?;
        ensure!(!self.failure_seen[index]
            && self.facts.pages[index].outcome == format!(
                "Failed {{ category: {kind}, stage: {at} }}"),
            "fault primary diagnosis mismatch");
        self.failure_seen[index] = true;
        Ok(())
    }

    ///
    /// 警告・主・補助・解除・causeの詳細を別roleで登録する。
    ///
    /// # 概要
    /// 一次の重複を拒否し、再帰stored_causeだけは元causeに対応する。
    ///
    /// # 引数
    /// * `line` - 非表示の詳細行
    ///
    /// # 戻り値
    /// 元事実を失わないrole別登録結果
    ///
    fn add_detail(&mut self, line: &str) -> Result<()> {
        /*
         * 既知ページ・roleと再帰可能な有限詳細を読む
         */
        let (head, payload) = line.split_once(" facts=")
            .ok_or_else(|| anyhow!("fault detail fields"))?;
        let values = fields(head.strip_prefix("detail: ").unwrap_or(""),
            &["page", "role"])?;
        if values[0] == "global" || values[1] == "global" {
            ensure!(values[0] == "global" && values[1] == "global",
                "fault global detail role");
            return self.add_global_detail(payload);
        }
        let index = self.index(values[0])?;
        let role = finite(values[1], &ROLES)?;
        let detail = parse_detail(payload, 0)?;
        if role == "stored_cause" {
            ensure!(self.cause_page == Some(index)
                && self.causes.first() == Some(&detail),
                "fault stored cause mismatch");
            self.causes.remove(0);
            return Ok(());
        }
        ensure!(self.causes.is_empty(), "fault missing cause record");
        self.cause_page = Some(index);
        collect_causes(&detail, &mut self.causes);
        /*
         * 警告列と主・補助・解除の所有先を明示する
         */
        let page = &mut self.facts.pages[index];
        match role.as_str() {
            "warning" => {
                let warning = page.warnings.last_mut()
                    .ok_or_else(|| anyhow!("fault warning detail order"))?;
                ensure!(warning.detail.is_none(), "fault duplicate warning");
                warning.detail = Some(detail);
            }
            "primary" => {
                ensure!(page.primary.is_none(), "fault duplicate primary");
                page.primary = Some(detail);
            }
            "auxiliary" => page.auxiliary.push(detail),
            "cleanup" => {
                ensure!(page.cleanup.is_none(), "fault duplicate cleanup");
                page.cleanup = Some(detail);
            }
            _ => return Err(anyhow!("fault detail role")),
        }
        Ok(())
    }

    ///
    /// 全体期限停止の詳細をページとは別に一度だけ保持する。
    ///
    /// # 概要
    /// 固定全体診断との対応と全fieldを検査し、未知の詳細を補完しない。
    ///
    /// # 引数
    /// * `payload` - 表示しない全体詳細
    ///
    /// # 戻り値
    /// 一意なLocal Deadline/Preflight、または固定拒否
    ///
    fn add_global_detail(&mut self, payload: &str) -> Result<()> {
        /*
         * 未完のcause、重複、対応のない全体詳細を拒否する
         */
        ensure!(self.causes.is_empty() && self.facts.global_details.is_empty()
            && self.facts.global_errors.iter().any(|error|
                error == "category=Deadline stage=Preflight"),
            "fault global detail association");
        let detail = parse_detail(payload, 0)?;
        let expected = DetailExpectation::local("Deadline",
            Some("Preflight"));
        ensure!(detail == expected, "fault unsupported global detail");
        self.facts.global_details.push(detail);
        Ok(())
    }

    ///
    /// 全行読取後に集計・関連行・ID対応を検査する。
    ///
    /// # 概要
    /// 不正な一次記録を未知値へ補完せず、case照合前に拒否する。
    ///
    /// # 戻り値
    /// 完全な有限報告として保持できる結果
    ///
    fn finish(&mut self) -> Result<()> {
        /*
         * 関連行と元操作IDをrole順に対応させる
         */
        ensure!(self.causes.is_empty(), "fault missing final cause");
        let mut counts = [0u64; 7];
        counts[4] = self.facts.startup_warnings.len() as u64;
        let mut first = None;
        for (index, page) in self.facts.pages.iter_mut().enumerate() {
            ensure!(page.recovery.is_none()
                || self.recovery_seen[index] == [true, true],
                "fault incomplete recovery");
            ensure!(self.failure_seen[index]
                == page.outcome.starts_with("Failed {"),
                "fault missing primary diagnosis");
            ensure!(self.stop_seen[index] == page.execution.as_ref()
                .is_some_and(|v| v.stop), "fault missing safety diagnosis");
            validate_page(page)?;
            bind_ids(page, &self.operation_ids[index])?;
            /*
             * 警告付き適用のactionを維持して七集計を作り直す
             */
            let slot = match page.outcome.as_str() {
                "Created" | "AppliedWithWarning { action: Create }" => 0,
                "SkippedIdentical" => 1,
                "Updated" | "AppliedWithWarning { action: Update }" => 2,
                "Recreated" | "AppliedWithWarning { action: Recreate }" => 3,
                "NotExecuted" => 6,
                _ => 5,
            };
            counts[slot] = counts[slot].checked_add(1)
                .ok_or_else(|| anyhow!("fault page count overflow"))?;
            counts[4] = counts[4].checked_add(page.warnings.len() as u64)
                .ok_or_else(|| anyhow!("fault warning count overflow"))?;
            if slot == 5 && first.is_none() {
                first = Some(page.path.clone());
            }
        }
        /*
         * 集計・最初の失敗・未実行診断と計画状態を照合する
         */
        ensure!(counts == self.facts.counts, "fault summary mismatch");
        ensure!(first == self.facts.first_failure,
            "fault first failure mismatch");
        ensure!(self.incomplete_seen == (counts[6] > 0 && counts[5] == 0),
            "fault incomplete diagnosis mismatch");
        ensure!(self.facts.planned || self.facts.pages.is_empty(),
            "fault unplanned page");
        Ok(())
    }
}

///
/// 製品Reportのページ内整合条件を有限事実でも確認する。
///
/// # 概要
/// 主原因と最後段階、解除試行、警告付き適用、未実行を混同しない。
///
/// # 引数
/// * `page` - 未表示の解析済みページ
///
/// # 戻り値
/// Reportが受け入れる関連事実の組合せ
///
fn validate_page(page: &PageExpectation) -> Result<()> {
    /*
     * 主失敗の原因段階と解除試行を製品の条件へ対応させる
     */
    let failed = page.outcome.starts_with("Failed {");
    if failed {
        let stage = if let Some(primary) = &page.primary {
            primary.stage.as_deref()
                .ok_or_else(|| anyhow!("fault primary stage missing"))?
        } else { page.stage.as_str() };
        ensure!(page.outcome.ends_with(&format!(", stage: {stage} }}")),
            "fault failure stage mismatch");
    }
    /*
     * client解除試行と解除失敗・表示行の対応を独立して確認する
     */
    let attempted = page.execution.as_ref()
        .is_some_and(|v| v.cleanup_attempted);
    let stopped = page.execution.as_ref().is_some_and(|v| v.stop);
    if let Some(cleanup) = &page.cleanup {
        ensure!(attempted && cleanup.origin == "Wiki"
            && cleanup.operation.as_deref() == Some("ReleaseLock")
            && cleanup.stage.as_deref() == Some("ReleaseLock"),
            "fault cleanup facts mismatch");
    }
    ensure!(!attempted || (failed && page.stage == "ReleaseLock"),
        "fault cleanup stage mismatch");
    let execution = page.primary.is_some() || !page.auxiliary.is_empty()
        || attempted || stopped;
    ensure!(page.execution.is_some() == execution,
        "fault execution record mismatch");
    /*
     * 排他的な結果に警告・失敗・未実行の事実が混在しないか確認する
     */
    ensure!(failed || (page.primary.is_none() && page.auxiliary.is_empty()
        && page.cleanup.is_none()), "fault success failure facts");
    let applied = page.outcome.starts_with("AppliedWithWarning {");
    ensure!(!applied || !page.warnings.is_empty(),
        "fault missing applied warning");
    ensure!(!stopped || applied, "fault safety stop result");
    ensure!(failed || applied || page.warnings.is_empty(),
        "fault ordinary result warnings");
    if page.outcome == "NotExecuted" {
        ensure!(page.stage == "Preflight" && page.recovery.is_none()
            && page.execution.is_none(), "fault not executed facts");
    }
    Ok(())
}

/// 閉じた詳細role。
const ROLES: [&str; 5] = ["warning", "primary", "auxiliary", "cleanup",
    "stored_cause"];
/// 製品が表示する固定起動警告。
const STARTUP_WARNINGS: [&str; 2] = [
    "CLI認証情報はプロセス一覧や履歴へ露出する可能性があります",
    "認証情報を含む設定ファイルがgroup/otherから読取可能です",
];
/// 製品が表示する固定手動確認文。
const ADVICE: [&str; 4] = [
    "サーバー状態を照合し、再実行は手動で判断してください",
    "旧ページは自動復元されません。状態確認後に手動で再作成してください",
    "判明した作成先IDの状態を確認し、手動で対応してください",
    "他者変更を確認し、競合解消後の実行を手動で判断してください",
];

///
/// 固定手動確認文を閉じた方針名へ写す。
///
/// # 引数
/// * `value` - 非表示の製品文
///
/// # 戻り値
/// 有限の手動確認方針名
///
fn advice(value: &str) -> Result<String> {
    let index = ADVICE.iter().position(|v| *v == value)
        .ok_or_else(|| anyhow!("fault unknown advice"))?;
    Ok(["InspectBeforeRetry", "RecreateManually", "InspectDraft",
        "ResolveConflict"][index].into())
}

///
/// 固定語彙だけを所有文字列へ写す。
///
/// # 引数
/// * `value` - 非表示の候補
/// * `allowed` - 閉じた語彙
///
/// # 戻り値
/// 検証済み値。未知値は固定エラー。
///
fn finite(value: &str, allowed: &[&str]) -> Result<String> {
    ensure!(allowed.contains(&value), "fault unknown finite value");
    Ok(value.to_owned())
}

///
/// 段階の閉じた語彙を検証する。
///
/// # 引数
/// * `value` - 非表示の候補
///
/// # 戻り値
/// 有限段階
///
fn stage(value: &str) -> Result<String> {
    finite(value, &["Preflight", "Lookup", "ReadExisting", "AcquireLock",
        "Recheck", "DeleteOld", "CreateDraft", "PutSource", "Reconcile",
        "ReleaseLock", "Finished"])
}

///
/// 再照合状態を検証する。
///
/// # 引数
/// * `value` - 非表示の候補
///
/// # 戻り値
/// 有限状態
///
fn observed(value: &str) -> Result<String> {
    finite(value, &["Applied", "NotApplied", "Deleted", "Present",
        "Unknown"])
}

///
/// HTTP statusまたは名前付き分類を検証する。
///
/// # 引数
/// * `value` - 非表示の分類候補
///
/// # 戻り値
/// 有限分類
///
fn category(value: &str) -> Result<String> {
    if value.starts_with("Http(") {
        return Ok(format!("Http({})", http_value(value)?));
    }
    finite(value, &["Authentication", "Permission", "Conflict", "Rejected",
        "Transport", "Uncertain", "NotApplied", "RecreationFailed",
        "Deadline", "ExecutionUnavailable", "Logging", "Cancelled",
        "Protocol", "InvalidBody", "Io", "Worker", "Internal",
        "LockUnavailable"])
}

///
/// 結果表示をactionと主失敗段階まで完全検証する。
///
/// # 概要
/// 結果名だけの部分一致で未知のpayloadを通さない。
///
/// # 引数
/// * `value` - 非表示の結果候補
///
/// # 戻り値
/// 正規形の有限結果
///
fn outcome(value: &str) -> Result<String> {
    /*
     * payloadなしとaction付きの正規形を読む
     */
    if let Ok(value) = finite(value, &["Created", "SkippedIdentical",
        "Updated", "Recreated", "NotExecuted"])
    { return Ok(value); }
    if let Some(action) = value.strip_prefix(
        "AppliedWithWarning { action: ").and_then(|v| v.strip_suffix(" }"))
    {
        let action = finite(action, &["Create", "Update", "Recreate"])?;
        return Ok(format!("AppliedWithWarning {{ action: {action} }}"));
    }
    /*
     * 主失敗の分類と段階をそれぞれ閉じた語彙で読む
     */
    let inner = value.strip_prefix("Failed { category: ")
        .and_then(|v| v.strip_suffix(" }"))
        .ok_or_else(|| anyhow!("fault outcome"))?;
    let (kind, at) = inner.split_once(", stage: ")
        .ok_or_else(|| anyhow!("fault failed outcome"))?;
    Ok(format!("Failed {{ category: {}, stage: {} }}", category(kind)?,
        stage(at)?))
}

///
/// 正規十進整数だけを読む。
///
/// # 引数
/// * `value` - 数値候補または欠落
///
/// # 戻り値
/// 非負整数
///
fn number(value: Option<&str>) -> Result<u64> {
    let value = value.ok_or_else(|| anyhow!("fault missing number"))?;
    let parsed = value.parse::<u64>()
        .map_err(|_| anyhow!("fault invalid number"))?;
    ensure!(parsed.to_string() == value, "fault noncanonical number");
    Ok(parsed)
}

///
/// 明示boolだけを読む。
///
/// # 引数
/// * `value` - 非表示の候補
///
/// # 戻り値
/// 明示されたbool
///
fn boolean(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true), "false" => Ok(false),
        _ => Err(anyhow!("fault invalid bool")),
    }
}

///
/// 明示Noneまたは完全なSomeを読む。
///
/// # 引数
/// * `value` - 非表示の候補
/// * `parse` - 内部値の有限検証
///
/// # 戻り値
/// Noneを補完していない観測
///
fn optional<T>(value: &str, parse: impl FnOnce(&str) -> Result<T>)
    -> Result<Option<T>>
{
    if value == "None" { return Ok(None); }
    let inner = value.strip_prefix("Some(")
        .and_then(|v| v.strip_suffix(')'))
        .ok_or_else(|| anyhow!("fault invalid option"))?;
    Ok(Some(parse(inner)?))
}

///
/// 明示Noneまたは検証済み引用IDだけを読む。
///
/// # 引数
/// * `value` - 非表示の候補
///
/// # 戻り値
/// 確認済みIDまたは明示None
///
fn optional_id(value: &str) -> Result<Option<String>> {
    optional(value, |v| {
        let id = v.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
            .ok_or_else(|| anyhow!("fault quoted ID"))?;
        ensure!(valid_id(id), "fault ID format");
        Ok(id.into())
    })
}

///
/// HTTP表示の正規statusを読む。
///
/// # 引数
/// * `value` - Http形式の候補
///
/// # 戻り値
/// 100～599のstatus
///
fn http_value(value: &str) -> Result<u16> {
    let inner = value.strip_prefix("Http(")
        .and_then(|v| v.strip_suffix(')'));
    let status = number(inner)?;
    ensure!((100..=599).contains(&status), "fault HTTP status");
    Ok(status as u16)
}

///
/// 空白区切りの正規field列を順番も含め検証する。
///
/// # 引数
/// * `value` - 非表示のfield列
/// * `names` - 必須のfield順序
///
/// # 戻り値
/// 重複・余分・欠落のないfield値
///
fn fields<'a>(value: &'a str, names: &[&str]) -> Result<Vec<&'a str>> {
    let parts = value.split(' ').collect::<Vec<_>>();
    ensure!(parts.len() == names.len(), "fault field count");
    parts.iter().zip(names).map(|(part, name)| {
        part.strip_prefix(&format!("{name}="))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("fault field name"))
    }).collect()
}

///
/// 全七集計を正規順序で読む。
///
/// # 引数
/// * `line` - 非表示の集計行
///
/// # 戻り値
/// createdからnot_executedまでの全七集計
///
fn summary(line: &str) -> Result<[u64; 7]> {
    let values = fields(line, &["created", "skipped", "updated", "recreated",
        "warnings", "failed", "not_executed"])?;
    let mut counts = [0; 7];
    for (target, value) in counts.iter_mut().zip(values) {
        *target = number(Some(value))?;
    }
    Ok(counts)
}

///
/// 任意外部文を通さず固定全体診断だけを読む。
///
/// # 引数
/// * `value` - 非表示の診断候補
///
/// # 戻り値
/// 固定文または有限category/stage
///
fn global_error(value: &str) -> Result<String> {
    if let Some(value) = value.strip_prefix("category=") {
        let (kind, at) = value.split_once(" stage=")
            .ok_or_else(|| anyhow!("fault global fields"))?;
        return Ok(format!("category={} stage={}", category(kind)?,
            stage(at)?));
    }
    finite(value, &["入力検証に失敗しました",
        "コマンドライン引数が不正です。--helpを確認してください",
        "実行処理はまだ利用できません"])
}

///
/// pathを出力せず構造だけを検査する。
///
/// # 引数
/// * `value` - 非表示のpath
///
/// # 戻り値
/// 危険要素のない絶対pathならtrue
///
fn path_valid(value: &str) -> bool {
    value == "/" || value.strip_prefix('/').is_some_and(relative_valid)
}

///
/// 入力参照を有限方式と安全な相対名に制限する。
///
/// # 引数
/// * `value` - 非表示の入力参照
///
/// # 戻り値
/// 既知入力方式と安全なentryならtrue
///
fn source_valid(value: &str) -> bool {
    value.split_once(':').is_some_and(|(kind, entry)| {
        ["Zip", "LocalGit", "HttpsGit", "GitHub"].contains(&kind)
            && !entry.starts_with('/') && relative_valid(entry)
    })
}

///
/// 制御文字・URL等を含まない相対識別子を検証する。
///
/// # 引数
/// * `value` - 非表示の識別子
///
/// # 戻り値
/// 安全な内部識別子ならtrue
///
fn relative_valid(value: &str) -> bool {
    !value.is_empty() && !value.contains('\\') && !value.contains("://")
        && !value.contains('\u{fffd}')
        && !value.chars().any(|v| v.is_control() || v.is_whitespace())
        && value.split('/').all(|v| !matches!(v, "" | "." | ".."))
}

///
/// 詳細Debugの最上位fieldを括弧深度で分離する。
///
/// # 概要
/// 再帰cause・診断内部のカンマを一次fieldと混同しない。
///
/// # 引数
/// * `value` - 非表示のDebug内側
///
/// # 戻り値
/// 順序を保った最上位field列
///
fn debug_fields(value: &str) -> Result<Vec<&str>> {
    /*
     * 対応する括弧だけを追跡し、一次カンマで区切る
     */
    let mut stack = Vec::new();
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'(' => stack.push(b')'), b'{' => stack.push(b'}'),
            b')' | b'}' => ensure!(stack.pop() == Some(byte),
                "fault detail bracket"),
            b',' if stack.is_empty() => {
                ensure!(value[index..].starts_with(", "),
                    "fault detail separator");
                parts.push(&value[start..index]);
                start = index + 2;
            }
            _ => {}
        }
    }
    /*
     * 未閉鎖括弧と末尾空fieldを拒否する
     */
    ensure!(stack.is_empty() && start < value.len(),
        "fault detail structure");
    parts.push(&value[start..]);
    Ok(parts)
}

///
/// 正規Debug構造体のfield名と順番を検証する。
///
/// # 引数
/// * `value` - 非表示のDebug表示
/// * `name` - 固定構造体名
/// * `names` - 固定field順序
///
/// # 戻り値
/// 完全なfield値の参照
///
fn debug_struct<'a>(value: &'a str, name: &str, names: &[&str])
    -> Result<Vec<&'a str>>
{
    let inner = value.strip_prefix(&format!("{name} {{ "))
        .and_then(|v| v.strip_suffix(" }"))
        .ok_or_else(|| anyhow!("fault detail structure name"))?;
    let parts = debug_fields(inner)?;
    ensure!(parts.len() == names.len(), "fault detail field count");
    parts.iter().zip(names).map(|(part, name)| {
        part.strip_prefix(&format!("{name}: "))
            .ok_or_else(|| anyhow!("fault detail field name"))
    }).collect()
}

///
/// 元詳細を有限語彙と明示Optionで再帰解析する。
///
/// # 概要
/// 任意のDebug文字列を保管せず、既知fieldだけを所有する。
///
/// # 引数
/// * `value` - 非表示の詳細Debug
/// * `depth` - 再帰の現在深度
///
/// # 戻り値
/// 元operation/kind/stage/sent/statusを失わない詳細
///
fn parse_detail(value: &str, depth: usize) -> Result<DetailExpectation> {
    /*
     * 再帰を制限し、正規順序の全fieldを読む
     */
    ensure!(depth < 16, "fault detail depth");
    let values = debug_struct(value, "FailureDetail", &["origin",
        "operation", "kind", "stage", "sent", "status", "diagnostics",
        "cause"])?;
    let origin = finite(values[0], &["Wiki", "Local"])?;
    let operation = optional(values[1], |v| finite(v, &["Initialize",
        "Lookup", "ReadSource", "ReadMeta", "CreateDraft", "AcquireLock",
        "RenewLock", "ReleaseLock", "PutSource", "DeletePage"]))?;
    let kind = finite(values[2], &["ContextMismatch", "Preparation",
        "Authentication", "Http", "Transport", "Protocol", "BodyUtf8",
        "BodyReplacement", "Io", "Worker", "Deadline", "Cancelled",
        "LockHeader", "LockExpired", "LeaseUnavailable", "LeaseMismatch",
        "PlanPosition", "PlanOpen", "ReceivedOpen", "CompareIo",
        "ResultInvariant", "Conflict"])?;
    let stage = optional(values[3], stage)?;
    let sent = optional(values[4], |v| finite(v, &["NotStarted", "Started"]))?;
    let status = optional(values[5], |v| {
        let status = number(Some(v))?;
        ensure!((100..=599).contains(&status), "fault detail status");
        Ok(status as u16)
    })?;
    /*
     * 診断と保管causeも独立して有限解析する
     */
    let diagnostics = optional(values[6], |v| {
        let fields = debug_struct(v, "SafeDiagnostics",
            &["kind", "line", "column"])?;
        Ok((finite(fields[0], &["Syntax", "Validation", "Unknown"])?,
            optional(fields[1], |v| number(Some(v)))?,
            optional(fields[2], |v| number(Some(v)))?))
    })?;
    let cause = optional(values[7], |v|
        Ok(Box::new(parse_detail(v, depth + 1)?)))?;
    ensure!(origin != "Local" || (operation.is_none() && sent.is_none()
        && status.is_none()), "fault local HTTP facts");
    Ok(DetailExpectation { origin, operation, kind, stage, sent, status,
        known_id: None, diagnostics, cause })
}

///
/// 一次詳細から再帰表示されるcause列を作る。
///
/// # 引数
/// * `detail` - 有限解析済み詳細
/// * `causes` - 発生順の照合先
///
/// # 戻り値
/// なし。causeの順番を保持する。
///
fn collect_causes(detail: &DetailExpectation,
    causes: &mut Vec<DetailExpectation>)
{
    if let Some(cause) = &detail.cause {
        causes.push((**cause).clone());
        collect_causes(cause, causes);
    }
}

///
/// 明示ID行をrole順の詳細へ一度ずつ対応させる。
///
/// # 概要
/// DebugではIDが消えるため、別行を対応させて完全比較に戻す。
///
/// # 引数
/// * `page` - 対応先ページ
/// * `ids` - 正規検証済みID行
///
/// # 戻り値
/// roleと発生順に矛盾しない対応結果
///
fn bind_ids(page: &mut PageExpectation, ids: &[(String, String)])
    -> Result<()>
{
    /*
     * 詳細を製品の表示順に走査する
     */
    let mut details = Vec::new();
    for warning in &mut page.warnings {
        if let Some(detail) = &mut warning.detail {
            detail_refs("warning", detail, &mut details);
        }
    }
    if let Some(detail) = &mut page.primary {
        detail_refs("primary", detail, &mut details);
    }
    for detail in &mut page.auxiliary {
        detail_refs("auxiliary", detail, &mut details);
    }
    if let Some(detail) = &mut page.cleanup {
        detail_refs("cleanup", detail, &mut details);
    }
    /*
     * 同roleのIDを順序付きで対応させ、曖昧な部分列は拒否する
     */
    for role in ROLES {
        let actual = ids.iter().filter(|(name, _)| name == role)
            .map(|(_, id)| id).collect::<Vec<_>>();
        if actual.is_empty() { continue; }
        let slots = details.iter_mut().filter(|(name, _)| *name == role)
            .map(|(_, id)| id).collect::<Vec<_>>();
        ensure!(slots.len() == actual.len(), "fault ambiguous operation ID");
        for (slot, id) in slots.into_iter().zip(actual) {
            **slot = Some(id.clone());
        }
    }
    /*
     * roleごとの割当後も、別roleをまたいだ元の表示順を維持する
     */
    let bound = details.iter().filter_map(|(role, id)|
        id.as_ref().map(|id| ((*role).to_owned(), id.clone())))
        .collect::<Vec<_>>();
    ensure!(bound.as_slice() == ids, "fault operation ID order");
    Ok(())
}

///
/// 詳細と再帰causeを製品の表示順に参照する。
///
/// # 引数
/// * `role` - 一次role
/// * `detail` - 対応させる詳細
/// * `output` - roleと詳細の参照先
///
/// # 戻り値
/// なし。causeも順番を保って追加する。
///
fn detail_refs<'a>(role: &'static str, detail: &'a mut DetailExpectation,
    output: &mut Vec<(&'static str, &'a mut Option<String>)>)
{
    if detail.origin == "Wiki" && !matches!(detail.operation.as_deref(),
        None | Some("Initialize" | "Lookup"))
    {
        output.push((role, &mut detail.known_id));
    }
    if let Some(cause) = &mut detail.cause {
        detail_refs("stored_cause", cause, output);
    }
}

///
/// 詳細を原文なしの固定fieldへ写す。
///
/// # 引数
/// * `record` - 有限記録
/// * `role` - 固定role
/// * `detail` - 検証済み有限詳細
///
/// # 戻り値
/// なし。任意の原因messageを表示しない。
///
fn append_detail(record: &mut String, role: &str,
    detail: &DetailExpectation)
{
    record.push_str(&format!(
        " role={role} origin={} operation={:?} kind={} stage={:?} \
        sent={:?} status={:?} known_id={:?} diagnostics={:?}",
        detail.origin, detail.operation, detail.kind, detail.stage,
        detail.sent, detail.status, detail.known_id, detail.diagnostics));
    if let Some(cause) = &detail.cause {
        append_detail(record, "stored_cause", cause);
    }
}

///
/// 実環境なしでstrict解析と完全比較の拒否境界を確認する。
///
/// # 概要
/// exit3・明示None・警告順・各role・未知/重複を合成出力で検査する。
///
/// # 戻り値
/// 合成例の期待一致と不正例の全拒否
///
/// # 注記
/// 製品CLIやHTTPを呼ばず、文字列の純粋解析だけを行う。
///
pub(crate) fn verify_finite_parser() -> Result<()> {
    /*
     * exit0～3と計画成立/未成立を独立して解析する
     */
    for exit in 0..=3 {
        for planned in [false, true] {
            let plan = if planned { "established" } else {
                "not-established" };
            let captured = Captured { code: Some(exit), stdout: format!(
                "result: exit={exit} plan={plan}\ncompleted=0\n\
                created=0 skipped=0 updated=0 recreated=0 warnings=0 \
                failed=0 not_executed=0\n"), stderr: String::new() };
            FaultReport::parse(&captured)?.expect(
                &FaultExpectation::new(exit, planned, [0; 7]))?;
        }
    }
    /*
     * 警告付き適用の元statusなしと順序を完全期待に対応させる
     */
    let base = synthetic_warning();
    let report = FaultReport::parse(&base)?;
    let mut page = PageExpectation::new("/fixture/a", "Zip:a.md",
        "AppliedWithWarning { action: Update }", "Finished");
    page.warnings.push(WarningExpectation::new("Put", None, "Applied",
        DetailExpectation::wiki("PutSource", "Transport", Some("PutSource"),
            Some("Started"), None)));
    let expected = FaultExpectation::new(3, true, [0, 0, 1, 0, 1, 0, 0])
        .page(page);
    report.expect(&expected)?;
    ensure!(!report.safe_record().contains("/fixture")
        && !report.safe_record().contains("a.md"), "fault record identity");
    let mut wrong = expected.clone();
    wrong.pages[0].warnings[0].detail.as_mut().unwrap().status = Some(500);
    ensure!(report.expect(&wrong).is_err(), "fault None comparison");
    /*
     * 不正なfield・未知payload・重複一次記録をすべて拒否する
     */
    let mutations = [
        base.stdout.replace("exit=3", "exit=0"),
        base.stdout.replace("warnings=1", "warnings=1 warnings=1"),
        base.stdout.replace("updated=1", "updated=18446744073709551615")
            .replace("created=0", "created=1"),
        base.stdout.replace("action: Update", "action: Other"),
        format!("{}{}", base.stdout,
            "page=/fixture/a source=Zip:a.md outcome=Updated stage=Finished\n"),
        format!("{}unknown=private\n", base.stdout),
        base.stdout.replace('\n', "\r\n"),
    ];
    for stdout in mutations {
        let captured = Captured { code: base.code, stdout,
            stderr: base.stderr.clone() };
        ensure!(FaultReport::parse(&captured).is_err(),
            "fault invalid result accepted");
    }
    for stderr in [base.stderr.replace("status: None", "status: Unknown"),
        base.stderr.replace("kind: Transport", "kind: Other"),
        base.stderr.replace("sent: Some(Started)", "sent: NoneExtra"),
        base.stderr.replace("diagnostics: None", concat!(
            "diagnostics: Some(SafeDiagnostics { kind: Syntax, ",
            "line: Some(18446744073709551616), column: None })")),
        format!("{}{}", base.stderr, base.stderr),
        format!("{}error: private\n", base.stderr)]
    {
        let captured = Captured { code: base.code,
            stdout: base.stdout.clone(), stderr };
        ensure!(FaultReport::parse(&captured).is_err(),
            "fault invalid diagnostic accepted");
    }
    verify_rich_reports()?;
    verify_global_stop()?;
    Ok(())
}

///
/// ページ期限失敗と全体停止詳細を混同せず解析する。
///
/// # 概要
/// N06/R14のglobal行を再現し、欠落/重複/未知roleを安全に検出する。
///
/// # 戻り値
/// 全体詳細の完全一致と不正な関連付けの拒否
///
/// # 注記
/// 固定合成出力だけを解析し、CLIやHTTPを実行しない。
///
fn verify_global_stop() -> Result<()> {
    /*
     * 主ページと全体停止が併存する出力を完全期待へ結合する
     */
    let global = concat!(
        "detail: page=global role=global facts=FailureDetail { ",
        "origin: Local, operation: None, kind: Deadline, ",
        "stage: Some(Preflight), sent: None, status: None, ",
        "diagnostics: None, cause: None }\n");
    let captured = Captured { code: Some(1), stdout: concat!(
        "result: exit=1 plan=established\ncompleted=0\n",
        "created=0 skipped=0 updated=0 recreated=0 warnings=0 ",
        "failed=1 not_executed=1\nfirst_failure=/fixture/a\n",
        "page=/fixture/a source=Zip:a.md ",
        "outcome=Failed { category: Deadline, stage: DeleteOld } ",
        "stage=DeleteOld\n",
        "execution_facts: path=/fixture/a cleanup_attempted=false ",
        "stop=None\n",
        "page=/fixture/b source=Zip:b.md ",
        "outcome=NotExecuted stage=Preflight\n").into(), stderr: format!(
            "{}{}", concat!(
                "detail: page=/fixture/a role=primary facts=FailureDetail { ",
                "origin: Wiki, operation: Some(DeletePage), kind: Deadline, ",
                "stage: Some(DeleteOld), sent: Some(Started), status: None, ",
                "diagnostics: None, cause: None }\n",
                "error: page=/fixture/a category=Deadline stage=DeleteOld\n",
                "error: category=Deadline stage=Preflight\n"), global) };
    let mut first = PageExpectation::new("/fixture/a", "Zip:a.md",
        "Failed { category: Deadline, stage: DeleteOld }", "DeleteOld");
    first.primary = Some(DetailExpectation::wiki("DeletePage", "Deadline",
        Some("DeleteOld"), Some("Started"), None));
    first.execution = Some(ExecutionExpectation {
        cleanup_attempted: false, stop: false });
    let mut expected = FaultExpectation::new(1, true, [0, 0, 0, 0, 0, 1, 1])
        .page(first).page(PageExpectation::new("/fixture/b", "Zip:b.md",
            "NotExecuted", "Preflight"));
    expected.first_failure = Some("/fixture/a".into());
    expected.global_errors.push("category=Deadline stage=Preflight".into());
    expected.global_details.push(DetailExpectation::local("Deadline",
        Some("Preflight")));
    let report = FaultReport::parse(&captured)?;
    report.expect(&expected)?;
    ensure!(report.safe_record().contains("role=global origin=Local")
        && !report.safe_record().contains("/fixture"),
        "fault global safe record");
    /*
     * 全体詳細を捨てず、欠落と未知/重複/誤roleを検出する
     */
    let missing = Captured { code: captured.code,
        stdout: captured.stdout.clone(),
        stderr: captured.stderr.replace(global, "") };
    ensure!(FaultReport::parse(&missing)?.expect(&expected).is_err(),
        "fault missing global detail accepted");
    for changed in [format!("{global}{global}"),
        global.replace("page=global", "page=unknown"),
        global.replace("role=global", "role=primary"),
        global.replace("page=global", "page=/fixture/a"),
        global.replace("cause: None }", "cause: None, extra: None }"),
        global.replace("diagnostics: None", "unknown: None"),
        global.replace("kind: Deadline", "kind: Transport"),
        global.replace("Some(Preflight)", "Some(DeleteOld)"),
        global.replace("status: None", "status: Some(500)")]
    {
        let invalid = Captured { code: captured.code,
            stdout: captured.stdout.clone(),
            stderr: captured.stderr.replace(global, &changed) };
        ensure!(FaultReport::parse(&invalid).is_err(),
            "fault invalid global detail accepted");
    }
    let invalid = Captured { code: captured.code,
        stdout: captured.stdout.clone(), stderr: captured.stderr.replace(
            "error: category=Deadline stage=Preflight\n", "") };
    ensure!(FaultReport::parse(&invalid).is_err(),
        "fault unassociated global detail accepted");
    Ok(())
}

///
/// 累積警告・安全停止・旧新復旧・主補助解除を合成報告で検査する。
///
/// # 概要
/// 各roleの置換や警告順の変更が完全比較で検出されることを確認する。
///
/// # 戻り値
/// 完全期待一致と不正報告の拒否
///
fn verify_rich_reports() -> Result<()> {
    /*
     * 二警告と起動警告を独立計上し、再作成安全停止を比較する
     */
    let stopped = synthetic_stopped();
    let report = FaultReport::parse(&stopped)?;
    let old = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let new = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
    let mut page = PageExpectation::new("/fixture/a", "Zip:a.md",
        "AppliedWithWarning { action: Recreate }", "Reconcile");
    let mut delete = DetailExpectation::wiki("DeletePage", "Http",
        Some("DeleteOld"), Some("Started"), Some(500));
    delete.known_id = Some(old.into());
    let mut put = DetailExpectation::wiki("PutSource", "Transport",
        Some("PutSource"), Some("Started"), None);
    put.known_id = Some(new.into());
    page.warnings = vec![WarningExpectation::new("Delete", Some(500),
        "Deleted", delete), WarningExpectation::new("Put", None,
        "Applied", put)];
    page.execution = Some(ExecutionExpectation {
        cleanup_attempted: false, stop: true });
    let mut recovery = RecoveryExpectation::new(Some(old), Some(7),
        Some(new), true, "PutSource", "Applied");
    recovery.old_lock = Some(true);
    recovery.lock = Some(false);
    recovery.path_state = "Found".into();
    recovery.path_id = Some(new.into());
    recovery.advice = Some("InspectBeforeRetry".into());
    page.recovery = Some(recovery);
    let mut expected = FaultExpectation::new(1, true, [1, 0, 0, 1, 3, 0, 1])
        .page(PageExpectation::new("/fixture/0", "Zip:0.md", "Created",
            "Finished"))
        .page(page)
        .page(PageExpectation::new("/fixture/b", "Zip:b.md", "NotExecuted",
            "Preflight"));
    expected.startup_warnings.push(STARTUP_WARNINGS[0].into());
    report.expect(&expected)?;
    let mut wrong = expected.clone();
    wrong.pages[1].warnings.swap(0, 1);
    ensure!(report.expect(&wrong).is_err(), "fault warning order ignored");
    wrong = expected.clone();
    wrong.pages[1].recovery.as_mut().unwrap().deleted = false;
    ensure!(report.expect(&wrong).is_err(), "fault deletion ignored");
    wrong = expected.clone();
    wrong.pages[1].execution.as_mut().unwrap().stop = false;
    ensure!(report.expect(&wrong).is_err(), "fault safety stop ignored");
    /*
     * 主・補助・解除と再帰causeを別々の完全期待で比較する
     */
    let failed = synthetic_failed();
    let report = FaultReport::parse(&failed)?;
    let mut page = PageExpectation::new("/fixture/a", "Zip:a.md",
        "Failed { category: Permission, stage: PutSource }", "ReleaseLock");
    let mut primary = DetailExpectation::wiki("PutSource", "Http",
        Some("PutSource"), Some("Started"), Some(403));
    primary.known_id = Some(old.into());
    let mut auxiliary = DetailExpectation::wiki("ReadMeta", "Transport",
        Some("Reconcile"), Some("Started"), None);
    auxiliary.known_id = Some(old.into());
    let mut cause = DetailExpectation::wiki("ReadSource", "Transport",
        None, Some("Started"), None);
    cause.known_id = Some(old.into());
    let mut local = DetailExpectation::local("ReceivedOpen", None);
    local.cause = Some(Box::new(cause));
    let mut cleanup = DetailExpectation::wiki("ReleaseLock", "Http",
        Some("ReleaseLock"), Some("Started"), Some(500));
    cleanup.known_id = Some(old.into());
    page.primary = Some(primary);
    page.auxiliary = vec![auxiliary, local];
    page.cleanup = Some(cleanup);
    page.execution = Some(ExecutionExpectation {
        cleanup_attempted: true, stop: false });
    let mut expected = FaultExpectation::new(1, true, [0, 0, 0, 0, 0, 1, 0])
        .page(page);
    expected.first_failure = Some("/fixture/a".into());
    report.expect(&expected)?;
    let mut wrong = expected.clone();
    wrong.pages[0].primary = wrong.pages[0].cleanup.take();
    ensure!(report.expect(&wrong).is_err(), "fault cleanup replaces primary");
    wrong = expected.clone();
    wrong.pages[0].auxiliary.swap(0, 1);
    ensure!(report.expect(&wrong).is_err(), "fault auxiliary order ignored");
    /*
     * 有限診断の分類と行・桁も完全比較する
     */
    let diagnostic = "Some(SafeDiagnostics { kind: Syntax, line: Some(7), \
        column: None })";
    let diagnosed = Captured { code: failed.code,
        stdout: failed.stdout.clone(), stderr: failed.stderr.replacen(
            "diagnostics: None", &format!("diagnostics: {diagnostic}"), 1) };
    let mut with_diagnostic = expected.clone();
    with_diagnostic.pages[0].primary.as_mut().unwrap().diagnostics =
        Some(("Syntax".into(), Some(7), None));
    FaultReport::parse(&diagnosed)?.expect(&with_diagnostic)?;
    ensure!(FaultReport::parse(&diagnosed)?.expect(&expected).is_err(),
        "fault diagnostics ignored");
    /*
     * 重複一次記録・復旧field欠落・停止関連行欠落を拒否する
     */
    let primary = failed.stderr.lines().find(|v|
        v.contains("role=primary")).unwrap();
    for stderr in [format!("{}{primary}\n", failed.stderr),
        failed.stderr.replace("role=cleanup", "role=primary"),
        failed.stderr.replace("role=stored_cause", "role=auxiliary")]
    {
        let captured = Captured { code: failed.code,
            stdout: failed.stdout.clone(), stderr };
        ensure!(FaultReport::parse(&captured).is_err(),
            "fault invalid role accepted");
    }
    for stdout in [stopped.stdout.replace(" revision=Some(7)", ""),
        stopped.stdout.replace("old_id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAV\")",
            "old_id=None old_id=None"),
        stopped.stdout.replace("stop=Some(RecreationUncertain)", "stop=None"),
        stopped.stdout.replace(concat!(
            "operation_fact: path=/fixture/a role=warning ",
            "id=01ARZ3NDEKTSV4RRFFQ69G5FAW\n"), ""),
        format!("{}execution_facts: path=/fixture/a \
            cleanup_attempted=false stop=None\n", stopped.stdout),
        stopped.stdout.replace("source=Zip:b.md outcome=NotExecuted \
            stage=Preflight", "source=Zip:b.md outcome=NotExecuted \
            stage=Reconcile")]
    {
        let captured = Captured { code: stopped.code, stdout,
            stderr: stopped.stderr.clone() };
        ensure!(FaultReport::parse(&captured).is_err(),
            "fault invalid recovery accepted");
    }
    /*
     * 同じIDでもroleをまたいだ表示順変更を拒否する
     */
    let stdout = failed.stdout.replace("role=primary ", "role=temporary ")
        .replace("role=cleanup ", "role=primary ")
        .replace("role=temporary ", "role=cleanup ");
    let reordered = Captured { code: failed.code, stdout,
        stderr: failed.stderr.clone() };
    ensure!(FaultReport::parse(&reordered).is_err(),
        "fault operation role order ignored");
    Ok(())
}

///
/// 先行成功・再作成の二警告・後続未実行の固定報告を合成する。
///
/// # 戻り値
/// 実環境に依存しない安全停止例
///
fn synthetic_stopped() -> Captured {
    Captured { code: Some(1), stdout: concat!(
        "result: exit=1 plan=established\ncompleted=2\n",
        "created=1 skipped=0 updated=0 recreated=1 warnings=3 ",
        "failed=0 not_executed=1\n",
        "page=/fixture/0 source=Zip:0.md outcome=Created stage=Finished\n",
        "page=/fixture/a source=Zip:a.md ",
        "outcome=AppliedWithWarning { action: Recreate } stage=Reconcile\n",
        "recovery: path=/fixture/a ",
        "old_id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAV\") revision=Some(7) ",
        "draft_id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAW\") ",
        "old_delete_confirmed=true old_lock_observed=Some(true) ",
        "reconcile_lock_observed=Some(false)\n",
        "path_history: path=/fixture/a state=Found ",
        "id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAW\")\n",
        "recovery_state: path=/fixture/a stage=PutSource observed=Applied\n",
        "manual_action: ",
        "サーバー状態を照合し、再実行は手動で判断してください\n",
        "operation_fact: path=/fixture/a role=warning ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAV\n",
        "operation_fact: path=/fixture/a role=warning ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAW\n",
        "execution_facts: path=/fixture/a cleanup_attempted=false ",
        "stop=Some(RecreationUncertain)\n",
        "page=/fixture/b source=Zip:b.md ",
        "outcome=NotExecuted stage=Preflight\n",
    ).into(), stderr: concat!(
        "warning: page=/fixture/a operation=Delete ",
        "response=Http(500) observed=Deleted\n",
        "detail: page=/fixture/a role=warning facts=FailureDetail { ",
        "origin: Wiki, operation: Some(DeletePage), kind: Http, ",
        "stage: Some(DeleteOld), sent: Some(Started), status: Some(500), ",
        "diagnostics: None, cause: None }\n",
        "warning: page=/fixture/a operation=Put ",
        "response=ResponseUncertain observed=Applied\n",
        "detail: page=/fixture/a role=warning facts=FailureDetail { ",
        "origin: Wiki, operation: Some(PutSource), kind: Transport, ",
        "stage: Some(PutSource), sent: Some(Started), status: None, ",
        "diagnostics: None, cause: None }\n",
        "error: page=/fixture/a safety_stop=RecreationUncertain\n",
        "warning: ",
        "CLI認証情報はプロセス一覧や履歴へ露出する可能性があります\n",
        "error: 未実行のページが残っています\n",
        "[INFO] report exit=1 warnings=3 failed=0 not_executed=1\n",
    ).into() }
}

///
/// 主・補助二件・再帰cause・解除失敗を含む固定報告を合成する。
///
/// # 戻り値
/// roleごとの元事実が異なる完全失敗例
///
fn synthetic_failed() -> Captured {
    Captured { code: Some(1), stdout: concat!(
        "result: exit=1 plan=established\ncompleted=0\n",
        "created=0 skipped=0 updated=0 recreated=0 warnings=0 ",
        "failed=1 not_executed=0\nfirst_failure=/fixture/a\n",
        "page=/fixture/a source=Zip:a.md ",
        "outcome=Failed { category: Permission, stage: PutSource } ",
        "stage=ReleaseLock\n",
        "operation_fact: path=/fixture/a role=primary ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAV\n",
        "operation_fact: path=/fixture/a role=auxiliary ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAV\n",
        "operation_fact: path=/fixture/a role=stored_cause ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAV\n",
        "operation_fact: path=/fixture/a role=cleanup ",
        "id=01ARZ3NDEKTSV4RRFFQ69G5FAV\n",
        "execution_facts: path=/fixture/a cleanup_attempted=true stop=None\n",
    ).into(), stderr: concat!(
        "detail: page=/fixture/a role=primary facts=FailureDetail { ",
        "origin: Wiki, operation: Some(PutSource), kind: Http, ",
        "stage: Some(PutSource), sent: Some(Started), status: Some(403), ",
        "diagnostics: None, cause: None }\n",
        "detail: page=/fixture/a role=auxiliary facts=FailureDetail { ",
        "origin: Wiki, operation: Some(ReadMeta), kind: Transport, ",
        "stage: Some(Reconcile), sent: Some(Started), status: None, ",
        "diagnostics: None, cause: None }\n",
        "detail: page=/fixture/a role=auxiliary facts=FailureDetail { ",
        "origin: Local, operation: None, kind: ReceivedOpen, ",
        "stage: None, sent: None, status: None, diagnostics: None, ",
        "cause: Some(FailureDetail { origin: Wiki, ",
        "operation: Some(ReadSource), kind: Transport, stage: None, ",
        "sent: Some(Started), status: None, diagnostics: None, ",
        "cause: None }) }\n",
        "detail: page=/fixture/a role=stored_cause facts=FailureDetail { ",
        "origin: Wiki, operation: Some(ReadSource), kind: Transport, ",
        "stage: None, sent: Some(Started), status: None, ",
        "diagnostics: None, cause: None }\n",
        "detail: page=/fixture/a role=cleanup facts=FailureDetail { ",
        "origin: Wiki, operation: Some(ReleaseLock), kind: Http, ",
        "stage: Some(ReleaseLock), sent: Some(Started), status: Some(500), ",
        "diagnostics: None, cause: None }\n",
        "error: page=/fixture/a category=Permission stage=PutSource\n",
    ).into() }
}

///
/// statusなしの警告付き更新を合成する。
///
/// # 戻り値
/// 実環境へ接続しない固定文字列
///
fn synthetic_warning() -> Captured {
    Captured { code: Some(3), stdout: concat!(
        "result: exit=3 plan=established\ncompleted=1\n",
        "created=0 skipped=0 updated=1 recreated=0 warnings=1 ",
        "failed=0 not_executed=0\n",
        "page=/fixture/a source=Zip:a.md ",
        "outcome=AppliedWithWarning { action: Update } stage=Finished\n",
    ).into(), stderr: concat!(
        "warning: page=/fixture/a operation=Put ",
        "response=ResponseUncertain observed=Applied\n",
        "detail: page=/fixture/a role=warning facts=FailureDetail { ",
        "origin: Wiki, operation: Some(PutSource), kind: Transport, ",
        "stage: Some(PutSource), sent: Some(Started), status: None, ",
        "diagnostics: None, cause: None }\n",
    ).into() }
}
