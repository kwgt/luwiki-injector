/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 入力種別とGit参照指定を検証するモジュール
//!

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use url::Url;

use crate::cmd_args::CliInput;

use super::ValidationError;

///
/// コミット済みGitツリーの参照指定。
///
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) enum GitSelector {
    MainThenMaster,
    Branch(String),
    Tag(String),
    Commit(String),
}

///
/// ローカル入力だけに許可する作業ツリー指定。
///
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) enum LocalSelector {
    Committed(GitSelector),
    Worktree,
}

///
/// 入力方式と適合する参照指定を組にしたソース指定。
///
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) enum SourceSpec {
    Zip(PathBuf),
    LocalGit { path: PathBuf, selector: LocalSelector },
    HttpsGit { url: Url, selector: GitSelector },
    GitHub { owner: String, repo: String, selector: GitSelector },
}

impl fmt::Debug for SourceSpec {
    ///
    /// 入力文字列を表示せず入力方式だけを示す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Zip(_) => "Zip([validated])",
            Self::LocalGit { .. } => "LocalGit([validated])",
            Self::HttpsGit { .. } => "HttpsGit([validated])",
            Self::GitHub { .. } => "GitHub([validated])",
        })
    }
}

///
/// 入力種別と参照指定を解決する。
///
/// # 概要
/// autoは実在パスを先に確認し、明示種別は対応形式を直接検証する。
///
/// # 引数
/// * `cli` - 未検証のCLI入力
///
/// # 戻り値
/// 排他的なSourceSpec、または安全なエラー
///
pub(super) fn resolve(cli: &CliInput)
    -> Result<SourceSpec, ValidationError>
{
    /*
     * refとworktreeの組合せを先に検証する
     */
    let selector = selector(cli)?;
    let kind = cli.source_type.as_deref().unwrap_or("auto");
    let value = cli.page_source.as_str();
    let path = Path::new(value);

    /*
     * 明示種別またはautoの優先順で実体を調べる
     */
    let spec = match kind {
        "zip" if fs::metadata(path).is_ok_and(|m| m.is_file())
            && path.extension().is_some_and(|v| v == "zip") =>
        {
            SourceSpec::Zip(path.to_path_buf())
        }
        "git" if fs::metadata(path).is_ok_and(|m| m.is_dir()) => {
            local_git(path, selector, cli.worktree)?
        }
        "git" => SourceSpec::HttpsGit {
            url: remote_url(value)?, selector,
        },
        "github" => github(value, selector)?,
        "auto" => match fs::metadata(path) {
            Ok(meta) if meta.is_file()
                && path.extension().is_some_and(|v| v == "zip") =>
            {
                SourceSpec::Zip(path.to_path_buf())
            }
            Ok(meta) if meta.is_dir() => {
                local_git(path, selector, cli.worktree)?
            }
            Ok(_) => return Err(ValidationError::Source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if value.contains("://") {
                    SourceSpec::HttpsGit { url: remote_url(value)?, selector }
                } else {
                    github(value, selector)?
                }
            }
            Err(_) => return Err(ValidationError::Source),
        },
        "zip" => return Err(ValidationError::Source),
        _ => return Err(ValidationError::SourceType),
    };

    /*
     * ZIPへのref指定とリモートへのworktreeを拒否する
     */
    if matches!(spec, SourceSpec::Zip(_)) && has_ref(cli) {
        return Err(ValidationError::RefConflict);
    }
    if cli.worktree && !matches!(spec, SourceSpec::LocalGit { .. }) {
        return Err(ValidationError::Worktree);
    }
    Ok(spec)
}

///
/// refの明示指定有無を返す。
///
/// # 引数
/// * `cli` - CLI入力
///
/// # 戻り値
/// branch/tag/commitのいずれかがある場合true
///
fn has_ref(cli: &CliInput) -> bool {
    cli.branch.is_some() || cli.tag.is_some() || cli.commit.is_some()
}

///
/// 排他的なGit参照指定を構築する。
///
/// # 概要
/// 指定数と空値を検証し、実ref解決前の方針として保持する。
///
/// # 引数
/// * `cli` - CLI入力
///
/// # 戻り値
/// 参照方針。実在のref解決はOP3で行う。
///
fn selector(cli: &CliInput) -> Result<GitSelector, ValidationError> {
    /*
     * 空指定と重複指定を拒否する
     */
    let count = [&cli.branch, &cli.tag, &cli.commit]
        .iter().filter(|v| v.is_some()).count();
    if count > 1 || (count > 0 && cli.worktree) {
        return Err(ValidationError::RefConflict);
    }
    for value in [&cli.branch, &cli.tag, &cli.commit].into_iter().flatten() {
        if value.is_empty() { return Err(ValidationError::RefValue); }
    }
    if let Some(value) = &cli.branch {
        Ok(GitSelector::Branch(value.clone()))
    } else if let Some(value) = &cli.tag {
        Ok(GitSelector::Tag(value.clone()))
    } else if let Some(value) = &cli.commit {
        Ok(GitSelector::Commit(value.clone()))
    } else {
        Ok(GitSelector::MainThenMaster)
    }
}

///
/// ローカルGit worktreeを検出する。
///
/// # 引数
/// * `path` - ローカルディレクトリ
/// * `selector` - コミット参照方針
/// * `worktree` - 作業ツリー指定
///
/// # 戻り値
/// bare以外のローカルGit指定
///
fn local_git(path: &Path, selector: GitSelector, worktree: bool)
    -> Result<SourceSpec, ValidationError>
{
    let repo = gix::open_opts(path, gix::open::Options::isolated())
        .map_err(|_| ValidationError::Source)?;
    let root = repo.workdir().ok_or(ValidationError::Source)?;
    Ok(SourceSpec::LocalGit {
        path: root.to_path_buf(),
        selector: if worktree {
            LocalSelector::Worktree
        } else {
            LocalSelector::Committed(selector)
        },
    })
}

///
/// 資格情報を埋め込まないHTTPS Git URLを検証する。
///
/// # 概要
/// 構文とプロトコルを確認し、URLの資格情報欄を拒否する。
///
/// # 引数
/// * `value` - 未検証URL
///
/// # 戻り値
/// ホストとパスを持つHTTPS URL
///
fn remote_url(value: &str) -> Result<Url, ValidationError> {
    /*
     * scheme区切りと空白・制御文字を解析前に検査する
     */
    if !value.contains("://")
        || value.chars().any(|v| v.is_whitespace() || v.is_control())
    {
        return Err(ValidationError::Source);
    }
    /*
     * HTTPSとhost・資格情報・query・fragmentの条件を照合する
     */
    let url = Url::parse(value).map_err(|_| ValidationError::Source)?;
    if url.scheme() != "https" || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || value.split("://").nth(1).unwrap_or("")
            .split('/').next().unwrap_or("").contains('@')
        || url.query().is_some() || url.fragment().is_some()
    {
        return Err(ValidationError::Source);
    }
    Ok(url)
}

///
/// owner/repoの構文を検証する。
///
/// # 引数
/// * `value` - 未検証GitHub指定
/// * `selector` - コミット参照方針
///
/// # 戻り値
/// GitHub指定。名前の実在・有効性はOP3のAPI境界で確認する。
///
fn github(value: &str, selector: GitSelector)
    -> Result<SourceSpec, ValidationError>
{
    let (owner, repo) = value.split_once('/').ok_or(ValidationError::Source)?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/')
        || value.chars().any(char::is_whitespace)
        || Url::parse(value).is_ok()
    {
        return Err(ValidationError::Source);
    }
    Ok(SourceSpec::GitHub {
        owner: owner.to_owned(), repo: repo.to_owned(), selector,
    })
}
