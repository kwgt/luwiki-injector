/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 製品ZIPアダプターの構造・フラグ・実読込量を検証する
//!

use std::fs;
use std::io::{Cursor, Write};

use tempfile::tempdir;
use ::zip::write::{FullFileOptions, SimpleFileOptions};
use ::zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::source::{AcquisitionLimits, EntryKind, SourceError, SourceSnapshot};

use super::zip_fixture::{corrupt_zip64_local_size, make_zip, make_zip64};
use super::zip_fixture::{replace_name, set_utf8_flags};
use super::{body, limits, stop};

///
/// 原ZIPを削除しても取得本文を読める境界で製品アダプターを試す。
///
/// # 引数
/// * `bytes` - fixtureのZIP
/// * `limits` - テスト上限
///
/// # 戻り値
/// 製品snapshotまたは安全な拒否分類
///
fn acquire(bytes: &[u8], limits: AcquisitionLimits)
    -> Result<SourceSnapshot, SourceError>
{
    let temporary = tempdir().expect("ZIP一時領域");
    let path = temporary.path().join("fixture.zip");
    fs::write(&path, bytes).expect("ZIP作成");
    crate::source::zip::acquire(&path, limits, &stop())
}

///
/// signed data descriptor付きZIPを作る。
///
/// # 引数
/// * `large` - 小規模ZIP64 entryにするか
///
/// # 戻り値
/// 非Seek writerで生成したZIP
///
fn stream_zip(large: bool) -> Vec<u8> {
    let mut writer = ZipWriter::new_stream(Vec::new());
    writer.start_file("page.md", SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored).large_file(large))
        .expect("stream entry");
    writer.write_all(b"descriptor\r\n").expect("stream本文");
    writer.finish().expect("stream footer").into_inner()
}

///
/// fixtureの構造signature位置を探す。
///
/// # 引数
/// * `bytes` - 小規模fixture
/// * `signature` - 構造signature
///
/// # 戻り値
/// fixture内の最後の一致位置
///
fn offset(bytes: &[u8], signature: &[u8]) -> usize {
    bytes.windows(4).rposition(|v| v == signature).expect("fixture signature")
}

///
/// 全entryと本文バイトを原ZIPから独立して保持する。
///
/// # 概要
/// source rootではなくleaseから本文を読み、破棄後の領域回収を確認する。
///
/// # 注記
/// 日本語名・directory・BOM/CRLF・非Markdownを同じZIPに入れる。
///
/// # 戻り値
/// なし。ZIPのentry・元bytes・取得元独立性・領域回収が異なればassertで失敗する。
///
#[test]
fn preserves_raw_entries_and_body_bytes() {
    /*
     * 種別と本文の形式が異なるentryを一つのZIPへ入れる
     */
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.add_directory("pages/", SimpleFileOptions::default())
        .expect("directory");
    writer.start_file("pages/日本語.md", SimpleFileOptions::default())
        .expect("日本語entry");
    writer.write_all(b"\xef\xbb\xbfbody\r\n").expect("本文");
    writer.start_file("plain.bin", SimpleFileOptions::default())
        .expect("非Markdown");
    writer.write_all(&[0xff, 0, 1]).expect("バイナリ");
    let bytes = writer.finish().expect("footer").into_inner();
    let snapshot = acquire(&bytes, limits()).expect("ZIP取得");
    assert_eq!(snapshot.entries().len(), 3);
    assert_eq!(snapshot.entries()[0].kind(), EntryKind::Directory);
    assert_eq!(body(&snapshot, "pages/日本語.md"), b"\xef\xbb\xbfbody\r\n");
    assert_eq!(body(&snapshot, "plain.bin"), [0xff, 0, 1]);
    assert!(!format!("{snapshot:?}").contains("body"));
    let path = snapshot.lease.0.path().to_path_buf();
    drop(snapshot);
    assert!(!path.exists());
}

///
/// ASCIIはbit 11任意、非ASCIIはbit 11必須を確認する。
///
/// # 注記
/// 同じ製品アダプターへ4組合せを渡す。
///
/// # 戻り値
/// なし。ASCII・非ASCII名とUTF-8 flagの許容条件が異なればassertで失敗する。
///
#[test]
fn enforces_utf8_flag_matrix() {
    for (name, flag, accepted) in [
        ("a.md", false, true), ("a.md", true, true),
        ("é.md", false, false), ("é.md", true, true),
    ] {
        let mut bytes = make_zip(&[(name, b"body")], CompressionMethod::Stored)
            .expect("ZIP生成");
        set_utf8_flags(&mut bytes, 0, flag, flag).expect("flag変更");
        assert_eq!(acquire(&bytes, limits()).is_ok(), accepted);
    }
}

///
/// lossy変換に頼らず破損名と禁止パスを拒否する。
///
/// # 注記
/// 生名を書き換え、置換文字や非Markdown危険名も対象にする。
///
/// # 戻り値
/// なし。破損UTF-8名や非Markdownを含む危険パスを拒否できなければ失敗する。
///
#[test]
fn rejects_invalid_names_and_unsafe_non_markdown_entries() {
    let mut invalid = make_zip(&[("é.md", b"x")], CompressionMethod::Stored)
        .expect("ZIP生成");
    replace_name(&mut invalid, 0, b"\xff!.md").expect("raw名変更");
    assert!(acquire(&invalid, limits()).is_err());
    for name in ["../x.txt", "/x.txt", "a/../x.txt", "a//x.txt",
                 "a\\x.txt", "a b.txt", "bad\u{fffd}.txt"] {
        let bytes = make_zip(&[(name, b"x")], CompressionMethod::Stored)
            .expect("ZIP生成");
        assert!(acquire(&bytes, limits()).is_err());
    }
}

///
/// raw重複とheader/CRCの不整合を拒否する。
///
/// # 概要
/// 中央/ローカルの整合と実解凍CRCを別の変造で確認する。
///
/// # 注記
/// APIが名前を集約する重複と、local flag/dataの変造を試す。
///
/// # 戻り値
/// なし。raw重複・header不整合・CRC破損の拒否が異なる場合はassertで失敗する。
///
#[test]
fn rejects_raw_duplicates_header_mismatch_and_bad_crc() {
    /*
     * 名前とflagの変造を解凍前に拒否する
     */
    let mut duplicate = make_zip(&[("a.md", b"a"), ("b.md", b"b")],
        CompressionMethod::Stored).expect("ZIP生成");
    replace_name(&mut duplicate, 1, b"a.md").expect("重複化");
    assert_eq!(acquire(&duplicate, limits()).err(),
        Some(SourceError::Duplicate));
    let mut mismatch = make_zip(&[("a.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    set_utf8_flags(&mut mismatch, 0, true, false).expect("header変更");
    assert!(acquire(&mismatch, limits()).is_err());
    let mut crc = make_zip(&[("a.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    let data = {
        let mut archive = ZipArchive::new(Cursor::new(&crc))
            .expect("fixture解析");
        let data = archive.by_index(0).expect("fixture entry")
            .data_start().expect("fixture data offset");
        let data = usize::try_from(data).expect("fixture offset fits usize");
        data
    };
    crc[data] ^= 1;
    assert!(acquire(&crc, limits()).is_err());
}

///
/// 非Markdown symlinkと破損footerを拒否する。
///
/// # 注記
/// symlinkと切り詰めたZIPを製品取得へ渡す。
///
/// # 戻り値
/// なし。symlinkや切り詰めたZIPの拒否が異なる場合はassertで失敗する。
///
#[test]
fn rejects_symlink_and_truncated_archives() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.add_symlink("link.txt", "outside", SimpleFileOptions::default())
        .expect("symlink生成");
    let bytes = writer.finish().expect("footer").into_inner();
    assert_eq!(acquire(&bytes, limits()).err(), Some(SourceError::Symlink));
    let mut bytes = make_zip(&[("a.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    bytes.truncate(bytes.len() - 5);
    assert!(acquire(&bytes, limits()).is_err());
}

///
/// ZIP64の小規模entryとfooterを受理し、localサイズの変造を拒否する。
///
/// # 注記
/// 既存の生成ヘルパーを使うが検査は製品アダプターだけで行う。
///
/// # 戻り値
/// なし。小規模ZIP64の本文保持やサイズ不整合拒否が異なればassertで失敗する。
///
#[test]
fn supports_small_zip64_and_rejects_size_mismatch() {
    for footer in [false, true] {
        let mut bytes = make_zip64(&[("a.md", b"body")], footer)
            .expect("ZIP64生成");
        let snapshot = acquire(&bytes, limits()).expect("ZIP64取得");
        assert_eq!(body(&snapshot, "a.md"), b"body");
        corrupt_zip64_local_size(&mut bytes, 0).expect("ZIP64変造");
        assert!(acquire(&bytes, limits()).is_err());
    }
}

///
/// 通常/ZIP64のsigned descriptorを検証する。
///
/// # 注記
/// stream writerの値を照合し、descriptor CRCだけの変造を拒否する。
///
/// # 戻り値
/// なし。signed descriptorの本文保持やCRC不整合拒否が異なればassertで失敗する。
///
#[test]
fn supports_stream_descriptors_and_checks_their_values() {
    for large in [false, true] {
        let mut bytes = stream_zip(large);
        let snapshot = acquire(&bytes, limits()).expect("descriptor取得");
        assert_eq!(body(&snapshot, "page.md"), b"descriptor\r\n");
        let at = offset(&bytes, b"PK\x07\x08");
        bytes[at + 4] ^= 1;
        assert!(acquire(&bytes, limits()).is_err());
    }
}

///
/// 署名なしの32/64bit descriptor fixtureを生成する。
///
/// # 概要
/// signatureを除き、通常/ZIP64 footerのoffsetを調整する。
///
/// # 引数
/// * `large` - ZIP64 entryにするか
///
/// # 戻り値
/// unsigned descriptorを持つZIPバイト列
///
fn unsigned_stream_zip(large: bool) -> Vec<u8> {
    let mut bytes = stream_zip(large);
    let at = offset(&bytes, b"PK\x07\x08");
    bytes.drain(at..at + 4);
    let end = offset(&bytes, b"PK\x05\x06");
    let old = u32::from_le_bytes(bytes[end + 16..end + 20]
        .try_into().expect("offset"));
    if old != u32::MAX {
        bytes[end + 16..end + 20].copy_from_slice(&(old - 4).to_le_bytes());
    }
    /*
     * ZIP64 footerがある場合も物理位置と相対offsetを合わせる
     */
    if let Some(locator) = bytes.windows(4).rposition(|v| v == b"PK\x06\x07") {
        let record = offset(&bytes, b"PK\x06\x06");
        let pointer = u64::from_le_bytes(bytes[locator + 8..locator + 16]
            .try_into().expect("ZIP64 locator"));
        bytes[locator + 8..locator + 16]
            .copy_from_slice(&(pointer - 4).to_le_bytes());
        let central = u64::from_le_bytes(bytes[record + 48..record + 56]
            .try_into().expect("ZIP64中央offset"));
        bytes[record + 48..record + 56]
            .copy_from_slice(&(central - 4).to_le_bytes());
    }
    bytes
}

///
/// 署名なしの32/64bit descriptorを実fixtureで確認する。
///
/// # 注記
/// signature除去後も両サイズ形式の本文を保持する。
///
/// # 戻り値
/// なし。unsigned descriptorの両サイズ形式で本文が異なればassertで失敗する。
///
#[test]
fn supports_unsigned_descriptor() {
    for large in [false, true] {
        let bytes = unsigned_stream_zip(large);
        let snapshot = acquire(&bytes, limits()).expect("unsigned取得");
        assert_eq!(body(&snapshot, "page.md"), b"descriptor\r\n");
    }
}

///
/// entry数・archive全体・展開量・本文の各上限を確認する。
///
/// # 注記
/// 境界値を受理し、一段小さい上限では拒否する。
///
/// # 戻り値
/// なし。ZIP全体・件数・展開量・本文上限の許容端点と拒否が異なれば失敗する。
///
#[test]
fn enforces_archive_entry_expansion_and_page_limits() {
    /*
     * 同じfixtureで各上限の境界を一つずつ変える
     */
    let bytes = make_zip(&[("root/a.md", b"12345"), ("b.txt", b"67890")],
        CompressionMethod::Stored).expect("ZIP生成");
    let mut cap = limits();
    cap.zip_bytes = bytes.len() as u64;
    cap.zip_entries = 2;
    cap.zip_expanded = 10;
    cap.page_bytes = 5;
    assert!(acquire(&bytes, cap).is_ok());
    for changed in [
        AcquisitionLimits { zip_bytes: cap.zip_bytes - 1, ..cap },
        AcquisitionLimits { zip_entries: 1, ..cap },
        AcquisitionLimits { zip_expanded: 9, ..cap },
        AcquisitionLimits { page_bytes: 4, ..cap },
    ] { assert!(acquire(&bytes, changed).is_err()); }
}

///
/// root外Markdownは本文上限から除外し、全展開量には含める。
///
/// # 概要
/// 同じZIPでページ上限の対象とarchive全体の上限を独立に確認する。
///
/// # 注記
/// root内2bytes、外側metadata8bytesへ本文上限2と展開上限10/9を適用する。
///
/// # 戻り値
/// なし。外側の本文拒否や全量上限の無効化で失敗する。
///
#[test]
fn outside_markdown_keeps_archive_limits_without_page_limit() {
    /*
     * root内だけが本文上限の対象であることを確認する
     */
    let bytes = make_zip(&[("root/a.md", b"ok"),
        ("README.md", b"metadata")], CompressionMethod::Stored).unwrap();
    let cap = AcquisitionLimits { page_bytes: 2, zip_expanded: 10,
        ..limits() };
    assert!(acquire(&bytes, cap).is_ok());
    /*
     * root外のbytesもソース全体の展開上限には残す
     */
    assert_eq!(acquire(&bytes, AcquisitionLimits {
        zip_expanded: 9, ..cap }).err(),
        Some(SourceError::Limit("max_zip_uncompressed_bytes")));
}

///
/// 偽の小さい宣言サイズでも実読込量を制限する。
///
/// # 注記
/// Stored本文のsize宣言だけを小さくし、実際の5バイトを上限3で拒否する。
///
/// # 戻り値
/// なし。宣言量とは独立した実読込上限の拒否分類が異なればassertで失敗する。
///
#[test]
fn actual_read_limit_is_independent_of_declared_size() {
    let mut bytes = make_zip(&[("a.txt", b"12345")], CompressionMethod::Stored)
        .expect("ZIP生成");
    let central = offset(&bytes, b"PK\x01\x02");
    bytes[22..26].copy_from_slice(&1_u32.to_le_bytes());
    bytes[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
    let cap = AcquisitionLimits { zip_expanded: 3, ..limits() };
    assert_eq!(acquire(&bytes, cap).err(),
        Some(SourceError::Limit("max_zip_uncompressed_bytes")));
}

///
/// 多volumeと不正な中央範囲・ZIP64 locatorを拒否する。
///
/// # 注記
/// footerだけを変造し、公開API列挙より先の安全な拒否を確認する。
///
/// # 戻り値
/// なし。不正volume・中央範囲・ZIP64 locatorを拒否できなければassertで失敗する。
///
#[test]
fn rejects_invalid_disk_directory_and_zip64_locator() {
    let ordinary = make_zip(&[("a.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    for field in [4, 8, 10, 12, 16] {
        let mut bytes = ordinary.clone();
        let at = offset(&bytes, b"PK\x05\x06");
        bytes[at + field] ^= 1;
        assert!(acquire(&bytes, limits()).is_err());
    }
    let mut bytes = make_zip64(&[("a.md", b"body")], true)
        .expect("ZIP64生成");
    let locator = offset(&bytes, b"PK\x06\x07");
    bytes[locator + 8] ^= 1;
    assert!(acquire(&bytes, limits()).is_err());
}

///
/// extra TLVの不正長を拒否する。
///
/// # 注記
/// ZIP64 extraの長さ欄を領域外まで伸ばす。
///
/// # 戻り値
/// なし。extra領域を超える不正長を拒否できなければassertで失敗する。
///
#[test]
fn rejects_truncated_extra_fields() {
    let mut bytes = make_zip64(&[("a.md", b"body")], false)
        .expect("ZIP64生成");
    let name_len = u16::from_le_bytes(bytes[26..28].try_into().expect("name"))
        as usize;
    let extra = 30 + name_len;
    bytes[extra + 2..extra + 4].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(acquire(&bytes, limits()).is_err());
}

///
/// well-formedな未知extraを無視しても構造検証を維持する。
///
/// # 注記
/// ZIP64記録に似たバイトをextraへ置き、footerの誤検出を防ぐ。
///
/// # 戻り値
/// なし。正しい未知extraの許容やZIP64 footerの判別が異なればassertで失敗する。
///
#[test]
fn handles_bounded_unknown_extra_without_false_zip64_footer() {
    let mut options = FullFileOptions::default()
        .compression_method(CompressionMethod::Stored).large_file(true);
    // zip 8.6.0で未定義のIDを使い、既知IDの生成制約を避ける。
    options.add_extra_data(0xfffe,
        b"PK\x06\x06\xff\xff\xff\xff\xff\xff\xff\xff", false)
        .expect("未知extra");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.set_raw_zip64_extensible_data_sector(Vec::new().into_boxed_slice());
    writer.start_file("page.md", options).expect("ZIP64 entry");
    writer.write_all(b"body").expect("本文");
    let bytes = writer.finish().expect("footer").into_inner();
    let snapshot = acquire(&bytes, limits()).expect("未知extraの取得");
    assert_eq!(body(&snapshot, "page.md"), b"body");
}

///
/// ZIP64 disk-start fieldだけを持つ単一volume fixtureを生成する。
///
/// # 概要
/// 中央extraへZIP64 disk-startだけを追加し、中央領域とEOCDの長さを調整する。
///
/// # 引数
/// * `disk` - ZIP64 disk-start値
///
/// # 戻り値
/// central extraとEOCD長を調整したfixture
///
fn disk64_zip(disk: u32) -> Vec<u8> {
    /*
     * 元のサイズ/offsetを保ち、centralのdisk fieldだけを追加する
     */
    let mut bytes = make_zip(&[("page.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    let central = offset(&bytes, b"PK\x01\x02");
    let name = u16::from_le_bytes(bytes[central + 28..central + 30]
        .try_into().expect("name長")) as usize;
    let extra = u16::from_le_bytes(bytes[central + 30..central + 32]
        .try_into().expect("extra長"));
    let mut field = vec![1, 0, 4, 0];
    field.extend_from_slice(&disk.to_le_bytes());
    bytes.splice(central + 46 + name + extra as usize..
        central + 46 + name + extra as usize, field);
    bytes[central + 30..central + 32]
        .copy_from_slice(&(extra + 8).to_le_bytes());
    bytes[central + 34..central + 36]
        .copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[central + 6..central + 8].copy_from_slice(&45_u16.to_le_bytes());
    let end = offset(&bytes, b"PK\x05\x06");
    let length = u32::from_le_bytes(bytes[end + 12..end + 16]
        .try_into().expect("中央長"));
    bytes[end + 12..end + 16].copy_from_slice(&(length + 8).to_le_bytes());
    bytes
}

///
/// ZIP64 fieldで単一diskを表す場合は受理し、multi-volumeは拒否する。
///
/// # 注記
/// 同じfieldの値0と1だけを変えて取得する。
///
/// # 戻り値
/// なし。ZIP64 disk-startの単一disk許容とmulti-volume拒否が異なれば失敗する。
///
#[test]
fn validates_zip64_disk_start_field() {
    let snapshot = acquire(&disk64_zip(0), limits()).expect("disk 0取得");
    assert_eq!(body(&snapshot, "page.md"), b"body");
    assert_eq!(acquire(&disk64_zip(1), limits()).err(),
        Some(SourceError::ZipUnsupported));
}

///
/// 復号条件を持たないencrypted entryを安全に拒否する。
///
/// # 注記
/// 対応するcentral/local両flagに暗号化bitを立てる。
///
/// # 戻り値
/// なし。暗号化flagを持つentryの安全な拒否分類が異なればassertで失敗する。
///
#[test]
fn rejects_encrypted_headers_without_requesting_password() {
    let mut bytes = make_zip(&[("page.md", b"body")], CompressionMethod::Stored)
        .expect("ZIP生成");
    let central = offset(&bytes, b"PK\x01\x02");
    for at in [6, central + 8] {
        let value = u16::from_le_bytes(bytes[at..at + 2]
            .try_into().expect("flags"));
        bytes[at..at + 2].copy_from_slice(&(value | 1).to_le_bytes());
    }
    assert_eq!(acquire(&bytes, limits()).err(),
        Some(SourceError::ZipUnsupported));
}
