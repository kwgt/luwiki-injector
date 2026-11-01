/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! プログラムのエントリポイント
//!

mod cmd_args;
// 試験専用の補助入口もモジュール内に保持する。
#[allow(dead_code)]
mod executor;
mod logging;
mod measurement;
// 取得方式別の補助入口もモジュール内に保持する。
#[allow(dead_code)]
mod projection;
// 呼出側が未使用の分類も安全な報告型として保持する。
#[allow(dead_code)]
mod report;
mod secret;
// 取得方式別の補助入口も製品内に保持する。
#[allow(dead_code)]
mod source;
// 共通検証だけを呼ぶ補助入口も製品内に保持する。
#[allow(dead_code)]
mod validation;
// 単発操作の補助入口もモジュール内に保持する。
#[allow(dead_code)]
mod wiki_client;

use std::io::Write;

use crate::cmd_args::{CollectedOutcome, ResolvedRunConfig};
use crate::executor::Executor;
use crate::projection::ProjectionPlan;
use crate::report::types::{FailureCategory, ProcessingStage};
use crate::report::{Failure, Report};
use crate::source::{CancelOnDrop, SourceError, StopContext};

///
/// コマンドラインを受け付け、実行関数へ渡す。
///
/// # 概要
/// 正常表示を先に返し、それ以外の結果をReport経由で集約する。
///
/// # 戻り値
/// 正常表示は終了状態0、起動失敗は2、実行失敗は1で終了する。
///
fn main() {
    measurement::init();
    /*
     * コマンドライン引数の収集
     */
    let collected = match cmd_args::parse() {
        CollectedOutcome::Help(text) | CollectedOutcome::Version(text) => {
            let mut stdout = std::io::stdout().lock();
            let written = stdout.write_all(text.as_bytes())
                .and_then(|_| stdout.flush());
            measurement::finish();
            std::process::exit(if written.is_ok() { 0 } else { 1 });
        }
        collected => collected,
    };
    /*
     * 取得開始前に一度だけ初期化し、依存側の詳細ログを遮断する
     */
    let logging_ready = logging::init();
    let report = dispatch(collected, logging_ready);

    /*
     * 報告の表示と終了状態を一つの境界へ集める
     */
    let code = finish(report);
    measurement::finish();
    std::process::exit(code);
}

///
/// 初期化結果と収集済み入力から通常実行または安全な失敗を選ぶ。
///
/// # 概要
/// logger失敗時は取得へ進まず、設定・入力の警告と分類も失わない。
///
/// # 引数
/// * `collected` - CLIの収集・設定解決結果
/// * `logging_ready` - 通常処理前のlogger初期化結果
///
/// # 戻り値
/// 実行結果、または警告を保持したpreflight報告
///
fn dispatch(collected: CollectedOutcome, logging_ready: bool) -> Report {
    /*
     * logger失敗時の検証済み入力は警告だけを報告へ渡す
     */
    let mut report = match collected {
        /*
         * 初期化の成否に従い通常処理か起動段階の報告を選ぶ
         */
        CollectedOutcome::Input(input) if logging_ready => run(input),
        CollectedOutcome::Input(input) => {
            let mut report = Report::preflight();
            report.add_warnings(input.warnings());
            report
        }
        CollectedOutcome::Invalid => {
            let mut report = Report::preflight();
            report.fail(Failure::Cli);
            report
        }
        CollectedOutcome::Error { error, warnings } => {
            let mut report = Report::preflight();
            report.add_warnings(&warnings);
            report.fail(Failure::Collection(error));
            report
        }
        // 正常表示は上の分岐で返しており、この経路には到達しない。
        CollectedOutcome::Help(_) | CollectedOutcome::Version(_) =>
            Report::preflight(),
    };
    /*
     * 起動不正の分類も保持し、logger失敗を既存実行失敗へ集約する
     */
    if !logging_ready {
        report.fail(Failure::Execution {
            category: FailureCategory::Logging,
            stage: ProcessingStage::Preflight,
        });
    }

    report
}

///
/// 検証済み設定から計画を確定し、Wiki投入結果を報告する。
///
/// # 概要
/// runtime構築失敗も生のmessageを出さず、起動警告を保持して報告する。
///
/// # 引数
/// * `input` - 検証済み実行設定
///
/// # 戻り値
/// 計画未成立の失敗、または確定計画の実行報告
///
fn run(input: ResolvedRunConfig) -> Report {
    /*
     * 絶対期限を作り直さず、既存async取得へruntimeを接続する
     */
    let result = match tokio::runtime::Builder::new_current_thread()
        .enable_all().build()
    {
        Ok(runtime) => runtime.block_on(run_async(&input)),
        Err(_) => Err(SourceError::Worker.into_failure()),
    };
    /*
     * 計画失敗時にも起動警告を保持する
     */
    let mut report = match result {
        Ok(report) => report,
        Err(failure) => {
            let mut report = Report::preflight();
            report.fail(failure);
            report
        }
    };
    report.add_warnings(input.warnings());
    report
}

///
/// 取得からWiki投入まで同じcontextと本文所有を引き継ぐ。
///
/// # 概要
/// 計画成立後の失敗をpreflightへ戻さず、確定ページと元失敗を保持する。
///
/// # 引数
/// * `config` - 採用値・警告・絶対期限を検証済みの設定
///
/// # 戻り値
/// 確定計画の報告、または計画成立前の安全な失敗
///
async fn run_async(config: &ResolvedRunConfig) -> Result<Report, Failure> {
    let stop = StopContext::new(config.deadline());
    let mut guard = CancelOnDrop::new(stop.clone());
    let plan = projection::acquire_and_plan_for_report(config,
        stop.clone()).await?;
    /*
     * 成立計画以降は失敗もReportへ保持し、通常返却で取消しない
     */
    let mut report = execute_plan(plan, config, stop.clone()).await;
    observe_final_stop(&mut report, &stop);
    guard.disarm();
    Ok(report)
}

///
/// 同期変換後の停止も確定結果を消さず全体失敗へ追加する。
///
/// # 引数
/// * `report` - 計画成立後の結果と既存の停止記録
/// * `stop` - 取得から共有した絶対期限と取消flag
///
/// # 戻り値
/// なし。同じ分類の全体停止だけは重複追加しない。
///
fn observe_final_stop(report: &mut Report, stop: &StopContext) {
    if let Err(error) = stop.checkpoint() {
        let category = match error {
            SourceError::Deadline => FailureCategory::Deadline,
            SourceError::Cancelled => FailureCategory::Cancelled,
            _ => FailureCategory::Internal,
        };
        if report.has_execution_failure(category) { return; }
        report.fail(Failure::Execution { category,
            stage: ProcessingStage::Preflight });
    }
}

///
/// 成立計画のmetadataを保存してから本文所有をExecutorへ移す。
///
/// # 概要
/// 初期化失敗でも全未実行を保持し、実行後停止はExecutorが記録する。
/// 空計画はクライアントを初期化せず、同じ停止境界だけを確認する。
///
/// # 引数
/// * `plan` - 全件検証済みの確定順計画
/// * `config` - 取得から共有する実行設定
/// * `stop` - 同じ絶対期限と取消flag
///
/// # 戻り値
/// 成立計画と確定した先行結果を失わない報告
///
async fn execute_plan(plan: ProjectionPlan, config: &ResolvedRunConfig,
    stop: StopContext) -> Report
{
    /*
     * 取消途中でも全件の識別子を保存し成立計画を部分化しない
     */
    let identities = plan.pages().iter().map(|page| page.identity().clone())
        .collect();
    let mut report = match Report::planned(identities) {
        Ok(report) => report,
        Err(_) => {
            let mut report = Report::preflight();
            report.fail(SourceError::Worker.into_failure());
            return report;
        }
    };
    /*
     * 空計画はHTTPなし、停止は成立済み報告へ元分類で追加する
     */
    if plan.pages().is_empty() {
        observe_final_stop(&mut report, &stop);
        return report;
    }
    /*
     * 初期化後は同じcontextで実行し、最終停止を二重記録しない
     */
    match Executor::new(plan, config, stop) {
        Ok(executor) => executor::report::into_report(executor.run().await),
        Err(failure) => {
            executor::report::record_failure(&mut report, failure);
            report
        }
    }
}

///
/// 安全な報告をstdout/stderrへ表示する。
///
/// # 概要
/// 初期化済みログへ補助情報を渡し、独立した最終結果を表示する。
///
/// # 引数
/// * `report` - 集約済みの結果
///
/// # 戻り値
/// 最終終了コード。表示の失敗は実行失敗1になる。
///
fn finish(report: Report) -> i32 {
    logging::record(&report);

    /*
     * 最終結果と診断をそれぞれの出力先へ渡す
     */
    let rendered = report.render();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    if stdout.write_all(rendered.results.as_bytes()).is_err()
        || stdout.flush().is_err()
        || stderr.write_all(rendered.diagnostics.as_bytes()).is_err()
        || stderr.flush().is_err()
    {
        return 1;
    }
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::time::{Duration, Instant};

    use super::*;

    ///
    /// logger失敗時に取得を開始せず、起動警告と安全な分類を保持する。
    ///
    /// # 注記
    /// 壊れたZIPなら取得開始時はInputになるため、Loggingだけで停止を証明。
    ///
    /// # 戻り値
    /// なし。取得前停止・起動警告・安全な失敗分類が異なる場合はassertで失敗する。
    ///
    #[test]
    fn logger_failure_stops_before_acquisition_and_keeps_warnings() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("broken.zip");
        fs::write(&path, b"not-a-zip").unwrap();
        let config = cmd_args::validation_config_fixture(path,
            Instant::now() + Duration::from_secs(60), 1_000_000);
        let report = dispatch(CollectedOutcome::Input(config), false);
        let rendered = report.render();
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.summary().warnings, 1);
        assert!(rendered.results.contains("plan=not-established"));
        assert!(rendered.diagnostics.contains("category=Logging"));
        assert!(!rendered.diagnostics.contains("入力検証に失敗"));
        assert!(!rendered.diagnostics.contains("dummy-wiki-secret"));
    }

    ///
    /// logger失敗時にもCLI不正の分類を失わない。
    ///
    /// # 注記
    /// global loggerを変更せず、dispatchへ初期化失敗を渡して照合する。
    ///
    /// # 戻り値
    /// なし。起動エラーの保持や実行失敗の優先順位が異なる場合はassertで失敗する。
    ///
    #[test]
    fn logger_failure_preserves_startup_error_and_prioritizes_execution() {
        let report = dispatch(CollectedOutcome::Invalid, false);
        let diagnostics = report.render().diagnostics;
        assert_eq!(report.exit_code(), 1);
        assert!(diagnostics.contains("category=Logging"));
        assert!(diagnostics.contains("コマンドライン引数が不正"));
    }

    ///
    /// 設定確定後の元入力消失と期限超過を安全な終了1へ写す。
    ///
    /// # 概要
    /// runtimeを含む通常実行を呼び、計画前の実行失敗と警告保持を確認する。
    ///
    /// # 注記
    /// loggerは初期化せず、I/O値や本文を診断へ出さないことを照合する。
    ///
    /// # 戻り値
    /// なし。計画前のI/O・期限失敗と警告非露出の契約が異なればassertで失敗する。
    ///
    #[test]
    fn execution_io_and_deadline_fail_before_plan_with_warnings() {
        /*
         * 設定確定後に本文取得の前提を変え、実行境界だけを検証する
         */
        for expired in [false, true] {
            let area = tempfile::tempdir().unwrap();
            let path = area.path().join("missing-after-config.zip");
            fs::write(&path, b"unique-body").unwrap();
            let deadline = if expired { Instant::now() }
                else { Instant::now() + Duration::from_secs(60) };
            let config = cmd_args::validation_config_fixture(path.clone(),
                deadline, 1_000_000);
            fs::remove_file(&path).unwrap();
            /*
             * 期限はI/Oより先に判定し、両者とも警告を失わない
             */
            let report = run(config);
            let rendered = report.render();
            assert_eq!(report.exit_code(), 1);
            assert_eq!(report.summary().warnings, 1);
            assert!(rendered.results.contains("plan=not-established"));
            let category = if expired { "Deadline" } else { "Transport" };
            assert!(rendered.diagnostics.contains(category));
            assert!(!rendered.diagnostics.contains("unique-body"));
            assert!(!rendered.diagnostics.contains("missing-after-config"));
        }
    }

    ///
    /// 成立計画の後で取消が来ても初期化失敗と全未実行を保持する。
    ///
    /// # 概要
    /// 製品の取得・検証で計画を作り、同じcontextだけを取消して初期化する。
    ///
    /// # 注記
    /// 全metadataを保持し、元の局所Cancelledをpreflight失敗へ丸めない。
    ///
    /// # 戻り値
    /// なし。成立計画・全未実行・元詳細が消えた場合に失敗する。
    ///
    #[tokio::test]
    async fn initialization_stop_preserves_established_plan_and_details() {
        let area = tempfile::tempdir().unwrap();
        let path = area.path().join("input.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        /*
         * Wiki操作へ進む前に全件成立した計画を実製品入口で作る
         */
        for name in ["b.md", "a.md"] {
            zip.start_file(format!("root/{name}"),
                zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"private-input-body").unwrap();
        }
        zip.finish().unwrap();
        let config = cmd_args::validation_config_fixture(path,
            Instant::now() + Duration::from_secs(60), 1_000_000);
        let stop = StopContext::new(config.deadline());
        let plan = projection::acquire_and_plan_for_report(&config,
            stop.clone()).await.unwrap();
        stop.cancel();
        /*
         * 同じ取消flagを初期化へ渡し全件未実行と元分類を確認する
         */
        let mut report = execute_plan(plan, &config, stop.clone()).await;
        observe_final_stop(&mut report, &stop);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.summary().not_executed, 2);
        assert_eq!(report.pages()[0].identity.path.as_str(), "/a");
        assert_eq!(report.pages()[1].identity.path.as_str(), "/b");
        let output = report.render();
        assert!(output.results.contains("plan=established"));
        assert!(output.diagnostics.contains("origin: Local"));
        assert!(output.diagnostics.contains("kind: Cancelled"));
        assert_eq!(output.diagnostics.matches("error: category=Cancelled")
            .count(), 1);
        assert!(!output.results.contains("private-input-body"));
        assert!(!output.diagnostics.contains("dummy-wiki-secret"));
    }

    ///
    /// 同期変換後の停止は先行成功を維持して一度だけ全体へ記録する。
    ///
    /// # 概要
    /// 成功確定後の取消を最終境界で観測し、既存全体停止の重複を防ぐ。
    ///
    /// # 注記
    /// 同じ最終境界を二度通し、成功一件/未実行ゼロ/終了1を確認する。
    ///
    /// # 戻り値
    /// なし。先行成功の消失や停止診断の重複があれば失敗する。
    ///
    #[test]
    fn final_stop_keeps_completed_page_and_records_global_stop_once() {
        use crate::report::details::ResultDetails;
        use crate::report::types::{PageIdentity, PageOutcome, PagePath,
            PageResult, SourceKind, SourceReference};

        let identity = PageIdentity { path: PagePath::new("/a").unwrap(),
            source: SourceReference::new(SourceKind::Zip, "a.md").unwrap() };
        let mut report = Report::planned(vec![identity.clone()]).unwrap();
        /*
         * 同期変換までに確定した成功記録を用意する
         */
        report.record_page(0, PageResult { identity,
            outcome: PageOutcome::Created, stage: ProcessingStage::Finished,
            recovery: None, details: ResultDetails::default() }).unwrap();
        let stop = StopContext::new(Instant::now() + Duration::from_secs(60));
        stop.cancel();
        /*
         * 新たな停止を保持し、同分類の全体失敗だけを重複抑制する
         */
        observe_final_stop(&mut report, &stop);
        observe_final_stop(&mut report, &stop);
        assert_eq!(report.summary().created, 1);
        assert_eq!(report.summary().not_executed, 0);
        assert_eq!(report.exit_code(), 1);
        let output = report.render();
        assert_eq!(output.diagnostics.matches("error: category=Cancelled")
            .count(), 1);
    }
}
