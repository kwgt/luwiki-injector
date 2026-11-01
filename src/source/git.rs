/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! ローカルGitの固定ツリーとworktreeを所有された本文へ取得する
//!

use std::collections::BTreeSet;
use std::fs::{self, File, Metadata};
use std::io::Cursor;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use gix::objs::tree::EntryKind as GitKind;

use crate::cmd_args::resolve::source::{GitSelector, LocalSelector};
use crate::report::types::SourceKind;

use super::store::{Builder, validate_name};
use super::{AcquisitionLimits, EntryKind, SourceError};
use super::{SourceSnapshot, StopContext};

///
/// ローカルGit指定を検査して一時snapshotへ取得する。
///
/// # 引数
/// * `path` - ローカルリポジトリ
/// * `selector` - コミット済み参照またはworktree
/// * `limits` - 取得上限
/// * `stop` - 協調停止境界
///
/// # 戻り値
/// 元リポジトリを変更しない所有されたsnapshot
///
pub(super) fn acquire(path: &Path, selector: LocalSelector,
    limits: AcquisitionLimits, stop: &StopContext)
    -> Result<SourceSnapshot, SourceError>
{
    /*
     * worktreeの実体を再確認し、専用leaseを準備する
     */
    stop.checkpoint()?;
    let repo = gix::open_opts(path, gix::open::Options::isolated())
        .map_err(|_| SourceError::InvalidGit)?;
    let root = repo.workdir().ok_or(SourceError::InvalidGit)?;
    let mut builder = Builder::new(SourceKind::LocalGit, limits.git_bytes,
        "max_git_bytes", limits.page_bytes)?;
    match selector {
        LocalSelector::Committed(selector) =>
            committed(&repo, selector, &mut builder, stop)?,
        LocalSelector::Worktree => worktree(&repo, root, &mut builder, stop)?,
    }
    builder.finish(stop)
}

///
/// 参照名を名前空間ごとに実在のcommitへ解決する。
///
/// # 概要
/// main/masterと明示参照を区別し、commit IDはhex表現の実体だけを許す。
///
/// # 引数
/// * `repo` - 隔離設定で開いたリポジトリ
/// * `selector` - 検証済み参照方針
///
/// # 戻り値
/// branch/tagをpeelしたcommit。HEADは既定参照に使わない。
///
pub(super) fn commit(repo: &gix::Repository, selector: GitSelector)
    -> Result<gix::Commit<'_>, SourceError>
{
    let explicit_id = matches!(&selector, GitSelector::Commit(_));
    /*
     * refの名前空間を確定し、HEADへのfallbackを避ける
     */
    let reference = match selector {
        GitSelector::MainThenMaster => {
            if repo.try_find_reference("refs/heads/main")
                .map_err(|_| SourceError::InvalidGit)?.is_some()
            {
                "refs/heads/main".to_owned()
            } else { "refs/heads/master".to_owned() }
        }
        GitSelector::Branch(name) => format!("refs/heads/{name}"),
        GitSelector::Tag(name) => format!("refs/tags/{name}"),
        GitSelector::Commit(id) => {
            if id.is_empty() || id.len() > 64
                || !id.bytes().all(|v| v.is_ascii_hexdigit())
            {
                return Err(SourceError::RefNotFound);
            }
            id
        }
    };
    /*
     * ID指定では名前の一致ではなく実object IDを照合する
     */
    if !explicit_id {
        let mut reference = repo.find_reference(reference.as_str())
            .map_err(|_| SourceError::RefNotFound)?;
        return reference.peel_to_commit().map_err(|_| SourceError::RefNotFound);
    }
    let id = repo.rev_parse_single(reference.as_str())
        .map_err(|_| SourceError::RefNotFound)?;
    if explicit_id && !id.to_string()
        .starts_with(&reference.to_ascii_lowercase())
    {
        return Err(SourceError::RefNotFound);
    }
    let object = id.object().map_err(|_| SourceError::RefNotFound)?;
    object.try_into_commit().map_err(|_| SourceError::RefNotFound)
}

///
/// 固定commitの全entryを安全検査して取得する。
///
/// # 概要
/// objectの宣言サイズを検査し、本文を一つずつ専用領域へ移す。
///
/// # 引数
/// * `repo` - リポジトリ
/// * `selector` - 参照方針
/// * `builder` - 未公開snapshot
/// * `stop` - 停止境界
///
/// # 戻り値
/// 全entryの取得成否
///
pub(super) fn committed(repo: &gix::Repository, selector: GitSelector,
    builder: &mut Builder, stop: &StopContext) -> Result<(), SourceError>
{
    let tree = commit(repo, selector)?.tree()
        .map_err(|_| SourceError::InvalidGit)?;
    let mut pending = vec![(tree.id, Vec::<u8>::new(), Vec::new())];
    /*
     * 木の各entryで停止を確認し、循環参照を拒否する
     */
    while let Some((id, prefix, mut ancestors)) = pending.pop() {
        stop.checkpoint()?;
        if ancestors.contains(&id) { return Err(SourceError::InvalidGit); }
        ancestors.push(id);
        let tree = repo.find_object(id).map_err(|_| SourceError::InvalidGit)?
            .try_into_tree().map_err(|_| SourceError::InvalidGit)?;
        for entry in gix::objs::TreeRefIter::from_bytes(&tree.data, id.kind()) {
            stop.checkpoint()?;
            let entry = entry.map_err(|_| SourceError::InvalidGit)?;
            let filename: &[u8] = entry.filename.as_ref();
            if filename.contains(&b'/') {
                return Err(SourceError::InvalidGit);
            }
            let mut raw = prefix.clone();
            if !raw.is_empty() { raw.push(b'/'); }
            raw.extend_from_slice(filename);
            validate_name(&raw, EntryKind::File)?;
            match entry.mode.kind() {
                GitKind::Link => return Err(SourceError::Symlink),
                GitKind::Commit => return Err(SourceError::Submodule),
                GitKind::Tree => {
                    builder.append(&raw, EntryKind::Directory,
                        &mut Cursor::new([]), 0, stop)?;
                    pending.push((entry.oid.to_owned(), raw,
                        ancestors.clone()));
                    continue;
                }
                _ => {}
            }
            /*
             * 宣言サイズを先に確認し、blobを一つずつ保管する
             */
            let oid = entry.oid.to_owned();
            let header = repo.find_header(oid)
                .map_err(|_| SourceError::InvalidGit)?;
            if header.kind() != gix::objs::Kind::Blob {
                return Err(SourceError::InvalidGit);
            }
            let name = validate_name(&raw, EntryKind::File)?;
            builder.check_size(header.size(), super::is_page_name(name))?;
            let blob = repo.find_object(oid)
                .map_err(|_| SourceError::InvalidGit)?
                .try_into_blob().map_err(|_| SourceError::InvalidGit)?;
            builder.append(&raw, EntryKind::File, &mut Cursor::new(&blob.data),
                header.size(), stop)?;
        }
    }
    Ok(())
}

///
/// worktreeの追跡済みとignore対象外の未追跡を列挙する。
///
/// # 概要
/// indexとstatusを使い、削除済みを除いて現在の実ファイルを読む。
///
/// # 引数
/// * `repo` - リポジトリ
/// * `root` - worktree root
/// * `builder` - 未公開snapshot
/// * `stop` - 停止境界
///
/// # 戻り値
/// 一意な相対名と本文の取得成否
///
fn worktree(repo: &gix::Repository, root: &Path,
    builder: &mut Builder, stop: &StopContext) -> Result<(), SourceError>
{
    let index = repo.index_or_empty().map_err(|_| SourceError::InvalidGit)?;
    let mut paths = BTreeSet::<Vec<u8>>::new();
    /*
     * staged/unstaged双方の対象となる追跡済み名を集める
     */
    for entry in index.entries() {
        stop.checkpoint()?;
        if entry.mode == gix::index::entry::Mode::COMMIT {
            return Err(SourceError::Submodule);
        }
        paths.insert(entry.path(&index).to_vec());
    }
    let status = repo.status(gix::progress::Discard)
        .map_err(|_| SourceError::InvalidGit)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(Vec::<gix::bstr::BString>::new())
        .map_err(|_| SourceError::InvalidGit)?;
    for item in status {
        stop.checkpoint()?;
        let item = item.map_err(|_| SourceError::InvalidGit)?;
        paths.insert(item.location().to_vec());
    }

    /*
     * 親symlinkも拒否し、実ファイルと同じmetadataを使う
     */
    let mut directories = BTreeSet::<Vec<u8>>::new();
    /*
     * Gitが列挙しない空rootも、実体と親symlinkを検査して保持する
     */
    if let Some((path, metadata)) = checked_path(root, super::PAGE_ROOT)? {
        if metadata.is_dir() {
            if path.join(".git").exists() {
                return Err(SourceError::InvalidGit);
            }
            directories.insert(super::PAGE_ROOT.as_bytes().to_vec());
        }
    }
    for raw in paths {
        stop.checkpoint()?;
        let name = validate_name(&raw, EntryKind::File)?;
        let Some((full, metadata)) = checked_path(root, name)? else {
            continue;
        };
        if metadata.is_dir() {
            if full.join(".git").exists() {
                return Err(SourceError::InvalidGit);
            }
            directories.insert(raw);
            continue;
        }
        if !metadata.is_file() { return Err(SourceError::InvalidPath); }
        let mut file = File::open(&full).map_err(|_| SourceError::Io)?;
        let opened = file.metadata().map_err(|_| SourceError::Io)?;
        if !same_file(&metadata, &opened) { return Err(SourceError::Changed); }
        builder.append(&raw, EntryKind::File, &mut file, opened.len(), stop)?;
        let after = file.metadata().map_err(|_| SourceError::Io)?;
        if after.len() != opened.len()
            || after.modified().ok() != opened.modified().ok()
        {
            return Err(SourceError::Changed);
        }
        let mut parent = name.rsplit_once('/').map(|v| v.0);
        while let Some(value) = parent {
            directories.insert(value.as_bytes().to_vec());
            parent = value.rsplit_once('/').map(|v| v.0);
        }
    }
    for raw in directories {
        builder.append(&raw, EntryKind::Directory,
            &mut Cursor::new([]), 0, stop)?;
    }
    Ok(())
}

///
/// 相対名の各段階をsymlinkを辿らずに確認する。
///
/// # 概要
/// rootから相対名の各segmentを追加してmetadataを検査し、symlinkを拒否する。
///
/// # 引数
/// * `root` - worktree root
/// * `name` - 検証済み相対名
///
/// # 戻り値
/// 実体とmetadata。削除済みはNone。
///
fn checked_path(root: &Path, name: &str)
    -> Result<Option<(PathBuf, Metadata)>, SourceError>
{
    let mut path = root.to_path_buf();
    let mut metadata = None;
    /*
     * rootから順に、親のsymlinkも検査する
     */
    for part in name.split('/') {
        path.push(part);
        let next = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(_) => return Err(SourceError::Io),
        };
        if next.file_type().is_symlink() { return Err(SourceError::Symlink); }
        metadata = Some(next);
    }
    Ok(metadata.map(|value| (path, value)))
}

///
/// open前後の実体を比較し、置換されたファイルを読まない。
///
/// # 引数
/// * `before` - symlink検査時metadata
/// * `opened` - openしたhandleのmetadata
///
/// # 戻り値
/// Linuxでは同じinode/devとサイズの通常ファイルならtrue
///
fn same_file(before: &Metadata, opened: &Metadata) -> bool {
    #[cfg(unix)]
    let identity = before.dev() == opened.dev() && before.ino() == opened.ino();
    #[cfg(not(unix))]
    let identity = true;
    identity && opened.is_file() && before.len() == opened.len()
}
