/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! ZIP中央/ローカルheaderとZIP64・descriptorの範囲検証
//!

use super::super::{AcquisitionLimits, SourceError, StopContext};

const CENTRAL: &[u8] = b"PK\x01\x02";
const LOCAL: &[u8] = b"PK\x03\x04";
const END: &[u8] = b"PK\x05\x06";
const END64: &[u8] = b"PK\x06\x06";
const LOCATOR: &[u8] = b"PK\x06\x07";
const DESCRIPTOR: u64 = 0x08074b50;
const U32_MAX: u64 = u32::MAX as u64;

///
/// 構造検証済みfooterと中央directoryの範囲。
///
pub(super) struct Footer {
    pub(super) start: usize,
    end: usize,
    pub(super) base: u64,
    count: usize,
}

///
/// 生の中央entryとローカル領域の終端。
///
pub(super) struct CentralEntry {
    pub(super) name: Vec<u8>,
    pub(super) flags: u16,
    pub(super) crc: u32,
    pub(super) compressed: u64,
    pub(super) size: u64,
    local: usize,
    end: usize,
}

///
/// 範囲内のバイト列だけを返す。
///
/// # 引数
/// * `bytes` - ZIPバイト列
/// * `at` - 開始位置
/// * `length` - 必要長
///
/// # 戻り値
/// 範囲内のスライス、または破損分類
///
fn field(bytes: &[u8], at: usize, length: usize)
    -> Result<&[u8], SourceError>
{
    let end = at.checked_add(length).ok_or(SourceError::ZipStructure)?;
    bytes.get(at..end).ok_or(SourceError::ZipStructure)
}

///
/// little endian整数を範囲確認して読む。
///
/// # 引数
/// * `bytes` - 対象バイト列
/// * `at` - 開始位置
/// * `width` - 2/4/8のいずれか
///
/// # 戻り値
/// u64に拡張した値
///
fn number(bytes: &[u8], at: usize, width: usize) -> Result<u64, SourceError> {
    let mut result = 0_u64;
    for (index, value) in field(bytes, at, width)?.iter().enumerate() {
        result |= u64::from(*value) << (index * 8);
    }
    Ok(result)
}

///
/// ZIP offsetの加算をusizeへ安全に変換する。
///
/// # 引数
/// * `base` - archive先頭の物理offset
/// * `relative` - ZIP内offset
///
/// # 戻り値
/// overflowのない位置
///
fn position(base: u64, relative: u64) -> Result<usize, SourceError> {
    usize::try_from(base.checked_add(relative)
        .ok_or(SourceError::ZipStructure)?)
        .map_err(|_| SourceError::ZipStructure)
}

///
/// EOCDとZIP64記録をライブラリのentry展開前に検査する。
///
/// # 概要
/// comment長とdisk/count/offsetを照合し、巨大entry数を先に拒否する。
///
/// # 引数
/// * `bytes` - サイズ上限内のZIPバイト列
/// * `maximum` - 許容entry数
/// * `stop` - 協調停止境界
///
/// # 戻り値
/// 中央directoryの検証済み範囲
///
pub(super) fn footer(bytes: &[u8], maximum: u64, stop: &StopContext)
    -> Result<Footer, SourceError>
{
    stop.checkpoint()?;
    let upper = bytes.len().checked_sub(22).ok_or(SourceError::ZipStructure)?;
    let lower = bytes.len().saturating_sub(22 + 65535);
    let at = (lower..=upper).rev().find(|at| {
        bytes.get(*at..*at + 4) == Some(END)
            && number(bytes, *at + 20, 2).is_ok_and(|n| {
                (*at + 22).checked_add(n as usize) == Some(bytes.len())
            })
    }).ok_or(SourceError::ZipStructure)?;
    if number(bytes, at + 4, 2)? != 0 || number(bytes, at + 6, 2)? != 0 {
        return Err(SourceError::ZipUnsupported);
    }
    let disk_count = number(bytes, at + 8, 2)?;
    let count = number(bytes, at + 10, 2)?;
    let length = number(bytes, at + 12, 4)?;
    let relative = number(bytes, at + 16, 4)?;
    if disk_count != count { return Err(SourceError::ZipStructure); }

    /*
     * ZIP64 locatorがある場合は物理記録と参照offsetを両方照合する
     */
    if at >= 20 && field(bytes, at - 20, 4)? == LOCATOR {
        if number(bytes, at - 16, 4)? != 0
            || number(bytes, at - 4, 4)? != 1
        { return Err(SourceError::ZipUnsupported); }
        let pointer = number(bytes, at - 12, 8)?;
        let mut found = None;
        for candidate in (0..at - 20).rev() {
            if candidate % 65536 == 0 { stop.checkpoint()?; }
            if bytes.get(candidate..candidate + 4) != Some(END64) { continue; }
            let record_size = number(bytes, candidate + 4, 8)?;
            if record_size < 44 { continue; }
            let record_end = 12_u64.checked_add(record_size)
                .and_then(|length| position(candidate as u64, length).ok());
            if record_end != Some(at - 20) { continue; }
            let count64 = number(bytes, candidate + 32, 8)?;
            let length64 = number(bytes, candidate + 40, 8)?;
            let relative64 = number(bytes, candidate + 48, 8)?;
            let Some(base) = (candidate as u64).checked_sub(pointer) else {
                continue;
            };
            if base.checked_add(relative64)
                .and_then(|v| v.checked_add(length64))
                != Some(candidate as u64)
            { continue; }
            if number(bytes, candidate + 16, 4)? != 0
                || number(bytes, candidate + 20, 4)? != 0
            { return Err(SourceError::ZipUnsupported); }
            if number(bytes, candidate + 24, 8)? != count64
                || (count != 65535 && count != count64)
                || (length != U32_MAX && length != length64)
                || (relative != U32_MAX && relative != relative64)
            { return Err(SourceError::ZipStructure); }
            let parsed = bounds(candidate, length64, relative64, count64,
                maximum)?;
            if position(parsed.base, pointer)? != candidate {
                return Err(SourceError::ZipStructure);
            }
            if found.is_some() { return Err(SourceError::ZipStructure); }
            found = Some(parsed);
        }
        return found.ok_or(SourceError::ZipStructure);
    }
    if count == 65535 || length == U32_MAX || relative == U32_MAX {
        return Err(SourceError::ZipStructure);
    }
    bounds(at, length, relative, count, maximum)
}

///
/// footerの長さと相対offsetから物理範囲を構築する。
///
/// # 引数
/// * `end` - 中央directoryの物理終端
/// * `length` - directoryの長さ
/// * `relative` - ZIP内開始位置
/// * `count` - 宣言entry数
/// * `maximum` - 許容entry数
///
/// # 戻り値
/// 範囲とarchive base
///
fn bounds(end: usize, length: u64, relative: u64, count: u64, maximum: u64)
    -> Result<Footer, SourceError>
{
    if count > maximum { return Err(SourceError::Limit("max_zip_entries")); }
    let length = usize::try_from(length)
        .map_err(|_| SourceError::ZipStructure)?;
    let start = end.checked_sub(length).ok_or(SourceError::ZipStructure)?;
    let base = (start as u64).checked_sub(relative)
        .ok_or(SourceError::ZipStructure)?;
    Ok(Footer { start, end, base,
        count: usize::try_from(count).map_err(|_| SourceError::ZipStructure)? })
}

///
/// 全extra fieldの長さとZIP64 fieldの重複を検査する。
///
/// # 引数
/// * `extra` - headerのextra領域
///
/// # 戻り値
/// ZIP64 fieldがあればその内容
///
fn extra64(extra: &[u8]) -> Result<Option<&[u8]>, SourceError> {
    let mut at = 0;
    let mut found = None;
    while at < extra.len() {
        let tag = number(extra, at, 2)?;
        let length = number(extra, at + 2, 2)? as usize;
        let data = field(extra, at + 4, length)?;
        if tag == 1 {
            if found.is_some() { return Err(SourceError::ZipStructure); }
            found = Some(data);
        }
        at = at.checked_add(4 + length).ok_or(SourceError::ZipStructure)?;
    }
    Ok(found)
}

///
/// ZIP64の番兵値だけを順序どおりの実値へ置換する。
///
/// # 引数
/// * `raw` - サイズ、圧縮量、offsetの順の生値
/// * `extra` - extra領域
///
/// # 戻り値
/// 実値とZIP64 fieldの有無
///
fn values<const N: usize>(mut raw: [u64; N], extra: &[u8])
    -> Result<([u64; N], bool), SourceError>
{
    let data = extra64(extra)?;
    let mut at = 0;
    for value in &mut raw {
        if *value == U32_MAX {
            *value = number(data.ok_or(SourceError::ZipStructure)?, at, 8)?;
            at += 8;
        }
    }
    Ok((raw, data.is_some()))
}

///
/// raw中央entryと全ローカル領域を検査する。
///
/// # 概要
/// 宣言量・件数を制限し、header/descriptorと重なった領域を拒否する。
///
/// # 引数
/// * `bytes` - ZIPバイト列
/// * `footer` - 検証済み範囲
/// * `limits` - 取得上限
/// * `stop` - 停止境界
///
/// # 戻り値
/// 重複を省略しないentry列
///
pub(super) fn entries(bytes: &[u8], footer: &Footer,
    limits: AcquisitionLimits, stop: &StopContext)
    -> Result<Vec<CentralEntry>, SourceError>
{
    let mut at = footer.start;
    let mut result = Vec::new();
    let mut total = 0_u64;
    /*
     * 件数とdirectory長の両方を満たすまで走査する
     */
    for _ in 0..footer.count {
        stop.checkpoint()?;
        let header = field(bytes, at, 46)?;
        if &header[..4] != CENTRAL { return Err(SourceError::ZipStructure); }
        let name_len = number(header, 28, 2)? as usize;
        let extra_len = number(header, 30, 2)? as usize;
        let comment_len = number(header, 32, 2)? as usize;
        let name = field(bytes, at + 46, name_len)?.to_vec();
        let extra = field(bytes, at + 46 + name_len, extra_len)?;
        let raw_size = number(header, 24, 4)?;
        let raw_compressed = number(header, 20, 4)?;
        let raw = [raw_size, raw_compressed, number(header, 42, 4)?];
        let (resolved, _) = values(raw, extra)?;
        let mut disk = number(header, 34, 2)?;
        if disk == 65535 {
            let at = raw.iter().filter(|v| **v == U32_MAX).count() * 8;
            disk = number(extra64(extra)?.ok_or(SourceError::ZipStructure)?,
                at, 4)?;
        }
        if disk != 0 {
            return Err(SourceError::ZipUnsupported);
        }
        let flags = number(header, 8, 2)? as u16;
        if flags & ((1 << 0) | (1 << 6) | (1 << 13)) != 0 {
            return Err(SourceError::ZipUnsupported);
        }
        total = total.checked_add(resolved[0])
            .ok_or(SourceError::ZipStructure)?;
        if total > limits.zip_expanded {
            return Err(SourceError::Limit("max_zip_uncompressed_bytes"));
        }
        let local = position(footer.base, resolved[2])?;
        let crc = number(header, 16, 4)? as u32;
        let method = number(header, 10, 2)?;
        let end = check_local(bytes, local, &name, flags, method, crc,
            resolved[0], resolved[1], raw_size == U32_MAX
                || raw_compressed == U32_MAX)?;
        if local < footer.base as usize || end > footer.start {
            return Err(SourceError::ZipStructure);
        }
        result.push(CentralEntry { name, flags, crc, local, end,
            size: resolved[0], compressed: resolved[1] });
        at = at.checked_add(46 + name_len + extra_len + comment_len)
            .ok_or(SourceError::ZipStructure)?;
        if at > footer.end { return Err(SourceError::ZipStructure); }
    }
    if at != footer.end { return Err(SourceError::ZipStructure); }

    /*
     * 同じローカル領域の別名利用やpayload同士の重なりを拒否する
     */
    let mut spans: Vec<_> = result.iter().map(|v| (v.local, v.end)).collect();
    spans.sort_unstable();
    if spans.windows(2).any(|v| v[0].1 > v[1].0) {
        return Err(SourceError::ZipStructure);
    }
    Ok(result)
}

///
/// 中央とlocal header、必要ならdata descriptorを照合する。
///
/// # 概要
/// bit 3がある場合はlocal宣言サイズを真値として扱わない。
///
/// # 引数
/// * `bytes` - ZIPバイト列
/// * `at` - local header位置
/// * `name` - 中央の名前
/// * `flags` - 中央のflags
/// * `method` - 中央の圧縮方式
/// * `crc` - 中央CRC
/// * `size` - 展開量
/// * `compressed` - 圧縮量
/// * `zip64` - 中央のサイズにZIP64を使うか
///
/// # 戻り値
/// data/descriptorを含む領域終端
///
fn check_local(bytes: &[u8], at: usize, name: &[u8], flags: u16,
    method: u64, crc: u32, size: u64, compressed: u64, zip64: bool)
    -> Result<usize, SourceError>
{
    let header = field(bytes, at, 30)?;
    if &header[..4] != LOCAL || number(header, 6, 2)? != u64::from(flags)
        || number(header, 8, 2)? != method
    { return Err(SourceError::ZipStructure); }
    let name_len = number(header, 26, 2)? as usize;
    let extra_len = number(header, 28, 2)? as usize;
    if field(bytes, at + 30, name_len)? != name {
        return Err(SourceError::ZipStructure);
    }
    let extra = field(bytes, at + 30 + name_len, extra_len)?;
    let raw_size = number(header, 22, 4)?;
    let raw_compressed = number(header, 18, 4)?;
    let (resolved, local64) = values([raw_size, raw_compressed], extra)?;
    let data = at + 30 + name_len + extra_len;
    let end = position(data as u64, compressed)?;
    field(bytes, data, end.checked_sub(data)
        .ok_or(SourceError::ZipStructure)?)?;
    /*
     * 通常headerの値か、明示されたdescriptorの値を照合する
     */
    if flags & (1 << 3) == 0 {
        if resolved != [size, compressed]
            || number(header, 14, 4)? != crc as u64
        {
            return Err(SourceError::ZipStructure);
        }
        return Ok(end);
    }
    descriptor_end(bytes, end, crc, size, compressed, zip64 || local64)
}

///
/// 32/64bit・署名あり/なしのdescriptorを中央値と照合する。
///
/// # 概要
/// 各候補のCRCとサイズを照合し、曖昧な解釈を拒否する。
///
/// # 引数
/// * `bytes` - ZIPバイト列
/// * `at` - 圧縮data直後
/// * `crc` - 中央CRC
/// * `size` - 展開量
/// * `compressed` - 圧縮量
/// * `zip64` - 64bitサイズ表現か
///
/// # 戻り値
/// 一意に解釈できるdescriptor終端
///
fn descriptor_end(bytes: &[u8], at: usize, crc: u32, size: u64,
    compressed: u64, zip64: bool) -> Result<usize, SourceError>
{
    let width = if zip64 { 8 } else { 4 };
    let mut found = None;
    /*
     * 署名の有無を値から検証し、一意な解釈だけを返す
     */
    for signature in [false, true] {
        if signature && number(bytes, at, 4)? != DESCRIPTOR { continue; }
        let start = at + if signature { 4 } else { 0 };
        let candidate = (|| {
            if number(bytes, start, 4)? != crc as u64
                || number(bytes, start + 4, width)? != compressed
                || number(bytes, start + 4 + width, width)? != size
            { return Err(SourceError::ZipStructure); }
            Ok(start + 4 + width * 2)
        })();
        if let Ok(end) = candidate {
            if found.is_some() { return Err(SourceError::ZipStructure); }
            found = Some(end);
        }
    }
    found.ok_or(SourceError::ZipStructure)
}
