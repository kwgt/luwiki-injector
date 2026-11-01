/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 全件検証済み本文の所有と決定的整列・取得から計画までの共通境界
//!

use std::fmt;
use std::fs::File;
use std::sync::Arc;

use crate::cmd_args::ResolvedRunConfig;
use crate::report::Failure;
use crate::report::types::PageIdentity;
use crate::source::{self, CancelOnDrop, SourceError, StopContext};
use crate::validation;
use crate::validation::{ValidatedInput, ValidationError};

///
/// 投影先・実サイズと検証済み本文の位置を保持する計画ページ。
///
pub(crate) struct PlannedPage {
    identity: PageIdentity,
    byte_len: u64,
    validated_index: usize,
}

impl PlannedPage {
    ///
    /// 投影先と安全な入力参照を取得する。
    ///
    /// # 戻り値
    /// 全件検証済みのページ識別情報
    ///
    pub(crate) fn identity(&self) -> &PageIdentity { &self.identity }

    ///
    /// 保管済み本文の実サイズを取得する。
    ///
    /// # 戻り値
    /// 本文バイト数（bytes）
    ///
    pub(crate) fn byte_len(&self) -> u64 { self.byte_len }
}

impl fmt::Debug for PlannedPage {
    ///
    /// 識別情報や本文位置を表示せずサイズだけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PlannedPage")
            .field("bytes", &self.byte_len).finish()
    }
}

///
/// ページ対応と不変な検証済み本文の寿命を所有する計画。
///
/// 全件検証と整列・停止確認に成功した公開入口からだけ生成する。
///
pub(crate) struct ProjectionPlan {
    pages: Vec<PlannedPage>,
    input: ValidatedInput,
}

impl ProjectionPlan {
    ///
    /// 計画中のページmetadataを読み取り専用で取得する。
    ///
    /// # 戻り値
    /// 本文を持たないページ群
    ///
    pub(crate) fn pages(&self) -> &[PlannedPage] { &self.pages }

    ///
    /// 計画位置に対応する保管済み本文を読込専用で開く。
    ///
    /// # 引数
    /// * `index` - 計画中のページ位置
    ///
    /// # 戻り値
    /// 読込handle、または値を含めない位置・I/O失敗
    ///
    pub(crate) fn open_body(&self, index: usize)
        -> Result<File, ValidationError>
    {
        let page = self.pages.get(index)
            .ok_or(ValidationError::Source(SourceError::InvalidPath))?;
        self.input.open_body(page.validated_index)
    }

    ///
    /// 成立済み計画と本文の寿命を送信workerへ共有する。
    ///
    /// # 引数
    /// * `index` - 確定計画内の位置
    ///
    /// # 戻り値
    /// 任意Fileやpathから生成できないreadonly送信資源
    ///
    pub(crate) fn upload_body(self: &Arc<Self>, index: usize)
        -> Result<PlannedBody, ValidationError>
    {
        let page = self.pages.get(index)
            .ok_or(ValidationError::Source(SourceError::InvalidPath))?;
        #[cfg(feature = "measurement")]
        let trace_id = crate::measurement::id();
        #[cfg(feature = "measurement")]
        crate::measurement::event(crate::measurement::Event::PlanBodyCreate,
            trace_id, 0, crate::measurement::Role::Payload, 0);
        Ok(PlannedBody { plan: Arc::clone(self), index,
            bytes: page.byte_len,
            #[cfg(feature = "measurement")]
            trace_id })
    }
}

///
/// 検証済み計画だけを保持し、workerへreadonly本文所有を渡す資源。
///
pub(crate) struct PlannedBody {
    plan: Arc<ProjectionPlan>,
    /// 計画への強参照を増やさず本文所有を識別する匿名観測ID。
    #[cfg(feature = "measurement")]
    trace_id: u64,
    index: usize,
    bytes: u64,
}

#[cfg(feature = "measurement")]
impl Drop for PlannedBody {
    ///
    /// 既存の共有所有者を解放し始める時点だけを記録する。
    ///
    /// # 戻り値
    /// なし。最後のpayload解放は領域側で別に観測する。
    ///
    fn drop(&mut self) {
        crate::measurement::event(crate::measurement::Event::PlanBodyDrop,
            self.trace_id, 0, crate::measurement::Role::Payload, 0);
    }
}

impl PlannedBody {
    ///
    /// 検証済み本文の実量を取得する。
    ///
    /// # 戻り値
    /// byte数
    ///
    pub(crate) fn byte_len(&self) -> u64 { self.bytes }

    ///
    /// worker内で対応する本文をreadonlyで開く。
    ///
    /// # 戻り値
    /// 読込handle、または値なしの失敗
    ///
    pub(crate) fn open(&self) -> Result<File, ValidationError> {
        self.plan.open_body(self.index)
    }
}

impl fmt::Debug for PlannedBody {
    ///
    /// 本文・位置・保管領域を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 表示先
    ///
    /// # 戻り値
    /// 実量だけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PlannedBody")
            .field("bytes", &self.bytes).finish()
    }
}

impl fmt::Debug for ProjectionPlan {
    ///
    /// 本文・識別情報・保管領域を含めず件数だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProjectionPlan")
            .field("pages", &self.pages.len()).finish()
    }
}

///
/// 検証済み入力だけを消費して計画の内部所有者を組み立てる。
///
/// # 概要
/// 本文は再読込・コピーせず、元位置をmetadataと一体で保持する。
/// 停止時は入力を回収する。整列前の組立結果は内部に閉じる。
///
/// # 引数
/// * `input` - 全件の名前・投影・本文検証に成功した所有された入力
/// * `stop` - 取得から共有する期限と取消境界
///
/// # 戻り値
/// 内部組立結果。投入順の確定を表明する公開APIではない。
///
fn assemble(input: ValidatedInput, stop: &StopContext)
    -> Result<ProjectionPlan, ValidationError>
{
    stop.checkpoint().map_err(ValidationError::Source)?;
    let mut pages = Vec::new();
    /*
     * 元位置を任意入力から受け取らず、検証済み全ページから生成する
     */
    for (validated_index, page) in input.pages().iter().enumerate() {
        stop.checkpoint().map_err(ValidationError::Source)?;
        pages.push(PlannedPage {
            identity: page.identity().clone(), byte_len: page.byte_len(),
            validated_index,
        });
        // 途中停止の注入はこのモジュールの試験だけに限定する。
        #[cfg(test)]
        tests::after_page(stop);
    }
    /*
     * 空入力を含め、全metadata組立後の停止を確認して所有権を移す
     */
    stop.checkpoint().map_err(ValidationError::Source)?;
    Ok(ProjectionPlan { pages, input })
}

///
/// 検証済み入力を深さ・絶対パスのUTF-8 bytes順に確定する。
///
/// # 概要
/// 同期metadata処理はworkerに所有させ、整列前後と返却後に停止を確認する。
/// comparatorの順序は取消によって変更せず、本文対応を一体で移動する。
///
/// # 引数
/// * `input` - 全件検証済みの本文所有者
/// * `stop` - 取得から共有する絶対期限と取消context
///
/// # 戻り値
/// 順序確定済みの計画、または安全な失敗。失敗時は本文領域を回収する。
///
pub(crate) async fn finalize(input: ValidatedInput, stop: StopContext)
    -> Result<ProjectionPlan, ValidationError>
{
    let worker_stop = stop.clone();
    // threadに依存する試験設定はworker開始前に取り出す。
    #[cfg(test)]
    let sort_action = tests::take_sort_action();
    /*
     * metadataだけを整列し、比較中には共有contextを参照しない
     */
    let result = source::run_blocking(worker_stop, move |context| {
        /*
         * 全件検証済みの所有入力からmetadataを組み立てる
         */
        let mut plan = match assemble(input, &context) {
            Ok(plan) => plan,
            Err(error) => return Ok(Err(error)),
        };
        /*
         * 本文位置をmetadataと一体で整列し、直後の停止を拒否する
         */
        context.checkpoint()?;
        plan.pages.sort_unstable_by(|left, right| {
            let left = left.identity.path.as_str().as_bytes();
            let right = right.identity.path.as_str().as_bytes();
            depth(left).cmp(&depth(right)).then_with(|| left.cmp(right))
        });
        #[cfg(test)]
        tests::after_sort(&context, &plan, sort_action);
        context.checkpoint()?;
        Ok(Ok(plan))
    }).await.map_err(ValidationError::Source)?;
    /*
     * worker返却後にも検出済みの停止を拒否し、計画を公開する
     */
    #[cfg(test)]
    tests::before_publish(&stop);
    stop.checkpoint().map_err(ValidationError::Source)?;
    result
}

///
/// 検証済み絶対パスのsegment深さを取得する。
///
/// # 引数
/// * `path` - 先頭slash以外に空segmentを持たないUTF-8 bytes
///
/// # 戻り値
/// 最終絶対パスの深さ
///
fn depth(path: &[u8]) -> usize {
    path.iter().filter(|value| **value == b'/').count()
}

///
/// 四入力方式の取得から検証・順序確定まで同じ停止境界で実行する。
///
/// # 引数
/// * `config` - 検証済み設定。起動警告の所有者は呼出し側
/// * `stop` - 取得・全検証・整列で共有する取消context
///
/// # 戻り値
/// 全件検証・順序確定済み計画。途中失敗は公開せず所有領域を回収する。
///
pub(crate) async fn acquire_and_plan(config: &ResolvedRunConfig,
    stop: StopContext) -> Result<ProjectionPlan, ValidationError>
{
    let mut guard = CancelOnDrop::new(stop.clone());
    let input = validation::acquire_and_validate_with_stop(config,
        stop.clone()).await?;
    let plan = finalize(input, stop.clone()).await?;
    stop.checkpoint().map_err(ValidationError::Source)?;
    guard.disarm();
    Ok(plan)
}

///
/// 取得から計画までの失敗を本文・秘密値を持たないReport分類へ写す。
///
/// # 引数
/// * `config` - 採用値と認証を検証済みの設定
/// * `stop` - CLIのReport生成まで共有する停止context
///
/// # 戻り値
/// 確定計画、またはReportへ渡せる安全な失敗
///
pub(crate) async fn acquire_and_plan_for_report(config: &ResolvedRunConfig,
    stop: StopContext) -> Result<ProjectionPlan, Failure>
{
    acquire_and_plan(config, stop).await
        .map_err(ValidationError::into_failure)
}

#[cfg(test)]
mod tests;
