/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 共通取得入口・四方式のsnapshot契約・Report変換の統合試験
//!

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::time::Instant;

use tempfile::tempdir;
use url::Url;
use ::zip::write::SimpleFileOptions;
use ::zip::ZipWriter;

use crate::cmd_args::resolve::source::{GitSelector, LocalSelector, SourceSpec};
use crate::report::{Failure, Report};
use crate::report::types::{FailureCategory, ProcessingStage, SourceKind};
use crate::secret::Secret;
use crate::source::{EntryKind, SourceError, SourceSnapshot, StopContext};

use super::{GitFixture, limits, stop};

///
/// Git fixtureの旧commitと同じentry・本文を持つZIPを生成する。
///
/// # 概要
/// ZIPのdirectory末尾slashは原名として保持し、比較時は参照名を使用する。
///
/// # 戻り値
/// 小規模ZIPのバイト列
///
fn archive() -> Vec<u8> {
    /*
     * directoryを正規のZIP directoryとして生成する
     */
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    writer.add_directory("root/", options).expect("ZIP root");
    writer.add_directory("root/pages/", options).expect("ZIP directory");
    for (name, bytes) in [(".gitignore", &b"*.skip.md\n"[..]),
        ("root/pages/page.md", &b"old\r\n"[..]),
        ("root/pages/deleted.md", &b"delete\n"[..]),
        ("plain.txt", &b"ordinary"[..])]
    {
        writer.start_file(name, options).expect("ZIP entry");
        writer.write_all(bytes).expect("ZIP body");
    }
    writer.finish().expect("ZIP footer").into_inner()
}

///
/// snapshotの参照名・種別・実サイズ・本文を方式非依存で比較する。
///
/// # 引数
/// * `snapshot` - 方式固有検査済みの取得結果
///
/// # 戻り値
/// 本文を表示せず比較できるentry対応表
///
fn contents(snapshot: &SourceSnapshot)
    -> BTreeMap<String, (EntryKind, u64, Vec<u8>)>
{
    use std::io::Read;

    /*
     * 生のfile名とdirectory末尾slashだけを除いた参照名を対応させる
     */
    snapshot.entries().iter().enumerate().map(|(index, entry)| {
        let mut bytes = Vec::new();
        if entry.kind() == EntryKind::File {
            assert!(snapshot.open_body(index).expect("owned body")
                .read_to_end(&mut bytes).is_ok());
        } else {
            assert!(snapshot.open_body(index).is_err());
        }
        (entry.reference().parts().1.to_owned(),
            (entry.kind(), entry.byte_len(), bytes))
    }).collect()
}

///
/// 四方式の製品pipelineから同じ内容のSnapshotを作る。
///
/// # 概要
/// ローカル二方式は共通dispatch、ネットワーク二方式は同じ製品pipelineの
/// file fixtureで比較する。HTTPS/GitHubの実dispatchは明示実接続試験で確認。
///
/// # 戻り値
/// 元入力を既に破棄した四方式の所有されたSnapshot
///
pub(crate) async fn validation_adapter_fixtures()
    -> Result<[SourceSnapshot; 4], SourceError>
{
    /*
     * ZIPとローカルGitを共通取得入口から取得する
     */
    let origin = GitFixture::rooted();
    let input = tempdir().map_err(|_| SourceError::Io)?;
    let path = input.path().join("fixture.zip");
    fs::write(&path, archive()).map_err(|_| SourceError::Io)?;
    let zip = crate::source::acquire(SourceSpec::Zip(path), None,
        limits(), stop()).await?;
    let git = crate::source::acquire(SourceSpec::LocalGit {
        path: origin.path().to_owned(),
        selector: LocalSelector::Committed(GitSelector::Commit(
            origin.initial.clone())),
    }, None, limits(), stop()).await?;
    /*
     * file transportは試験だけで、製品の許容schemeを変更しない
     */
    let path = origin.path().to_owned();
    let commit = origin.initial.clone();
    let (https, github) = crate::source::runtime::run(stop(), move |context| {
        let https = crate::source::remote::fixture(&path,
            GitSelector::Commit(commit.clone()), limits(), &context,
            &mut |_, _| Ok(()))?;
        let github = crate::source::remote::github_fixture(&path,
            &commit, limits(), &context, &mut |_, _| Ok(()))?;
        Ok((https, github))
    }).await?;
    drop(origin);
    drop(input);
    Ok([zip, git, https, github])
}

///
/// 四方式の同じ内容が所有された共通entryとして返る。
///
/// # 概要
/// 共通検証側と同じfixture生成を使い、所有権とentry契約を比較する。
///
/// # 注記
/// 元入力を削除したSnapshotの本文保持と全leaseの最終回収を確認する。
///
/// # 戻り値
/// なし。四方式のentry内容・参照種別・所有領域回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn four_adapters_share_entry_body_and_owned_lifetime_contract() {
    /*
     * 種別と内容を比較し、snapshot破棄で親領域まで回収する
     */
    let snapshots = validation_adapter_fixtures().await
        .expect("adapter fixtures");
    let expected = contents(&snapshots[0]);
    let mut areas = Vec::new();
    for (snapshot, kind) in snapshots.iter().zip([
        SourceKind::Zip, SourceKind::LocalGit, SourceKind::HttpsGit,
        SourceKind::GitHub])
    {
        assert!(contents(snapshot) == expected);
        assert!(snapshot.entries().iter().all(|e|
            e.reference().parts().0 == kind));
        areas.push(snapshot.lease.0.path().to_owned());
        if let Some(area) = &snapshot.acquisition {
            areas.push(area.0.path().to_owned());
        }
    }
    drop(snapshots);
    assert!(areas.iter().all(|path| !path.exists()));
}

///
/// 四方式の取消・期限到達を取得開始前に拒否する。
///
/// # 概要
/// 共通dispatchの停止確認が方式ごとのI/Oより先に成立するか検査する。
///
/// # 注記
/// 存在しないローカル入力と接続不能hostでも通信を開始せず停止を優先する。
///
/// # 戻り値
/// なし。四方式の取得前取消・期限拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn dispatch_stops_all_sources_before_io_or_network() {
    /*
     * 全入力方式に同じ停止境界を適用する
     */
    let specs = [SourceSpec::Zip(PathBuf::from("missing.zip")),
        SourceSpec::LocalGit { path: PathBuf::from("missing-repo"),
            selector: LocalSelector::Worktree },
        SourceSpec::HttpsGit { url: Url::parse("https://host.invalid/repo")
            .expect("URL"), selector: GitSelector::MainThenMaster },
        SourceSpec::GitHub { owner: "missing".into(), repo: "repo".into(),
            selector: GitSelector::MainThenMaster }];
    let token = Secret::new("dummy-github-secret".into());
    for spec in specs {
        let context = stop();
        context.cancel();
        assert!(matches!(crate::source::acquire(spec.clone(), Some(&token),
            limits(), context).await, Err(SourceError::Cancelled)));
        assert!(matches!(crate::source::acquire(spec, Some(&token), limits(),
            StopContext::new(Instant::now())).await,
            Err(SourceError::Deadline)));
    }
}

///
/// 共通入口の構造不正・上限・非HTTPS入力がsnapshotを公開しない。
///
/// # 概要
/// 方式固有の拒否と上限を共通入口から確認し、失敗だけを返す。
///
/// # 注記
/// ローカル入力不正とZIP上限・HTTPS拒否を実入口へ渡す。
///
/// # 戻り値
/// なし。構造不正・上限・接続条件違反の拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn dispatch_rejects_invalid_inputs_and_limits_without_snapshot() {
    /*
     * ZIP上限と壊れた入力を別々に拒否する
     */
    let input = tempdir().expect("input area");
    let path = input.path().join("fixture.zip");
    fs::write(&path, archive()).expect("archive");
    let mut bound = limits();
    bound.zip_bytes = 1;
    assert!(matches!(crate::source::acquire(SourceSpec::Zip(path.clone()),
        None, bound, stop()).await, Err(SourceError::Limit(_))));
    fs::write(&path, b"broken archive").expect("invalid archive");
    assert!(crate::source::acquire(SourceSpec::Zip(path), None, limits(),
        stop()).await.is_err());
    /*
     * GitHub tokenは通常HTTPSの拒否経路へ渡さない
     */
    let token = Secret::new("dummy-github-secret".into());
    let spec = SourceSpec::HttpsGit {
        url: Url::parse("http://host.invalid/repo").expect("URL"),
        selector: GitSelector::MainThenMaster,
    };
    assert!(matches!(crate::source::acquire(spec, Some(&token), limits(),
        stop())
        .await, Err(SourceError::InvalidGit)));
    assert!(crate::source::acquire(SourceSpec::LocalGit {
        path: input.path().to_owned(), selector: LocalSelector::Worktree,
    }, None, limits(), stop()).await.is_err());
}

///
/// 取得失敗分類がReportの入力2・実行1へ対応する。
///
/// # 概要
/// 全variantを検査し、表示に上限の生名や秘密値が残らないことを確認する。
///
/// # 注記
/// 入力不正と実行失敗を別に列挙し、ページ計画を生成しないまま報告する。
///
/// # 戻り値
/// なし。取得失敗の安全分類・終了コード・秘密非露出が異なればassertで失敗する。
///
#[test]
fn all_source_errors_map_to_safe_report_and_exit_contract() {
    /*
     * 入力構造・参照・上限の不正は2で終了する
     */
    for error in [SourceError::InvalidPath, SourceError::InvalidUtf8,
        SourceError::Symlink, SourceError::Submodule, SourceError::InvalidGit,
        SourceError::RefNotFound, SourceError::ZipStructure,
        SourceError::ZipUnsupported, SourceError::Duplicate,
        SourceError::Limit("dummy-body-secret"), SourceError::GitHubMissing,
        SourceError::GitHubIdentity]
    {
        let failure = error.into_failure();
        assert!(matches!(failure, Failure::Input));
        let mut report = Report::preflight();
        report.fail(failure);
        assert!(report.exit_code() == 2);
        assert!(report.summary().not_executed == 0);
        let rendered = report.render();
        assert!(!rendered.diagnostics.contains("dummy-body-secret"));
    }
    /*
     * 停止・取得中の失敗は1と安全な分類を維持する
     */
    for (error, category) in [(SourceError::Io, FailureCategory::Transport),
        (SourceError::GitFetch, FailureCategory::Transport),
        (SourceError::GitHubApi, FailureCategory::Transport),
        (SourceError::GitCheckout, FailureCategory::Rejected),
        (SourceError::GitHubAuth, FailureCategory::Authentication),
        (SourceError::GitHubDenied, FailureCategory::Rejected),
        (SourceError::Changed, FailureCategory::Conflict),
        (SourceError::Cancelled, FailureCategory::NotApplied),
        (SourceError::Deadline, FailureCategory::Deadline),
        (SourceError::Worker, FailureCategory::NotApplied),
        (SourceError::NotAvailable, FailureCategory::ExecutionUnavailable)]
    {
        let failure = error.into_failure();
        assert!(matches!(failure, Failure::Execution {
            category: actual, stage: ProcessingStage::Preflight,
        } if actual == category));
        let mut report = Report::preflight();
        report.fail(failure);
        assert!(report.exit_code() == 1);
    }
}
