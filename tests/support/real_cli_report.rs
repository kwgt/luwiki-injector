/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実CLI子出力を表示せず有限事実だけへ写す共有解析
//!

use anyhow::{Result, anyhow, ensure};

use super::real_cli_http::{ROOT, valid_id, within};

///
/// 子の結果を有限事実へ参照する受け手。raw出力は表示しない。
///
pub(crate) struct Captured {
    /// 子の終了コード。
    pub(crate) code: Option<i32>,
    /// 本文/明確な認証表現を除いた結果。
    pub(crate) stdout: String,
    /// 同じ秘匿境界を通した診断。
    pub(crate) stderr: String,
}

impl Captured {
    ///
    /// 実終了コードと一意な固定結果行から計画状態を読む。
    ///
    /// # 戻り値
    /// established、not-establishedまたはunknown
    ///
    pub(crate) fn safe_plan(&self) -> &'static str {
        let Some(code @ 0..=2) = self.code else { return "unknown"; };
        let line = unique_line(&self.stdout, "result:");
        for plan in ["established", "not-established"] {
            if line == Some(format!("result: exit={code} plan={plan}")
                .as_str())
            { return plan; }
        }
        "unknown"
    }

    ///
    /// 計画未成立の原因を原文なしの固定ラベルへ写す。
    ///
    /// # 戻り値
    /// 計画状態と入力/引数/収集/実行失敗の有限分類
    ///
    pub(crate) fn safe_startup(&self) -> String {
        let error = unique_line(&self.stderr, "error: ");
        let kind = match error {
            Some("error: 入力検証に失敗しました") => "InputValidation",
            Some("error: コマンドライン引数が不正です。--helpを確認してください")
                => "CliArguments",
            Some(line) if collection_diagnostic(line) => "Collection",
            Some(line) if line.starts_with("error: category=")
                && !self.safe_global().contains("unknown") => "Execution",
            _ => "unknown",
        };
        format!("MS51 startup plan={} failure={kind}", self.safe_plan())
    }

    ///
    /// remoteページ件数の検査より先に計画成立を確認する。
    ///
    /// # 戻り値
    /// 一意で実終了コードと一致した計画成立。原文は表示しない。
    ///
    pub(crate) fn require_remote_plan(&self) -> Result<()> {
        eprintln!("{}", self.safe_startup());
        ensure!(self.safe_plan() == "established",
            "remote plan not established or invalid");
        Ok(())
    }

    ///
    /// MS51の無警告結果を検査し、系列を混ぜず記録する。
    ///
    /// # 引数
    /// * `code` - 期待する製品終了コード
    ///
    /// # 戻り値
    /// 終了/計画成立/無警告の一致
    ///
    pub(crate) fn expect_ms51(&self, code: i32) -> Result<()> {
        ensure!(self.code == Some(code) && self.stdout.starts_with(&format!(
            "result: exit={code} plan=established\n")), "CLI result mismatch");
        ensure!(!self.stderr.contains("warning:"), "unexpected CLI warning");
        eprintln!("MS51 invocation=CLI exit={code} plan=established");
        Ok(())
    }

    ///
    /// 旧ID/revision/削除確定表示を独立事前状態と照合する。
    ///
    /// # 引数
    /// * `path` - 既知case path
    /// * `id` - 独立観測した旧ID
    /// * `revision` - 独立観測した旧revision
    /// * `deleted` - CLIが確定表示すべき旧削除状態
    ///
    /// # 戻り値
    /// 一意な復旧表示の完全一致
    ///
    pub(crate) fn expect_old_recovery(&self, path: &str, id: &str,
        revision: u64, deleted: bool) -> Result<()>
    {
        let line = unique_line(&self.stdout, &format!("recovery: path={path} "))
            .ok_or_else(|| anyhow!("recovery missing or duplicate"))?;
        ensure!(self.id(path, "old_id")? == id
            && line.matches(" revision=").count() == 1
            && observed_number(Some(line), " revision=") == Some(revision),
            "old recovery identity/revision");
        let fields: Vec<_> = line.split_ascii_whitespace()
            .filter(|v| v.starts_with("old_delete_confirmed=")).collect();
        ensure!(fields.len() == 1 && fields[0]
            == format!("old_delete_confirmed={deleted}"), "old delete fact");
        Ok(())
    }

    ///
    /// 全件数が無警告の期待結果へ一致するか確認する。
    ///
    /// # 引数
    /// * `counts` - created/skipped/updated/recreated/failed/not_executedの順
    ///
    /// # 戻り値
    /// 欠落/重複を含まず件数が完全一致した結果
    ///
    pub(crate) fn expect_counts(&self, counts: [u64; 6]) -> Result<()> {
        let [created, skipped, updated, recreated, failed, not_executed] =
            counts;
        ensure!(self.safe_summary() == format!(
            "MS43 summary created={created} skipped={skipped} \
            updated={updated} recreated={recreated} warnings=0 \
            failed={failed} not_executed={not_executed}"), "CLI counts");
        Ok(())
    }

    ///
    /// MS51の既知pathを固定page aliasへ写して有限記録を返す。
    ///
    /// # 引数
    /// * `path` - 内部比較にだけ使う検査済みcase path
    /// * `alias` - Pと2桁の数字だけの固定ラベル
    ///
    /// # 戻り値
    /// 本文/source/path原文を含まない有限記録
    ///
    pub(crate) fn alias_record(&self, path: &str, alias: &str) -> String {
        if alias.len() != 3 || !alias.starts_with('P')
            || !alias.as_bytes()[1..].iter().all(u8::is_ascii_digit)
            || !within(path, super::real_cli_http::MS51_ROOT)
        { return "MS51 output=rejected recovery=unknown".to_owned(); }
        self.safe_record(path)
            .replacen("MS43 ", "MS51 ", 1)
            .replacen(&format!("path={path} "), &format!("page={alias} "), 1)
    }

    ///
    /// ページ主原因と分離して全体失敗の有限分類/段階を記録する。
    ///
    /// # 戻り値
    /// 一意な全体原因。欠落/重複/任意文はunknown。
    ///
    pub(crate) fn safe_global(&self) -> String {
        let line = unique_line(&self.stderr, "error: category=");
        let fields = line.and_then(|v| v.strip_prefix("error: category="))
            .and_then(|v| v.split_once(" stage="));
        format!("MS43 role=global category={} stage={}",
            safe_category(fields.map(|(category, _)| category)),
            safe_stage(fields.map(|(_, stage)| stage)))
    }

    ///
    /// 集計は一意な行/fieldの整数だけへ限定して記録する。
    ///
    /// # 概要
    /// 生の集計行を転記せず、欠落/重複/不正値をunknownとして残す。
    ///
    /// # 戻り値
    /// 七つの件数だけの安全な記録
    ///
    pub(crate) fn safe_summary(&self) -> String {
        let line = unique_line(&self.stdout, "created=");
        /*
         * それぞれの件数を一意な整数として検証する
         */
        let count = |name: &str| {
            let prefix = format!("{name}=");
            let value = line.and_then(|line| {
                let mut fields = line.split_ascii_whitespace()
                    .filter(|v| v.starts_with(&prefix));
                let first = fields.next()?.strip_prefix(&prefix)?;
                if fields.next().is_some() { return None; }
                first.parse::<u64>().ok()
            });
            value.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
        };
        /*
         * 元行を使わず固定書式だけへ写す
         */
        format!("MS43 summary created={} skipped={} updated={} recreated={} \
            warnings={} failed={} not_executed={}", count("created"),
            count("skipped"), count("updated"), count("recreated"),
            count("warnings"), count("failed"), count("not_executed"))
    }

    ///
    /// 期待外でも既知pathの有限復旧事実だけを安全な記録へ写す。
    ///
    /// # 概要
    /// 未知/重複/不正値はunknownにし、raw行や任意の値を出力しない。
    ///
    /// # 引数
    /// * `path` - ハーネスが用意した承認領域内の期待path
    ///
    /// # 戻り値
    /// 実exit・有限分類/段階・検証済みID/数値と削除観測の記録
    ///
    pub(crate) fn safe_record(&self, path: &str) -> String {
        if !within(path, ROOT) { return "MS43 invalid_case_path".to_owned(); }
        let page = unique_line(&self.stdout, &format!("page={path} "))
            .filter(|v| v.matches(" outcome=").count() == 1
                && v.matches(" stage=").count() == 1);
        /*
         * 結果と段階は閉じた語彙だけを採用する
         */
        let outcome = ["Created", "Updated", "Recreated", "SkippedIdentical",
            "NotExecuted"].into_iter().find(|name| page.is_some_and(|line|
                line.contains(&format!("outcome={name} stage="))))
            .unwrap_or_else(|| {
                if page.is_some_and(|v| v.contains("outcome=Failed {")) {
                    "Failed"
                } else if page.is_some_and(|v|
                    v.contains("outcome=AppliedWithWarning {"))
                { "AppliedWithWarning" } else { "unknown" }
            });
        let last = safe_stage(page.and_then(|v| v.rsplit_once(" stage="))
            .map(|(_, stage)| stage));
        let error = unique_line(&self.stderr,
            &format!("error: page={path} category="));
        let cause = error.and_then(|v| v.rsplit_once(" stage="));
        let stage = safe_stage(cause.map(|(_, stage)| stage));
        let category = safe_category(error.and_then(|v|
            v.split_once(" category=")).and_then(|(_, tail)|
                tail.split_once(" stage=")).map(|(category, _)| category));
        /*
         * 復旧情報も既知ID/数値/明示bool以外を転記しない
         */
        let recovery = unique_line(&self.stdout,
            &format!("recovery: path={path} "));
        let revision = observed_number(recovery, " revision=")
            .map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        let deleted = if recovery.is_some_and(|v|
            v.contains(" old_delete_confirmed=true ")) { "true" }
            else if recovery.is_some_and(|v|
                v.contains(" old_delete_confirmed=false ")) { "false" }
            else { "unknown" };
        let old = self.id(path, "old_id")
            .unwrap_or_else(|_| "unknown".to_owned());
        let new = self.id(path, "draft_id")
            .unwrap_or_else(|_| "unknown".to_owned());
        format!("MS43 path={path} exit={:?} outcome={outcome} \
            last_stage={last} category={category} cause_stage={stage} \
            old_id={old} new_id={new} revision={revision} \
            old_delete_confirmed={deleted}", self.code)
    }

    ///
    /// 計画成立・終了状態・無警告と出力分離を照合する。
    ///
    /// # 引数
    /// * `code` - このcaseで期待した有限終了コード
    ///
    /// # 戻り値
    /// 期待一致。raw出力はerrorへ含めない。
    ///
    pub(crate) fn expect(&self, code: i32) -> Result<()> {
        ensure!(self.code == Some(code) && self.stdout.starts_with(&format!(
            "result: exit={code} plan=established\n")), "CLI result mismatch");
        ensure!(!self.stderr.contains("warning:"), "unexpected CLI warning");
        eprintln!("MS43 invocation=CLI exit={code} plan=established");
        Ok(())
    }

    ///
    /// 一件だけのページ記録と有限outcomeを確認する。
    ///
    /// # 引数
    /// * `path` - 既知case内のページ
    /// * `outcome` - コード内の期待状態
    ///
    /// # 戻り値
    /// 一致、または値をechoしない不整合
    ///
    pub(crate) fn page(&self, path: &str, outcome: &str) -> Result<()> {
        let prefix = format!("page={path} ");
        let mut lines = self.stdout.lines().filter(|v| v.starts_with(&prefix));
        let line = lines.next().ok_or_else(|| anyhow!("CLI page missing"))?;
        ensure!(lines.next().is_none()
            && line.contains(&format!("outcome={outcome}")),
            "CLI page outcome mismatch");
        Ok(())
    }

    ///
    /// 指定ページの確定復旧IDを有限形式で取得する。
    ///
    /// # 引数
    /// * `path` - 既知case path
    /// * `field` - old_idまたはdraft_id
    ///
    /// # 戻り値
    /// 完全に検証したID。候補や欠落を補完しない。
    ///
    pub(crate) fn id(&self, path: &str, field: &str) -> Result<String> {
        ensure!(matches!(field, "old_id" | "draft_id"), "recovery field");
        let prefix = format!("recovery: path={path} ");
        let mut lines = self.stdout.lines().filter(|v| v.starts_with(&prefix));
        let line = lines.next().ok_or_else(|| anyhow!("recovery missing"))?;
        ensure!(lines.next().is_none(), "duplicate recovery");
        let marker = format!("{field}=Some(\"");
        ensure!(line.matches(&marker).count() == 1, "recovery ID ambiguous");
        let value = line.split_once(&marker)
            .and_then(|(_, tail)| tail.split_once("\")"))
            .map(|(id, _)| id).ok_or_else(|| anyhow!("recovery ID missing"))?;
        ensure!(valid_id(value), "recovery ID format");
        Ok(value.to_owned())
    }

    ///
    /// 特定主原因と単発operation/statusの診断を確認する。
    ///
    /// # 引数
    /// * `category` - 有限な期待分類
    /// * `stage` - 有限な原因段階
    /// * `operation` - 元操作
    /// * `status` - 確定HTTP status
    ///
    /// # 戻り値
    /// 元原因を上書きしていない診断
    ///
    pub(crate) fn failure(&self, category: &str, stage: &str, operation: &str,
        status: u16) -> Result<()>
    {
        ensure!(self.stderr.contains(&format!(
            "category={category} stage={stage}")), "CLI failure mismatch");
        ensure!(self.stderr.lines().any(|line| line.contains("role=primary")
            && line.contains(&format!("operation: Some({operation})"))
            && line.contains(&format!("status: Some({status})"))),
            "CLI original facts missing");
        Ok(())
    }
}

///
/// 指定prefixで始まる一意の行だけを参照する。
///
/// # 引数
/// * `text` - 非公開の子出力
/// * `prefix` - ハーネスの固定形式と検証済みpath
///
/// # 戻り値
/// 一意の行。欠落/重複はNone。
///
pub(crate) fn unique_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut lines = text.lines().filter(|v| v.starts_with(prefix));
    let first = lines.next()?;
    if lines.next().is_some() { None } else { Some(first) }
}

///
/// 値を含まない既知の収集診断だけを完全一致で認識する。
///
/// # 概要
/// 未知の文や重複を分類に使わず、項目名も閉じた集合へ限定する。
///
/// # 引数
/// * `line` - 外部へ表示しない一意なerror行
///
/// # 戻り値
/// 既知の設定/環境/検証診断ならtrue
///
fn collection_diagnostic(line: &str) -> bool {
    /*
     * 固定の設定/検証診断を完全一致で確認する
     */
    let message = line.strip_prefix("error: ").unwrap_or("");
    if ["既定の設定パスを特定できません", "設定ファイルを読み取れません",
        "設定ファイルのTOML構文が不正です", "設定ファイルに未知のキーがあります",
        "設定ファイルの値の型が不正です", "対象Wiki URLが不正です",
        "ページプレフィクスが不正です", "衝突モードが不正です",
        "Wiki認証情報が不足しています", "BearerとBasicが同時指定されています",
        "Basicパスワードが不足しています", "パスワードを読み取れません",
        "ページソースが不正です", "入力種別とページソースが一致しません",
        "Git参照指定が競合しています", "Git参照指定が空です",
        "worktreeはローカルGit専用です", "処理期限を構築できません"]
        .contains(&message)
    { return true; }
    /*
     * 可変部分の項目名を許可された名前だけに閉じる
     */
    ["LUWIKI_URL", "LUWIKI_TOKEN", "LUWIKI_USER", "LUWIKI_PASSWORD",
        "GITHUB_TOKEN"].iter().any(|name|
            message == format!("{name}の文字列を読み取れません"))
        || ["GitHub token", "Wiki token", "Basic user", "Basic password"]
            .iter().any(|name| message == format!("{name}が空です"))
}

///
/// 原文を転記せず既知段階だけを返す。
///
/// # 引数
/// * `stage` - 未検証の段階候補
///
/// # 戻り値
/// 固定語彙またはunknown
///
pub(crate) fn safe_stage(stage: Option<&str>) -> &'static str {
    ["Preflight", "Lookup", "ReadExisting", "AcquireLock", "Recheck",
        "DeleteOld", "CreateDraft", "PutSource", "Reconcile", "ReleaseLock",
        "Finished"].into_iter().find(|v| Some(*v) == stage).unwrap_or("unknown")
}

///
/// 失敗分類を有限語彙またはHTTP数値だけへ制限する。
///
/// # 概要
/// 生の任意分類/理由を診断へ転記しない。
///
/// # 引数
/// * `category` - 非公開出力中の分類候補
///
/// # 戻り値
/// 検証済み分類またはunknown
///
pub(crate) fn safe_category(category: Option<&str>) -> String {
    /*
     * 名前付き分類は閉じた語彙から選ぶ
     */
    if let Some(name) = ["Authentication", "Permission", "Conflict", "Rejected",
        "Transport", "Uncertain", "NotApplied", "RecreationFailed", "Deadline",
        "ExecutionUnavailable", "Logging", "Cancelled", "Protocol",
        "InvalidBody", "Io", "Worker", "Internal", "LockUnavailable"]
        .into_iter().find(|v| Some(*v) == category)
    { return name.to_owned(); }
    /*
     * 数値HTTP分類以外の原文はunknownにする
     */
    let status = category.and_then(|v| v.strip_prefix("Http("))
        .and_then(|v| v.strip_suffix(')')).and_then(|v| v.parse::<u16>().ok());
    status.map_or_else(|| "unknown".to_owned(), |v| format!("Http({v})"))
}

///
/// 明示Someの数値だけを取り出し、Noneや不正値を補完しない。
///
/// # 引数
/// * `line` - 一意の既知形式行
/// * `field` - 固定の数値field名
///
/// # 戻り値
/// 整数として検証した観測値
///
pub(crate) fn observed_number(line: Option<&str>, field: &str) -> Option<u64> {
    let value = line?.split_once(field)?.1.strip_prefix("Some(")?
        .split_once(')')?.0;
    if !value.bytes().all(|v| v.is_ascii_digit()) { return None; }
    value.parse().ok()
}

