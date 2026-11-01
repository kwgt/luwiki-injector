/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 全entryの名前検査とWikiページ投影。本文検証は後続境界で行う。
//!

mod body;

pub(crate) use body::{BodyLimits, ValidatedInput, validate_bodies};
// MS3.3の計画生成がページmetadata型を利用する。
#[allow(unused_imports)]
pub(crate) use body::ValidatedPage;

use std::collections::HashSet;
use std::fmt;
use std::fs::File;

use crate::cmd_args::ResolvedRunConfig;
use crate::cmd_args::resolve::RunLimits;
use crate::report::Failure;
use crate::report::types::{PageIdentity, PagePath, SourceReference};
use crate::source::{self, EntryKind, SourceError, SourceSnapshot, StopContext};

///
/// 名前と投影の失敗分類。入力値や本文を保持しない。
///
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ValidationError {
    /// ソース最上位に取込rootがない。
    MissingRoot,
    /// 取込rootがdirectoryとして扱えない。
    InvalidRoot,
    InvalidPrefix, EmptyName, RawDuplicate, TargetDuplicate, PageLimit,
    ReferenceMismatch, Source(SourceError),
    BodyUtf8, BodyReplacement, BodyLimit,
}

impl ValidationError {
    ///
    /// 共通検査の失敗を既存の報告契約へ写す。
    ///
    /// # 戻り値
    /// 取得境界の分類、またはWiki変更前の入力エラー
    ///
    pub(crate) fn into_failure(self) -> Failure {
        match self {
            Self::Source(error) => error.into_failure(),
            _ => Failure::Input,
        }
    }

    ///
    /// 公開可能な固定分類を取得する。
    ///
    /// # 戻り値
    /// 元の値やSourceErrorの上限名を含めない分類
    ///
    fn classification(&self) -> &'static str {
        match self {
            /*
             * 入力値を含めず、rootと既存の拒否分類を固定語彙へ写す
             */
            Self::MissingRoot => "MissingRoot",
            Self::InvalidRoot => "InvalidRoot",
            Self::InvalidPrefix => "InvalidPrefix",
            Self::EmptyName => "EmptyName",
            Self::RawDuplicate => "RawDuplicate",
            Self::TargetDuplicate => "TargetDuplicate",
            Self::PageLimit => "PageLimit",
            Self::ReferenceMismatch => "ReferenceMismatch",
            Self::BodyUtf8 => "BodyUtf8",
            Self::BodyReplacement => "BodyReplacement",
            Self::BodyLimit => "BodyLimit",
            Self::Source(_) => "Source",
        }
    }
}

impl fmt::Debug for ValidationError {
    ///
    /// 入力値を含めずに失敗を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 固定分類の表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.classification())
    }
}

impl fmt::Display for ValidationError {
    ///
    /// 入力値を含めずに失敗を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 固定分類の表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ページ投影に失敗しました: {}",
            self.classification())
    }
}

impl std::error::Error for ValidationError {}

///
/// 検証済み設定から取り出す投影ページ件数上限。
///
#[derive(Clone, Copy)]
pub(crate) struct ProjectionLimits {
    max_pages: u64,
}

impl From<&RunLimits> for ProjectionLimits {
    ///
    /// 検証済み設定からページ件数上限を取り出す。
    ///
    /// # 引数
    /// * `limits` - 設定解決で検証済みの上限
    ///
    /// # 戻り値
    /// 投影境界用の上限
    ///
    fn from(limits: &RunLimits) -> Self {
        Self { max_pages: limits.max_pages }
    }
}

///
/// 名前と投影を検査済みの候補。本文の妥当性はまだ保証しない。
///
pub(crate) struct ProjectedPage {
    identity: PageIdentity,
    source_index: usize,
    expected_bytes: u64,
}

impl ProjectedPage {
    ///
    /// 投影先と安全な入力参照を取得する。
    ///
    /// # 戻り値
    /// 名前検査済みの識別子
    ///
    pub(crate) fn identity(&self) -> &PageIdentity { &self.identity }

    ///
    /// 元snapshot内のentry位置を取得する。
    ///
    /// # 戻り値
    /// entryの位置
    ///
    pub(crate) fn source_index(&self) -> usize { self.source_index }

    ///
    /// 取得時の本文サイズを取得する。
    ///
    /// # 戻り値
    /// 未検証本文の期待サイズ（bytes）
    ///
    pub(crate) fn expected_bytes(&self) -> u64 { self.expected_bytes }
}

impl fmt::Debug for ProjectedPage {
    ///
    /// 識別子や本文を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 本文サイズだけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProjectedPage")
            .field("bytes", &self.expected_bytes).finish()
    }
}

///
/// 全名前・投影検査成功時だけ公開する、本文未検証の所有された入力。
///
pub(crate) struct ProjectedInput {
    snapshot: SourceSnapshot,
    pages: Vec<ProjectedPage>,
}

impl ProjectedInput {
    ///
    /// 投影候補を取得する。投入順はまだ確定していない。
    ///
    /// # 戻り値
    /// 名前・投影検査済みの全候補
    ///
    pub(crate) fn pages(&self) -> &[ProjectedPage] { &self.pages }

    ///
    /// snapshotが所有する未検証本文を開く。
    ///
    /// # 引数
    /// * `page_index` - 投影候補の位置
    ///
    /// # 戻り値
    /// 読込handle、または値を含めない失敗。不正位置は拒否する。
    ///
    pub(crate) fn open_unvalidated_body(&self, page_index: usize)
        -> Result<File, ValidationError>
    {
        let page = self.pages.get(page_index)
            .ok_or(ValidationError::Source(SourceError::InvalidPath))?;
        self.snapshot.open_body(page.source_index)
            .map_err(ValidationError::Source)
    }
}

impl fmt::Debug for ProjectedInput {
    ///
    /// 元名・本文・保管先を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 件数だけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProjectedInput")
            .field("pages", &self.pages.len()).finish()
    }
}

///
/// 所有された入力の全名前を検査してからページへ投影する。
///
/// # 引数
/// * `snapshot` - 取得済みentryと本文の所有者
/// * `prefix` - 設定で採用された絶対Wikiプレフィクス
/// * `limits` - 投影件数上限
/// * `stop` - 全体と共有する停止境界
///
/// # 戻り値
/// 全成功時だけ返す本文未検証の入力。整列は後続境界で行う。
///
pub(crate) async fn project(snapshot: SourceSnapshot, prefix: String,
    limits: ProjectionLimits, stop: StopContext)
    -> Result<ProjectedInput, ValidationError>
{
    source::run_blocking(stop, move |stop| {
        Ok(project_inner(snapshot, &prefix, limits, &stop))
    }).await.map_err(ValidationError::Source)?
}

///
/// 取得済みsnapshotの名前・投影と全候補本文を検証する。
///
/// # 引数
/// * `snapshot` - 取得済み本文とentryの所有者
/// * `prefix` - 採用されたWikiプレフィクス
/// * `limits` - 設定解決で検証済みの上限
/// * `stop` - 全phaseで共有する絶対期限と取消境界
///
/// # 戻り値
/// 全件検証済みの所有された本文。投入順はまだ確定しない。
///
pub(crate) async fn validate_snapshot(snapshot: SourceSnapshot, prefix: String,
    limits: &RunLimits, stop: StopContext)
    -> Result<ValidatedInput, ValidationError>
{
    let projected = project(snapshot, prefix, ProjectionLimits::from(limits),
        stop.clone()).await?;
    validate_bodies(projected, BodyLimits::from(limits), stop).await
}

///
/// 検証済み設定から取得と全共通検証を同じ停止境界で実行する。
///
/// # 引数
/// * `config` - 採用値・上限・絶対期限を検証済みの設定
///
/// # 戻り値
/// 全本文検証済み入力、または値を含めない失敗
///
pub(crate) async fn acquire_and_validate(config: &ResolvedRunConfig)
    -> Result<ValidatedInput, ValidationError>
{
    let stop = StopContext::new(config.deadline());
    acquire_and_validate_with_stop(config, stop).await
}

///
/// 外側の停止境界を取得から全共通検証まで共有する。
///
/// # 引数
/// * `config` - 検証済み入力指定・上限・prefix・認証
/// * `stop` - 計画確定まで同じ取消フラグを共有するcontext
///
/// # 戻り値
/// 全本文検証済み入力、または値を含めない失敗
///
pub(crate) async fn acquire_and_validate_with_stop(
    config: &ResolvedRunConfig, stop: StopContext,
) -> Result<ValidatedInput, ValidationError> {
    stop.checkpoint().map_err(ValidationError::Source)?;
    let limits = &config.settings().limits;
    let snapshot = source::acquire(config.source().clone(),
        config.github_token(), source::AcquisitionLimits::from(limits),
        stop.clone()).await.map_err(ValidationError::Source)?;
    validate_snapshot(snapshot, config.settings().prefix.clone(), limits,
        stop).await
}

///
/// 取得と全共通検証の失敗を既存の報告契約へ写す。
///
/// # 引数
/// * `config` - 検証済み設定。起動警告の保持は呼出し側が担当する。
///
/// # 戻り値
/// 全本文検証済み入力、またはReportへ渡せる失敗
///
pub(crate) async fn acquire_and_validate_for_report(config: &ResolvedRunConfig)
    -> Result<ValidatedInput, Failure>
{
    acquire_and_validate(config).await.map_err(ValidationError::into_failure)
}

///
/// 全entryの安全検査後に小文字.mdの通常ファイルだけを投影する。
///
/// # 概要
/// 途中結果を外へ公開せず、失敗時はsnapshotのleaseも回収する。
/// 本文を読まないため、UTF-8・サイズの本文共通検証は後続へ残る。
///
/// # 引数
/// * `snapshot` - 生名と本文の所有者
/// * `prefix` - 再検査する絶対Wikiプレフィクス
/// * `limits` - ページ件数上限
/// * `stop` - 協調停止境界
///
/// # 戻り値
/// 全件の名前と投影を検査した入力、または安全な失敗分類
///
fn project_inner(snapshot: SourceSnapshot, prefix: &str,
    limits: ProjectionLimits, stop: &StopContext)
    -> Result<ProjectedInput, ValidationError>
{
    /*
     * 候補抽出前にprefixと全entryの生名・参照を検査する
     */
    stop.checkpoint().map_err(ValidationError::Source)?;
    PagePath::new(prefix).map_err(|_| ValidationError::InvalidPrefix)?;
    let mut raw_names = HashSet::new();
    let mut root_present = false;
    for entry in snapshot.entries() {
        stop.checkpoint().map_err(ValidationError::Source)?;
        let name = source::validate_entry_name(entry.name_raw(), entry.kind())
            .map_err(ValidationError::Source)?;
        if !raw_names.insert(entry.name_raw()) {
            return Err(ValidationError::RawDuplicate);
        }
        validate_reference(name, entry.reference())?;
        /*
         * 明示rootか、その配下のentryから取込directoryの存在を確定する
         */
        if name == source::PAGE_ROOT {
            if entry.kind() != EntryKind::Directory {
                return Err(ValidationError::InvalidRoot);
            }
            root_present = true;
        } else if source::root_relative_name(name).is_some() {
            root_present = true;
        }
    }
    drop(raw_names);
    if !root_present { return Err(ValidationError::MissingRoot); }

    /*
     * 名前の変換をせず、最後の拡張子だけを除いて投影する
     */
    let mut pages = Vec::new();
    let mut targets = HashSet::new();
    for (source_index, entry) in snapshot.entries().iter().enumerate() {
        stop.checkpoint().map_err(ValidationError::Source)?;
        if entry.kind() != EntryKind::File { continue; }
        let name = entry.reference().parts().1;
        let Some(relative) = source::root_relative_name(name) else {
            continue;
        };
        let Some(stem) = relative.strip_suffix(".md") else { continue; };
        if stem.rsplit('/').next().is_none_or(str::is_empty) {
            return Err(ValidationError::EmptyName);
        }
        /*
         * prefix結合後の最終パスと重複・件数を検査する
         */
        let target = if prefix == "/" { format!("/{stem}") }
            else { format!("{prefix}/{stem}") };
        let path = PagePath::new(&target)
            .map_err(|_| ValidationError::Source(SourceError::InvalidPath))?;
        guard_target(&mut targets, path.as_str(), limits)?;
        pages.push(ProjectedPage {
            identity: PageIdentity { path, source: entry.reference().clone() },
            source_index, expected_bytes: entry.byte_len(),
        });
    }
    stop.checkpoint().map_err(ValidationError::Source)?;
    Ok(ProjectedInput { snapshot, pages })
}

///
/// 生名の検査結果と報告用参照が同じentryを示すか確認する。
///
/// # 引数
/// * `name` - directory末尾slashだけを除いた検査済み名
/// * `reference` - snapshot内の報告用参照
///
/// # 戻り値
/// 完全一致なら成功
///
fn validate_reference(name: &str, reference: &SourceReference)
    -> Result<(), ValidationError>
{
    if name != reference.parts().1 {
        return Err(ValidationError::ReferenceMismatch);
    }
    Ok(())
}

///
/// 投影先重複と件数上限を検査する。
///
/// # 引数
/// * `targets` - 既に投影した最終パス。成功時だけ追加する。
/// * `target` - 再検査済みの最終パス
/// * `limits` - ページ件数上限
///
/// # 戻り値
/// 新しい投影先が上限内なら成功
///
fn guard_target(targets: &mut HashSet<String>, target: &str,
    limits: ProjectionLimits) -> Result<(), ValidationError>
{
    if targets.contains(target) {
        return Err(ValidationError::TargetDuplicate);
    }
    if targets.len() as u64 >= limits.max_pages {
        return Err(ValidationError::PageLimit);
    }
    targets.insert(target.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests;
