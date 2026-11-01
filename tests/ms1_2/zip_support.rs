/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! ZIPの生ヘッダと実読込上限を検証するテスト専用補助
//!

use std::collections::HashSet;
use std::io::{Cursor, Read, Write};

use anyhow::{Context, Result, ensure};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, HasZipMetadata, ZipArchive, ZipWriter};

const UTF8_BIT: u16 = 1 << 11;
const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
const LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
const CENTRAL_END: &[u8; 4] = b"PK\x05\x06";
const ZIP64_CENTRAL_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const ZIP64_EXTRA_TAG: u16 = 1;

///
/// ZIP実証用の読込上限
///
pub(super) struct ZipLimits {
    pub entries: usize,
    pub archive_bytes: u64,
    pub uncompressed: u64,
    pub actual: u64,
}

impl ZipLimits {
    ///
    /// 制限しない実証用上限を返す
    ///
    /// # 戻り値
    /// 各上限を最大値にした設定
    ///
    pub fn unlimited() -> Self {
        Self {
            entries: usize::MAX,
            archive_bytes: u64::MAX,
            uncompressed: u64::MAX,
            actual: u64::MAX,
        }
    }
}

///
/// 中央ディレクトリの1エントリ
///
struct CentralEntry {
    offset: usize,
    local_offset: usize,
    flags: u16,
    name: Vec<u8>,
    compressed: u64,
    uncompressed: u64,
}

///
/// ZIP検査で計測したサイズ
///
#[derive(Debug, PartialEq)]
pub(super) struct ZipTotals {
    pub entries: usize,
    pub archive_bytes: u64,
    pub compressed: u64,
    pub uncompressed: u64,
    pub actual: u64,
}

///
/// 指定範囲のバイト列を安全に取り出す
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `start`: 読み始める位置
/// - `length`: 読み取る長さ
///
/// # 戻り値
/// 範囲内ならスライス、範囲外ならエラー
///
fn field(bytes: &[u8], start: usize, length: usize) -> Result<&[u8]> {
    let end = start.checked_add(length).context("offset overflow")?;
    bytes.get(start..end).context("truncated ZIP field")
}

///
/// little endianの16ビット値を読み取る
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `start`: 値の開始位置
///
/// # 戻り値
/// 読み取った値、または範囲エラー
///
fn le16(bytes: &[u8], start: usize) -> Result<u16> {
    let value: [u8; 2] = field(bytes, start, 2)?.try_into()?;
    Ok(u16::from_le_bytes(value))
}

///
/// little endianの32ビット値を読み取る
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `start`: 値の開始位置
///
/// # 戻り値
/// 読み取った値、または範囲エラー
///
fn le32(bytes: &[u8], start: usize) -> Result<u32> {
    let value: [u8; 4] = field(bytes, start, 4)?.try_into()?;
    Ok(u32::from_le_bytes(value))
}

///
/// little endianの64ビット値を読み取る
///
/// # 引数
/// - `bytes`: ZIP全体または拡張フィールドのバイト列
/// - `start`: 値の開始位置
///
/// # 戻り値
/// 読み取った値、または範囲エラー
///
fn le64(bytes: &[u8], start: usize) -> Result<u64> {
    let value: [u8; 8] = field(bytes, start, 8)?.try_into()?;
    Ok(u64::from_le_bytes(value))
}

///
/// 可変長領域から指定タグの拡張フィールドを探す
///
/// # 概要
/// extraのタグ・長さ・内容を順に検査し、指定タグの重複を拒否して一致内容を返す。
///
/// # 引数
/// - `extra`: ZIP拡張フィールド全体
/// - `wanted`: 取得したいタグ番号
///
/// # 戻り値
/// 一致したフィールドの内容、または構造エラー
///
fn find_extra(extra: &[u8], wanted: u16) -> Result<Option<&[u8]>> {
    /*
     * extraを先頭から走査する状態を準備する
     */
    let mut offset = 0;
    let mut found = None;
    while offset < extra.len() {
        /*
         * TLVの領域と指定タグの重複を検査して次位置へ進む
         */
        let tag = le16(extra, offset)?;
        let length = usize::from(le16(extra, offset + 2)?);
        let data = field(extra, offset + 4, length)?;
        if tag == wanted {
            ensure!(found.is_none(), "duplicate ZIP extra field");
            found = Some(data);
        }
        offset = offset
            .checked_add(4 + length)
            .context("ZIP extra offset overflow")?;
    }
    Ok(found)
}

///
/// ZIP64拡張フィールドから番兵値の実値を読む
///
/// # 引数
/// - `data`: ZIP64拡張フィールドの内容
/// - `position`: 次に読む位置
///
/// # 戻り値
/// 64ビットの実値、または構造エラー
///
fn next_zip64(data: &[u8], position: &mut usize) -> Result<u64> {
    let value = le64(data, *position)?;
    *position = (*position)
        .checked_add(8)
        .context("ZIP64 field offset overflow")?;
    Ok(value)
}

///
/// 中央ヘッダのZIP64サイズとローカル位置を解決
///
/// # 概要
/// 中央ヘッダの番兵値を判定し、必要なZIP64 extraからサイズとlocal位置を順に解決する。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `offset`: 中央ヘッダ開始位置
/// - `name_len`: エントリ名の長さ
/// - `extra_len`: 拡張フィールドの長さ
///
/// # 戻り値
/// 展開後サイズ、圧縮後サイズ、ローカル位置
///
fn central_values(
    bytes: &[u8],
    offset: usize,
    name_len: usize,
    extra_len: usize,
) -> Result<(u64, u64, u64)> {
    /*
     * 中央ヘッダの番兵値とZIP64 extraの必要性を判定する
     */
    let raw_uncompressed = le32(bytes, offset + 24)?;
    let raw_compressed = le32(bytes, offset + 20)?;
    let raw_local = le32(bytes, offset + 42)?;
    let needs_zip64 = [raw_uncompressed, raw_compressed, raw_local]
        .contains(&u32::MAX);
    let extra = field(bytes, offset + 46 + name_len, extra_len)?;
    let zip64 = find_extra(extra, ZIP64_EXTRA_TAG)?;
    let data = if needs_zip64 {
        zip64.context("missing ZIP64 central extra field")?
    } else {
        &[][..]
    };
    /*
     * 必要なサイズとlocal位置だけをZIP64順序で解決する
     */
    let mut position = 0;
    let uncompressed = if raw_uncompressed == u32::MAX {
        next_zip64(data, &mut position)?
    } else {
        u64::from(raw_uncompressed)
    };
    let compressed = if raw_compressed == u32::MAX {
        next_zip64(data, &mut position)?
    } else {
        u64::from(raw_compressed)
    };
    let local = if raw_local == u32::MAX {
        next_zip64(data, &mut position)?
    } else {
        u64::from(raw_local)
    };
    Ok((uncompressed, compressed, local))
}

///
/// 中央ディレクトリの後続ZIP64記録を検証
///
/// # 概要
/// ZIP64 EOCDのサイズと件数を検査し、計算したlocatorと通常EOCDのsignatureを照合する。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `offset`: ZIP64 EOCD開始位置
/// - `entries`: 生の中央ディレクトリエントリ数
///
/// # 戻り値
/// ZIP64 EOCD、locator、通常EOCDが整合すれば成功
///
fn check_zip64_footer(
    bytes: &[u8],
    offset: usize,
    entries: usize,
) -> Result<()> {
    /*
     * ZIP64 EOCDの最低長とentry件数を照合する
     */
    let record_size = usize::try_from(le64(bytes, offset + 4)?)?;
    ensure!(record_size >= 44, "short ZIP64 EOCD");
    ensure!(le64(bytes, offset + 32)? == u64::try_from(entries)?);
    /*
     * overflowを拒否して後続locatorと通常EOCDを照合する
     */
    let locator = offset
        .checked_add(12)
        .and_then(|value| value.checked_add(record_size))
        .context("ZIP64 EOCD offset overflow")?;
    ensure!(field(bytes, locator, 4)? == ZIP64_LOCATOR);
    ensure!(field(bytes, locator + 20, 4)? == CENTRAL_END);
    Ok(())
}

///
/// 中央ディレクトリを生バイト列から列挙する
///
/// # 概要
/// 可変長の中央レコードを生名のまま列挙し、ZIP64値・local位置・終端件数を検査する。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `start`: zipクレートが確認した
///   中央ディレクトリの開始位置
/// - `archive_offset`: ZIPデータ前の余剰データ長
///
/// # 戻り値
/// 重複名を含む全エントリ、または構造エラー
///
fn central_entries(
    bytes: &[u8],
    start: u64,
    archive_offset: u64,
) -> Result<Vec<CentralEntry>> {
    let mut offset = usize::try_from(start)?;
    let mut entries = Vec::new();

    /*
     * 可変長レコードをEOCDまで列挙
     */
    loop {
        let signature = field(bytes, offset, 4)?;
        if signature == CENTRAL_END {
            let count = le16(bytes, offset + 10)?;
            ensure!(count == u16::MAX || usize::from(count) == entries.len());
            break;
        }
        if signature == ZIP64_CENTRAL_END {
            check_zip64_footer(bytes, offset, entries.len())?;
            break;
        }
        ensure!(signature == CENTRAL_HEADER, "invalid central header");
        let name_len = usize::from(le16(bytes, offset + 28)?);
        let extra_len = usize::from(le16(bytes, offset + 30)?);
        let comment_len = usize::from(le16(bytes, offset + 32)?);
        let variable_len = name_len
            .checked_add(extra_len)
            .and_then(|len| len.checked_add(comment_len))
            .context("central record length overflow")?;
        let name = field(bytes, offset + 46, name_len)?.to_vec();
        let (uncompressed, compressed, local_offset) =
            central_values(bytes, offset, name_len, extra_len)?;
        let local_offset = local_offset
            .checked_add(archive_offset)
            .context("local offset overflow")?;
        entries.push(CentralEntry {
            offset,
            local_offset: usize::try_from(local_offset)?,
            flags: le16(bytes, offset + 8)?,
            name,
            compressed,
            uncompressed,
        });
        offset = offset
            .checked_add(46)
            .and_then(|value| value.checked_add(variable_len))
            .context("central record offset overflow")?;
    }
    Ok(entries)
}

///
/// ローカルヘッダと中央ディレクトリの
/// 名前・フラグを照合
///
/// # 概要
/// 両ヘッダの名前とflagを照合し、descriptorを使わない場合はZIP64を含むサイズも比べる。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `entry`: 中央ディレクトリのエントリ
///
/// # 戻り値
/// 一致すれば成功、異なればエラー
///
fn check_local_header(bytes: &[u8], entry: &CentralEntry) -> Result<()> {
    /*
     * localヘッダのsignature・flag・生名を中央記録と照合する
     */
    let offset = entry.local_offset;
    ensure!(field(bytes, offset, 4)? == LOCAL_HEADER, "bad local header");
    ensure!(le16(bytes, offset + 6)? == entry.flags, "flag mismatch");
    let name_len = usize::from(le16(bytes, offset + 26)?);
    let extra_len = usize::from(le16(bytes, offset + 28)?);
    ensure!(name_len == entry.name.len(), "name length mismatch");
    ensure!(
        field(bytes, offset + 30, name_len)? == entry.name,
        "name mismatch"
    );
    if entry.flags & (1 << 3) == 0 {
        /*
         * descriptorなしの場合のサイズ値とZIP64 extraを検査する
         */
        let raw_compressed = le32(bytes, offset + 18)?;
        let raw_uncompressed = le32(bytes, offset + 22)?;
        let extra = field(bytes, offset + 30 + name_len, extra_len)?;
        let zip64 = find_extra(extra, ZIP64_EXTRA_TAG)?;
        let data = if raw_compressed == u32::MAX
            || raw_uncompressed == u32::MAX
        {
            zip64.context("missing ZIP64 local extra field")?
        } else {
            &[][..]
        };
        /*
         * ZIP64番兵値を解決して中央のサイズと照合する
         */
        let mut position = 0;
        let uncompressed = if raw_uncompressed == u32::MAX {
            next_zip64(data, &mut position)?
        } else {
            u64::from(raw_uncompressed)
        };
        let compressed = if raw_compressed == u32::MAX {
            next_zip64(data, &mut position)?
        } else {
            u64::from(raw_compressed)
        };
        ensure!(uncompressed == entry.uncompressed, "size mismatch");
        ensure!(compressed == entry.compressed, "size mismatch");
    }
    Ok(())
}

///
/// 生のZIP名をページソースのパス条件で検証
///
/// # 概要
/// 生名の厳格UTF-8・禁止文字・segmentを検査し、非ASCIIにはUTF-8 flagを要求する。
///
/// # 引数
/// - `entry`: 中央ディレクトリのエントリ
///
/// # 戻り値
/// 有効なUTF-8名、または拒否理由
///
fn check_name(entry: &CentralEntry) -> Result<&str> {
    /*
     * 厳格UTF-8と絶対パス・禁止文字を検査する
     */
    let name = std::str::from_utf8(&entry.name).context("invalid UTF-8 name")?;
    ensure!(!name.contains('\u{fffd}'), "replacement character");
    ensure!(!name.starts_with('/'), "absolute path");
    ensure!(!name.contains('\\'), "backslash");
    ensure!(!name.chars().any(char::is_whitespace), "whitespace");
    ensure!(!name.chars().any(char::is_control), "control character");
    /*
     * segmentの安全性と非ASCIIのUTF-8 flagを照合する
     */
    let body = name.strip_suffix('/').unwrap_or(name);
    ensure!(
        body.split('/').all(|part| !part.is_empty()
            && part != "." && part != ".."),
        "unsafe path segment"
    );
    ensure!(
        name.is_ascii() || entry.flags & UTF8_BIT != 0,
        "non-ASCII name without UTF-8 flag"
    );
    Ok(name)
}

///
/// ZIPを事前検査し実際の読込量も計数する
///
/// # 概要
/// 解凍前の構造・全entry名・宣言量を検査し、実解凍量も別上限内で計数する。
///
/// # 引数
/// - `bytes`: ZIP全体のバイト列
/// - `limits`: 実証用の上限値
///
/// # 戻り値
/// サイズ集計、または最初の拒否理由
///
pub(super) fn inspect_zip(
    bytes: &[u8],
    limits: &ZipLimits,
) -> Result<ZipTotals> {
    let archive_bytes = u64::try_from(bytes.len())?;
    ensure!(archive_bytes <= limits.archive_bytes, "ZIP file size limit");
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let entries = central_entries(
        bytes,
        archive.central_directory_start(),
        archive.offset(),
    )?;
    ensure!(entries.len() <= limits.entries, "entry limit");
    ensure!(entries.len() == archive.len(), "duplicate raw names");
    let mut seen = HashSet::new();
    let mut totals = ZipTotals {
        entries: entries.len(),
        archive_bytes,
        compressed: 0,
        uncompressed: 0,
        actual: 0,
    };

    /*
     * 全エントリを解凍前に検査
     */
    for (index, entry) in entries.iter().enumerate() {
        check_local_header(bytes, entry)?;
        let name = check_name(entry)?;
        ensure!(seen.insert(name), "duplicate decoded names");
        let file = archive.by_index_raw(index)?;
        ensure!(file.name_raw() == entry.name, "archive name mismatch");
        ensure!(file.get_metadata().flags == entry.flags, "flag mismatch");
        ensure!(file.enclosed_name().is_some(), "unsafe enclosed path");
        ensure!(!file.is_symlink(), "symbolic link");
        totals.compressed = totals.compressed
            .checked_add(entry.compressed)
            .context("compressed size overflow")?;
        totals.uncompressed = totals.uncompressed
            .checked_add(entry.uncompressed)
            .context("uncompressed size overflow")?;
        ensure!(totals.uncompressed <= limits.uncompressed, "size limit");
    }

    /*
     * 実際の解凍量を小分けに計数
     */
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let mut buffer = [0_u8; 7];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            totals.actual = totals.actual
                .checked_add(u64::try_from(count)?)
                .context("actual size overflow")?;
            ensure!(totals.actual <= limits.actual, "actual read limit");
        }
    }
    Ok(totals)
}

///
/// 名前と内容の列からZIPを生成する
///
/// # 引数
/// - `files`: エントリ名と本文の組
/// - `method`: ZIP圧縮方式
///
/// # 戻り値
/// 完成したZIPバイト列
///
pub(super) fn make_zip(
    files: &[(&str, &[u8])],
    method: CompressionMethod,
) -> Result<Vec<u8>> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in files {
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file(*name, options)?;
        writer.write_all(body)?;
    }
    Ok(writer.finish()?.into_inner())
}

///
/// 小さい本文をZIP64エントリとして生成
///
/// # 概要
/// 任意のZIP64 footer設定とStoredのZIP64 entryを使い、小規模fixtureの本文を保存する。
///
/// # 引数
/// - `files`: エントリ名と本文の組
/// - `zip64_footer`: ZIP64 EOCDを小規模fixtureにも付けるか
///
/// # 戻り値
/// ZIP64サイズ拡張フィールド付きZIP
///
pub(super) fn make_zip64(
    files: &[(&str, &[u8])],
    zip64_footer: bool,
) -> Result<Vec<u8>> {
    /*
     * ZIP writerと任意のZIP64 footerを準備する
     */
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    if zip64_footer {
        writer.set_raw_zip64_extensible_data_sector(
            Vec::new().into_boxed_slice(),
        );
    }
    /*
     * 小規模本文をStoredのZIP64 entryとして保存する
     */
    for (name, body) in files {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true);
        writer.start_file(*name, options)?;
        writer.write_all(body)?;
    }
    Ok(writer.finish()?.into_inner())
}

///
/// ZIP64ローカル拡張フィールドのサイズ値を壊す
///
/// # 概要
/// 指定entryのlocal ZIP64 extraを検査し、展開後サイズの先頭byteを反転させる。
///
/// # 引数
/// - `bytes`: 変更対象ZIP
/// - `index`: 対象エントリ番号
///
/// # 戻り値
/// ZIP64サイズ値を変更できれば成功
///
pub(super) fn corrupt_zip64_local_size(
    bytes: &mut [u8],
    index: usize,
) -> Result<()> {
    /*
     * 中央記録を列挙して対象entryを解決する
     */
    let archive = ZipArchive::new(Cursor::new(&*bytes))?;
    let entries = central_entries(
        bytes,
        archive.central_directory_start(),
        archive.offset(),
    )?;
    let entry = entries.get(index).context("entry not found")?;
    /*
     * local ZIP64 extraを確認して展開後サイズのbyteを変更する
     */
    let local = entry.local_offset;
    let name_len = usize::from(le16(bytes, local + 26)?);
    let extra_start = local + 30 + name_len;
    ensure!(le16(bytes, extra_start)? == ZIP64_EXTRA_TAG);
    ensure!(le16(bytes, extra_start + 2)? >= 16);
    let value = bytes.get_mut(extra_start + 4)
        .context("missing ZIP64 uncompressed size")?;
    *value ^= 1;
    Ok(())
}

///
/// 中央・ローカル両ヘッダのUTF-8フラグを変更
///
/// # 概要
/// 指定entryの中央/local位置を解決し、両ヘッダのUTF-8 bitを個別の指定で更新する。
///
/// # 引数
/// - `bytes`: 変更対象ZIP
/// - `index`: 変更対象エントリ番号
/// - `central`: 中央ヘッダでフラグを立てるか
/// - `local`: ローカルヘッダでフラグを立てるか
///
/// # 戻り値
/// 変更に成功した場合は成功
///
pub(super) fn set_utf8_flags(
    bytes: &mut [u8],
    index: usize,
    central: bool,
    local: bool,
) -> Result<()> {
    /*
     * 中央記録から対象entryの両ヘッダ位置を解決する
     */
    let archive = ZipArchive::new(Cursor::new(&*bytes))?;
    let entries = central_entries(
        bytes,
        archive.central_directory_start(),
        archive.offset(),
    )?;
    let entry = entries.get(index).context("entry not found")?;
    /*
     * 中央とlocalの指定に従ってUTF-8 bitだけを変更する
     */
    for (at, enabled) in [
        (entry.offset + 8, central),
        (entry.local_offset + 6, local),
    ] {
        let old = le16(bytes, at)?;
        let flags = if enabled { old | UTF8_BIT } else { old & !UTF8_BIT };
        bytes[at..at + 2].copy_from_slice(&flags.to_le_bytes());
    }
    Ok(())
}

///
/// 中央・ローカル両ヘッダの名前を書き換える
///
/// # 概要
/// 指定entryと置換名の長さを照合し、中央とlocalの生名だけを同じbytesへ書き換える。
///
/// # 引数
/// - `bytes`: 変更対象ZIP
/// - `index`: 変更対象エントリ番号
/// - `name`: 元の名前と同じ長さの生バイト名
///
/// # 戻り値
/// 変更に成功した場合は成功
///
pub(super) fn replace_name(
    bytes: &mut [u8],
    index: usize,
    name: &[u8],
) -> Result<()> {
    /*
     * 中央記録から対象entryと元の生名を解決する
     */
    let archive = ZipArchive::new(Cursor::new(&*bytes))?;
    let entries = central_entries(
        bytes,
        archive.central_directory_start(),
        archive.offset(),
    )?;
    let entry = entries.get(index).context("entry not found")?;
    /*
     * 長さの一致を確認して両ヘッダの生名だけを書き換える
     */
    ensure!(name.len() == entry.name.len(), "name length changed");
    for start in [entry.offset + 46, entry.local_offset + 30] {
        bytes[start..start + name.len()].copy_from_slice(name);
    }
    Ok(())
}
