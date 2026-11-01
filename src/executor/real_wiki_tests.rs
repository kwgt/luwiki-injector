/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 明示ignored実行だけで許可された実Wiki領域を検証する入口
//!

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd_args::ResolvedRunConfig;
use crate::cmd_args::resolve::real_wiki_config;
use crate::projection::{self, ProjectionPlan};
use crate::report::types::{PageId, PagePath, ProcessingStage};
use crate::source::{CancelOnDrop, EntryKind, SourceSnapshot, StopContext};
use crate::validation::validate_snapshot;
use crate::wiki_client::{LookupResult, WikiClient};
use crate::wiki_client::types::{PathKind, PathRelation, WikiFailure};

use super::compare::{BodyRelation, compare_planned};
use super::normal::tests::pause;
use super::types::{Completion, ExecutionBatch, ExecutorFailure, PageExecution};
use super::{Executor, LocalKind};

const POSITIVE: &str = "/Test/luwiki-injector-ms42";
const CONTROL: &str = "/LuWikiInjectorMS42Control";
static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const ORIGINAL: &[u8] = "\u{feff}初回\r\n末尾".as_bytes();
const CHANGED: &[u8] = "\u{feff}変更\r\n末尾\n".as_bytes();

///
/// 本文・外部errorを表示せず固定診断で結果を取得する。
///
/// # 引数
/// * `result` - 任意境界の戻り値
///
/// # 戻り値
/// 成功値。失敗なら値を表示せず停止する。
///
fn checked<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("MS42 test boundary failed; no automatic cleanup"),
    }
}

///
/// 許可済み領域のsegment境界を検査する。
///
/// # 引数
/// * `path` - 検証対象
/// * `root` - 許可された固定領域
///
/// # 戻り値
/// 配下ならtrue。領域自体への操作は許さない。
///
fn within(path: &str, root: &str) -> bool {
    path.strip_prefix(root).is_some_and(|rest| {
        rest.starts_with('/') && rest.len() > 1
            && PagePath::new(path).is_ok()
    })
}

///
/// 承認済みlocalhost接続先のHTTP/HTTPS表記だけを許可する。
///
/// # 引数
/// * `target` - 専用環境から得た接続先
///
/// # 戻り値
/// 固定host/portと空pathまたはルートpathならtrue。
/// HTTPSも製品の証明書検証をそのまま使用する。
///
fn approved_target(target: &str) -> bool {
    matches!(target, "http://localhost:8040" | "http://localhost:8040/"
        | "https://localhost:8040" | "https://localhost:8040/")
}

///
/// 明示環境と固定対象をHTTP前に検査する。
///
/// # 戻り値
/// なし。不一致・不足なら値を表示せず停止する。
///
fn environment_guard() {
    let prefix = checked(std::env::var("LUWIKI_MS42_TEST_PREFIX"));
    let target = checked(std::env::var("LUWIKI_MS42_TEST_URL"));
    assert!(prefix == POSITIVE, "MS42 approved prefix mismatch");
    assert!(approved_target(&target), "MS42 target mismatch");
}

///
/// 時刻・process ID・連番による新しいcase領域を用意する。
///
/// # 引数
/// * `root` - 通常領域または承認済みcontrol領域
/// * `case` - コード内の固定case名
///
/// # 戻り値
/// この実行だけの領域。衝突は事前照会で拒否する。
///
fn region(root: &str, case: &str) -> String {
    environment_guard();
    let time = checked(SystemTime::now().duration_since(UNIX_EPOCH));
    let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tag = format!("{}-{}-{sequence}", std::process::id(),
        time.as_nanos());
    let prefix = format!("{root}/{case}-{tag}");
    assert!(within(&prefix, root), "MS42 region guard failed");
    eprintln!("MS42 case={case} path={prefix}");
    prefix
}

///
/// 製品設定の同じ期限から停止contextを一度だけ作る。
///
/// # 引数
/// * `prefix` - 検査済みcase領域
/// * `mode` - 固定製品mode
/// * `bearer` - 選択する認証方式
///
/// # 戻り値
/// 設定と後続の検証・計画・HTTPへ共有するcontext
///
fn config(prefix: &str, mode: &str, bearer: bool)
    -> (ResolvedRunConfig, StopContext)
{
    environment_guard();
    assert!(within(prefix, POSITIVE) || (!bearer && within(prefix, CONTROL)),
        "MS42 configuration region guard failed");
    let config = checked(real_wiki_config(prefix, mode, bearer));
    let shared = StopContext::new(config.deadline());
    (config, shared)
}

///
/// 合成snapshotを製品の全件検証・整列へ通す。
///
/// # 概要
/// 全plan pathを設定領域で再検査し任意pathの投入を阻止する。
/// 実source取得や実CLI成功の証拠にはしない。
///
/// # 引数
/// * `config` - 同じ実設定
/// * `shared` - 同じ期限と取消
/// * `entries` - 固定の生相対名と合成本文
///
/// # 戻り値
/// 成立済み・許可範囲内の計画
///
async fn plan(config: &ResolvedRunConfig, shared: &StopContext,
    entries: &[(&[u8], &[u8])]) -> ProjectionPlan
{
    /*
     * 未検証fixtureを製品の全件検証へ渡す
     */
    let input: Vec<_> = entries.iter().map(|(name, body)|
        (*name, EntryKind::File, *body)).collect();
    let snapshot = checked(SourceSnapshot::validation_root_fixture(&input));
    let prefix = &config.settings().prefix;
    let validated = checked(validate_snapshot(snapshot, prefix.clone(),
        &config.settings().limits, shared.clone()).await);
    /*
     * 整列後にも全ページを許可case配下へ限定する
     */
    let plan = checked(projection::finalize(validated, shared.clone()).await);
    assert!(plan.pages().iter().all(|page|
        within(page.identity().path.as_str(), prefix)),
        "MS42 plan region guard failed");
    plan
}

///
/// 実計画・実clientを持つ独立invocationを準備する。
///
/// # 引数
/// * `prefix` - 検査済み領域
/// * `mode` - 製品mode
/// * `bearer` - 認証選択
/// * `entries` - 検証する合成入力
///
/// # 戻り値
/// 独立設定のExecutor。main期限を再起算しない。
///
async fn executor(prefix: &str, mode: &str, bearer: bool,
    entries: &[(&[u8], &[u8])]) -> Executor
{
    let (config, shared) = config(prefix, mode, bearer);
    let plan = plan(&config, &shared, entries).await;
    checked(Executor::new(plan, &config, shared))
}

///
/// 単発失敗の有限分類と既知IDだけを記録する。
///
/// # 引数
/// * `failure` - 製品が秘匿済みの失敗
///
/// # 戻り値
/// なし。raw error/header/bodyを表示しない。
///
fn wiki_failure(failure: &WikiFailure) {
    eprintln!("MS42 operation={:?} status={:?} kind={:?} id={}",
        failure.operation(), failure.status(), failure.kind(),
        failure.known_id().map_or("unknown", PageId::as_str));
}

///
/// 復旧用識別事実と元結果を保持する試験記録。
///
struct Record {
    /// 主結果。秘密・本文を含まない。
    execution: PageExecution,
    /// 確定した有効ID。旧・新の選択は正常結果でのみ行う。
    id: Option<PageId>,
    /// 旧DELETE正常返却の確認。falseは未確認で未削除を意味しない。
    old_delete_confirmed: bool,
}

impl Record {
    ///
    /// 正常結果の確定IDを取得する。
    ///
    /// # 戻り値
    /// 既知ID。未確認なら固定失敗で停止する。
    ///
    fn id(&self) -> &PageId {
        self.id.as_ref().expect("MS42 identity unavailable")
    }
}

///
/// 全結果から有限事実を記録し未確認値を補完しない。
///
/// # 概要
/// 主失敗と解除失敗を独立表示し先行・未実行を維持する。
///
/// # 引数
/// * `batch` - 実runの所有結果
///
/// # 戻り値
/// 同じ順序の試験記録
///
fn records(batch: ExecutionBatch) -> Vec<Record> {
    let (pages, _, global, rejected) = batch.into_parts();
    /*
     * 確定識別情報を記録してから結果を検査する
     */
    let mut cleanup_failed = false;
    let records = pages.into_iter().map(|page| {
        let (identity, known, stage, execution, cleanup, attempted, details) =
            page.into_parts();
        let (old, revision, new, deleted, locked) = known.into_parts();
        eprintln!("MS42 path={} stage={stage:?} old={} revision={revision:?}",
            identity.path.as_str(), old.as_ref().map_or("unknown",
                PageId::as_str));
        eprintln!("MS42 new={} old_delete_confirmed={deleted} \
            locked={locked:?} cleanup_attempted={attempted}",
            new.as_ref().map_or("unknown", PageId::as_str));
        /*
         * 主失敗と解除失敗は別の有限診断で保持する
         */
        if details.last_stage.is_some() {
            eprintln!("MS42 reconciliation={details:?}");
            for warning in &details.warnings { wiki_failure(warning.facts()); }
            for failure in details.auxiliary.iter().chain(&details.retained) {
                eprintln!("MS42 auxiliary={failure:?}");
            }
        }
        match &execution {
            PageExecution::AppliedWithWarning(_) =>
                panic!("MS42 unexpected applied warning; preserve facts"),
            PageExecution::Completed(value) =>
                eprintln!("MS42 completion={value:?}"),
            PageExecution::Failed(_) => eprintln!("MS42 execution=Failed"),
            PageExecution::NotExecuted =>
                eprintln!("MS42 execution=NotExecuted"),
        }
        if let PageExecution::Failed(failure) = &execution {
            eprintln!("MS42 failure_stage={:?}", failure.stage());
            match failure {
                ExecutorFailure::Wiki(failure) => wiki_failure(failure.facts()),
                ExecutorFailure::Local(failure) =>
                    eprintln!("MS42 local_kind={:?}", failure.kind()),
            }
        }
        if let Some(failure) = cleanup {
            eprintln!("MS42 cleanup_stage={:?}", failure.stage());
            wiki_failure(failure.facts());
            cleanup_failed = true;
        }
        Record { execution, id: new.or(old),
            old_delete_confirmed: deleted }
    }).collect();
    /*
     * 全体停止を成功へ読み替えず追加要求なしで停止する
     */
    if let Some(failure) = global {
        eprintln!("MS42 global_stage={:?}", failure.stage());
        panic!("MS42 invocation stopped; preserve recorded recovery facts");
    }
    assert!(!cleanup_failed,
        "MS42 cleanup failed; preserve recorded recovery facts");
    assert!(rejected.is_empty(), "MS42 rejected result invariant");
    records
}

///
/// 一件の正常完了種別を本文の表示なしに検査する。
///
/// # 引数
/// * `records` - 実結果列
/// * `expected` - 必要な確定種別
///
/// # 戻り値
/// なし。不一致なら固定診断で停止する。
///
fn completed(records: &[Record], expected: Completion) {
    assert!(records.len() == 1 && matches!(records[0].execution,
        PageExecution::Completed(value) if value == expected),
        "MS42 completion mismatch");
}

///
/// case領域内の合成page pathを型付けする。
///
/// # 引数
/// * `prefix` - 許可case領域
/// * `name` - 固定page名
///
/// # 戻り値
/// 検査済みpath。actorにも同じguardを適用する。
///
fn path(prefix: &str, name: &str) -> PagePath {
    let path = format!("{prefix}/{name}");
    assert!(within(&path, prefix), "MS42 actor path guard failed");
    checked(PagePath::new(&path))
}

///
/// 実単発結果を有限診断付きで取得する。
///
/// # 引数
/// * `result` - WikiClientの結果
///
/// # 戻り値
/// 正常値。不確定時は追加照会・cleanupを行わない。
///
fn wiki<T>(result: Result<T, WikiFailure>) -> T {
    match result {
        Ok(value) => value,
        Err(failure) => {
            wiki_failure(&failure);
            panic!("MS42 Wiki operation failed; preserve recovery facts");
        }
    }
}

///
/// 新しいrun pageの不存在を変更前に確認する。
///
/// # 概要
/// 各投影先を照会し、既存ページや不確定応答を検出したら停止する。
///
/// # 引数
/// * `executor` - 検証済み計画と同じclient
///
/// # 戻り値
/// なし。既存や不確定応答なら変更しない。
///
async fn fresh(executor: &Executor) {
    for page in executor.plan.pages() {
        /*
         * 投影先の不存在を確認し、既存や照会失敗では変更を始めない
         */
        match executor.client.lookup(&page.identity().path).await {
            Ok(LookupResult::NotFound | LookupResult::Deleted) => {}
            Err(failure) => {
                wiki_failure(&failure);
                panic!("MS42 initial existence is uncertain");
            }
            Ok(LookupResult::Found(id)) => {
                eprintln!("MS42 existing path={} id={}",
                    page.identity().path.as_str(), id.as_str());
                panic!("MS42 run page already exists; refusing overwrite");
            }
        }
    }
}

///
/// 実sourceを所有比較workerで入力bytesと照合する。
///
/// # 概要
/// 有効ID・metadataを照合後、同じcontextでbytes比較を完了する。
///
/// # 引数
/// * `prefix` - 許可領域
/// * `bearer` - 認証方式
/// * `id` - 確定したpage ID
/// * `expected` - 合成入力の期待bytes
///
/// # 戻り値
/// なし。既存本文はassert/ログへ渡さない。
///
async fn bytes(prefix: &str, bearer: bool, id: &PageId, expected: &[u8]) {
    let executor = executor(prefix, "update", bearer,
        &[(b"page.md", expected)]).await;
    /*
     * 期待pathの有効IDとmetadataを確認する
     */
    let page = path(prefix, "page");
    assert!(matches!(wiki(executor.client.lookup(&page).await),
        LookupResult::Found(found) if found == *id),
        "MS42 bytes verification identity mismatch");
    let meta = wiki(executor.client.read_meta(id, &page).await);
    assert!(!meta.deleted() && meta.path_kind() == PathKind::Current
        && meta.path_relation() == PathRelation::MatchesExpected,
        "MS42 bytes verification metadata mismatch");
    /*
     * 同じ期限内でreadonly所有比較を行う
     */
    let body = wiki(executor.client.read_source(id).await);
    let (relation, _) = checked(compare_planned(executor.plan.clone(), 0,
        body, executor.stop.clone()).await);
    assert!(relation == BodyRelation::Identical, "MS42 source bytes differ");
}

///
/// Basic/Bearerの全mode作成・再実行・更新・再作成を確認する。
///
/// # 概要
/// 新旧ID、soft-deleteとBOM/CRLFを保持したbytesを実照会する。
///
/// # 注記
/// 明示ignored実行のみ。各modeを新領域へ置き既存を無断変更しない。
///
/// # 戻り値
/// なし。実CLI/source取得や未提供scopeの成功は表明しない。
///
#[tokio::test]
#[ignore = "requires explicit approved real Wiki environment"]
async fn ms42_real_normal_modes() {
    /*
     * 両認証の不存在・同一再実行・error拒否を実runで確認する
     */
    for bearer in [false, true] {
        for mode in ["error", "update", "overwrite"] {
            eprintln!("MS42 normal bearer={bearer} mode={mode}");
            let prefix = region(POSITIVE, "normal");
            let first = executor(&prefix, mode, bearer,
                &[(b"page.md", ORIGINAL)]).await;
            fresh(&first).await;
            let result = records(first.run().await);
            completed(&result, Completion::Created);
            let old = result[0].id().clone();
            bytes(&prefix, bearer, &old, ORIGINAL).await;
            let again = executor(&prefix, mode, bearer,
                &[(b"page.md", ORIGINAL)]).await;
            let result = records(again.run().await);
            if mode == "error" {
                assert!(matches!(result[0].execution,
                    PageExecution::Failed(ExecutorFailure::Local(ref failure))
                    if failure.kind() == LocalKind::Conflict),
                    "MS42 existing error mismatch");
                continue;
            }
            completed(&result, Completion::SkippedIdentical);
            /*
             * 相違更新・再作成後に旧soft-deleteと新bytesを確認する
             */
            let change = executor(&prefix, mode, bearer,
                &[(b"page.md", CHANGED)]).await;
            let result = records(change.run().await);
            completed(&result, if mode == "update" { Completion::Updated }
                else { Completion::Recreated });
            let new = result[0].id();
            bytes(&prefix, bearer, new, CHANGED).await;
            let observer = executor(&prefix, "update", bearer,
                &[(b"page.md", CHANGED)]).await;
            let page = path(&prefix, "page");
            assert!(matches!(wiki(observer.client.lookup(&page).await),
                LookupResult::Found(found) if found == *new),
                "MS42 active identity mismatch");
            if mode == "overwrite" {
                assert!(old != *new && result[0].old_delete_confirmed,
                    "MS42 recreation identity mismatch");
                let meta = wiki(observer.client.read_meta(&old, &page).await);
                assert!(meta.deleted()
                    && meta.path_kind() == PathKind::LastDeleted
                    && meta.path_relation() == PathRelation::MatchesExpected,
                    "MS42 old soft-delete mismatch");
            } else {
                assert!(old == *new, "MS42 update changed identity");
            }
            completed(&records(observer.run().await),
                Completion::SkippedIdentical);
        }
    }
}

///
/// 前ロック競合と保持中排他を別の実caseで検査する。
///
/// # 概要
/// 独立actorの入力同一化でもmainがConflictで停止する。
///
/// # 注記
/// 既存Pauseで同期しmainのcontext期限を再起算しない。
/// actorは通常実Executorで変更しtokenを取得・表示しない。
///
/// # 戻り値
/// なし。応答喪失時は自動cleanup/retryしない。
///
#[tokio::test]
#[ignore = "requires explicit approved real Wiki environment"]
async fn ms42_real_conflict_and_lock_exclusion() {
    /*
     * Acquire前raceをupdateとoverwriteの双方で確認する
     */
    for mode in ["update", "overwrite"] {
        let prefix = region(POSITIVE, "race");
        let seed = executor(&prefix, "update", false,
            &[(b"page.md", ORIGINAL)]).await;
        fresh(&seed).await;
        let seed = records(seed.run().await);
        completed(&seed, Completion::Created);
        let id = seed[0].id().clone();
        let main = executor(&prefix, mode, true,
            &[(b"page.md", CHANGED)]).await;
        let actor = executor(&prefix, "update", false,
            &[(b"page.md", CHANGED)]).await;
        let shared = main.stop.clone();
        let (ready, resume) = pause(ProcessingStage::AcquireLock, 0);
        let actor = async move {
            // actor停止・panic時はmainを取消し再開通知を成功としない。
            let mut guard = CancelOnDrop::new(shared.clone());
            tokio::select! {
                result = ready => checked(result),
                _ = shared.wait_stopped() => panic!("MS42 pause stopped"),
            }
            completed(&records(actor.run().await), Completion::Updated);
            checked(resume.send(()));
            guard.disarm();
        };
        let (batch, ()) = tokio::join!(main.run(), actor);
        let result = records(batch);
        assert!(matches!(result[0].execution,
            PageExecution::Failed(ExecutorFailure::Local(ref failure))
            if failure.kind() == LocalKind::Conflict
                && failure.stage() == ProcessingStage::Recheck),
            "MS42 race did not stop at conflict");
        bytes(&prefix, false, &id, CHANGED).await;
    }
    /*
     * mainが保持中のlockは別actorの取得を拒否する
     */
    let prefix = region(POSITIVE, "exclusion");
    let seed = executor(&prefix, "update", false,
        &[(b"page.md", ORIGINAL)]).await;
    fresh(&seed).await;
    let seed = records(seed.run().await);
    completed(&seed, Completion::Created);
    let id = seed[0].id().clone();
    let main = executor(&prefix, "update", true,
        &[(b"page.md", CHANGED)]).await;
    let actor = executor(&prefix, "update", false,
        &[(b"page.md", CHANGED)]).await;
    let shared = main.stop.clone();
    let (ready, resume) = pause(ProcessingStage::Recheck, 0);
    let actor = async move {
        // 予期しない取得・失敗時もmainを取消し追加変更を止める。
        let mut guard = CancelOnDrop::new(shared.clone());
        tokio::select! {
            result = ready => checked(result),
            _ = shared.wait_stopped() => panic!("MS42 pause stopped"),
        }
        match actor.client.acquire_lock(&id).await {
            Err(failure) => {
                wiki_failure(&failure);
                assert!(failure.status() == Some(409),
                    "MS42 lock exclusion rejection mismatch");
            }
            Ok(_) => panic!("MS42 independent lock acquired unexpectedly"),
        }
        checked(resume.send(()));
        guard.disarm();
    };
    let (batch, ()) = tokio::join!(main.run(), actor);
    completed(&records(batch), Completion::Updated);
}

///
/// 実error runの先行成功・失敗・後続未実行を確認する。
///
/// # 概要
/// 三ページ計画の中央だけを事前作成し先行・後続の境界を確認する。
///
/// # 注記
/// seed以外の二ページも事前不存在を確認し全計画を製品検証する。
///
/// # 戻り値
/// なし。自動巻戻し・後続書込は行わない。
///
#[tokio::test]
#[ignore = "requires explicit approved real Wiki environment"]
async fn ms42_real_partial_success() {
    /*
     * 衝突する中央ページだけを独立invocationで作る
     */
    let prefix = region(POSITIVE, "partial");
    let seed = executor(&prefix, "error", false,
        &[(b"b.md", ORIGINAL)]).await;
    fresh(&seed).await;
    completed(&records(seed.run().await), Completion::Created);
    let new = executor(&prefix, "error", true,
        &[(b"a.md", CHANGED), (b"c.md", CHANGED)]).await;
    fresh(&new).await;
    let main = executor(&prefix, "error", true, &[(b"a.md", CHANGED),
        (b"b.md", CHANGED), (b"c.md", CHANGED)]).await;
    let result = records(main.run().await);
    assert!(result.len() == 3 && matches!(result[0].execution,
        PageExecution::Completed(Completion::Created))
        && matches!(result[1].execution,
            PageExecution::Failed(ExecutorFailure::Local(ref failure))
            if failure.kind() == LocalKind::Conflict)
        && matches!(result[2].execution, PageExecution::NotExecuted),
        "MS42 partial success mismatch");
    /*
     * 先行bytes維持と後続不存在を実clientで確認する
     */
    let body = wiki(new.client.read_source(result[0].id()).await);
    let (relation, _) = checked(compare_planned(new.plan.clone(), 0, body,
        new.stop.clone()).await);
    assert!(relation == BodyRelation::Identical,
        "MS42 prior success bytes mismatch");
    match new.client.lookup(&path(&prefix, "c")).await {
        Ok(LookupResult::NotFound | LookupResult::Deleted) => {}
        Err(failure) => {
            wiki_failure(&failure);
            panic!("MS42 later page existence uncertain");
        }
        Ok(LookupResult::Found(id)) => {
            eprintln!("MS42 later page id={}", id.as_str());
            panic!("MS42 later page existence mismatch");
        }
    }
}

///
/// /Test制約Bearerの既知prefix外metadata拒否を確認する。
///
/// # 概要
/// Basicでcontrol一ページを作り存在を確認する。Bearer変更はゼロ。
///
/// # 注記
/// metadata成功200は認可不合格として試験を失敗させる。
/// local基線のprefix未適用を成功へ読み替えない。
///
/// # 戻り値
/// なし。controlを自動削除しない。
///
#[tokio::test]
#[ignore = "requires explicit approved real Wiki environment"]
async fn ms42_real_prefix_boundary() {
    /*
     * 承認済み追加領域にBasicで合成page一件だけを置く
     */
    let prefix = region(CONTROL, "boundary");
    let basic = executor(&prefix, "error", false,
        &[(b"page.md", ORIGINAL)]).await;
    fresh(&basic).await;
    let basic = records(basic.run().await);
    completed(&basic, Completion::Created);
    let id = basic[0].id();
    let observer = executor(&prefix, "update", false,
        &[(b"page.md", ORIGINAL)]).await;
    let page = path(&prefix, "page");
    assert!(matches!(wiki(observer.client.lookup(&page).await),
        LookupResult::Found(found) if found == *id),
        "MS42 control identity mismatch");
    let meta = wiki(observer.client.read_meta(id, &page).await);
    assert!(!meta.deleted() && meta.path_kind() == PathKind::Current,
        "MS42 control existence mismatch");
    /*
     * 正常領域のBearer設定で既知control IDのGETだけを一度行う
     */
    let positive = region(POSITIVE, "boundary-reader");
    let (config, shared) = config(&positive, "update", true);
    let client = checked(WikiClient::new(&config, shared));
    match client.read_meta(id, &page).await {
        Err(failure) => {
            wiki_failure(&failure);
            assert!(failure.status() == Some(403),
                "MS42 prefix boundary expected HTTP 403");
        }
        Ok(_) => panic!("MS42 prefix authorization FAILED: outside metadata \
            returned HTTP 200; CP4.2 remains incomplete"),
    }
}

///
/// 領域・接続先検査が未許可pathとURLを拒否することを確認する。
///
/// # 概要
/// 正常HTTP/HTTPS表記とsegment境界、接続先の制約を確認する。
///
/// # 注記
/// env/HTTPを参照せず通常cargo testで安全guardだけを検証する。
///
/// # 戻り値
/// なし。prefix誤一致や接続先の範囲拡大を許せば失敗する。
///
#[test]
fn ms42_region_guard_rejects_neighbors_and_unsafe_paths() {
    /*
     * 許可された領域のsegment境界だけを受け入れる
     */
    assert!(within(&format!("{POSITIVE}/run/page"), POSITIVE));
    for suffix in ["", "x/page", "/", "/../page", "/run//page"] {
        assert!(!within(&format!("{POSITIVE}{suffix}"), POSITIVE),
            "MS42 unsafe region accepted");
    }
    assert!(!within("/LuWikiInjectorMS42Control/run/page", POSITIVE));
    /*
     * 両schemeを同じhost/port/rootに限定して受け入れる
     */
    for target in ["http://localhost:8040", "http://localhost:8040/",
        "https://localhost:8040", "https://localhost:8040/"]
    {
        assert!(approved_target(target), "MS42 approved URL rejected");
    }
    for target in ["https://example.com:8040", "https://localhost:8041",
        "https://user@localhost:8040", "https://user:secret@localhost:8040",
        "https://localhost:8040/api", "https://localhost:8040?query=value",
        "https://localhost:8040/#fragment", "https://localhost:8040//",
        "ftp://localhost:8040", "https://127.0.0.1:8040"]
    {
        assert!(!approved_target(target), "MS42 unsafe URL accepted");
    }
}
