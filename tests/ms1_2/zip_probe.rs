/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! ZIP入力の文字コードと安全性を実証するモジュール
//!

use std::io::Cursor;

use anyhow::{Context, Result, ensure};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, HasZipMetadata, ZipArchive, ZipWriter};

#[path = "zip_support.rs"]
mod zip_support;

use zip_support::{
    ZipLimits, corrupt_zip64_local_size, inspect_zip, make_zip, make_zip64,
    replace_name, set_utf8_flags,
};

///
/// 公開メタデータとraw名によるUTF-8フラグ判定を実証
///
/// # 概要
/// ASCII・非ASCII名のbit 11を切り替え、公開metadataと生名検査の許容条件を照合する。
///
/// # 戻り値
/// 全組合せが仕様どおりなら成功
///
/// # 注記
/// ASCII／非ASCII名でbit 11の有無を切り替える。
///
#[test]
fn utf8_flag_matrix() -> Result<()> {
    /*
     * ASCIIと非ASCII名のfixtureを準備する
     */
    let ascii = make_zip(&[("root/a.md", b"a")], CompressionMethod::Stored)?;
    let non_ascii =
        make_zip(&[("root/é.md", b"b")], CompressionMethod::Stored)?;
    for (source, expected, enabled) in [
        (&ascii, true, false),
        (&ascii, true, true),
        (&non_ascii, false, false),
        (&non_ascii, true, true),
    ] {
        /*
         * bit 11を切り替えmetadataと検査結果を照合する
         */
        let mut bytes = source.clone();
        set_utf8_flags(&mut bytes, 0, enabled, enabled)?;
        let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
        let file = archive.by_index(0)?;
        assert_eq!(file.get_metadata().is_utf8, enabled);
        assert_eq!(
            inspect_zip(&bytes, &ZipLimits::unlimited()).is_ok(),
            expected
        );
    }
    Ok(())
}

///
/// 不正UTF-8と置換文字の拒否を実証
///
/// # 戻り値
/// 両方を拒否できれば成功
///
/// # 注記
/// 生名を不正バイトへ置換する。
/// 置換文字は実行時に構成する。
///
#[test]
fn rejects_invalid_utf8_and_replacement() -> Result<()> {
    let mut invalid =
        make_zip(&[("root/é.md", b"body")], CompressionMethod::Stored)?;
    replace_name(&mut invalid, 0, b"root/\xff!.md")?;
    ensure!(inspect_zip(&invalid, &ZipLimits::unlimited()).is_err());

    let replacement = char::from_u32(0xfffd).context("replacement char")?;
    let name = format!("root/{replacement}.md");
    let bytes = make_zip(&[(&name, b"body")], CompressionMethod::Stored)?;
    ensure!(inspect_zip(&bytes, &ZipLimits::unlimited()).is_err());
    Ok(())
}

///
/// ヘッダ不整合とライブラリによる重複名統合を実証
///
/// # 概要
/// 中央/ローカルのflag不一致と生名重複を別々に作り、実証用検査の拒否を確認する。
///
/// # 戻り値
/// 両方を拒否できれば成功
///
/// # 注記
/// 同じ長さの名前へ書き換える。
/// 中央ディレクトリは2件のままにする。
///
#[test]
fn rejects_mismatch_and_duplicate_names() -> Result<()> {
    /*
     * 中央とlocalのflag不一致を拒否する
     */
    let mut mismatch =
        make_zip(&[("root/a.md", b"a")], CompressionMethod::Stored)?;
    set_utf8_flags(&mut mismatch, 0, true, false)?;
    ensure!(inspect_zip(&mismatch, &ZipLimits::unlimited()).is_err());

    /*
     * 生名重複を作り公開列挙数とraw検査の差を照合する
     */
    let mut duplicate = make_zip(
        &[("root/a.md", b"a"), ("root/b.md", b"b")],
        CompressionMethod::Stored,
    )?;
    replace_name(&mut duplicate, 1, b"root/a.md")?;
    let archive = ZipArchive::new(Cursor::new(&duplicate))?;
    assert_eq!(archive.len(), 1);
    ensure!(inspect_zip(&duplicate, &ZipLimits::unlimited()).is_err());
    Ok(())
}

///
/// 危険パス、リンク、破損ZIPの拒否を実証
///
/// # 概要
/// 危険パス・symlink・切り詰めたZIPを別fixtureで生成し、検査の拒否を確認する。
///
/// # 戻り値
/// 全入力を拒否できれば成功
///
/// # 注記
/// 絶対パスと親参照を含むZIPを生成する。
///
#[test]
fn rejects_unsafe_paths_symlink_and_corruption() -> Result<()> {
    /*
     * 危険パスの各入力を拒否する
     */
    for name in ["/root/a.md", "root/../a.md", "../root/a.md"] {
        let bytes = make_zip(&[(name, b"body")], CompressionMethod::Stored)?;
        ensure!(inspect_zip(&bytes, &ZipLimits::unlimited()).is_err());
    }

    /*
     * symlink entryの入力を拒否する
     */
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.add_symlink(
        "root/link.md",
        "../outside.md",
        SimpleFileOptions::default(),
    )?;
    let symlink = writer.finish()?.into_inner();
    ensure!(inspect_zip(&symlink, &ZipLimits::unlimited()).is_err());

    /*
     * 切り詰めたZIPの構造不正を拒否する
     */
    let mut broken =
        make_zip(&[("root/a.md", b"body")], CompressionMethod::Stored)?;
    broken.truncate(broken.len() - 12);
    ensure!(inspect_zip(&broken, &ZipLimits::unlimited()).is_err());
    Ok(())
}

///
/// 宣言サイズと実際の読込量の上限を実証
///
/// # 概要
/// 圧縮ZIPの宣言/実読込量を測り、上限同量の受理と各上限を下げた時の拒否を確認する。
///
/// # 戻り値
/// 境界値を受理し超過値を拒否できれば成功
///
/// # 注記
/// 圧縮後と展開後に差が出る本文で4種類の上限を調べる。
///
#[test]
fn enforces_entry_and_byte_limits() -> Result<()> {
    /*
     * 圧縮したfixtureの宣言量と実読込量を計測する
     */
    let body = vec![b'a'; 4096];
    let bytes = make_zip(
        &[("root/a.md", &body), ("root/b.md", &body)],
        CompressionMethod::Deflated,
    )?;
    let totals = inspect_zip(&bytes, &ZipLimits::unlimited())?;
    assert_eq!(totals.entries, 2);
    assert_eq!(totals.uncompressed, 8192);
    assert_eq!(totals.actual, 8192);
    ensure!(totals.compressed < totals.uncompressed);

    /*
     * 各上限が実量と同じ場合の受理を確認する
     */
    let mut limits = ZipLimits {
        entries: totals.entries,
        archive_bytes: totals.archive_bytes,
        uncompressed: totals.uncompressed,
        actual: totals.actual,
    };
    assert_eq!(inspect_zip(&bytes, &limits)?, totals);
    /*
     * 各上限だけを下げて超過拒否を確認する
     */
    limits.entries -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.entries += 1;
    limits.archive_bytes -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.archive_bytes += 1;
    limits.uncompressed -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.uncompressed += 1;
    limits.actual -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    Ok(())
}

///
/// 小規模ZIP64エントリとZIP64 EOCDを実証
///
/// # 戻り値
/// 64ビット値を解決して本文と件数を読めれば成功
///
/// # 注記
/// `large_file(true)`で小さい本文にもZIP64拡張を付ける。
///
#[test]
fn zip64_small_entries_and_footer() -> Result<()> {
    let bytes =
        make_zip64(&[("root/é.md", b"alpha"), ("root/b.md", b"beta")], true)?;
    ensure!(bytes.windows(4).any(|part| part == b"PK\x06\x06"));
    let totals = inspect_zip(&bytes, &ZipLimits::unlimited())?;
    assert_eq!(totals.entries, 2);
    assert_eq!(totals.uncompressed, 9);
    assert_eq!(totals.actual, 9);
    let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
    let file = archive.by_index(0)?;
    assert!(file.get_metadata().is_utf8);
    Ok(())
}

///
/// ZIP64でも重複、フラグ、両ヘッダの不整合を拒否
///
/// # 概要
/// ZIP64のUTF-8 flag欠落・不一致、localサイズ不整合、生名重複を個別に拒否する。
///
/// # 戻り値
/// すべての細工済みfixtureを拒否できれば成功
///
/// # 注記
/// ZIP64拡張サイズとbit 11の双方を変更する。
///
#[test]
fn zip64_rejects_duplicate_flag_and_header_mismatch() -> Result<()> {
    /*
     * ZIP64 fixtureのUTF-8 flag欠落と不一致を拒否する
     */
    let original =
        make_zip64(&[("root/é.md", b"one"), ("root/a.md", b"two")], true)?;
    let mut flag_missing = original.clone();
    set_utf8_flags(&mut flag_missing, 0, false, false)?;
    ensure!(inspect_zip(&flag_missing, &ZipLimits::unlimited()).is_err());

    let mut flag_mismatch = original.clone();
    set_utf8_flags(&mut flag_mismatch, 0, true, false)?;
    ensure!(inspect_zip(&flag_mismatch, &ZipLimits::unlimited()).is_err());

    /*
     * local ZIP64サイズの不整合を拒否する
     */
    let mut size_mismatch = original;
    corrupt_zip64_local_size(&mut size_mismatch, 0)?;
    ensure!(inspect_zip(&size_mismatch, &ZipLimits::unlimited()).is_err());

    /*
     * ZIP64 entryの生名重複を拒否する
     */
    let mut duplicate =
        make_zip64(&[("root/a.md", b"one"), ("root/b.md", b"two")], false)?;
    replace_name(&mut duplicate, 1, b"root/a.md")?;
    ensure!(inspect_zip(&duplicate, &ZipLimits::unlimited()).is_err());
    Ok(())
}

///
/// 小規模ZIP64にも件数とサイズの上限を適用
///
/// # 概要
/// ZIP64のサイズ集計を照合し、上限同量の受理と件数/各サイズ上限超過の拒否を確認する。
///
/// # 戻り値
/// 境界値を受理し超過を拒否できれば成功
///
/// # 注記
/// 圧縮・展開サイズはZIP64拡張フィールドから取得する。
///
#[test]
fn zip64_obeys_entry_archive_and_read_limits() -> Result<()> {
    /*
     * ZIP64 fixtureのサイズ集計を確認する
     */
    let bytes =
        make_zip64(&[("root/a.md", b"alpha"), ("root/b.md", b"beta")], true)?;
    let totals = inspect_zip(&bytes, &ZipLimits::unlimited())?;
    assert_eq!(totals.compressed, 9);
    /*
     * 件数と各サイズ上限の端点を受理する
     */
    let mut limits = ZipLimits {
        entries: totals.entries,
        archive_bytes: totals.archive_bytes,
        uncompressed: totals.uncompressed,
        actual: totals.actual,
    };
    assert_eq!(inspect_zip(&bytes, &limits)?, totals);
    /*
     * 各上限の一段超過を個別に拒否する
     */
    limits.entries -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.entries += 1;
    limits.archive_bytes -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.archive_bytes += 1;
    limits.uncompressed -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    limits.uncompressed += 1;
    limits.actual -= 1;
    ensure!(inspect_zip(&bytes, &limits).is_err());
    Ok(())
}

///
/// ZIP実証モジュールが統合テストに接続されたことを確認
///
/// # 注記
/// グループ1で用意した共有ヘルパーを確認する。
///
/// # 戻り値
/// なし。ZIP実証モジュールの共有ヘルパー接続が異なればassertで失敗する。
///
#[test]
fn zip_probe_is_connected() {
    assert_eq!(super::support::probe_group_name(), "ms1.2");
}
