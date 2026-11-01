/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 全entryを構造検証してから取得するZIP/ZIP64アダプター
//!

mod raw;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::Path;

use ::zip::{HasZipMetadata, ZipArchive};

use crate::report::types::SourceKind;

use super::store::{Builder, validate_name};
use super::{AcquisitionLimits, EntryKind, SourceError};
use super::{SourceSnapshot, StopContext};

///
/// 全ZIP entryを検査し、本文を専用領域へ保管する。
///
/// # 概要
/// 生の全件検査を解凍前に終え、chunk読込と回収をbuilderに委ねる。
///
/// # 引数
/// * `path` - ローカルZIP
/// * `limits` - 取得上限
/// * `stop` - 期限と停止境界
///
/// # 戻り値
/// 取得済みsnapshot。本文UTF-8と投影の最終検証はMS3.2で行う。
///
pub(super) fn acquire(path: &Path, limits: AcquisitionLimits,
    stop: &StopContext) -> Result<SourceSnapshot, SourceError>
{
    let bytes = read_archive(path, limits.zip_bytes, stop)?;
    let footer = raw::footer(&bytes, limits.zip_entries, stop)?;
    let entries = raw::entries(&bytes, &footer, limits, stop)?;
    let mut seen = HashSet::new();
    for entry in &entries {
        if !seen.insert(entry.name.as_slice()) {
            return Err(SourceError::Duplicate);
        }
    }
    let mut archive = ZipArchive::new(Cursor::new(&bytes))
        .map_err(|_| SourceError::ZipStructure)?;
    if entries.len() != archive.len()
        || archive.central_directory_start() != footer.start as u64
        || archive.offset() != footer.base
    { return Err(SourceError::ZipStructure); }
    let mut kinds = Vec::new();

    /*
     * 解凍前に生名・API metadata・種別を全件照合する
     */
    for (index, entry) in entries.iter().enumerate() {
        stop.checkpoint()?;
        let file = archive.by_index_raw(index)
            .map_err(|_| SourceError::ZipStructure)?;
        if file.name_raw() != entry.name
            || file.get_metadata().flags != entry.flags
            || file.size() != entry.size
            || file.compressed_size() != entry.compressed
            || file.crc32() != entry.crc
        { return Err(SourceError::ZipStructure); }
        if file.is_symlink() { return Err(SourceError::Symlink); }
        let mode = file.unix_mode().unwrap_or(0) & 0o170000;
        if !matches!(mode, 0 | 0o100000 | 0o040000) {
            return Err(SourceError::ZipUnsupported);
        }
        let kind = if entry.name.ends_with(b"/") || mode == 0o040000 {
            EntryKind::Directory
        } else { EntryKind::File };
        validate_name(&entry.name, kind)?;
        if !entry.name.is_ascii() && entry.flags & (1 << 11) == 0 {
            return Err(SourceError::InvalidUtf8);
        }
        kinds.push(kind);
    }

    /*
     * 本文を変換せず保存し、実読込量を再度制限する
     */
    let mut builder = Builder::new(SourceKind::Zip, limits.zip_expanded,
        "max_zip_uncompressed_bytes", limits.page_bytes)?;
    for (index, entry) in entries.iter().enumerate() {
        stop.checkpoint()?;
        let mut file = archive.by_index(index).map_err(|_| SourceError::Io)?;
        builder.append(&entry.name, kinds[index], &mut file, entry.size, stop)?;
    }
    builder.finish(stop)
}

///
/// ZIP全体を上限と停止点を守って読み取る。
///
/// # 概要
/// metadataと実読込量の両方を照合し、圧縮データだけを上限内で保持する。
///
/// # 引数
/// * `path` - ZIPファイル
/// * `maximum` - 全体サイズ上限
/// * `stop` - 停止境界
///
/// # 戻り値
/// 上限内の圧縮データ。展開本文はこのVecへ常駐させない。
///
fn read_archive(path: &Path, maximum: u64, stop: &StopContext)
    -> Result<Vec<u8>, SourceError>
{
    stop.checkpoint()?;
    let before = fs::metadata(path).map_err(|_| SourceError::Io)?;
    if !before.is_file() { return Err(SourceError::InvalidPath); }
    if before.len() > maximum {
        return Err(SourceError::Limit("max_zip_compressed_bytes"));
    }
    let mut file = File::open(path).map_err(|_| SourceError::Io)?;
    let metadata = file.metadata().map_err(|_| SourceError::Io)?;
    if !metadata.is_file() { return Err(SourceError::InvalidPath); }
    if metadata.len() > maximum {
        return Err(SourceError::Limit("max_zip_compressed_bytes"));
    }
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    /*
     * サイズ変更や停止を読みながら確認する
     */
    loop {
        stop.checkpoint()?;
        let count = file.read(&mut buffer).map_err(|_| SourceError::Io)?;
        if count == 0 { break; }
        let size = (bytes.len() as u64).checked_add(count as u64)
            .ok_or(SourceError::ZipStructure)?;
        if size > maximum {
            return Err(SourceError::Limit("max_zip_compressed_bytes"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 != metadata.len() {
        return Err(SourceError::Changed);
    }
    stop.checkpoint()?;
    Ok(bytes)
}
