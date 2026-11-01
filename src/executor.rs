/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 成立計画の所有・共有停止と私有の順序付きページ実行基盤
//!

pub(crate) mod compare;
mod normal;
mod prepare;
mod reconcile;
pub(crate) mod report;
pub(crate) mod types;

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::cmd_args::ResolvedRunConfig;
use crate::cmd_args::resolve::Mode;
use crate::projection::ProjectionPlan;
use crate::report::types::ProcessingStage;
use crate::source::{CancelOnDrop, StopContext};
use crate::wiki_client::WikiClient;
use crate::wiki_client::adapter::execution_failure;

use self::types::{ExecutionBatch, ExecutorFailure, LocalKind, PageFlow,
    PageStep};

///
/// 計画と同一停止contextを保つ実行側所有者。
///
pub(crate) struct Executor {
    /// workerへ共有する全件検証済み計画。
    plan: Arc<ProjectionPlan>,
    /// 同じcontextから生成した単発クライアント。
    client: WikiClient,
    /// 検証済みの衝突モード。認証情報を複製しない。
    mode: Mode,
    /// 取得から呼出側が引き継ぐ同じ停止境界。
    stop: StopContext,
}

impl Executor {
    ///
    /// 計画を単回消費して通常processorを確定順に実行する。
    ///
    /// # 戻り値
    /// 元失敗/観測事実と先行結果・後続未実行。Reportへは投影しない。
    ///
    pub(crate) async fn run(self) -> ExecutionBatch {
        self.drive(&mut normal::NormalProcessor).await
    }

    ///
    /// 成立済み計画を所有し、HTTPなしで実行基盤を準備する。
    ///
    /// # 概要
    /// 呼出側は取得・検証・計画の同じcontext cloneを渡す。
    /// 期限一致だけで以前の取消flag同一性を遡及証明しない。
    ///
    /// # 引数
    /// * `plan` - 製品の全件検証/整列入口で成立した計画
    /// * `config` - 検証済み設定。秘密値は結果へ保持しない。
    /// * `stop` - 設定の同じ絶対期限・共有取消context
    ///
    /// # 戻り値
    /// 実行基盤、または安全な開始前失敗
    ///
    pub(crate) fn new(plan: ProjectionPlan, config: &ResolvedRunConfig,
        stop: StopContext) -> Result<Self, ExecutorFailure>
    {
        /*
         * 新contextや期限の再起算を許さず送信前に照合する
         */
        let stage = ProcessingStage::Preflight;
        if stop.deadline() != config.deadline() {
            return Err(ExecutorFailure::local(LocalKind::ContextMismatch,
                stage));
        }
        stop.checkpoint().map_err(|error|
            ExecutorFailure::stopped(error, stage))?;
        let client = WikiClient::new(config, stop.clone()).map_err(|failure|
            ExecutorFailure::Wiki(execution_failure(failure, stage,
                Vec::new())))?;
        let mode = match &config.settings().mode {
            Mode::Error => Mode::Error,
            Mode::Update => Mode::Update,
            Mode::Overwrite => Mode::Overwrite,
        };
        Ok(Self { plan: Arc::new(plan), client, mode, stop })
    }

    ///
    /// 私有processorを一件ずつawaitし先行結果を維持する。
    ///
    /// # 概要
    /// 通常モードprocessorを接続し、公開run成功stubは設けない。
    /// 結果返却の通常失敗で取消guardを発火させない。
    ///
    /// # 引数
    /// * `processor` - crate外へ公開しない一件処理の所有者
    ///
    /// # 戻り値
    /// 投影前の順序付き結果と未実行・安全な停止
    ///
    async fn drive<P: PageProcessor>(&self, processor: &mut P)
        -> ExecutionBatch
    {
        let mut batch = ExecutionBatch::new(&self.plan);
        let mut guard = CancelOnDrop::new(self.stop.clone());
        /*
         * 共有停止を確認し計画順の一件だけを開始する
         */
        for index in 0..self.plan.pages().len() {
            if let Err(error) = self.stop.checkpoint() {
                batch.stop_before_page(ExecutorFailure::stopped(error,
                    ProcessingStage::Preflight));
                break;
            }
            let step = processor.process(index, self).await;
            let flow = step.flow();
            /*
             * 確定先行結果を記録し最初の失敗後は処理を開始しない
             */
            if let Err(failure) = batch.record(index, step) {
                batch.stop_before_page(failure);
                break;
            }
            if flow == PageFlow::Stop { break; }
        }
        /*
         * 通常返却は取消を解除し最終停止でも先行結果を失わない
         */
        guard.disarm();
        if let Err(error) = self.stop.checkpoint() {
            batch.stop_before_page(ExecutorFailure::stopped(error,
                ProcessingStage::Preflight));
        }
        batch
    }
}

impl fmt::Debug for Executor {
    ///
    /// 接続先・本文・識別子を表示せず計画件数だけを表示する。
    ///
    /// # 引数
    /// * `f` - 表示先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor").field("pages", &self.plan.pages().len())
            .finish()
    }
}

///
/// 通常処理の具体実装を外部へ露出しない私有境界。
///
trait PageProcessor: Send {
    ///
    /// 指定位置の一件を完了または元失敗として返す。
    ///
    /// # 引数
    /// * `index` - 次に一度だけ処理する計画位置
    /// * `executor` - 計画と共有停止の所有者
    ///
    /// # 戻り値
    /// 実処理の結果。未実装を成功へ補完しない。
    ///
    fn process(&mut self, index: usize, executor: &Executor)
        -> impl Future<Output = PageStep> + Send;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod real_wiki_tests;
