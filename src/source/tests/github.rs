/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! GitHub APIの情報検証・停止・限定資格情報と実接続を検証する
//!

mod mock;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gix::credentials::helper::Action;

use crate::cmd_args::resolve::source::{GitSelector, SourceSpec};
use crate::report::types::SourceKind;
use crate::secret::Secret;
use crate::source::github::{self, Metadata};
use crate::source::{SourceError, StopContext, remote};

use super::{GitFixture, body, limits, stop};
use mock::{MockApi, Reply};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TAG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

///
/// repo情報の最小JSONを構成する。
///
/// # 引数
/// * `url` - fixtureのclone先
/// * `private` - fixtureのprivate値
///
/// # 戻り値
/// 固定repo名のJSON
///
fn repo_json(url: &str, private: bool) -> String {
    format!("{{\"full_name\":\"fixture/repo\",\"clone_url\":\"{url}\",\
        \"private\":{private},\"default_branch\":\"other\"}}")
}

///
/// refの最小JSONを構成する。
///
/// # 引数
/// * `name` - 完全ref名
/// * `kind` - commit/tag/tree
/// * `sha` - 参照先ID
///
/// # 戻り値
/// object URLを参照しないことも確認できるJSON
///
fn ref_json(name: &str, kind: &str, sha: &str) -> String {
    format!("{{\"ref\":\"{name}\",\"object\":{{\"type\":\"{kind}\",\
        \"sha\":\"{sha}\",\"url\":\"https://untrusted.invalid\"}}}}")
}

///
/// repo確認とcommit実体確認の共通応答を用意する。
///
/// # 戻り値
/// 一時loopback API。外部通信を行わない。
///
fn api() -> MockApi {
    MockApi::new(vec![
        ("/repos/fixture/repo".to_owned(), Reply::json(200,
            repo_json("https://github.com/fixture/repo.git", false))),
        (format!("/repos/fixture/repo/git/commits/{SHA}"),
            Reply::json(200, format!("{{\"sha\":\"{SHA}\"}}"))),
    ])
}

///
/// 模擬APIから製品のmetadata解決を実行する。
///
/// # 引数
/// * `api` - loopback API
/// * `selector` - 参照方針
///
/// # 戻り値
/// 検証済み情報または安全な失敗分類
///
async fn resolve(api: &MockApi, selector: GitSelector)
    -> Result<Metadata, SourceError>
{
    github::metadata_fixture(&api.base(), "fixture", "repo", selector,
        None, &stop()).await
}

///
/// GitHub default branchよりmainを優先する。
///
/// # 注記
/// repo情報のdefaultをotherにし、mainだけの要求を確認する。
///
/// # 戻り値
/// なし。mainの優先選択と不要な参照照会の非発行が異なればassertで失敗する。
///
#[tokio::test]
async fn prefers_main_without_using_default_branch() {
    let api = api();
    api.reply("/repos/fixture/repo/git/ref/heads/main", Reply::json(200,
        ref_json("refs/heads/main", "commit", SHA)));
    let metadata = resolve(&api, GitSelector::MainThenMaster).await
        .expect("main情報");
    assert!(metadata.commit() == SHA);
    assert!(!metadata.is_private());
    assert!(!api.paths().iter().any(|p| p.contains("master")));
    assert!(!api.paths().iter().any(|p| p.contains("other")));
}

///
/// main不存在の404の場合にだけmasterを確認する。
///
/// # 注記
/// mainを未登録にし、masterの実体確認までの要求を照合する。
///
/// # 戻り値
/// なし。main不存在時だけのmaster照会や要求数が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn falls_back_to_master_only_when_main_is_absent() {
    let api = api();
    api.reply("/repos/fixture/repo/git/ref/heads/master", Reply::json(200,
        ref_json("refs/heads/master", "commit", SHA)));
    assert!(resolve(&api, GitSelector::MainThenMaster).await
        .expect("master情報").commit() == SHA);
    assert!(api.paths().len() == 4);
}

///
/// branch名の#やslashをURL構文と混同しない。
///
/// # 注記
/// encoded pathを模擬APIへ登録し、元の完全ref名と照合する。
///
/// # 戻り値
/// なし。branch名のescapeと完全refの対応が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn escapes_branch_names_and_keeps_namespace() {
    let api = api();
    api.reply("/repos/fixture/repo/git/ref/heads/test%231%2Fx", Reply::json(200,
        ref_json("refs/heads/test#1/x", "commit", SHA)));
    assert!(resolve(&api, GitSelector::Branch("test#1/x".into())).await
        .expect("escaped branch").commit() == SHA);
}

///
/// 注釈tagをcommitまでpeelし、応答内URLを要求先に使用しない。
///
/// # 注記
/// tag objectの参照を返し、固定endpointからcommit実体を取得する。
///
/// # 戻り値
/// なし。tagからcommitの確認や固定endpointの要求数が異なればassertで失敗する。
///
#[tokio::test]
async fn peels_annotated_tag_without_following_object_urls() {
    let api = api();
    api.reply("/repos/fixture/repo/git/ref/tags/release", Reply::json(200,
        ref_json("refs/tags/release", "tag", TAG)));
    api.reply(&format!("/repos/fixture/repo/git/tags/{TAG}"), Reply::json(200,
        format!("{{\"sha\":\"{TAG}\",\"object\":{{\"type\":\"commit\",\
            \"sha\":\"{SHA}\",\"url\":\"https://untrusted.invalid\"}}}}")));
    assert!(resolve(&api, GitSelector::Tag("release".into())).await
        .expect("tag情報").commit() == SHA);
    assert!(api.paths().len() == 4);
}

///
/// tag循環とcommit以外のobjectを入力として拒否する。
///
/// # 注記
/// 自分を指すtagとtree tagを別の模擬APIで調べる。
///
/// # 戻り値
/// なし。tag循環や非commit objectの拒否分類が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_cyclic_tags_and_noncommit_objects() {
    /*
     * 循環tagを再訪する前に失敗させる
     */
    let api = api();
    api.reply("/repos/fixture/repo/git/ref/tags/cycle", Reply::json(200,
        ref_json("refs/tags/cycle", "tag", TAG)));
    api.reply(&format!("/repos/fixture/repo/git/tags/{TAG}"), Reply::json(200,
        format!("{{\"sha\":\"{TAG}\",\"object\":{{\"type\":\"tag\",\
            \"sha\":\"{TAG}\"}}}}")));
    assert!(matches!(resolve(&api, GitSelector::Tag("cycle".into())).await,
        Err(SourceError::GitHubIdentity)));
    api.reply("/repos/fixture/repo/git/ref/tags/tree", Reply::json(200,
        ref_json("refs/tags/tree", "tree", SHA)));
    assert!(matches!(resolve(&api, GitSelector::Tag("tree".into())).await,
        Err(SourceError::RefNotFound)));
}

///
/// 完全・短縮hex IDをcommit用endpointで実IDへ照合する。
///
/// # 注記
/// prefix一致と不一致を返し、revision式を拒否する。
///
/// # 戻り値
/// なし。commitの完全ID・prefix照合や不正参照の拒否が異なればassertで失敗する。
///
#[tokio::test]
async fn resolves_commit_prefix_and_rejects_mismatches() {
    let api = api();
    for id in [SHA, &SHA[..10]] {
        api.reply(&format!("/repos/fixture/repo/commits/{id}"),
            Reply::json(200, format!("{{\"sha\":\"{SHA}\"}}")));
        assert!(resolve(&api, GitSelector::Commit(id.into())).await
            .expect("commit情報").commit() == SHA);
    }
    api.reply("/repos/fixture/repo/commits/bbbbbbbb", Reply::json(200,
        format!("{{\"sha\":\"{SHA}\"}}")));
    for id in ["bbbbbbbb", "HEAD~1"] {
        assert!(matches!(resolve(&api, GitSelector::Commit(id.into())).await,
            Err(SourceError::RefNotFound)));
    }
}

///
/// repo不存在とref不存在を区別し、認証拒否でmasterへfallbackしない。
///
/// # 概要
/// 不存在・認証・拒否・server障害を静的分類へ写し、再送を確認する。
///
/// # 注記
/// 外部messageにダミー秘密値を入れても分類だけが返ることを確認する。
///
/// # 戻り値
/// なし。API失敗分類・再送非発行・秘密非露出が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn classifies_api_errors_without_retry_or_secret_exposure() {
    /*
     * mainの失敗を認証・拒否・API障害へ分け、自動再送を拒否する
     */
    for (status, expected) in [(401, SourceError::GitHubAuth),
        (403, SourceError::GitHubDenied), (429, SourceError::GitHubDenied),
        (500, SourceError::GitHubApi)]
    {
        let api = api();
        api.reply("/repos/fixture/repo/git/ref/heads/main", Reply::json(status,
            "{\"message\":\"dummy-github-secret dummy-body\"}".into()));
        let error = resolve(&api, GitSelector::MainThenMaster).await
            .expect_err("API失敗");
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("dummy-"));
        assert!(api.paths().len() == 2);
    }
    /*
     * repoとrefの404を別の分類で返す
     */
    let api = MockApi::new(Vec::new());
    assert!(matches!(resolve(&api, GitSelector::MainThenMaster).await,
        Err(SourceError::GitHubMissing)));
    let api = self::api();
    assert!(matches!(resolve(&api, GitSelector::Branch("missing".into())).await,
        Err(SourceError::RefNotFound)));
}

///
/// APIのidentity不一致や資格情報付きclone URLを取得前に拒否する。
///
/// # 注記
/// host・repo・userinfo・query・fragment・portの異常をrepo応答へ入れる。
///
/// # 戻り値
/// なし。取得前のclone URL検証や要求数が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_untrusted_clone_urls() {
    /*
     * APIが返したURLを信頼せず、GitHub同一repoだけを許容する
     */
    for url in ["https://other.invalid/fixture/repo.git",
        "https://github.com/fixture/other.git",
        "https://dummy-secret@github.com/fixture/repo.git",
        "http://github.com/fixture/repo.git",
        "https://github.com:444/fixture/repo.git",
        "https://github.com/fixture/repo.git?dummy-secret",
        "https://github.com/fixture/repo.git#dummy-secret"]
    {
        let api = api();
        api.reply("/repos/fixture/repo", Reply::json(200,
            repo_json(url, true)));
        assert!(matches!(resolve(&api, GitSelector::MainThenMaster).await,
            Err(SourceError::GitHubIdentity)));
        assert!(api.paths().len() == 1);
    }
}

///
/// 不正JSON・repo名・ref名・IDの不一致を安全に拒否する。
///
/// # 概要
/// API応答の必須項目と要求したidentityを段階ごとに検査する。
///
/// # 注記
/// 必須情報欠落と不一致を別のmock応答へ入れる。
///
/// # 戻り値
/// なし。API必須情報・要求identity・objectの不正拒否が異なればassertで失敗する。
///
#[tokio::test]
async fn rejects_malformed_and_mismatched_api_information() {
    /*
     * repo名と必須情報の検査をref解決より先に行う
     */
    let api = api();
    for body in ["{\"full_name\":\"other/repo\",\"clone_url\":\
        \"https://github.com/fixture/repo.git\",\"private\":false}",
        "{\"message\":\"dummy-secret\"}"]
    {
        api.reply("/repos/fixture/repo", Reply::json(200, body.into()));
        assert!(resolve(&api, GitSelector::MainThenMaster).await.is_err());
    }
    /*
     * ref名・object ID・commit実体の不一致も拒否する
     */
    api.reply("/repos/fixture/repo", Reply::json(200,
        repo_json("https://github.com/fixture/repo.git", false)));
    for response in [ref_json("refs/tags/main", "commit", SHA),
        ref_json("refs/heads/main", "commit", "invalid"),
        ref_json("refs/heads/main", "tree", SHA)]
    {
        api.reply("/repos/fixture/repo/git/ref/heads/main",
            Reply::json(200, response));
        assert!(matches!(resolve(&api, GitSelector::MainThenMaster).await,
            Err(SourceError::GitHubIdentity)));
    }
}

///
/// API redirectで認証情報を別の要求へ引き継がない。
///
/// # 注記
/// 同一mock originの別pathへ302を返し、要求が一件だけで終わるか調べる。
///
/// # 戻り値
/// なし。redirect拒否・認証送信・後続要求非発行が異なればassertで失敗する。
///
#[tokio::test]
async fn refuses_api_redirect_and_does_not_forward_token() {
    let api = api();
    api.reply("/repos/fixture/repo", Reply::redirect(
        format!("{}/redirect-target", api.base())));
    let token = Secret::new("dummy-github-secret".into());
    let result = github::metadata_fixture(&api.base(), "fixture", "repo",
        GitSelector::MainThenMaster, Some(&token), &stop()).await;
    assert!(result.is_err());
    assert!(api.paths().len() == 1);
    assert!(api.authenticated());
}

///
/// HTTP要求中も取消・共有期限を検査する。
///
/// # 概要
/// 遅延した要求でHTTP完了より停止が先に処理されることを確認する。
///
/// # 注記
/// 応答を遅延させた模擬APIで期限停止と取消通知を分けて確認する。
///
/// # 戻り値
/// なし。HTTP待機中の共有期限・取消の失敗分類が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn interrupts_metadata_requests_on_deadline_and_cancel() {
    /*
     * 期限に達したHTTP futureを破棄する
     */
    let api = api();
    api.reply("/repos/fixture/repo",
        Reply::delayed(Duration::from_millis(200)));
    let deadline = StopContext::new(Instant::now() + Duration::from_millis(50));
    let result = github::metadata_fixture(&api.base(), "fixture", "repo",
        GitSelector::MainThenMaster, None, &deadline).await;
    assert!(matches!(result, Err(SourceError::Deadline)));
    /*
     * 期限前の明示取消も通知する
     */
    let context = stop();
    let cancellation = context.clone();
    let notify = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    });
    let result = github::metadata_fixture(&api.base(), "fixture", "repo",
        GitSelector::MainThenMaster, None, &context).await;
    notify.await.expect("取消通知");
    assert!(matches!(result, Err(SourceError::Cancelled)));
}

///
/// API bearer送信とprivate状態を確認し、metadata Debugに秘密値を含めない。
///
/// # 注記
/// ダミーbearerだけをmockへ送り、header本文を記録せず真偽だけ確認する。
///
/// # 戻り値
/// なし。API認証・private状態・metadataの秘密非露出が異なればassertで失敗する。
///
#[tokio::test]
async fn sends_api_token_and_redacts_metadata_debug() {
    let api = api();
    api.reply("/repos/fixture/repo", Reply::json(200,
        repo_json("https://github.com/fixture/repo.git", true)));
    api.reply("/repos/fixture/repo/git/ref/heads/main", Reply::json(200,
        ref_json("refs/heads/main", "commit", SHA)));
    let token = Secret::new("dummy-github-secret".into());
    let metadata = github::metadata_fixture(&api.base(), "fixture", "repo",
        GitSelector::MainThenMaster, Some(&token), &stop()).await
        .expect("private mock情報");
    assert!(api.authenticated());
    assert!(metadata.is_private());
    assert!(!format!("{metadata:?} {token:?}").contains("dummy-github-secret"));
    assert!(!format!("{metadata:?}").contains("https://"));
}

///
/// 不正headerとなるtokenをpanicやHTTP送信なしで拒否する。
///
/// # 注記
/// 空値と改行入りダミー値を渡し、秘密値が失敗分類に含まれないか確認する。
///
/// # 戻り値
/// なし。不正tokenの認証失敗分類や送信ゼロが異なる場合はassertで失敗する。
///
#[tokio::test]
async fn rejects_invalid_token_header_before_request() {
    let api = api();
    for value in ["", "dummy-github-secret\r\nInjected: dummy-body"] {
        let token = Secret::new(value.into());
        let result = github::metadata_fixture(&api.base(), "fixture", "repo",
            GitSelector::MainThenMaster, Some(&token), &stop()).await;
        assert!(matches!(result, Err(SourceError::GitHubAuth)));
    }
    assert!(api.paths().is_empty());
}

///
/// gixのURLだけの要求からhost/port/repoを検査して資格情報を限定する。
///
/// # 概要
/// 正しいGetと境界外Get・保存要求を分けて検査する。
///
/// # 注記
/// 他host・scheme・port・repo・userinfoを拒否し、Store/Eraseも保存しない。
///
/// # 戻り値
/// なし。同一repo限定の資格情報供給や非永続化が異なる場合はassertで失敗する。
///
#[test]
fn scopes_git_credentials_and_does_not_store_them() {
    /*
     * 実際のgixと同じURLだけのGetを適切に解析して供給する
     */
    let auth = remote::authentication_fixture("fixture/repo.git",
        Secret::new("dummy-github-secret".into()));
    let accepted = auth.credential(Action::get_for_url(
        "https://github.com/fixture/repo.git")).expect("callback成功")
        .expect("限定した資格情報");
    assert!(accepted.identity.username == "x-access-token");
    assert!(accepted.identity.password == "dummy-github-secret");
    /*
     * URLに含まれるauthorityとrepoが違えば返さず、保存要求を無視する
     */
    for url in ["http://github.com/fixture/repo.git",
        "https://other.invalid/fixture/repo.git",
        "https://github.com:444/fixture/repo.git",
        "https://github.com/fixture/other.git",
        "https://user@github.com/fixture/repo.git", "invalid"]
    {
        assert!(auth.credential(Action::get_for_url(url))
            .expect("安全な拒否").is_none());
    }
    for action in [Action::Store("password=dummy-github-secret".into()),
        Action::Erase("password=dummy-github-secret".into())]
    {
        assert!(auth.credential(action).expect("保存しない").is_none());
    }
}

///
/// 模擬APIの確定IDを共通取得へ渡し、GitHub種別と回収・非永続化を確認する。
///
/// # 概要
/// file transportは試験だけであり、製品のGitHub HTTPS条件を変更しない。
///
/// # 注記
/// gixを同一workerへ閉じ、ダミートークンが取得repo設定にないことを調べる。
///
/// # 戻り値
/// なし。mock確定commitの取得・参照種別・非永続化・回収が異なればassertで失敗する。
///
#[tokio::test]
async fn mock_api_commit_connects_to_owned_github_snapshot() {
    /*
     * APIでローカルfixtureの完全IDを確認する
     */
    let origin = GitFixture::new();
    let api = api();
    api.reply(&format!("/repos/fixture/repo/commits/{}", origin.initial),
        Reply::json(200, format!("{{\"sha\":\"{}\"}}", origin.initial)));
    let metadata = resolve(&api, GitSelector::Commit(origin.initial.clone()))
        .await.expect("mock commit");
    let commit = metadata.commit().to_owned();
    let path = origin.path().to_owned();
    /*
     * 同じ取得pipelineへ渡し、資格情報の保存がないことを確認する
     */
    let (snapshot, area) = crate::source::runtime::run(stop(), move |context| {
        let mut area = PathBuf::new();
        let snapshot = remote::github_fixture(&path, &commit, limits(),
            &context, &mut |_, path| { area = path.to_owned(); Ok(()) })?;
        Ok((snapshot, area))
    }).await.expect("GitHub snapshot");
    assert!(snapshot.entries().iter().all(|e|
        e.reference().parts().0 == SourceKind::GitHub));
    let config = fs::read(area.join("repo/.git/config")).expect("設定検査");
    assert!(!config.windows(b"dummy-github-secret".len())
        .any(|v| v == b"dummy-github-secret"));
    assert!(body(&snapshot, "pages/page.md") == b"old\r\n");
    drop(snapshot);
    assert!(!area.exists());
}

///
/// 明示されたGitHub試験対象を値を表示せず取得する。
///
/// # 引数
/// * `private` - private fixture用変数を使用するか
///
/// # 戻り値
/// owner・repo・branch・固定SHA
///
fn network_fixture(private: bool) -> (String, String, String, String) {
    let prefix = if private { "LUWIKI_MS31_PRIVATE_GITHUB" }
        else { "LUWIKI_MS31_PUBLIC_GITHUB" };
    let target = std::env::var(format!("{prefix}_REPO"))
        .expect("GitHub fixture repo environment required");
    let (owner, repo) = target.split_once('/').expect("fixture owner/repo");
    assert!(!owner.is_empty() && !repo.is_empty() && !repo.contains('/'));
    let branch = std::env::var(format!("{prefix}_BRANCH"))
        .expect("GitHub fixture branch environment required");
    let sha = std::env::var(format!("{prefix}_COMMIT"))
        .expect("GitHub fixture commit environment required");
    (owner.to_owned(), repo.to_owned(), branch, sha)
}

///
/// 公開GitHub APIと製品gix取得を匿名で確認する。
///
/// # 概要
/// APIで参照を確認し、SourceSpecの共通dispatchで固定commitを取得する。
///
/// # 注記
/// LUWIKI_MS31_PUBLIC_GITHUBのrepo/branch/commitを指定して明示実行する。
///
/// # 戻り値
/// なし。指定公開repoの匿名API確認・固定commit取得が異なればassertで失敗する。
///
#[tokio::test]
#[ignore = "public GitHub API and HTTPS network access required"]
async fn public_github_api_and_acquisition() {
    /*
     * APIのbranch情報と固定SHAをトークンなしで確認する
     */
    let (owner, repo, branch, sha) = network_fixture(false);
    let context = StopContext::new(Instant::now() + Duration::from_secs(60));
    let metadata = github::inspect(&owner, &repo,
        GitSelector::Branch(branch), None, &context).await.expect("public API");
    assert!(!metadata.is_private());
    assert!(metadata.commit() == sha);
    /*
     * APIで確認した固定commitを製品のGitHub取得入口へ渡す
     */
    let mut bound = limits();
    bound.git_bytes = 64 * 1024 * 1024;
    let snapshot = crate::source::acquire(SourceSpec::GitHub {
        owner, repo, selector: GitSelector::Commit(sha),
    }, None, bound, context).await.expect("public GitHub acquisition");
    assert!(snapshot.entries().iter().all(|e|
        e.reference().parts().0 == SourceKind::GitHub));
}

///
/// private repoの実API認証と製品HTTPS token取得を確認する。
///
/// # 概要
/// API・gixの実認証と、取得repo設定・Debugへの秘密情報非露出を確認する。
///
/// # 注記
/// LUWIKI_MS31_PRIVATE_GITHUBとGITHUB_TOKENを環境へ設定して明示実行する。
///
/// # 戻り値
/// なし。指定private repoの認証取得や秘密非露出・非永続化が異なればassertで失敗する。
///
#[tokio::test]
#[ignore = "private GitHub fixture and GITHUB_TOKEN required"]
async fn private_github_api_and_acquisition() {
    /*
     * 秘密値を表示しないままAPI上のprivate状態と固定SHAを確認する
     */
    let (owner, repo, branch, sha) = network_fixture(true);
    let token = Secret::new(std::env::var("GITHUB_TOKEN")
        .expect("GITHUB_TOKEN environment required"));
    let context = StopContext::new(Instant::now() + Duration::from_secs(60));
    let metadata = github::inspect(&owner, &repo,
        GitSelector::Branch(branch), Some(&token), &context)
        .await.expect("private API");
    assert!(metadata.is_private());
    assert!(metadata.commit() == sha);
    /*
     * tokenは同じGitHub repoへのcredential callbackでだけ供給する
     */
    let mut bound = limits();
    bound.git_bytes = 64 * 1024 * 1024;
    let snapshot = crate::source::acquire(SourceSpec::GitHub {
        owner, repo, selector: GitSelector::Commit(sha),
    }, Some(&token), bound, context).await.expect("private acquisition");
    assert!(snapshot.entries().iter().all(|e|
        e.reference().parts().0 == SourceKind::GitHub));
    assert!(!format!("{snapshot:?}").contains(token.expose()));
    /*
     * 実tokenを表示せず、取得repo設定に保存されていないことを確認する
     */
    let area = snapshot.acquisition.as_ref().expect("owned acquisition");
    let config = fs::read(area.0.path().join("repo/.git/config"))
        .expect("private config inspection");
    assert!(!config.windows(token.expose().len())
        .any(|v| v == token.expose().as_bytes()),
        "private token must not be persisted");
}

///
/// 実private repoへの未認証・無効token失敗を安全な分類で確認する。
///
/// # 概要
/// 実tokenを変更せず、未指定と無効ダミーを別々に拒否させる。
///
/// # 注記
/// 実tokenを読み込まず、未指定と無効なダミーだけで明示実行する。
///
/// # 戻り値
/// なし。未認証・無効tokenの拒否分類や秘密非露出が異なればassertで失敗する。
///
#[tokio::test]
#[ignore = "private GitHub fixture network access required"]
async fn private_github_authentication_failure() {
    /*
     * private repoが匿名で取得できないことを確認する
     */
    let (owner, repo, branch, _) = network_fixture(true);
    let context = StopContext::new(Instant::now() + Duration::from_secs(60));
    let mut bound = limits();
    bound.git_bytes = 64 * 1024 * 1024;
    let result = crate::source::acquire(SourceSpec::GitHub {
        owner: owner.clone(), repo: repo.clone(),
        selector: GitSelector::Branch(branch.clone()),
    }, None, bound, context.clone()).await;
    assert!(matches!(result, Err(SourceError::GitHubMissing)
        | Err(SourceError::GitHubAuth) | Err(SourceError::GitHubDenied)));
    /*
     * 実tokenを変更せず、無効なダミーtokenの拒否と非露出を確認する
     */
    let token = Secret::new("invalid-dummy-github-secret".into());
    let result = crate::source::acquire(SourceSpec::GitHub {
        owner, repo, selector: GitSelector::Branch(branch),
    }, Some(&token), bound, context).await;
    let error = result.expect_err("invalid token must fail");
    assert!(matches!(error,
        SourceError::GitHubAuth | SourceError::GitHubDenied));
    assert!(!format!("{error:?} {error}").contains(token.expose()));
}
