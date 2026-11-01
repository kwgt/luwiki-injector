/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 名前・投影・所有寿命・停止と本文未検証段階の契約試験
//!

use std::collections::HashSet;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::report::types::{FailureCategory, ProcessingStage, SourceKind};

use super::*;

///
/// 試験用の期限内停止境界を生成する。
///
/// # 戻り値
/// 60秒の期限を持つ未停止の境界
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(60))
}

///
/// root基準の相対名・種別・本文から所有snapshotを生成する。
///
/// # 引数
/// * `entries` - 試験するentry群
///
/// # 戻り値
/// 名前を共通検査する前の所有された入力
///
fn fixture(entries: &[(&[u8], EntryKind, &[u8])]) -> SourceSnapshot {
    SourceSnapshot::validation_root_fixture(entries).expect("fixture creation")
}

///
/// 所有された本文の保管領域をLinuxの開いたfdから観察する。
///
/// # 引数
/// * `snapshot` - 本文を一件以上持つ試験入力
///
/// # 戻り値
/// 本文保管領域のパス。製品APIでは公開しない。
///
fn storage_path(snapshot: &SourceSnapshot) -> PathBuf {
    let file = snapshot.open_body(0).expect("fixture body");
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .expect("Linux fd link");
    path.parent().expect("body parent").to_path_buf()
}

///
/// 日本語名・directory・末尾拡張子だけの除去と親非生成を確認する。
///
/// # 概要
/// 候補抽出と識別子の維持を一つの所有入力で確認する。
///
/// # 注記
/// 大文字拡張子と.md名のdirectoryを混ぜ、候補の識別子を照合する。
///
/// # 戻り値
/// なし。小文字Markdownの候補・識別子・元位置の対応が異なればassertで失敗する。
///
#[tokio::test]
async fn projects_only_lowercase_markdown_files() {
    let snapshot = fixture(&[
        ("親/".as_bytes(), EntryKind::Directory, b""),
        ("親/日本語.md".as_bytes(), EntryKind::File, b"body"),
        (b"double.md.md", EntryKind::File, b"second"),
        (b"upper.MD", EntryKind::File, b"ignored"),
        (b"directory.md/", EntryKind::Directory, b""),
    ]);
    let result = project(snapshot, "/".to_owned(),
        ProjectionLimits { max_pages: 2 }, stop()).await.unwrap();
    /*
     * root・親directoryを生成せず、参照と元entry位置も維持する
     */
    assert_eq!(result.pages().len(), 2);
    let first = &result.pages()[0];
    assert_eq!(first.identity().path.as_str(), "/親/日本語");
    assert_eq!(first.identity().source.parts().1, "root/親/日本語.md");
    assert_eq!(first.source_index(), 1);
    assert_eq!(first.expected_bytes(), 4);
    assert_eq!(result.pages()[1].identity().path.as_str(), "/double.md");
}

///
/// root以外のprefixをそのまま結合し、投入順をまだ変更しない。
///
/// # 注記
/// 子を親より先に並べ、候補順とprefix付き投影を確認する。
///
/// # 戻り値
/// なし。prefix結合や投影段階の候補順保持が異なればassertで失敗する。
///
#[tokio::test]
async fn applies_prefix_without_sorting() {
    let snapshot = fixture(&[
        (b"parent/child.md", EntryKind::File, b"child"),
        (b"parent.md", EntryKind::File, b"parent"),
    ]);
    let result = project(snapshot, "/投入先".to_owned(),
        ProjectionLimits { max_pages: 2 }, stop()).await.unwrap();
    assert_eq!(result.pages()[0].identity().path.as_str(),
        "/投入先/parent/child");
    assert_eq!(result.pages()[1].identity().path.as_str(), "/投入先/parent");
}

///
/// prefixを候補件数にかかわらず再検査する。
///
/// # 注記
/// 空snapshotへ禁止prefixを渡し、入力エラーとなることを確認する。
///
/// # 戻り値
/// なし。空入力での不正prefix拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_invalid_prefixes_before_empty_success() {
    for prefix in [
        "", "relative", "/a/", "//a", "/a//b", "/a/./b", "/a/../b",
        "/a\\b", "/a b", "/a\u{3000}b", "/a\0b", "/a\u{fffd}",
    ] {
        assert_eq!(project(fixture(&[]), prefix.to_owned(),
            ProjectionLimits { max_pages: 1 }, stop()).await.unwrap_err(),
            ValidationError::InvalidPrefix);
    }
}

///
/// Markdown以外でも生名の禁止要素を拒否する。
///
/// # 注記
/// UTF-8破損・置換文字・空segment・空白等を非MD名で照合する。
///
/// # 戻り値
/// なし。非Markdownを含む生名の安全検査が異なればassertで失敗する。
///
#[tokio::test]
async fn checks_all_raw_names_before_candidate_selection() {
    for raw in [
        &b"bad\xff.txt"[..], b"", b"/absolute.txt", b"a//b.txt",
        b"a/./b.txt", b"a/../b.txt", b"a\\b.txt", b"a\0b.txt",
        b"a\nb.txt", b" a.txt", b"a.txt ", b"a b.txt",
        "a\u{fffd}.txt".as_bytes(), "a\u{3000}b.txt".as_bytes(),
    ] {
        let result = project(fixture(&[(raw, EntryKind::File, b"untouched")]),
            "/".to_owned(), ProjectionLimits { max_pages: 1 }, stop()).await;
        assert!(matches!(result, Err(ValidationError::Source(_))));
    }
}

///
/// directoryだけに末尾slashを許し、生名を保持する。
///
/// # 注記
/// 同じslash付き名をFileとDirectoryで試し、二重slashも拒否する。
///
/// # 戻り値
/// なし。FileとDirectoryの末尾slash許容や生名保持が異なればassertで失敗する。
///
#[tokio::test]
async fn distinguishes_directory_names_from_file_names() {
    let snapshot = fixture(&[(b"dir/", EntryKind::Directory, b"")]);
    let result = project(snapshot, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
    assert!(result.pages().is_empty());
    assert_eq!(result.snapshot.entries()[0].name_raw(), b"root/dir/");
    for kind in [EntryKind::File, EntryKind::Directory] {
        assert!(project(fixture(&[(b"dir//", kind, b"")]), "/".to_owned(),
            ProjectionLimits { max_pages: 1 }, stop()).await.is_err());
    }
    assert!(project(fixture(&[(b"dir/", EntryKind::File, b"")]),
        "/".to_owned(), ProjectionLimits { max_pages: 1 }, stop())
        .await.is_err());
}

///
/// 拡張子だけのbasenameを拒否し、全体の成功結果を返さない。
///
/// # 注記
/// 正常候補の後に.mdとdir/.mdを置き、lease回収も確認する。
///
/// # 戻り値
/// なし。空basenameの拒否や部分結果非公開・lease回収が異なれば失敗する。
///
#[tokio::test]
async fn rejects_empty_basename_and_discards_partial_projection() {
    for name in [&b".md"[..], b"dir/.md"] {
        let snapshot = fixture(&[
            (b"valid.md", EntryKind::File, b"body"),
            (name, EntryKind::File, b"body"),
        ]);
        let path = storage_path(&snapshot);
        assert_eq!(project(snapshot, "/".to_owned(),
            ProjectionLimits { max_pages: 2 }, stop()).await.unwrap_err(),
            ValidationError::EmptyName);
        assert!(!path.exists());
    }
}

///
/// 安全検査を通過した0候補を成功として返す。
///
/// # 注記
/// 空入力、directory、非MDの不正本文を個別に確認する。
///
/// # 戻り値
/// なし。安全検査後の空候補や存在しない本文位置の拒否が異なれば失敗する。
///
#[tokio::test]
async fn allows_zero_candidates_after_name_validation() {
    for snapshot in [
        fixture(&[]), fixture(&[(b"directory/", EntryKind::Directory, b"")]),
        fixture(&[(b"binary.dat", EntryKind::File, b"\xff\0")]),
    ] {
        let result = project(snapshot, "/".to_owned(),
            ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
        assert!(result.pages().is_empty());
        assert!(result.open_unvalidated_body(0).is_err());
    }
}

///
/// ページ件数の上下境界を候補だけに適用する。
///
/// # 注記
/// 非MDを混ぜ、上限と同数は成功、1件超過は全体失敗と確認する。
///
/// # 戻り値
/// なし。候補件数の許容端点や一件超過の拒否が異なればassertで失敗する。
///
#[tokio::test]
async fn enforces_page_count_boundaries() {
    let entries: &[(&[u8], EntryKind, &[u8])] = &[
        (b"a.md", EntryKind::File, b"a"),
        (b"ignored.txt", EntryKind::File, b"ignored"),
        (b"b.md", EntryKind::File, b"b"),
    ];
    let result = project(fixture(entries), "/".to_owned(),
        ProjectionLimits { max_pages: 2 }, stop()).await.unwrap();
    assert_eq!(result.pages().len(), 2);
    assert_eq!(project(fixture(entries), "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap_err(),
        ValidationError::PageLimit);
}

///
/// 非MDやdirectoryでも生名重複を拒否する。
///
/// # 注記
/// 同じ生名を異なる種別で与え、候補なしでも重複エラーと確認する。
///
/// # 戻り値
/// なし。非Markdownやdirectoryの生名重複を拒否できなければassertで失敗する。
///
#[tokio::test]
async fn rejects_raw_duplicates_without_markdown_candidates() {
    let snapshot = fixture(&[
        (b"same", EntryKind::File, b"body"),
        (b"same", EntryKind::Directory, b""),
    ]);
    assert_eq!(project(snapshot, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap_err(),
        ValidationError::RawDuplicate);
}

///
/// 名前の正規化で衝突を作らず、最終パス重複guardを確認する。
///
/// # 注記
/// 同じ検査済み最終パスを直接再投入し、件数上限より重複を優先する。
///
/// # 戻り値
/// なし。最終パス重複の拒否や重複優先の判定が異なればassertで失敗する。
///
#[test]
fn rejects_duplicate_final_paths_without_normalization() {
    let mut targets = HashSet::new();
    let limits = ProjectionLimits { max_pages: 1 };
    assert_eq!(guard_target(&mut targets, "/日本語", limits), Ok(()));
    assert_eq!(guard_target(&mut targets, "/日本語", limits),
        Err(ValidationError::TargetDuplicate));
    assert_eq!(targets.len(), 1);
}

///
/// 安全な参照でも元entryと一致しなければ拒否する。
///
/// # 注記
/// 方式を変えず、異なる正規相対名との一致検査を試す。
///
/// # 戻り値
/// なし。入力参照と元entry名の一致検査が異なればassertで失敗する。
///
#[test]
fn rejects_mismatched_source_reference() {
    let reference = SourceReference::new(SourceKind::Zip, "other.md").unwrap();
    assert_eq!(validate_reference("actual.md", &reference),
        Err(ValidationError::ReferenceMismatch));
    assert_eq!(validate_reference("other.md", &reference), Ok(()));
}

///
/// 本文未検証段階で取得済みbytesを変換せず保持する。
///
/// # 注記
/// 不正UTF-8・U+FFFD・BOM・CRLFを含む本文も読込APIで一致を確認する。
///
/// # 戻り値
/// なし。本文未検証段階の元bytes・所有寿命・回収が異なればassertで失敗する。
///
#[tokio::test]
async fn retains_owned_unvalidated_body_bytes() {
    let expected = b"\xef\xbb\xbf\xff\xef\xbf\xbd\r\nprivate-body";
    let mut original = expected.to_vec();
    let snapshot = fixture(&[(b"private-name.md", EntryKind::File, &original)]);
    original.fill(0);
    drop(original);
    let path = storage_path(&snapshot);
    let result = project(snapshot, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
    /*
     * snapshotから所有権を移しても保管先が生存し、本文を変換しない
     */
    assert!(path.exists());
    let mut bytes = Vec::new();
    result.open_unvalidated_body(0).unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
    assert!(result.open_unvalidated_body(1).is_err());
    drop(result);
    assert!(!path.exists());
}

///
/// 停止済み・期限超過時に投影結果を公開せずleaseを回収する。
///
/// # 注記
/// worker開始前の停止と期限超過を与え、分類と保管先回収を確認する。
///
/// # 戻り値
/// なし。投影前の取消・期限拒否やsnapshot回収が異なればassertで失敗する。
///
#[tokio::test]
async fn rejects_stopped_work_and_reclaims_snapshot() {
    let cancelled = stop();
    cancelled.cancel();
    let expired = StopContext::new(Instant::now() - Duration::from_secs(1));
    for (stop, error) in [
        (cancelled, SourceError::Cancelled), (expired, SourceError::Deadline),
    ] {
        let snapshot = fixture(&[(b"a.md", EntryKind::File, b"body")]);
        let path = storage_path(&snapshot);
        assert_eq!(project(snapshot, "/".to_owned(),
            ProjectionLimits { max_pages: 1 }, stop).await.unwrap_err(),
            ValidationError::Source(error));
        assert!(!path.exists());
    }
}

///
/// Debug・Displayに本文・元名・保管先・内部上限名を含めない。
///
/// # 概要
/// 投影結果と全共通エラーの表示経路から入力値を除外する。
///
/// # 注記
/// 所有結果・候補・各エラーの表示を秘密のsentinelと照合する。
///
/// # 戻り値
/// なし。共通検証結果や失敗表示へ入力値・保管先が含まれれば失敗する。
///
#[tokio::test]
async fn hides_input_values_in_all_validation_displays() {
    let snapshot = fixture(&[(b"private-name.md", EntryKind::File,
        b"private-body")]);
    let path = storage_path(&snapshot);
    let result = project(snapshot, "/private-prefix".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
    let mut rendered = format!("{result:?} {:?}", result.pages()[0]);
    /*
     * 外部エラーをそのまま表示せず、固定分類だけに制限する
     */
    for error in [
        ValidationError::MissingRoot, ValidationError::InvalidRoot,
        ValidationError::InvalidPrefix, ValidationError::EmptyName,
        ValidationError::RawDuplicate, ValidationError::TargetDuplicate,
        ValidationError::PageLimit, ValidationError::ReferenceMismatch,
        ValidationError::Source(SourceError::Limit("private-limit")),
    ] {
        rendered.push_str(&format!("{error:?} {error}"));
    }
    for secret in ["private-name", "private-body", "private-prefix",
        "private-limit", path.to_str().unwrap()]
    {
        assert!(!rendered.contains(secret));
    }
}

///
/// 共通入力不正と取得・停止の失敗分類を既存報告へ写す。
///
/// # 概要
/// 共通入力不正とSource由来の実行失敗を既存Report分類へ照合する。
///
/// # 注記
/// 全共通分類はInput、Sourceの停止・I/Oは既存分類へ委譲する。
///
/// # 戻り値
/// なし。共通入力不正とSource実行失敗のReport分類が異なればassertで失敗する。
///
#[test]
fn maps_validation_failures_to_report_contract() {
    for error in [
        ValidationError::MissingRoot, ValidationError::InvalidRoot,
        ValidationError::InvalidPrefix, ValidationError::EmptyName,
        ValidationError::RawDuplicate, ValidationError::TargetDuplicate,
        ValidationError::PageLimit, ValidationError::ReferenceMismatch,
        ValidationError::Source(SourceError::InvalidUtf8),
    ] {
        assert!(matches!(error.into_failure(), Failure::Input));
    }
    /*
     * Source由来の実行失敗は既存の分類を維持する
     */
    for (error, expected) in [
        (SourceError::Cancelled, FailureCategory::NotApplied),
        (SourceError::Deadline, FailureCategory::Deadline),
        (SourceError::Io, FailureCategory::Transport),
    ] {
        assert!(matches!(ValidationError::Source(error).into_failure(),
            Failure::Execution { category, stage: ProcessingStage::Preflight }
                if category == expected));
    }
}

///
/// 生snapshotにrootがない場合や通常fileのrootを拒否する。
///
/// # 概要
/// rootを自動付与するfixtureを使わず、名前と存在の境界を直接確認する。
///
/// # 注記
/// root外Markdown、大小文字違い、外側wrapper、通常fileを個別入力にする。
///
/// # 戻り値
/// なし。root不在のfallbackや通常file許可で失敗する。
///
#[tokio::test]
async fn requires_top_level_root_directory_without_fallback() {
    /*
     * rootを付けない生入力で不在と不適合を区別する
     */
    for (name, error) in [
        (b"README.md".as_slice(), ValidationError::MissingRoot),
        (b"Root/a.md".as_slice(), ValidationError::MissingRoot),
        (b"package/root/a.md".as_slice(), ValidationError::MissingRoot),
        (b"root".as_slice(), ValidationError::InvalidRoot),
    ] {
        let snapshot = SourceSnapshot::validation_fixture(&[
            (name, EntryKind::File, b"body"),
        ]).unwrap();
        assert_eq!(project(snapshot, "/".to_owned(),
            ProjectionLimits { max_pages: 1 }, stop()).await.unwrap_err(),
            error);
    }
}

///
/// 明示directory entryがなくてもroot内だけを一段除去する。
///
/// # 概要
/// 外側metadataを混ぜ、候補上限とprefix、元のsource参照を照合する。
///
/// # 注記
/// nested rootは残し、READMEが4件目のページに数えられないことも確認する。
///
/// # 戻り値
/// なし。root選択/除去/入力参照のいずれかが異なれば失敗する。
///
#[tokio::test]
async fn selects_root_subtree_and_preserves_original_references() {
    /*
     * 元entryの順序と位置を保つ生snapshotを用意する
     */
    let snapshot = SourceSnapshot::validation_fixture(&[
        (b"README.md", EntryKind::File, b"metadata"),
        (b"root/a.md", EntryKind::File, b"a"),
        (b"root/dir/b.md", EntryKind::File, b"b"),
        (b"root/root/c.md", EntryKind::File, b"c"),
    ]).unwrap();
    let result = project(snapshot, "/target".to_owned(),
        ProjectionLimits { max_pages: 3 }, stop()).await.unwrap();
    /*
     * 投影先はroot基準、復旧用参照は元ソース基準とする
     */
    assert_eq!(result.pages().len(), 3);
    for (page, (target, source)) in result.pages().iter().zip([
        ("/target/a", "root/a.md"),
        ("/target/dir/b", "root/dir/b.md"),
        ("/target/root/c", "root/root/c.md"),
    ]) {
        assert_eq!(page.identity().path.as_str(), target);
        assert_eq!(page.identity().source.parts().1, source);
    }
}

///
/// root外の不正本文をページ本文検証へ渡さない。
///
/// # 概要
/// root内だけの本文上限とページ上限で全本文検証を成功させる。
///
/// # 注記
/// root外READMEを不正UTF-8かつ本文上限超過にし、root内は1byteとする。
///
/// # 戻り値
/// なし。外側本文の検査や候補追加で失敗する。
///
#[tokio::test]
async fn ignores_outside_markdown_during_body_validation() {
    /*
     * 取得済み本文の検証対象だけをroot内へ限定する
     */
    let snapshot = SourceSnapshot::validation_fixture(&[
        (b"README.md", EntryKind::File, b"\xff\xff\xff"),
        (b"root/a.md", EntryKind::File, b"a"),
    ]).unwrap();
    let projected = project(snapshot, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
    let limits = RunLimits {
        max_pages: 1, max_page_bytes: 1, max_zip_entries: 10,
        max_zip_compressed_bytes: 1000, max_zip_uncompressed_bytes: 1000,
        max_git_bytes: 1000, max_processing_seconds: 60,
        max_http_concurrency: 1,
    };
    let validated = validate_bodies(projected, BodyLimits::from(&limits),
        stop()).await.unwrap();
    assert_eq!(validated.pages().len(), 1);
    assert_eq!(validated.pages()[0].identity().path.as_str(), "/a");
}

///
/// rootだけの入力は正常空計画とし、完全な空入力は拒否する。
///
/// # 注記
/// 生snapshotの有無を変えてMissingRootと0候補成功を照合する。
///
/// # 戻り値
/// なし。root不在が空計画へfallbackした場合は失敗する。
///
#[tokio::test]
async fn distinguishes_empty_root_from_missing_root() {
    let empty = SourceSnapshot::validation_fixture(&[]).unwrap();
    assert_eq!(project(empty, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap_err(),
        ValidationError::MissingRoot);
    let root = SourceSnapshot::validation_fixture(&[
        (b"root/", EntryKind::Directory, b""),
    ]).unwrap();
    let projected = project(root, "/".to_owned(),
        ProjectionLimits { max_pages: 1 }, stop()).await.unwrap();
    assert!(projected.pages().is_empty());
}
