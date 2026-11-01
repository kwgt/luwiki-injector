/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 成立計画・私有driver・元失敗の受け渡しと比較用HTTP fixture
//!

use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::projection;
use crate::report::types::{ObservedState, PageId};
use crate::source::{EntryKind, SourceSnapshot};
use crate::validation::validate_snapshot;
use crate::wiki_client::adapter::{execution_failure, operation_evidence};
use crate::wiki_client::ReceivedBody;

use super::types::{Completion, KnownPageFacts, LocalFailure, PageExecution};
use super::*;

///
/// 試験全体で共有する期限付き停止境界を作る。
///
/// # 戻り値
/// 30秒の同じ境界
///
pub(super) fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(30))
}

///
/// 製品の全件検証と整列を通した計画を作る。
///
/// # 概要
/// 未検証型の私有フィールドを直接組み立てない。
///
/// # 引数
/// * `entries` - 生相対名と本文
/// * `shared` - 同じ停止境界
///
/// # 戻り値
/// 成立済み計画
///
pub(super) async fn plan(entries: &[(&[u8], &[u8])], shared: &StopContext)
    -> ProjectionPlan
{
    let input: Vec<_> = entries.iter().map(|(name, bytes)|
        (*name, EntryKind::File, *bytes)).collect();
    let snapshot = SourceSnapshot::validation_root_fixture(&input).unwrap();
    let config = config(shared);
    /*
     * 安全検査済み型と順序確定の公開入口を順に使用する
     */
    let validated = validate_snapshot(snapshot, "/fixture".to_owned(),
        &config.settings().limits, shared.clone()).await.unwrap();
    projection::finalize(validated, shared.clone()).await.unwrap()
}

///
/// ダミー認証と共有期限だけを持つ実行設定を作る。
///
/// # 引数
/// * `shared` - 同一絶対期限
///
/// # 戻り値
/// 実サーバーへ送信しない設定
///
fn config(shared: &StopContext) -> ResolvedRunConfig {
    let target = Url::parse("http://127.0.0.1:9").unwrap();
    crate::cmd_args::resolve::wiki_config_fixture(&target, true,
        shared.deadline())
}

///
/// 既知fixture IDを検証済み型へ変換する。
///
/// # 戻り値
/// ダミーの確定ID
///
fn id() -> PageId {
    PageId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

///
/// 単発要求だけを受け、指定応答または応答喪失を返す。
///
/// # 概要
/// 本文/資格情報をログへ出さず、loopback fixtureへ閉じる。
///
/// # 引数
/// * `reply` - 応答bytes。Noneはheader前切断
///
/// # 戻り値
/// 専用URLと一回のserver task
///
async fn server(reply: Option<Vec<u8>>)
    -> (Url, tokio::task::JoinHandle<()>)
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        /*
         * dummy要求header全体を受け取ってから一回だけ応答する
         */
        loop {
            let mut bytes = [0; 1024];
            let count = stream.read(&mut bytes).await.unwrap();
            if count == 0 { break; }
            request.extend_from_slice(&bytes[..count]);
            if request.windows(4).any(|part| part == b"\r\n\r\n") { break; }
        }
        if let Some(reply) = reply {
            stream.write_all(&reply).await.unwrap();
        }
    });
    (Url::parse(&format!("http://{address}")).unwrap(), task)
}

///
/// 製品のHTTP受信/厳格検証で独立本文を作る。
///
/// # 引数
/// * `bytes` - UTF-8 fixture本文
/// * `shared` - 比較まで同じ停止境界
///
/// # 戻り値
/// Fileだけに分解しない受信所有者
///
pub(super) async fn body(bytes: &[u8], shared: &StopContext) -> ReceivedBody {
    let mut reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
        Connection: close\r\n\r\n", bytes.len()).into_bytes();
    reply.extend_from_slice(bytes);
    let (url, server) = server(Some(reply)).await;
    let config = crate::cmd_args::resolve::wiki_config_fixture(&url, true,
        shared.deadline());
    let client = WikiClient::new(&config, shared.clone()).unwrap();
    let result = client.read_source(&id()).await.unwrap();
    server.await.unwrap();
    result
}

///
/// Linux fd参照から試験専用の回収観察位置を得る。
///
/// # 引数
/// * `file` - readonly本文handle
///
/// # 戻り値
/// 本文領域内のパス。製品APIへは公開しない。
///
pub(super) fn location(file: &std::fs::File) -> PathBuf {
    fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
}

///
/// 順序と失敗停止だけを試すfixture専用processor。
///
struct Processor {
    /// 呼ばれた計画位置。
    calls: Vec<usize>,
    /// 局所失敗を返す位置。
    fail: Option<usize>,
    /// 完了返却前に共有取消する位置。
    cancel: Option<usize>,
}

// 実モードを実装しないfixtureとして私有境界をこの試験内へ分離する。
impl PageProcessor for Processor {
    ///
    /// fixtureの確定結果か局所失敗を返す。
    ///
    /// # 引数
    /// * `index` - 呼び出された位置
    /// * `executor` - 順序と共有停止の所有者
    ///
    /// # 戻り値
    /// 順序driver検証だけの結果
    ///
    async fn process(&mut self, index: usize, executor: &Executor) -> PageStep {
        self.calls.push(index);
        if self.cancel == Some(index) { executor.stop.cancel(); }
        if self.fail == Some(index) {
            PageStep::failed(ExecutorFailure::local(LocalKind::CompareIo,
                ProcessingStage::ReadExisting), KnownPageFacts::default(),
                None)
        } else {
            PageStep::completed(Completion::Created, KnownPageFacts::default())
        }
    }
}

///
/// 親不在の子を含む確定順と最初の失敗後未実行を保つ。
///
/// # 概要
/// fixture processorだけでdriverの順序・停止を検証する。
///
/// # 注記
/// 実モード成功とは扱わず、明示した期待pathと呼出位置を照合する。
///
/// # 戻り値
/// なし。順序/先行保持/後続未実行が異なれば失敗する。
///
#[tokio::test]
async fn driver_keeps_order_and_stops_after_first_failure() {
    let shared = stop();
    let plan = plan(&[(b"z.md", b"z"), (b"absent/child.md", b"c"),
        (b"a.md", b"a")], &shared).await;
    let names: Vec<_> = plan.pages().iter().map(|page|
        page.identity().path.as_str()).collect();
    assert_eq!(names, ["/fixture/a", "/fixture/z", "/fixture/absent/child"]);
    let executor = Executor::new(plan, &config(&shared), shared.clone())
        .unwrap();
    let mut processor = Processor { calls: Vec::new(), fail: Some(1),
        cancel: None };
    let batch = executor.drive(&mut processor).await;
    /*
     * 先行成功・元失敗・未開始の三状態を独立に確認する
     */
    assert_eq!(processor.calls, [0, 1]);
    assert!(matches!(batch.pages()[0].execution(),
        PageExecution::Completed(Completion::Created)));
    assert!(matches!(batch.pages()[1].execution(), PageExecution::Failed(_)));
    assert!(matches!(batch.pages()[2].execution(), PageExecution::NotExecuted));
    let (_, first, global, _) = batch.into_parts();
    assert_eq!(first, Some(1));
    assert!(global.is_none());
    assert!(shared.checkpoint().is_ok());
}

///
/// 空計画はprocessorを呼ばずHTTPなしで正常な空受け手を返す。
///
/// # 注記
/// 接続不能設定でもconstructor/driverが要求を開始しないことを確認する。
///
/// # 戻り値
/// なし。架空処理が呼ばれれば失敗する。
///
#[tokio::test]
async fn empty_plan_never_calls_processor_or_sends_http() {
    let shared = stop();
    let executor = Executor::new(plan(&[], &shared).await, &config(&shared),
        shared.clone()).unwrap();
    let mut processor = Processor { calls: Vec::new(), fail: None,
        cancel: None };
    assert!(executor.drive(&mut processor).await.pages().is_empty());
    assert!(processor.calls.is_empty());
    assert!(shared.checkpoint().is_ok());
}

///
/// 完了返却直後の取消でも先行結果を保ち次件を開始しない。
///
/// # 注記
/// 完了と停止を別事実として保持し、未実行へ書き戻さない。
///
/// # 戻り値
/// なし。停止後に次件を呼べば失敗する。
///
#[tokio::test]
async fn driver_preserves_completed_page_when_shared_stop_arrives() {
    let shared = stop();
    let executor = Executor::new(plan(&[(b"a.md", b"a"), (b"b.md", b"b")],
        &shared).await, &config(&shared), shared.clone()).unwrap();
    let mut processor = Processor { calls: Vec::new(), fail: None,
        cancel: Some(0) };
    let batch = executor.drive(&mut processor).await;
    assert_eq!(processor.calls, [0]);
    assert!(matches!(batch.pages()[0].execution(),
        PageExecution::Completed(_)));
    assert!(matches!(batch.pages()[1].execution(), PageExecution::NotExecuted));
    let (_, _, error, _) = batch.into_parts();
    assert!(matches!(error, Some(ExecutorFailure::Local(error))
        if error.kind() == LocalKind::Cancelled));
}

///
/// 不一致期限と開始前取消を送信せず拒否する。
///
/// # 概要
/// plan所有者の回収と安全な開始前分類を確認する。
///
/// # 注記
/// 計画は元の正常contextで成立させ、その後の開始境界を変更する。
///
/// # 戻り値
/// なし。不整合を受け入れれば失敗する。
///
#[tokio::test]
async fn constructor_rejects_mismatch_and_cancel_without_http() {
    let shared = stop();
    let input = plan(&[(b"a.md", b"a")], &shared).await;
    let error = Executor::new(input, &config(&shared), stop()).unwrap_err();
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::ContextMismatch));
    let input = plan(&[(b"a.md", b"a")], &shared).await;
    let path = location(&input.open_body(0).unwrap());
    shared.cancel();
    let error = Executor::new(input, &config(&shared), shared.clone())
        .unwrap_err();
    assert!(matches!(error, ExecutorFailure::Local(error)
        if error.kind() == LocalKind::Cancelled));
    assert!(!path.parent().unwrap().exists());
}

///
/// 二重記録と順序外記録を拒否し既存結果を上書きしない。
///
/// # 概要
/// 不正payloadもmove出口へ保持し、通常結果と分離して照合する。
///
/// # 注記
/// 一度確定した結果を参照し不正な第二記録を明示する。
///
/// # 戻り値
/// なし。先行結果が失われれば失敗する。
///
#[tokio::test]
async fn batch_rejects_duplicate_and_out_of_order_recording() {
    let shared = stop();
    let input = plan(&[(b"a.md", b"a"), (b"b.md", b"b")], &shared).await;
    let mut batch = ExecutionBatch::new(&input);
    /*
     * 順序外と二重記録を拒否し元payloadを別列へ保存する
     */
    assert!(batch.record(1, PageStep::completed(Completion::Updated,
        KnownPageFacts::default())).is_err());
    batch.record(0, PageStep::completed(Completion::Created,
        KnownPageFacts::default())).unwrap();
    assert!(batch.record(0, PageStep::completed(Completion::Recreated,
        KnownPageFacts::default())).is_err());
    assert!(matches!(batch.pages()[0].execution(),
        PageExecution::Completed(Completion::Created)));
    let (_, _, _, rejected) = batch.into_parts();
    assert_eq!(rejected.len(), 2);
    assert!(matches!(&rejected[0].1,
        PageExecution::Completed(Completion::Updated)));
    assert!(matches!(&rejected[1].1,
        PageExecution::Completed(Completion::Recreated)));
}

///
/// 実際の応答喪失を元のstatusなし先行事実として作る。
///
/// # 引数
/// * `shared` - 同一停止境界
///
/// # 戻り値
/// 既存adapterが保持する元事実
///
pub(super) async fn no_status(shared: &StopContext)
    -> crate::wiki_client::adapter::OperationEvidence
{
    let (url, server) = server(None).await;
    let config = crate::cmd_args::resolve::wiki_config_fixture(&url, true,
        shared.deadline());
    let client = WikiClient::new(&config, shared.clone()).unwrap();
    let failure = client.read_source(&id()).await.unwrap_err();
    server.await.unwrap();
    operation_evidence(failure, ObservedState::Unknown).unwrap()
}

///
/// 局所失敗への結合でstatusなし元事実を落とさない。
///
/// # 注記
/// 応答喪失の元operation/IDをmove後に照合し旧警告Noneも確認する。
///
/// # 戻り値
/// なし。元事実が失われれば失敗する。
///
#[tokio::test]
async fn local_failure_preserves_no_status_prior_evidence() {
    let shared = stop();
    let evidence = no_status(&shared).await;
    assert!(evidence.warning().is_none());
    let failure = ExecutorFailure::Local(LocalFailure::new(
        LocalKind::CompareIo, ProcessingStage::Recheck, Vec::new()))
        .with_prior_evidence(vec![evidence]).unwrap();
    assert_eq!(failure.stage(), ProcessingStage::Recheck);
    assert_eq!(failure.evidence().len(), 1);
    assert!(failure.evidence()[0].facts().status().is_none());
    assert_eq!(failure.evidence()[0].facts().known_id(), Some(&id()));
    assert!(shared.checkpoint().is_ok());
}

///
/// 主失敗と解除失敗のcaller段階を別所有で保持する。
///
/// # 概要
/// cleanup採取自体は後続範囲で、ここは受け手の独立所有を検証する。
///
/// # 注記
/// 実際の単発失敗をcaller段階付きadapterへ渡し各元事実を確認する。
///
/// # 戻り値
/// なし。cleanupが主原因を上書きすれば失敗する。
///
#[tokio::test]
async fn cleanup_failure_does_not_replace_primary_failure() {
    let shared = stop();
    let evidence = no_status(&shared).await;
    let (url, server) = server(None).await;
    let config = crate::cmd_args::resolve::wiki_config_fixture(&url, true,
        shared.deadline());
    let client = WikiClient::new(&config, shared.clone()).unwrap();
    let failure = client.read_source(&id()).await.unwrap_err();
    server.await.unwrap();
    let cleanup = execution_failure(failure, ProcessingStage::ReleaseLock,
        vec![evidence]);
    let input = plan(&[(b"private-name.md", b"private-body")], &shared).await;
    let mut batch = ExecutionBatch::new(&input);
    batch.record(0, PageStep::failed(ExecutorFailure::local(
        LocalKind::Conflict, ProcessingStage::Recheck),
        KnownPageFacts { old_id: Some(id()), old_revision: Some(7),
            new_id: None, ..KnownPageFacts::default() }, Some(cleanup)))
        .unwrap();
    /*
     * 表示の秘匿とmove後の主原因・解除事実を独立に照合する
     */
    let rendered = format!("{:?} {:?}", batch, batch.pages());
    let known_id = id();
    for value in ["private-name", "private-body", known_id.as_str(),
        "dummy-wiki-secret", "/fixture"]
    {
        assert!(!rendered.contains(value));
    }
    let (mut pages, first, _, _) = batch.into_parts();
    assert_eq!(first, Some(0));
    let (_, known, last, execution, cleanup, attempted, _details) =
        pages.remove(0).into_parts();
    assert!(attempted);
    assert_eq!(known.into_parts().0, Some(id()));
    assert_eq!(last, ProcessingStage::ReleaseLock);
    assert!(matches!(execution, PageExecution::Failed(error)
        if error.stage() == ProcessingStage::Recheck));
    let cleanup = cleanup.unwrap();
    assert_eq!(cleanup.stage(), ProcessingStage::ReleaseLock);
    assert_eq!(cleanup.warnings().len(), 1);
}
