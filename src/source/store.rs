/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 元ファイル名を展開先に使わず、本文を専用領域へ不変に保存する
//!

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Write};

use crate::report::types::{SourceKind, SourceReference};

use super::{EntryKind, SourceEntry, SourceError, SourceLease};
use super::{SourceSnapshot, StopContext};

///
/// 未公開のsnapshotを構築する所有者。
///
pub(super) struct Builder {
    lease: SourceLease,
    entries: Vec<SourceEntry>,
    seen: HashSet<Vec<u8>>,
    total: u64,
    maximum: u64,
    limit_name: &'static str,
    page_bytes: u64,
    source_kind: SourceKind,
}

impl Builder {
    ///
    /// 専用領域と累積上限を用意する。
    ///
    /// # 引数
    /// * `kind` - 入力方式
    /// * `maximum` - 累積保管量の最大バイト数
    /// * `limit_name` - 安全な上限名
    /// * `page_bytes` - Markdownの最大バイト数
    ///
    /// # 戻り値
    /// 未公開の所有者
    ///
    pub(super) fn new(kind: SourceKind, maximum: u64,
        limit_name: &'static str, page_bytes: u64) -> Result<Self, SourceError>
    {
        let directory = crate::measurement::area(
            crate::measurement::Role::Source).map_err(|_| SourceError::Io)?;
        Ok(Self::with_directory(kind, maximum, limit_name, page_bytes,
            directory))
    }

    ///
    /// リモート取得領域内に本文を保管し、全体計量の対象に含める。
    ///
    /// # 引数
    /// * `parent` - 取得全体の所有者が保持する親領域
    /// * `maximum` - Git取得上限（bytes）
    /// * `page_bytes` - Markdown本文上限（bytes）
    /// * `kind` - HTTPS GitまたはGitHubの入力方式
    ///
    /// # 戻り値
    /// HTTPS Git用の未公開builder
    ///
    pub(super) fn in_acquisition(parent: &std::path::Path, maximum: u64,
        page_bytes: u64, kind: SourceKind) -> Result<Self, SourceError>
    {
        let directory = crate::measurement::area_in(parent,
            crate::measurement::Role::Source)
            .map_err(|_| SourceError::Io)?;
        Ok(Self::with_directory(kind, maximum,
            "max_git_bytes", page_bytes, directory))
    }

    ///
    /// 保管領域と方式固有の上限から未公開builderを構築する。
    ///
    /// # 引数
    /// * `kind` - 入力方式
    /// * `maximum` - 累積サイズ上限（bytes）
    /// * `limit_name` - 公開可能な上限名
    /// * `page_bytes` - Markdown本文上限（bytes）
    /// * `directory` - 所有する保管領域
    ///
    /// # 戻り値
    /// entryをまだ持たないbuilder
    ///
    fn with_directory(kind: SourceKind, maximum: u64,
        limit_name: &'static str, page_bytes: u64,
        directory: crate::measurement::Area) -> Self
    {
        Self {
            lease: SourceLease(directory), entries: Vec::new(),
            seen: HashSet::new(), total: 0,
            maximum, limit_name, page_bytes, source_kind: kind,
        }
    }

    ///
    /// entryを小分けに読み、検査済み取得元へ保存する。
    ///
    /// # 概要
    /// 本文を全件メモリ常駐させず、名前に関係しない連番へ保存する。
    ///
    /// # 引数
    /// * `raw` - 生の相対名
    /// * `kind` - entry種別
    /// * `reader` - 本文取得元
    /// * `expected` - 宣言またはmetadataによるサイズ
    /// * `stop` - 期限と協調停止境界
    ///
    /// # 戻り値
    /// 保存の成否。途中失敗ではbuilderを公開しない。
    ///
    pub(super) fn append(&mut self, raw: &[u8], kind: EntryKind,
        reader: &mut impl Read, expected: u64, stop: &StopContext)
        -> Result<(), SourceError>
    {
        stop.checkpoint()?;
        let name = validate_name(raw, kind)?;
        if !self.seen.insert(raw.to_vec()) {
            return Err(SourceError::Duplicate);
        }
        let reference = SourceReference::new(self.source_kind, name)
            .map_err(|_| SourceError::InvalidPath)?;
        let markdown = kind == EntryKind::File && super::is_page_name(name);
        self.check_size(expected, markdown)?;
        let ordinal = self.entries.len();
        let mut output = if kind == EntryKind::File {
            Some(OpenOptions::new().write(true).create_new(true)
                .open(self.lease.0.path().join(ordinal.to_string()))
                .map(|file| crate::measurement::file(file,
                    crate::measurement::Role::Source))
                .map_err(|_| SourceError::Io)?)
        } else { None };

        /*
         * 実読込量を宣言値とは独立に監視する
         */
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            stop.checkpoint()?;
            let count = reader.read(&mut buffer).map_err(|_| SourceError::Io)?;
            stop.checkpoint()?;
            if count == 0 { break; }
            size = size.checked_add(count as u64)
                .ok_or(SourceError::ZipStructure)?;
            self.check_size(size, markdown)?;
            if let Some(file) = &mut output {
                file.write_all(&buffer[..count]).map_err(|_| SourceError::Io)?;
            }
        }
        if size != expected { return Err(SourceError::Changed); }
        if let Some(file) = &mut output {
            file.flush().map_err(|_| SourceError::Io)?;
        }
        self.total += size;
        self.entries.push(SourceEntry {
            raw_name: raw.to_vec(), kind, byte_len: size, reference,
            body: if kind == EntryKind::File { Some(ordinal) } else { None },
        });
        Ok(())
    }

    ///
    /// 次のentryが累積/本文上限に収まるか確認する。
    ///
    /// # 引数
    /// * `size` - 次のentryのバイト数
    /// * `markdown` - Markdown本文か
    ///
    /// # 戻り値
    /// 上限内なら成功
    ///
    pub(super) fn check_size(&self, size: u64, markdown: bool)
        -> Result<(), SourceError>
    {
        if self.total.checked_add(size).is_none_or(|v| v > self.maximum) {
            return Err(SourceError::Limit(self.limit_name));
        }
        if markdown && size > self.page_bytes {
            return Err(SourceError::Limit("max_page_bytes"));
        }
        Ok(())
    }

    ///
    /// 全取得成功時だけsnapshotを公開する。
    ///
    /// # 引数
    /// * `stop` - 最終停止確認の境界
    ///
    /// # 戻り値
    /// entryと本文保管先を一体で所有するsnapshot
    ///
    pub(super) fn finish(self, stop: &StopContext)
        -> Result<SourceSnapshot, SourceError>
    {
        stop.checkpoint()?;
        Ok(SourceSnapshot {
            entries: self.entries, lease: self.lease, acquisition: None,
        })
    }

    ///
    /// 回収試験でだけ保管領域の寿命を観察する。
    ///
    /// # 戻り値
    /// テスト用の領域パス。製品の表示経路へは公開しない。
    ///
    #[cfg(test)]
    pub(super) fn storage_path(&self) -> std::path::PathBuf {
        self.lease.0.path().to_path_buf()
    }
}

///
/// entryの名前をlossy変換せずに検査する。
///
/// # 引数
/// * `raw` - 元の生バイト名
/// * `kind` - entry種別
///
/// # 戻り値
/// directoryの末尾slashだけを除いた検証済み相対名
///
pub(crate) fn validate_name(raw: &[u8], kind: EntryKind)
    -> Result<&str, SourceError>
{
    let value = std::str::from_utf8(raw).map_err(|_| SourceError::InvalidUtf8)?;
    let name = if kind == EntryKind::Directory {
        value.strip_suffix('/').unwrap_or(value)
    } else { value };
    if name.starts_with('/') || name.contains('\\') || name.contains('\u{fffd}')
        || name.chars().any(|v| v.is_whitespace() || v.is_control())
        || name.split('/').any(|v| matches!(v, "" | "." | ".."))
    {
        return Err(SourceError::InvalidPath);
    }
    Ok(name)
}
