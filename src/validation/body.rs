/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 全候補本文の厳格検証と所有された連番ファイルへの不変保管
//!

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

use crate::cmd_args::resolve::RunLimits;
use crate::measurement::Area as TempDir;
use crate::report::types::PageIdentity;
use crate::source::{self, SourceError, StopContext};

use super::{ProjectedInput, ValidationError};

///
/// 共通本文検証で実読込量へ適用するページ単位上限。
///
#[derive(Clone, Copy)]
pub(crate) struct BodyLimits {
    max_page_bytes: u64,
}

impl From<&RunLimits> for BodyLimits {
    ///
    /// 検証済み設定からページ本文上限を取り出す。
    ///
    /// # 引数
    /// * `limits` - 設定解決で検証済みの上限
    ///
    /// # 戻り値
    /// 共通本文検証用の上限
    ///
    fn from(limits: &RunLimits) -> Self {
        Self { max_page_bytes: limits.max_page_bytes }
    }
}

///
/// 検証済み本文の連番ファイルを所有する専用一時領域。
///
struct PayloadLease(TempDir);

///
/// 全本文検証済みのページ識別子・実サイズと専用領域内参照。
///
pub(crate) struct ValidatedPage {
    identity: PageIdentity,
    byte_len: u64,
    ordinal: usize,
}

impl ValidatedPage {
    ///
    /// 名前・投影検査済みの識別子を取得する。
    ///
    /// # 戻り値
    /// 投影先と安全な元入力参照
    ///
    pub(crate) fn identity(&self) -> &PageIdentity { &self.identity }

    ///
    /// 検証・保管済み本文の実サイズを取得する。
    ///
    /// # 戻り値
    /// 本文バイト数（bytes）
    ///
    pub(crate) fn byte_len(&self) -> u64 { self.byte_len }
}

impl fmt::Debug for ValidatedPage {
    ///
    /// 元名・本文・保管先を含めず実サイズだけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 安全な表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ValidatedPage")
            .field("bytes", &self.byte_len).finish()
    }
}

///
/// 全共通検証成功時だけ公開する所有された不変本文。整列前の型。
///
pub(crate) struct ValidatedInput {
    pages: Vec<ValidatedPage>,
    lease: PayloadLease,
}

impl ValidatedInput {
    ///
    /// 全本文検証済みページを参照する。
    ///
    /// # 戻り値
    /// 入力候補順のページ群。投入順は後続で確定する。
    ///
    pub(crate) fn pages(&self) -> &[ValidatedPage] { &self.pages }

    ///
    /// 元入力を再読込せず、保管済み本文を読込専用で開く。
    ///
    /// # 引数
    /// * `index` - 検証済みページの位置
    ///
    /// # 戻り値
    /// 読込専用handle。不正位置とI/Oは値を含めず拒否する。
    ///
    pub(crate) fn open_body(&self, index: usize)
        -> Result<File, ValidationError>
    {
        let page = self.pages.get(index)
            .ok_or(ValidationError::Source(SourceError::InvalidPath))?;
        File::open(self.lease.0.path().join(page.ordinal.to_string()))
            .map_err(|_| ValidationError::Source(SourceError::Io))
    }
}

impl fmt::Debug for ValidatedInput {
    ///
    /// 本文・識別子・保管先を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 件数だけの表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ValidatedInput")
            .field("pages", &self.pages.len()).finish()
    }
}

///
/// 名前・投影検査済み入力を消費して全候補本文を検証する。
///
/// # 引数
/// * `projected` - 全entry名と投影先検査済みの所有された入力
/// * `limits` - ページ本文の実量上限
/// * `stop` - 全phaseで共有する絶対期限と取消境界
///
/// # 戻り値
/// 全成功時だけ返す検証済み本文。失敗時は両leaseを回収する。
///
pub(crate) async fn validate_bodies(projected: ProjectedInput,
    limits: BodyLimits, stop: StopContext)
    -> Result<ValidatedInput, ValidationError>
{
    source::run_blocking(stop, move |stop| {
        let area = crate::measurement::area(
            crate::measurement::Role::Payload).map_err(|_| SourceError::Io)?;
        Ok(validate_inner(projected, area, limits, &stop))
    }).await.map_err(ValidationError::Source)?
}

///
/// 所有されたsnapshotの各候補を専用領域に検証・保管する。
///
/// # 概要
/// 1ページ分だけをメモリに保持し、本文成功後にmetadataを再照合する。
/// 未完成の型は公開せず、最終停止確認後にsnapshotを解放する。
///
/// # 引数
/// * `projected` - 名前・投影検査済み入力
/// * `area` - この処理が所有する空の専用本文領域
/// * `limits` - ページ本文上限
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 全本文を検証・保管済みの入力、または安全な失敗分類
///
fn validate_inner(projected: ProjectedInput, area: impl Into<TempDir>,
    limits: BodyLimits, stop: &StopContext)
    -> Result<ValidatedInput, ValidationError>
{
    let area = area.into();
    let mut pages = Vec::new();
    /*
     * 元名を保管パスに使わず、全候補を順に検証・保管する
     */
    for (ordinal, page) in projected.pages().iter().enumerate() {
        stop.checkpoint().map_err(ValidationError::Source)?;
        let input = projected.snapshot.open_body(page.source_index())
            .map_err(ValidationError::Source)?;
        let mut input = crate::measurement::file(input,
            crate::measurement::Role::Source);
        let before = input.metadata()
            .map_err(|_| ValidationError::Source(SourceError::Io))?;
        if !before.is_file() || before.len() != page.expected_bytes() {
            return Err(ValidationError::Source(SourceError::Changed));
        }
        /*
         * 実量と本文を検査し、取得済みmetadataの変化を再確認する
         */
        let output = OpenOptions::new().write(true).create_new(true)
            .open(area.path().join(ordinal.to_string()))
            .map_err(|_| ValidationError::Source(SourceError::Io))?;
        let mut output = crate::measurement::file(output,
            crate::measurement::Role::Payload);
        let byte_len = copy_body(&mut input, &mut output,
            page.expected_bytes(), limits, stop)?;
        // metadata再照合直前の変化・停止注入は試験だけで有効にする。
        #[cfg(test)]
        tests::after_copy(&mut input, stop);
        let after = input.metadata()
            .map_err(|_| ValidationError::Source(SourceError::Io))?;
        if !after.is_file() || after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
        {
            return Err(ValidationError::Source(SourceError::Changed));
        }
        pages.push(ValidatedPage {
            identity: page.identity().clone(), byte_len, ordinal,
        });
    }
    /*
     * 全成功時だけ取得元を解放して検証済み所有者を公開する
     */
    stop.checkpoint().map_err(ValidationError::Source)?;
    drop(projected);
    Ok(ValidatedInput { pages, lease: PayloadLease(area) })
}

///
/// 上限以内の1本文をchunk読込し、厳格検証後にそのまま保存する。
///
/// # 概要
/// chunkをまたぐUTF-8列も全体で検査し、本文を変換しない。
/// generic I/Oは失敗注入試験にも用いるが、生のエラーを保持しない。
///
/// # 引数
/// * `input` - snapshot由来の本文reader
/// * `output` - 専用領域の連番writer
/// * `expected` - 取得時実読込サイズ（bytes）
/// * `limits` - ページ本文の実量上限
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 検証・保管済みの実本文サイズ（bytes）、または安全な分類
///
fn copy_body(input: &mut impl Read, output: &mut impl Write, expected: u64,
    limits: BodyLimits, stop: &StopContext) -> Result<u64, ValidationError>
{
    let mut bytes = Vec::new();
    let mut total = 0_u64;
    let mut chunk = [0_u8; 64 * 1024];
    /*
     * chunk間で停止と実量上限を確認し、1ページ分だけを保持する
     */
    loop {
        stop.checkpoint().map_err(ValidationError::Source)?;
        let count = input.read(&mut chunk)
            .map_err(|_| ValidationError::Source(SourceError::Io))?;
        stop.checkpoint().map_err(ValidationError::Source)?;
        if count == 0 { break; }
        total = total.checked_add(count as u64)
            .ok_or(ValidationError::BodyLimit)?;
        if total > limits.max_page_bytes {
            return Err(ValidationError::BodyLimit);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    /*
     * 期待実量と厳格UTF-8・置換文字を検査し、bytesを変えず保存する
     */
    if total != expected {
        return Err(ValidationError::Source(SourceError::Changed));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| ValidationError::BodyUtf8)?;
    if text.contains('\u{fffd}') {
        return Err(ValidationError::BodyReplacement);
    }
    for chunk in bytes.chunks(64 * 1024) {
        stop.checkpoint().map_err(ValidationError::Source)?;
        output.write_all(chunk)
            .map_err(|_| ValidationError::Source(SourceError::Io))?;
        stop.checkpoint().map_err(ValidationError::Source)?;
    }
    output.flush().map_err(|_| ValidationError::Source(SourceError::Io))?;
    stop.checkpoint().map_err(ValidationError::Source)?;
    Ok(total)
}

#[cfg(test)]
mod tests;
