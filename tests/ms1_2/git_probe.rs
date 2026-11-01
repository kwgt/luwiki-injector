/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */
//!
//! GitおよびGitHub入力の技術実証
//!

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail, ensure};
use tempfile::TempDir;

use gix::credentials::helper::{Action, NextAction};
use gix::credentials::protocol::Outcome;
use gix::sec::identity::Account;

///
/// 実ユーザーデータから隔離したGit fixture
///
struct GitFixture {
    dir: TempDir,
}

impl GitFixture {
    ///
    /// mainを持つローカルGit fixtureを作成
    ///
    /// # 戻り値
    /// 初期コミット済みの一時リポジトリ
    ///
    fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        git(dir.path(), &["init", "-q", "-b", "main"])?;
        fs::create_dir_all(dir.path().join("root"))?;
        fs::write(dir.path().join("root/page.md"), b"main\n")?;
        fs::write(dir.path().join("root/deleted.md"), b"old\n")?;
        fs::write(dir.path().join(".gitignore"), b"*.skip.md\n")?;
        git(dir.path(), &["add", "."])?;
        git(dir.path(), &["commit", "-qm", "initial"])?;
        Ok(Self { dir })
    }

    ///
    /// fixtureの作業ツリーパスを返す
    ///
    /// # 戻り値
    /// 一時リポジトリの作業ツリーパス
    ///
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

///
/// 一時fixtureだけでGitコマンドを実行
///
/// # 概要
/// 一時repo向けの設定と環境でGitを実行し、終了成功とUTF-8のstdoutを確認する。
///
/// # 引数
/// - `dir`: 一時リポジトリの作業ツリー
/// - `args`: Gitコマンド引数
///
/// # 戻り値
/// 標準出力、またはコマンド失敗
///
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    /*
     * 一時repo用の設定・環境でGitを実行する
     */
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()?;
    /*
     * 終了成功を確認してUTF-8のstdoutを返す
     */
    ensure!(
        output.status.success(),
        "fixture git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

///
/// mainを優先し、無ければmasterを選ぶ
///
/// # 引数
/// - `repo`: 読取対象のローカルリポジトリ
///
/// # 戻り値
/// 選択した完全ref名、または参照なしエラー
///
fn default_ref(repo: &gix::Repository) -> Result<&'static str> {
    if repo.rev_parse_single("refs/heads/main").is_ok() {
        return Ok("refs/heads/main");
    }
    if repo.rev_parse_single("refs/heads/master").is_ok() {
        return Ok("refs/heads/master");
    }
    bail!("main and master are absent")
}

///
/// gixで指定参照のMarkdown blobを読む
///
/// # 引数
/// - `repo`: 読取対象のリポジトリ
/// - `reference`: ブランチ、タグ、コミットIDのいずれか
///
/// # 戻り値
/// `root/page.md`の内容、または参照・オブジェクトエラー
///
fn page_at(repo: &gix::Repository, reference: &str) -> Result<Vec<u8>> {
    let commit = repo
        .rev_parse_single(reference)?
        .object()?
        .peel_to_commit()?;
    let entry = commit
        .tree()?
        .lookup_entry_by_path("root/page.md")?
        .context("page blob is absent")?;
    Ok(entry.object()?.into_blob().data.clone())
}

///
/// gixのindexとstatusから作業ツリー候補を集める
///
/// # 概要
/// indexとstatusの候補を合わせ、実在するroot配下のMarkdownだけをsymlink検査後に返す。
///
/// # 引数
/// - `dir`: ローカルGit作業ツリー
///
/// # 戻り値
/// 実在するMarkdown相対パス、またはUTF-8・symlink等のエラー
///
fn worktree_pages(dir: &Path) -> Result<BTreeSet<String>> {
    let repo = gix::open(dir)?;
    let index = repo.index_or_empty()?;
    let mut paths = BTreeSet::new();

    /*
     * index内の追跡済みパスを収集
     */
    for entry in index.entries() {
        let raw: &[u8] = entry.path(&index).as_ref();
        paths.insert(std::str::from_utf8(raw)?.to_owned());
    }

    /*
     * statusからignore対象外の未追跡パスを追加
     */
    let status = repo
        .status(gix::progress::Discard)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(Vec::<gix::bstr::BString>::new())?;
    for item in status {
        let item = item?;
        let raw: &[u8] = item.location().as_ref();
        paths.insert(std::str::from_utf8(raw)?.to_owned());
    }

    /*
     * 作業ツリーに現存する通常ファイルだけを残す
     */
    let mut pages = BTreeSet::new();
    for path in paths {
        if !path.starts_with("root/") || !path.ends_with(".md") {
            continue;
        }
        let full = dir.join(&path);
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(!metadata.file_type().is_symlink(), "symlink in worktree");
        if metadata.is_file() {
            pages.insert(path);
        }
    }
    Ok(pages)
}

///
/// HTTPSのGitリポジトリを隔離ディレクトリへ取得
///
/// # 概要
/// HTTPSと資格情報非埋込を確認し、限定した認証callbackでfetch・checkoutしてIDを返す。
///
/// # 引数
/// - `url`: 資格情報を含まないHTTPS clone URL
/// - `reference`: 取得する完全ref名
/// - `dir`: 一時ディレクトリ内の未使用パス
/// - `token`: private GitHub用の実行時トークン
///
/// # 戻り値
/// 取得したコミットID、またはGit取得エラー
///
fn clone_https(
    url: &str,
    reference: &str,
    dir: &Path,
    token: Option<String>,
) -> Result<String> {
    /*
     * HTTPSと資格情報非埋込を確認して取得を準備する
     */
    ensure!(url.starts_with("https://"), "HTTPS URL required");
    ensure!(!url.contains('@'), "credentials in URL");
    let mut prepare = gix::clone::PrepareFetch::new(
        url,
        dir,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::isolated(),
    )?
    .with_revision(Some(reference))?
    .configure_connection(move |connection| {
        let token = token.clone();
        connection.set_credentials(move |action| {
            github_credential(action, token.as_deref())
        });
        Ok(())
    });
    /*
     * 同じ同期処理でfetch・checkoutしてcommit IDを返す
     */
    let interrupted = AtomicBool::new(false);
    let (mut checkout, _) =
        prepare.fetch_then_checkout(gix::progress::Discard, &interrupted)?;
    let (repo, _) =
        checkout.main_worktree(gix::progress::Discard, &interrupted)?;
    Ok(repo.head_commit()?.id.to_string())
}

///
/// GitHub HTTPSだけに実行時トークンを供給
///
/// # 概要
/// HTTPSのgithub.comへのGetだけに任意tokenを供給し、StoreとEraseでは保存しない。
///
/// # 引数
/// - `action`: gixの資格情報操作
/// - `token`: 実行時にメモリへ渡されたトークン
///
/// # 戻り値
/// GitHub HTTPSの取得要求なら資格情報、それ以外はなし
///
fn github_credential(
    action: Action,
    token: Option<&str>,
) -> gix::credentials::protocol::Result {
    /*
     * 操作種別を分け、Getだけの資格情報供給を判定する
     */
    match action {
        Action::Get(context) => {
            /*
             * HTTPSのGitHub hostと任意tokenの存在を確認する
             */
            if context.protocol.as_deref() != Some("https")
                || context.host.as_deref() != Some("github.com")
            {
                return Ok(None);
            }
            let Some(secret) = token else {
                return Ok(None);
            };
            Ok(Some(Outcome {
                identity: Account {
                    username: "x-access-token".to_owned(),
                    password: secret.to_owned(),
                    oauth_refresh_token: None,
                },
                next: NextAction::from(context),
            }))
        }
        Action::Store(_) | Action::Erase(_) => Ok(None),
    }
}

///
/// トークンをGitHub HTTPS以外へ渡さないことを確認
///
/// # 概要
/// ダミー資格情報のGitHub HTTPSへの供給と別hostへの供給拒否を確認する。
///
/// # 戻り値
/// 対象hostだけにメモリ内資格情報が返れば成功
///
/// # 注記
/// ダミートークンのみを使い、外部通信を行わない。
///
#[test]
fn private_credential_scope() -> Result<()> {
    use gix::credentials::protocol::Context as CredentialContext;

    /*
     * GitHub HTTPSへダミー資格情報が返ることを照合する
     */
    let github = Action::Get(CredentialContext {
        protocol: Some("https".to_owned()),
        host: Some("github.com".to_owned()),
        ..Default::default()
    });
    let credential = github_credential(github, Some("dummy-secret"))
        .map_err(|_| anyhow::anyhow!("credential callback failed"))?
        .context("GitHub credential missing")?;
    assert_eq!(credential.identity.username, "x-access-token");
    assert_eq!(credential.identity.password, "dummy-secret");

    /*
     * 別hostにはダミー資格情報を供給しないことを照合する
     */
    let other = Action::Get(CredentialContext {
        protocol: Some("https".to_owned()),
        host: Some("example.org".to_owned()),
        ..Default::default()
    });
    let unrelated = github_credential(other, Some("dummy-secret"))
        .map_err(|_| anyhow::anyhow!("credential callback failed"))?;
    ensure!(unrelated.is_none());
    Ok(())
}

///
/// main→masterの選択が現在のHEADに依存しないことを実証
///
/// # 概要
/// HEADをmasterへ移してmain優先を確認し、main削除後だけmasterを採用する。
///
/// # 戻り値
/// 選択順と本文が一致すれば成功
///
/// # 注記
/// HEADをmasterへ移し、両ブランチとmaster単独を調べる。
///
#[test]
fn main_then_master_ignores_head() -> Result<()> {
    /*
     * HEADをmasterへ移した一時repoを用意する
     */
    let fixture = GitFixture::new()?;
    git(fixture.path(), &["switch", "-q", "-c", "master"])?;
    fs::write(fixture.path().join("root/page.md"), b"master\n")?;
    git(fixture.path(), &["add", "root/page.md"])?;
    git(fixture.path(), &["commit", "-qm", "master page"])?;

    /*
     * main優先と本文を照合する
     */
    let repo = gix::open(fixture.path())?;
    assert_eq!(default_ref(&repo)?, "refs/heads/main");
    assert_eq!(page_at(&repo, default_ref(&repo)?)?, b"main\n");
    drop(repo);

    /*
     * mainを削除してからmaster採用を照合する
     */
    git(fixture.path(), &["branch", "-D", "main"])?;
    let repo = gix::open(fixture.path())?;
    assert_eq!(default_ref(&repo)?, "refs/heads/master");
    assert_eq!(page_at(&repo, default_ref(&repo)?)?, b"master\n");
    Ok(())
}

///
/// ブランチ、注釈付きタグ、コミットIDをgixで解決
///
/// # 戻り値
/// 指定参照の本文が一致すれば成功
///
/// # 注記
/// fixture作成にGit CLIを使い、読取はgixだけで行う。
///
#[test]
fn explicit_branch_tag_and_commit() -> Result<()> {
    let fixture = GitFixture::new()?;
    let first_commit = git(fixture.path(), &["rev-parse", "HEAD"])?;
    git(fixture.path(), &["switch", "-q", "-c", "feature"])?;
    fs::write(fixture.path().join("root/page.md"), b"feature\n")?;
    git(fixture.path(), &["add", "root/page.md"])?;
    git(fixture.path(), &["commit", "-qm", "feature page"])?;
    git(fixture.path(), &["tag", "-a", "v1", "-m", "release"])?;

    let repo = gix::open(fixture.path())?;
    assert_eq!(page_at(&repo, "refs/heads/feature")?, b"feature\n");
    assert_eq!(page_at(&repo, "refs/tags/v1")?, b"feature\n");
    assert_eq!(page_at(&repo, &first_commit)?, b"main\n");
    Ok(())
}

///
/// staged、unstaged、untracked、ignored、deleted、symlinkを検証
///
/// # 概要
/// 各状態のファイルとsymlinkを用意し、symlink拒否後の対象集合と本文を確認する。
///
/// # 戻り値
/// 対象集合とsymlink拒否が仕様どおりなら成功
///
/// # 注記
/// symlinkだけを取り除いて再度対象集合を確認する。
///
#[cfg(unix)]
#[test]
fn worktree_target_set_and_symlink() -> Result<()> {
    use std::os::unix::fs::symlink;

    /*
     * 各追跡状態の本文とsymlinkを一時repoへ用意する
     */
    let fixture = GitFixture::new()?;
    fs::write(fixture.path().join("root/staged.md"), b"staged\n")?;
    git(fixture.path(), &["add", "root/staged.md"])?;
    fs::write(fixture.path().join("root/page.md"), b"unstaged\n")?;
    fs::write(fixture.path().join("root/untracked.md"), b"new\n")?;
    fs::write(fixture.path().join("root/hidden.skip.md"), b"ignored\n")?;
    fs::remove_file(fixture.path().join("root/deleted.md"))?;
    symlink("page.md", fixture.path().join("root/link.md"))?;

    /*
     * symlinkを拒否してから対象集合と実本文を照合する
     */
    ensure!(worktree_pages(fixture.path()).is_err());
    fs::remove_file(fixture.path().join("root/link.md"))?;
    let pages = worktree_pages(fixture.path())?;
    let expected = BTreeSet::from([
        "root/page.md".to_owned(),
        "root/staged.md".to_owned(),
        "root/untracked.md".to_owned(),
    ]);
    assert_eq!(pages, expected);
    assert_eq!(
        fs::read(fixture.path().join("root/page.md"))?,
        b"unstaged\n"
    );
    Ok(())
}

///
/// gixで公開GitHubリポジトリをHTTPS clone
///
/// # 戻り値
/// masterのコミットが取得できれば成功
///
/// # 注記
/// 外部ネットワークを使うため、
/// 明示的に選択した場合だけ実行する。
///
#[test]
#[ignore = "public HTTPS Git network access required"]
fn public_https_clone() -> Result<()> {
    let temp = TempDir::new()?;
    let id = clone_https(
        "https://github.com/octocat/Hello-World.git",
        "refs/heads/master",
        &temp.path().join("public"),
        None,
    )?;
    ensure!(!id.is_empty(), "empty commit id");
    Ok(())
}

///
/// octocrabで公開GitHubのrepoとrefを読む
///
/// # 戻り値
/// repo情報とmaster参照が読めれば成功
///
/// # 注記
/// 外部APIを使うため明示的に選択した場合だけ実行する。
///
#[tokio::test]
#[ignore = "public GitHub API network access required"]
async fn public_github_repo_and_ref() -> Result<()> {
    use octocrab::params::repos::Reference;

    let client = octocrab::OctocrabBuilder::default().build()?;
    let repo = client.repos("octocat", "Hello-World").get().await?;
    assert_eq!(repo.name, "Hello-World");
    client
        .repos("octocat", "Hello-World")
        .get_ref(&Reference::Branch("master".to_owned()))
        .await?;
    Ok(())
}

///
/// private GitHubのAPI確認とgix HTTPS取得を試す
///
/// # 概要
/// 実行時指定のprivate repoをAPIで確認し、mainまたはmasterをtoken付きHTTPSで取得する。
///
/// # 戻り値
/// 指定repoがprivateでmainまたはmasterを取得できれば成功
///
/// # 注記
/// `GITHUB_TOKEN`と`MS12_PRIVATE_GITHUB_REPO`は実行時だけ読む。
///
#[tokio::test]
#[ignore = "private GitHub fixture and token required"]
async fn private_github_api_and_clone() -> Result<()> {
    use octocrab::params::repos::Reference;

    /*
     * 実行時指定のtokenと対象repoを収集する
     */
    let token =
        std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN is required")?;
    let target = std::env::var("MS12_PRIVATE_GITHUB_REPO")
        .context("private owner/repo fixture is required")?;
    let (owner, name) = target
        .split_once('/')
        .context("private fixture must be owner/repo")?;
    ensure!(
        !owner.is_empty() && !name.is_empty() && !name.contains('/'),
        "invalid fixture"
    );
    /*
     * APIでprivate状態と取得するbranchを確認する
     */
    let client = octocrab::OctocrabBuilder::default()
        .personal_token(token.clone())
        .build()?;
    let repo = client.repos(owner, name).get().await?;
    ensure!(repo.private == Some(true), "fixture is not private");
    let branch = if client
        .repos(owner, name)
        .get_ref(&Reference::Branch("main".to_owned()))
        .await
        .is_ok()
    {
        "main"
    } else {
        client
            .repos(owner, name)
            .get_ref(&Reference::Branch("master".to_owned()))
            .await?;
        "master"
    };
    /*
     * APIのHTTPS URLを検査して専用領域へGit取得する
     */
    let url = repo.clone_url.context("HTTPS clone URL is absent")?;
    ensure!(url.scheme() == "https", "HTTPS clone URL required");
    ensure!(url.username().is_empty(), "credentials in clone URL");
    let temp = TempDir::new()?;
    clone_https(
        url.as_str(),
        &format!("refs/heads/{branch}"),
        &temp.path().join("private"),
        Some(token),
    )?;
    Ok(())
}

///
/// Git実証モジュールが統合テストへ接続済みか確認
///
/// # 注記
/// グループ1の共有ヘルパーを確認する。
///
/// # 戻り値
/// なし。実証モジュールの共有ヘルパー接続が異なる場合はassertで失敗する。
///
#[test]
fn git_probe_is_connected() {
    assert_eq!(super::support::probe_group_name(), "ms1.2");
}
