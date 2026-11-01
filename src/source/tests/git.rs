/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! ローカルGitの固定ref・worktree・安全なsnapshotを確認する
//!

use std::collections::BTreeSet;
use std::fs;

use crate::cmd_args::resolve::source::{GitSelector, LocalSelector};
use crate::source::{EntryKind, SourceError, SourceSnapshot};

use super::{GitFixture, body, git_command, limits, stop};

///
/// 製品ローカルアダプターでfixtureを取得する。
///
/// # 引数
/// * `fixture` - 独立したGit履歴
/// * `selector` - 入力方針
///
/// # 戻り値
/// 取得結果または安全な拒否分類
///
fn acquire(fixture: &GitFixture, selector: LocalSelector)
    -> Result<SourceSnapshot, SourceError>
{
    crate::source::git::acquire(fixture.path(), selector, limits(), &stop())
}

///
/// HEADに依存しないmain→masterを確認する。
///
/// # 注記
/// 別branchをcheckoutし、main削除後はmaster、両方不在なら拒否する。
///
/// # 戻り値
/// なし。HEAD非依存のmain・master選択と不存在時の拒否が異なればassertで失敗する。
///
#[test]
fn default_reference_prefers_main_and_then_master() {
    let fixture = GitFixture::new();
    git_command(fixture.path(), &["checkout", "-q", "fixture-branch"]);
    let main = acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).expect("main取得");
    assert_eq!(body(&main, "pages/page.md"), b"main\n");
    git_command(fixture.path(), &["branch", "-D", "main"]);
    let master = acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).expect("master取得");
    assert_eq!(body(&master, "pages/page.md"), b"old\r\n");
    git_command(fixture.path(), &["branch", "-D", "master"]);
    assert_eq!(acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).err(), Some(SourceError::RefNotFound));
}

///
/// branch・注釈tag・commit IDを実際に解決する。
///
/// # 概要
/// branch・注釈tag・完全/短縮commitの同一本文と元HEAD保持を照合し、不存在を拒否する。
///
/// # 注記
/// 全指定が同じ旧commitの内容を取得し、元HEADを変えないことを確認する。
///
/// # 戻り値
/// なし。明示参照の取得本文・元HEAD保持・不存在の拒否が異なればassertで失敗する。
///
#[test]
fn resolves_branch_tag_full_and_abbreviated_commit() {
    let fixture = GitFixture::new();
    let head = git_command(fixture.path(), &["rev-parse", "HEAD"]);
    /*
     * 参照ごとに取得し、元HEADを維持する
     */
    for selector in [
        GitSelector::Branch("fixture-branch".to_owned()),
        GitSelector::Tag("fixture-tag".to_owned()),
        GitSelector::Commit(fixture.initial.clone()),
        GitSelector::Commit(fixture.initial[..12].to_owned()),
    ] {
        let snapshot = acquire(&fixture, LocalSelector::Committed(selector))
            .expect("明示ref取得");
        assert_eq!(body(&snapshot, "pages/page.md"), b"old\r\n");
    }
    assert_eq!(git_command(fixture.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(acquire(&fixture, LocalSelector::Committed(
        GitSelector::Branch("missing-secret-name".to_owned()))).err(),
        Some(SourceError::RefNotFound));
}

///
/// worktreeが現在のstaged/unstagedと未追跡を含むことを確認する。
///
/// # 概要
/// staged後の変更、ignored、削除済み、trackedなignored名を区別する。
///
/// # 注記
/// 取得した名前集合と本文を照合し、元repoのstatusを維持する。
///
/// # 戻り値
/// なし。worktreeの対象集合・実本文・元repo状態の保持が異なればassertで失敗する。
///
#[test]
fn worktree_collects_actual_files_and_excludes_ignored_and_deleted() {
    let fixture = GitFixture::new();
    let path = fixture.path();
    /*
     * trackedなignore名も対象にするfixtureを作る
     */
    fs::write(path.join("tracked.skip.md"), b"tracked").expect("tracked本文");
    git_command(path, &["add", "-f", "tracked.skip.md"]);
    git_command(path, &["commit", "-qm", "tracked ignored name"]);
    fs::write(path.join("pages/page.md"), b"unstaged\r\n").expect("変更");
    fs::write(path.join("pages/new.md"), b"staged").expect("staged本文");
    git_command(path, &["add", "pages/new.md"]);
    fs::write(path.join("pages/new.md"), b"after-stage").expect("再変更");
    fs::write(path.join("pages/untracked.md"), b"new").expect("未追跡");
    fs::write(path.join("pages/ignored.skip.md"), b"ignored").expect("ignore");
    fs::remove_file(path.join("pages/deleted.md")).expect("削除");
    let before = git_command(path, &["status", "--porcelain"]);

    /*
     * 製品の取得結果だけで集合と本文を照合する
     */
    let snapshot = acquire(&fixture, LocalSelector::Worktree)
        .expect("worktree");
    let files: BTreeSet<_> = snapshot.entries().iter()
        .filter(|v| v.kind() == EntryKind::File)
        .map(|v| v.name_raw().to_vec()).collect();
    for name in ["pages/page.md", "pages/new.md", "pages/untracked.md",
                 "tracked.skip.md", "plain.txt", ".gitignore"] {
        assert!(files.contains(name.as_bytes()));
    }
    assert!(!files.contains(b"pages/deleted.md".as_slice()));
    assert!(!files.contains(b"pages/ignored.skip.md".as_slice()));
    assert_eq!(body(&snapshot, "pages/page.md"), b"unstaged\r\n");
    assert_eq!(body(&snapshot, "pages/new.md"), b"after-stage");
    assert_eq!(git_command(path, &["status", "--porcelain"]), before);
}

///
/// snapshotが元入力の変更や削除から独立していることを確認する。
///
/// # 注記
/// 取得後に元本文を書き換えて削除し、leaseの本文を読む。
///
/// # 戻り値
/// なし。元入力変更後のsnapshot本文保持や領域回収が異なればassertで失敗する。
///
#[test]
fn snapshot_keeps_body_after_original_worktree_changes() {
    let fixture = GitFixture::new();
    let snapshot = acquire(&fixture, LocalSelector::Worktree).expect("取得");
    fs::write(fixture.path().join("pages/page.md"), b"changed-after-snapshot")
        .expect("元本文変更");
    fs::remove_file(fixture.path().join("plain.txt")).expect("元本文削除");
    assert_eq!(body(&snapshot, "pages/page.md"), b"main\n");
    assert_eq!(body(&snapshot, "plain.txt"), b"ordinary");
    let storage = snapshot.lease.0.path().to_path_buf();
    assert!(storage.exists());
    drop(snapshot);
    assert!(!storage.exists());
}

///
/// Git非Markdown symlinkをworktreeとcommitの両方で拒否する。
///
/// # 注記
/// dangling linkでも辿らずに拒否することを確認する。
///
/// # 戻り値
/// なし。worktree・commitのsymlink拒否が異なる場合はassertで失敗する。
///
#[cfg(unix)]
#[test]
fn rejects_non_markdown_symlink_in_worktree_and_commit() {
    let fixture = GitFixture::new();
    std::os::unix::fs::symlink("../outside",
        fixture.path().join("pages/link.txt"))
        .expect("symlink生成");
    assert_eq!(acquire(&fixture, LocalSelector::Worktree).err(),
        Some(SourceError::Symlink));
    git_command(fixture.path(), &["add", "."]);
    git_command(fixture.path(), &["commit", "-qm", "symlink"]);
    assert_eq!(acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).err(), Some(SourceError::Symlink));
}

///
/// gitlinkを初期化状態によらず拒否する。
///
/// # 注記
/// 外部取得せずindexへgitlinkを入れ、worktreeとcommitで確認する。
///
/// # 戻り値
/// なし。worktree・commitのgitlink拒否が異なる場合はassertで失敗する。
///
#[test]
fn rejects_submodule_gitlinks_without_fetching() {
    let fixture = GitFixture::new();
    let value = format!("160000,{},modules/child", fixture.initial);
    git_command(fixture.path(), &[
        "update-index", "--add", "--cacheinfo", &value,
    ]);
    assert_eq!(acquire(&fixture, LocalSelector::Worktree).err(),
        Some(SourceError::Submodule));
    git_command(fixture.path(), &["commit", "-qm", "gitlink"]);
    assert_eq!(acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).err(), Some(SourceError::Submodule));
}

///
/// ローカル取得にも保存量とMarkdown上限を適用する。
///
/// # 注記
/// worktreeとcommitの両方を上限1バイトで拒否する。
///
/// # 戻り値
/// なし。ローカル取得の保存量・本文上限の拒否が異なる場合はassertで失敗する。
///
#[test]
fn enforces_git_snapshot_and_markdown_size_limits() {
    let fixture = GitFixture::rooted();
    for selector in [LocalSelector::Worktree,
                     LocalSelector::Committed(GitSelector::MainThenMaster)] {
        for cap in [
            crate::source::AcquisitionLimits { git_bytes: 1, ..limits() },
            crate::source::AcquisitionLimits { page_bytes: 1, ..limits() },
        ] {
            let error = crate::source::git::acquire(fixture.path(),
                selector.clone(), cap, &stop()).err().expect("上限超過");
            assert!(matches!(error, SourceError::Limit(_)));
        }
    }
}

///
/// Gitが列挙しない空のworktree rootもdirectoryとして保持する。
///
/// # 注記
/// root外に既存ファイルを持つrepoへ空rootだけを作りsnapshotを検査する。
///
/// # 戻り値
/// なし。空rootの欠落または外側をroot内と誤認した場合は失敗する。
///
#[test]
fn worktree_preserves_empty_import_root() {
    let fixture = GitFixture::new();
    fs::create_dir(fixture.path().join("root")).unwrap();
    let snapshot = acquire(&fixture, LocalSelector::Worktree).unwrap();
    assert!(snapshot.entries().iter().any(|entry|
        entry.name_raw() == b"root" && entry.kind() == EntryKind::Directory));
}

///
/// Gitの外側metadataへページ本文上限を適用しない。
///
/// # 概要
/// committedとworktreeで同じroot/metadataを取得し、対象本文と区別する。
///
/// # 注記
/// root内最大7bytes、root外64bytesへ本文上限8を設定する。
///
/// # 戻り値
/// なし。root外Markdownをページ上限で拒否した場合は失敗する。
///
#[test]
fn outside_git_markdown_is_not_page_limited() {
    let fixture = GitFixture::rooted();
    /*
     * root外metadataを同じcommitにもworktreeにも持たせる
     */
    fs::write(fixture.path().join("README.md"), [b'x'; 64]).unwrap();
    git_command(fixture.path(), &["add", "README.md"]);
    git_command(fixture.path(), &["commit", "-qm", "metadata"]);
    for selector in [LocalSelector::Worktree,
        LocalSelector::Committed(GitSelector::MainThenMaster)]
    {
        let snapshot = crate::source::git::acquire(fixture.path(), selector,
            crate::source::AcquisitionLimits { page_bytes: 8, ..limits() },
            &stop()).unwrap();
        assert_eq!(body(&snapshot, "README.md"), [b'x'; 64]);
    }
}

///
/// 不正UTF-8ファイル名をlossy変換しない。
///
/// # 注記
/// Unixの生名を作り、worktreeとcommitの両経路で拒否する。
///
/// # 戻り値
/// なし。生名の不正UTF-8を両取得経路で拒否できなければassertで失敗する。
///
#[cfg(unix)]
#[test]
fn rejects_invalid_utf8_git_paths() {
    use std::os::unix::ffi::OsStringExt;

    let fixture = GitFixture::new();
    let name = std::ffi::OsString::from_vec(b"bad\xff.md".to_vec());
    fs::write(fixture.path().join(name), b"body").expect("生名fixture");
    assert_eq!(acquire(&fixture, LocalSelector::Worktree).err(),
        Some(SourceError::InvalidUtf8));
    git_command(fixture.path(), &["add", "."]);
    git_command(fixture.path(), &["commit", "-qm", "raw name"]);
    assert_eq!(acquire(&fixture, LocalSelector::Committed(
        GitSelector::MainThenMaster)).err(), Some(SourceError::InvalidUtf8));
}

///
/// 明示refはrevision式ではなく、指定した名前空間の実在名だけを許す。
///
/// # 注記
/// 同名branch/tagとmain~1を使い、別refや親commitへ誤解決しないか確認する。
///
/// # 戻り値
/// なし。名前空間の区別やrevision式の拒否が異なる場合はassertで失敗する。
///
#[test]
fn reference_names_do_not_accept_revision_expressions_or_other_namespace() {
    let fixture = GitFixture::new();
    git_command(fixture.path(), &["branch", "fixture-tag", "main"]);
    let tag = acquire(&fixture, LocalSelector::Committed(
        GitSelector::Tag("fixture-tag".to_owned()))).expect("tag取得");
    assert_eq!(body(&tag, "pages/page.md"), b"old\r\n");
    let branch = acquire(&fixture, LocalSelector::Committed(
        GitSelector::Branch("fixture-tag".to_owned()))).expect("branch取得");
    assert_eq!(body(&branch, "pages/page.md"), b"main\n");
    for selector in [GitSelector::Branch("main~1".to_owned()),
                     GitSelector::Tag("fixture-tag~1".to_owned())] {
        assert_eq!(acquire(&fixture, LocalSelector::Committed(selector)).err(),
            Some(SourceError::RefNotFound));
    }
}
