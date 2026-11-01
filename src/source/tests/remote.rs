/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! リモート取得の共通clone経路と専用領域の検査・回収を検証する
//!

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use url::Url;

use crate::cmd_args::resolve::source::{GitSelector, SourceSpec};
use crate::report::types::SourceKind;
use crate::source::remote::{self, Phase};
use crate::source::{SourceError, SourceSnapshot, StopContext};

use super::{GitFixture, body, git_command, limits, stop};

///
/// fixtureを製品と同じclone・検査・checkout経路へ渡す。
///
/// # 引数
/// * `origin` - テスト用のGit fixture
/// * `selector` - 取得参照方針
///
/// # 戻り値
/// 所有された取得結果
///
fn acquire(origin: &GitFixture, selector: GitSelector)
    -> Result<SourceSnapshot, SourceError>
{
    remote::fixture(origin.path(), selector, limits(), &stop(),
        &mut |_, _| Ok(()))
}

///
/// リモートHEADよりmainを優先し、mainがなければmasterを選ぶ。
///
/// # 注記
/// HEADを旧内容のmasterへ移し、main有無で本文を比較する。
///
/// # 戻り値
/// なし。remote HEAD非依存のmain・master選択と本文が異なればassertで失敗する。
///
#[test]
fn default_reference_does_not_follow_remote_head() {
    let origin = GitFixture::new();
    git_command(origin.path(), &["switch", "-q", "master"]);
    let snapshot = acquire(&origin, GitSelector::MainThenMaster)
        .expect("main取得");
    assert_eq!(body(&snapshot, "pages/page.md"), b"main\n");
    git_command(origin.path(), &["branch", "-D", "main"]);
    let snapshot = acquire(&origin, GitSelector::MainThenMaster)
        .expect("master取得");
    assert_eq!(body(&snapshot, "pages/page.md"), b"old\r\n");
}

///
/// branch・注釈tag・完全/短縮commitの取得が同じ契約で成立する。
///
/// # 概要
/// 同名branch/tagと#を含むbranchを区別し、実commitの本文を照合する。
///
/// # 注記
/// mainと旧commitの異なる本文を指定し、名前空間を分けて比較する。
///
/// # 戻り値
/// なし。branch・tag・完全/短縮commitの参照区別と本文が異なればassertで失敗する。
///
#[test]
fn resolves_branches_tags_and_commit_ids() {
    /*
     * 同名refと#を含むbranch名を別のGit参照として準備する
     */
    let origin = GitFixture::new();
    git_command(origin.path(), &["branch", "fixture-tag", "main"]);
    git_command(origin.path(), &["branch", "test#1", "main"]);
    let hash_branch = acquire(&origin, GitSelector::Branch("test#1".into()))
        .expect("fragmentではなくbranch名");
    assert_eq!(body(&hash_branch, "pages/page.md"), b"main\n");
    let branch = acquire(&origin, GitSelector::Branch("fixture-tag".into()))
        .expect("branch取得");
    assert_eq!(body(&branch, "pages/page.md"), b"main\n");
    /*
     * tagと完全・短縮IDは同じ旧commitを指すことを確認する
     */
    for selector in [GitSelector::Tag("fixture-tag".into()),
        GitSelector::Commit(origin.initial.clone()),
        GitSelector::Commit(origin.initial[..10].to_owned())]
    {
        let snapshot = acquire(&origin, selector).expect("旧commit取得");
        assert_eq!(body(&snapshot, "pages/page.md"), b"old\r\n");
    }
}

///
/// main/masterのない入力と不正refをHEADへfallbackせず拒否する。
///
/// # 注記
/// ref不存在、revision式、tag namespace誤用をそれぞれ取得へ渡す。
///
/// # 戻り値
/// なし。不正参照・名前空間・既定参照不存在の拒否が異なればassertで失敗する。
///
#[test]
fn rejects_absent_and_nonliteral_references() {
    let origin = GitFixture::new();
    for selector in [GitSelector::Branch("missing".into()),
        GitSelector::Tag("main".into()),
        GitSelector::Branch("main~1".into()),
        GitSelector::Commit("HEAD~1".into())]
    {
        assert_eq!(acquire(&origin, selector).err(),
            Some(SourceError::RefNotFound));
    }
    git_command(origin.path(), &["switch", "-q", "fixture-branch"]);
    git_command(origin.path(), &["branch", "-D", "main", "master"]);
    assert_eq!(acquire(&origin, GitSelector::MainThenMaster).err(),
        Some(SourceError::RefNotFound));
}

///
/// 元repoを削除してもsnapshotが本文と取得領域を保持する。
///
/// # 概要
/// blob保管とcheckoutの内容を確認し、元repoと取得leaseの寿命を分ける。
///
/// # 注記
/// source種別・全phase・checkout本文・snapshot破棄後の回収を確認する。
///
/// # 戻り値
/// なし。取得phase・元bytes・入力破棄後の所有寿命・回収が異なればassertで失敗する。
///
#[test]
fn owns_acquisition_and_preserves_unmodified_blob_bytes() {
    /*
     * 全phaseと生成した取得領域を記録する
     */
    let origin = GitFixture::new();
    let mut area = PathBuf::new();
    let mut phases = Vec::new();
    let snapshot = remote::fixture(origin.path(),
        GitSelector::Tag("fixture-tag".into()), limits(), &stop(),
        &mut |phase, path| {
            area = path.to_owned();
            phases.push(phase);
            Ok(())
        }).expect("snapshot取得");
    /*
     * 生本文・参照種別・元入力削除後の寿命と最終回収を確認する
     */
    assert!(phases == [Phase::Prepared, Phase::Fetched, Phase::Validated,
        Phase::CheckedOut]);
    assert_eq!(fs::read(area.join("repo/pages/page.md")).expect("checkout"),
        b"old\r\n");
    assert!(snapshot.entries().iter().all(|entry|
        entry.reference().parts().0 == SourceKind::HttpsGit));
    drop(origin);
    assert_eq!(body(&snapshot, "pages/page.md"), b"old\r\n");
    assert!(area.exists());
    drop(snapshot);
    assert!(!area.exists());
}

///
/// .git・checkout・本文領域を全体計量し、上限ちょうどを許容する。
///
/// # 注記
/// 3種類のfileを別directoryへ置き、合計12bytesの境界を調べる。
///
/// # 戻り値
/// なし。専用領域の全体計量や上限端点の許容・拒否が異なればassertで失敗する。
///
#[test]
fn measures_entire_area_and_checks_exact_byte_boundary() {
    let area = tempdir().expect("計量fixture");
    for (directory, bytes) in [("repo/.git", 3), ("repo/pages", 4),
        ("payload", 5)]
    {
        fs::create_dir_all(area.path().join(directory)).expect("directory");
        fs::write(area.path().join(directory).join("data"), vec![0; bytes])
            .expect("計量file");
    }
    assert_eq!(remote::size(area.path(), 12, &stop()), Ok(12));
    assert_eq!(remote::size(area.path(), 11, &stop()),
        Err(SourceError::Limit("max_git_bytes")));
}

///
/// fetch後と本文保管後の超過をcheckout前に拒否し回収する。
///
/// # 概要
/// 専用領域を増やしてphase境界の全体上限検査を確認する。
///
/// # 注記
/// 各phaseへ上限超過fileを注入し、CheckedOutへ到達しないことを確認する。
///
/// # 戻り値
/// なし。checkout前の容量超過拒否・後続非実行・領域回収が異なればassertで失敗する。
///
#[test]
fn rejects_fetch_and_payload_limits_before_checkout() {
    /*
     * fetch後と本文保管後それぞれへ容量超過を注入する
     */
    let origin = GitFixture::new();
    for target in [Phase::Fetched, Phase::Validated] {
        /*
         * 対象phaseだけに大きなfileを追加し、後続checkoutを観測する
         */
        let mut area = PathBuf::new();
        let mut checked_out = false;
        let bound = limits();
        let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
            bound, &stop(), &mut |phase, path| {
                area = path.to_owned();
                checked_out |= phase == Phase::CheckedOut;
                if phase == target {
                    fs::write(path.join("oversized"),
                        vec![0; bound.git_bytes as usize]).expect("超過注入");
                }
                Ok(())
            });
        assert_eq!(result.err(), Some(SourceError::Limit("max_git_bytes")));
        assert!(!checked_out);
        assert!(!area.exists());
    }
}

///
/// checkout後の超過もsnapshotを返さず領域全体を回収する。
///
/// # 注記
/// CheckedOutで専用領域を増やし、最終全体検査の拒否を確認する。
///
/// # 戻り値
/// なし。checkout後超過の拒否や未公開領域回収が異なる場合はassertで失敗する。
///
#[test]
fn rejects_post_checkout_limit_and_reclaims_all_storage() {
    /*
     * checkout後のphaseへ容量超過を注入する
     */
    let origin = GitFixture::new();
    let mut area = PathBuf::new();
    let bound = limits();
    let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
        bound, &stop(), &mut |phase, path| {
            area = path.to_owned();
            if phase == Phase::CheckedOut {
                fs::write(path.join("oversized"),
                    vec![0; bound.git_bytes as usize]).expect("超過注入");
            }
            Ok(())
        });
    assert_eq!(result.err(), Some(SourceError::Limit("max_git_bytes")));
    assert!(!area.exists());
}

///
/// Markdown本文上限による拒否でも未公開の取得領域を回収する。
///
/// # 注記
/// fetch成功後、最初の本文保管時に1byte上限へ到達する。
///
/// # 戻り値
/// なし。Markdown本文上限の拒否分類や領域回収が異なる場合はassertで失敗する。
///
#[test]
fn rejects_page_limit_and_reclaims_acquisition() {
    let origin = GitFixture::rooted();
    let mut bound = limits();
    bound.page_bytes = 1;
    let mut area = PathBuf::new();
    let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
        bound, &stop(), &mut |_, path| { area = path.to_owned(); Ok(()) });
    assert_eq!(result.err(), Some(SourceError::Limit("max_page_bytes")));
    assert!(!area.exists());
}

///
/// 取得・checkoutの失敗とphase間停止で未完成snapshotを公開しない。
///
/// # 概要
/// 停止通知と安全な失敗注入を分け、各phaseのRAII回収を調べる。
///
/// # 注記
/// 全phaseで停止を注入し、外部生エラーを持たない分類と回収を確認する。
///
/// # 戻り値
/// なし。phase取消・注入失敗の分類や領域回収が異なる場合はassertで失敗する。
///
#[test]
fn phase_cancellation_and_injected_failure_reclaim_area() {
    /*
     * 各phaseで共有flagを更新して停止させる
     */
    let origin = GitFixture::new();
    for phase in [Phase::Prepared, Phase::Fetched, Phase::Validated,
        Phase::CheckedOut]
    {
        let context = stop();
        let mut area = PathBuf::new();
        let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
            limits(), &context, &mut |current, path| {
                area = path.to_owned();
                if current == phase { context.cancel(); }
                Ok(())
            });
        assert_eq!(result.err(), Some(SourceError::Cancelled));
        assert!(!area.exists());
    }
    /*
     * fetch開始前とcheckout終了後の失敗で公開と回収を確認する
     */
    for (phase, error) in [(Phase::Prepared, SourceError::GitFetch),
        (Phase::CheckedOut, SourceError::GitCheckout)]
    {
        let mut area = PathBuf::new();
        let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
            limits(), &stop(), &mut |current, path| {
                area = path.to_owned();
                if current == phase { return Err(error.clone()); }
                Ok(())
            });
        assert_eq!(result.err(), Some(error));
        assert!(!area.exists());
    }
}

///
/// 存在しないtransport対象の取得失敗を安全に分類し回収する。
///
/// # 注記
/// ダミー秘密名を含む存在しないfile URLを試験専用入口へ渡す。
///
/// # 戻り値
/// なし。fetch失敗の安全分類・秘密非露出・回収が異なる場合はassertで失敗する。
///
#[test]
fn fetch_failure_does_not_expose_external_error_or_path() {
    let root = tempdir().expect("失敗fixture");
    let missing = root.path().join("dummy-secret-missing-repository");
    let mut area = PathBuf::new();
    let result = remote::fixture(&missing, GitSelector::MainThenMaster,
        limits(), &stop(), &mut |_, path| { area = path.to_owned(); Ok(()) });
    let error = result.expect_err("取得失敗");
    assert_eq!(error, SourceError::GitFetch);
    assert!(!format!("{error:?} {error}").contains("dummy-secret"));
    assert!(!area.exists());
}

///
/// fetch診断が外部エラーに含まれる秘密値や本文を表示しない。
///
/// # 注記
/// ダミーURL・資格情報・本文を持つI/O失敗から診断を作成して確認する。
///
/// # 戻り値
/// なし。fetch診断が外部エラーのURL・資格情報・本文を含めばassertで失敗する。
///
#[test]
fn fetch_diagnostic_excludes_external_error_text() {
    let error = gix::Error::from_error(std::io::Error::other(
        "https://dummy-user:dummy-secret@example.invalid dummy-body"));
    let diagnostic = remote::fetch_diagnostic(&error);
    assert!(diagnostic.contains("io=Some(Other)"));
    for value in ["dummy-user", "dummy-secret", "dummy-body",
        "https://", "example.invalid"]
    {
        assert!(!diagnostic.contains(value));
    }
}

///
/// 実際のcheckout衝突が未完成snapshotを公開せず回収する。
///
/// # 概要
/// 検査後に対象fileと衝突するdirectoryを作り、gixの失敗を分類する。
///
/// # 注記
/// 元repoは変更せず、worker専用領域だけへ衝突を注入する。
///
/// # 戻り値
/// なし。checkout衝突の拒否分類や未公開領域回収が異なる場合はassertで失敗する。
///
#[test]
fn checkout_collision_does_not_publish_snapshot() {
    /*
     * checkout直前に通常fileの投影先をdirectoryとして占有する
     */
    let origin = GitFixture::new();
    let mut area = PathBuf::new();
    let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
        limits(), &stop(), &mut |phase, path| {
            area = path.to_owned();
            if phase == Phase::Validated {
                fs::create_dir(path.join("repo/plain.txt"))
                    .expect("checkout衝突fixture");
            }
            Ok(())
        });
    assert_eq!(result.err(), Some(SourceError::GitCheckout));
    assert!(!area.exists());
}

///
/// 容量監視が処理中のgix向け停止flagを更新する。
///
/// # 注記
/// phase完了前に上限超過fileを作り、監視通知を待って失敗を返す。
///
/// # 戻り値
/// なし。容量監視の協調停止通知や上限失敗分類が異なる場合はassertで失敗する。
///
#[test]
fn size_monitor_signals_cooperative_stop_during_operation() {
    let area = tempdir().expect("監視fixture");
    let context = stop();
    let result: Result<(), SourceError> = remote::watch::run(area.path(), 10,
        &context, || {
            fs::write(area.path().join("in-flight"), [0; 11])
                .expect("取得中の超過");
            while context.checkpoint().is_ok() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(SourceError::GitFetch)
        });
    assert_eq!(result, Err(SourceError::Limit("max_git_bytes")));
}

///
/// 外側timeoutなしでも監視が同期作業へ期限停止を通知する。
///
/// # 注記
/// checkpointを呼ばない模擬処理で共有flagを待ち、期限分類を確認する。
///
/// # 戻り値
/// なし。同期作業への期限停止通知や失敗分類が異なる場合はassertで失敗する。
///
#[test]
fn deadline_monitor_notifies_blocking_operation() {
    use std::sync::atomic::Ordering;

    let area = tempdir().expect("期限fixture");
    let context = StopContext::new(Instant::now() + Duration::from_millis(50));
    let result = remote::watch::run(area.path(), 100, &context, || {
        let safety = Instant::now() + Duration::from_secs(2);
        while !context.cancelled.load(Ordering::Relaxed) {
            assert!(Instant::now() < safety, "監視の期限通知");
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    });
    assert_eq!(result, Err(SourceError::Deadline));
}

///
/// 製品入口がfile/HTTP/SSHとURL資格情報を接続前に拒否する。
///
/// # 注記
/// 異なるscheme、userinfo、query、fragmentを解析済みURLとして渡す。
///
/// # 戻り値
/// なし。製品入口での接続前URL拒否が異なる場合はassertで失敗する。
///
#[test]
fn production_boundary_rejects_disallowed_urls() {
    for value in ["file:///tmp/repo", "http://example.invalid/repo",
        "ssh://example.invalid/repo", "https://dummy-secret@host.invalid/repo",
        "https://host.invalid/repo?token=dummy-secret",
        "https://host.invalid/repo#dummy-secret"]
    {
        let result = remote::acquire(&Url::parse(value).expect("URL"),
            GitSelector::MainThenMaster, limits(), &stop());
        assert_eq!(result.err(), Some(SourceError::InvalidGit));
    }
}

///
/// 非Markdown symlink・gitlink・危険名をcheckout前に拒否する。
///
/// # 概要
/// 選択ツリーの禁止entryを列挙し、checkoutへ進まないことを確認する。
///
/// # 注記
/// 別々のfixtureで各種危険entryをcommitし、取得先の未公開と回収を確認する。
///
/// # 戻り値
/// なし。禁止entryの拒否・未checkout・取得領域回収が異なる場合はassertで失敗する。
///
#[cfg(unix)]
#[test]
fn rejects_unsafe_commit_entries_before_checkout() {
    use std::os::unix::fs::symlink;

    /*
     * 禁止entryを持つcommitを一時repoへ作る
     */
    for error in [SourceError::Symlink, SourceError::Submodule,
        SourceError::InvalidPath]
    {
        let origin = GitFixture::new();
        match error {
            SourceError::Symlink => {
                symlink("plain.txt", origin.path().join("link.txt"))
                    .expect("symlink fixture");
                git_command(origin.path(), &["add", "."]);
            }
            SourceError::Submodule => {
                git_command(origin.path(), &["update-index", "--add",
                    "--cacheinfo", "160000", &origin.initial, "module"]);
            }
            _ => {
                fs::write(origin.path().join("bad name.txt"), b"unsafe")
                    .expect("危険名fixture");
                git_command(origin.path(), &["add", "."]);
            }
        }
        /*
         * 検査エラー・未checkout・全取得領域回収を照合する
         */
        git_command(origin.path(), &["commit", "-qm", "unsafe entry"]);
        let mut checked_out = false;
        let mut area = PathBuf::new();
        let result = remote::fixture(origin.path(), GitSelector::MainThenMaster,
            limits(), &stop(), &mut |phase, path| {
                checked_out |= phase == Phase::CheckedOut;
                area = path.to_owned();
                Ok(())
            });
        assert_eq!(result.err(), Some(error));
        assert!(!checked_out);
        assert!(!area.exists());
    }
}

///
/// 非Sendのclone値をworker内へ閉じ、所有済みsnapshotだけを返す。
///
/// # 注記
/// fixtureのfile transportを同じblocking実行基盤で取得する。
///
/// # 戻り値
/// なし。blocking workerから返されたsnapshotの取得本文が異なればassertで失敗する。
///
#[tokio::test]
async fn fixture_snapshot_crosses_blocking_boundary() {
    let origin = GitFixture::new();
    let path = origin.path().to_owned();
    let snapshot = crate::source::runtime::run(stop(), move |context| {
        remote::fixture(&path, GitSelector::MainThenMaster, limits(),
            &context, &mut |_, _| Ok(()))
    }).await.expect("worker取得");
    assert_eq!(body(&snapshot, "pages/page.md"), b"main\n");
}

///
/// 製品HTTPS入口から公開repoのbranchと固定commitを取得する。
///
/// # 概要
/// 明示指定の公開repoを匿名取得し、固定commitとbranchの一致を照合する。
///
/// # 注記
/// LUWIKI_MS31_PUBLIC_GIT_URL/BRANCH/COMMITを実行時に指定する。
/// 通常試験では実行せず、匿名HTTPSと固定内容の証拠だけを扱う。
///
/// # 戻り値
/// なし。指定公開HTTPSのbranch・固定commit取得と本文が異なればassertで失敗する。
///
#[tokio::test]
#[ignore = "public HTTPS network access required"]
async fn public_https_fixed_commit() {
    /*
     * 明示指定された公開fixtureのURLと完全commit IDを検査する
     */
    let value = std::env::var("LUWIKI_MS31_PUBLIC_GIT_URL")
        .expect("public URL environment required");
    let branch = std::env::var("LUWIKI_MS31_PUBLIC_GIT_BRANCH")
        .expect("public branch environment required");
    let id = std::env::var("LUWIKI_MS31_PUBLIC_GIT_COMMIT")
        .expect("public commit environment required");
    assert!(matches!(id.len(), 40 | 64)
        && id.bytes().all(|v| v.is_ascii_hexdigit()), "full commit required");
    let url = Url::parse(&value).expect("public URL syntax");
    let mut bound = limits();
    bound.git_bytes = 64 * 1024 * 1024;

    /*
     * branchと固定IDを別々に取得し、本文を表示せず全entryの一致を確認する
     */
    let context = StopContext::new(Instant::now() + Duration::from_secs(60));
    let branch = crate::source::acquire(SourceSpec::HttpsGit {
        url: url.clone(), selector: GitSelector::Branch(branch),
    }, None, bound, context)
        .await.expect("public branch acquisition");
    eprintln!("public HTTPS branch acquisition: succeeded");
    let context = StopContext::new(Instant::now() + Duration::from_secs(60));
    let snapshot = crate::source::acquire(SourceSpec::HttpsGit {
        url, selector: GitSelector::Commit(id),
    }, None, bound, context)
        .await.expect("public fixed commit acquisition");
    eprintln!("public HTTPS fixed commit acquisition: succeeded");
    assert!(snapshot.entries().len() == branch.entries().len());
    /*
     * directoryと非Markdownも含め、取得した内容を表示せず照合する
     */
    for entry in snapshot.entries() {
        let other = branch.entries().iter()
            .find(|e| e.name_raw() == entry.name_raw()).expect("same entry");
        assert!(other.kind() == entry.kind());
        assert!(other.byte_len() == entry.byte_len());
        if entry.kind() == crate::source::EntryKind::File {
            let name = std::str::from_utf8(entry.name_raw()).expect("raw name");
            assert!(body(&snapshot, name) == body(&branch, name),
                "public branch must match fixed commit bytes");
        }
    }
}
