/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! G6だけの合成ZIP、完全要求列、子回収と独立実Wiki観測
//!

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, ensure};
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use super::ms5_1_fault_http::{Fault, FaultKind, Op, Relay, RelayEvidence,
    RelayPage, RelayPlan, Reply, Row, SharedControl};
use super::ms5_1_fault_report::{DetailExpectation, ExecutionExpectation,
    FaultExpectation, FaultReport, PageExpectation, RecoveryExpectation,
    WarningExpectation};
use super::ms5_1_harness::{Drained, decode_capture, drain, status_record};
use super::real_cli_http::{Auth, Environment, Lookup, MS51_ROOT, Metadata,
    Observer, within};
use super::real_cli_report::Captured;

const OLD: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NEW: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
const LATER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FB0";
const LOCK: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const LOCK2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const BODY0: &[u8] = b"MS51-G6-initial-body\r\n";
const BODY1: &[u8] = b"\xef\xbb\xbfMS51-G6-changed-body\r\n";
const ACTOR: &[u8] = b"MS51-G6-actor-body\n";
const INVALID: &[u8] = b"---\nMS51-G6-invalid-frontmatter\n";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

///
/// 実領域と合成ローカル入力を新runだけへ閉じる所有者。
///
struct Case {
    /// 非公開の対象case prefix。
    prefix: String,
    /// UTF-8/LFの設定と合成ZIPだけを所有する。
    area: TempDir,
    /// 同case内で入力/設定を再利用しない連番。
    sequence: AtomicU64,
}

impl Case {
    ///
    /// 固定ラベルから新しいcaseを生成する。
    ///
    /// # 引数
    /// * `name` - コード内で決めたASCIIラベル
    ///
    /// # 戻り値
    /// 既存runを再利用しないcase所有者。
    ///
    fn new(name: &str) -> Result<Self> {
        ensure!(!name.is_empty() && name.bytes().all(|v|
            v.is_ascii_alphanumeric() || v == b'-'), "fault case label");
        let tick = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("fault clock"))?.as_nanos();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("{MS51_ROOT}/fault-{}-{tick}-{sequence}/{name}",
            std::process::id());
        ensure!(within(&prefix, MS51_ROOT), "fault case scope");
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir()
            .map_err(|_| anyhow!("fault case area"))?;
        eprintln!("MS51 fault_case={name} scope=new");
        Ok(Self { prefix, area, sequence: AtomicU64::new(0) })
    }

    ///
    /// 連番pageを新caseへ結合する。
    ///
    /// # 引数
    /// * `page` - 0～2の固定ページ位置
    ///
    /// # 戻り値
    /// 対象case配下の完全path。
    ///
    fn path(&self, page: usize) -> String {
        format!("{}/{}", self.prefix, ["a", "b", "c"][page])
    }

    ///
    /// 固定合成Markdownから別所有のZIPを生成する。
    ///
    /// # 引数
    /// * `bodies` - a,b,cの順の合成本文
    ///
    /// # 戻り値
    /// case所有の一意な入力path。
    ///
    fn archive(&self, bodies: &[&[u8]]) -> Result<PathBuf> {
        self.archive_at(0, bodies)
    }

    ///
    /// seed対象の開始位置だけを固定して合成ZIPを作る。
    ///
    /// # 引数
    /// * `start` - a/b/cの開始位置
    /// * `bodies` - 連続する固定合成本文
    ///
    /// # 戻り値
    /// case内の一意な入力。
    ///
    fn archive_at(&self, start: usize, bodies: &[&[u8]]) -> Result<PathBuf> {
        ensure!(start + bodies.len() <= 3, "fault archive page count");
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let path = self.area.path().join(format!("input-{sequence}.zip"));
        let mut writer = ZipWriter::new(File::create(&path)
            .map_err(|_| anyhow!("fault ZIP file"))?);
        /*
         * 逆順entryから製品の確定順を確認できる入力を作る
         */
        for index in (0..bodies.len()).rev() {
            let name = ["a.md", "b.md", "c.md"][start + index];
            writer.start_file(format!("root/{name}"),
                SimpleFileOptions::default())
                .map_err(|_| anyhow!("fault ZIP entry"))?;
            writer.write_all(bodies[index])
                .map_err(|_| anyhow!("fault ZIP body"))?;
        }
        writer.finish().map_err(|_| anyhow!("fault ZIP finish"))?;
        Ok(path)
    }

    ///
    /// 認証や任意targetを受け取らない製品コマンドを構築する。
    ///
    /// # 概要
    /// 設定は初回から0600とし、絶対期限の元になる秒数を固定する。
    ///
    /// # 引数
    /// * `input` - このcaseで生成した合成ZIP
    /// * `mode` - 固定の衝突mode
    /// * `deadline` - 共通期限10秒を使うか
    /// * `limited` - 入力件数上限を1へ下げるか
    ///
    /// # 戻り値
    /// 起動前のコマンド。資格情報は別責務で設定する。
    ///
    fn command(&self, input: &PathBuf, mode: &str, deadline: bool,
        limited: bool) -> Result<Command>
    {
        ensure!(matches!(mode, "error" | "update" | "overwrite"),
            "fault mode");
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let config = self.area.path().join(format!("config-{sequence}.toml"));
        let seconds = if deadline { 10 } else { 30 };
        let mut document = format!("[limits]\nmax_processing_seconds=\
            {seconds}\n");
        if limited { document.push_str("max_pages=1\n"); }
        /*
         * 空の既存設定も読ませず秘密なしの0600設定だけを使用する
         */
        let mut file = OpenOptions::new().create_new(true).write(true)
            .mode(0o600).open(&config)
            .map_err(|_| anyhow!("fault config create"))?;
        file.write_all(document.as_bytes())
            .map_err(|_| anyhow!("fault config write"))?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
        command.arg("--config-path").arg(config)
            .args(["--prefix", &self.prefix, "--mode", mode,
                "--source-type", "zip"])
            .arg(input).stdin(Stdio::null())
            .env("XDG_CONFIG_HOME", self.area.path()).env("RUST_LOG", "trace");
        Ok(command)
    }

    ///
    /// 全要求列と既知IDを同caseへ結合する。
    ///
    /// # 引数
    /// * `count` - 入力page数
    /// * `known` - 初回から既知のaのID
    /// * `rows` - 期待した完全操作列
    /// * `faults` - 固定の単回故障
    ///
    /// # 戻り値
    /// 任意領域や任意URLを受けないrelay計画。
    ///
    fn plan(&self, count: usize, known: Option<&str>, rows: Vec<Row>,
        faults: Vec<Fault>) -> RelayPlan
    {
        RelayPlan { prefix: self.prefix.clone(),
            pages: (0..count).map(|page| RelayPage { path: self.path(page),
                known_id: if page == 0 { known.map(str::to_owned) }
                    else { None } }).collect(), rows, faults }
    }
}

/// 子起動前に準備するpipe workerのjob。
type Job = Option<Box<dyn Read + Send>>;

///
/// pipeを受け取る前から回収可能なworkerを用意する。
///
/// # 引数
/// * `receiver` - 子pipeまたは起動中止の通知
///
/// # 戻り値
/// 1MiB有限保持とEOFまでのdrainを実施するworker。
///
fn worker(receiver: Receiver<Job>) -> Result<JoinHandle<Drained>> {
    thread::Builder::new().spawn(move || match receiver.recv() {
        Ok(Some(reader)) => drain(reader),
        _ => Drained { bytes: Vec::new(), rejected: true },
    }).map_err(|_| anyhow!("fault drain worker"))
}

///
/// gateを閉じた後にだけ子を強制停止し、必ず両pipeを回収する。
///
/// # 概要
/// 60秒監視は製品の期限ではなく試験失敗の上限である。
///
/// # 引数
/// * `command` - 実行対象を固定した製品コマンド
/// * `control` - fault子だけが持つ転送gate
///
/// # 戻り値
/// 自然終了し両streamを完全回収した場合だけ解析可能な出力。
///
fn capture(mut command: Command, control: Option<SharedControl>)
    -> Result<Captured>
{
    let (out_tx, out_rx) = channel();
    let (err_tx, err_rx) = channel();
    let out_worker = match worker(out_rx) {
        Ok(worker) => worker,
        Err(error) => {
            if let Some(control) = &control { control.close(); }
            return Err(error);
        }
    };
    let err_worker = match worker(err_rx) {
        Ok(worker) => worker,
        Err(error) => {
            if let Some(control) = &control { control.close(); }
            let _sent = out_tx.send(None);
            let _joined = out_worker.join();
            return Err(error);
        }
    };
    /*
     * 両drain worker準備後にだけ子を起動する
     */
    let started = Instant::now();
    let child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_) => {
            if let Some(control) = &control { control.close(); }
            let _sent = out_tx.send(None);
            let _sent = err_tx.send(None);
            let _joined = out_worker.join();
            let _joined = err_worker.join();
            return Err(anyhow!("fault CLI spawn"));
        }
    };
    let out_sent = out_tx.send(child.stdout.take()
        .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)).is_ok();
    let err_sent = err_tx.send(child.stderr.take()
        .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)).is_ok();
    let mut forced = !out_sent || !err_sent;
    /*
     * 監視失敗とrelay拒否は試験失敗へ保持し子を残さない
     */
    let status = loop {
        if started.elapsed() >= Duration::from_secs(60)
            || control.as_ref().is_some_and(SharedControl::failed)
        { forced = true; }
        if forced {
            if let Some(control) = &control { control.close(); }
            let _killed = child.kill();
            break child.wait();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {},
            Err(_) => { forced = true; }
        }
    };
    if let Some(control) = &control { control.close(); }
    let stdout = out_worker.join();
    let stderr = err_worker.join();
    /*
     * childと両worker回収の後にだけ成功/失敗を返す
     */
    let status = status.map_err(|_| anyhow!("fault CLI wait"))?;
    eprintln!("{}", status_record(&status));
    let stdout = stdout.map_err(|_| anyhow!("fault stdout join"))?;
    let stderr = stderr.map_err(|_| anyhow!("fault stderr join"))?;
    ensure!(!forced, "fault capture forced termination");
    decode_capture(status.code(), stdout, stderr)
}

///
/// blocking子回収をTokio workerから分離する。
///
/// # 引数
/// * `command` - 検査済みコマンド
/// * `control` - 任意のrelay停止gate
///
/// # 戻り値
/// 両pipe/child回収後の出力。
///
async fn child(command: Command, control: Option<SharedControl>)
    -> Result<Captured>
{
    tokio::task::spawn_blocking(move || capture(command, control)).await
        .map_err(|_| anyhow!("fault capture join"))?
}

///
/// 本文とheaderの出力混入を生表示せず拒否する。
///
/// # 引数
/// * `captured` - 完全回収した子出力
///
/// # 戻り値
/// 本文/lock/明確な認証header非露出とstream分離。
///
fn private_output(captured: &Captured) -> Result<()> {
    for text in [&captured.stdout, &captured.stderr] {
        for value in ["MS51-G6-initial-body", "MS51-G6-changed-body",
            "MS51-G6-actor-body", "MS51-G6-invalid-frontmatter", LOCK, LOCK2,
            "Authorization", "X-Lock-Authentication", "X-Page-Lock"]
        { ensure!(!text.contains(value), "fault private output"); }
    }
    ensure!(!captured.stdout.contains("warning:")
        && !captured.stdout.contains("error:")
        && !captured.stderr.contains("result: exit="), "fault streams");
    Ok(())
}

///
/// relayを全結果で回収し、要求列と有限Reportを返す。
///
/// # 概要
/// 子失敗でもfinishを先に実施し、上流jobとsocketを残さない。
///
/// # 引数
/// * `case` - 合成入力所有者
/// * `relay` - このinvocationだけの固定転送計画
/// * `input` - case内で生成したZIP
/// * `mode` - 衝突mode
/// * `deadline` - 共通期限10秒の試験か
/// * `limited` - 入力件数上限を下げるか
///
/// # 戻り値
/// 完全出力、厳密要求証拠、解析済み有限Report。
///
async fn run(case: &Case, relay: Relay, input: &PathBuf, mode: &str,
    deadline: bool, limited: bool)
    -> Result<(Captured, RelayEvidence, FaultReport)>
{
    /*
     * command準備失敗でも最初にgateを閉じrelayを回収する
     */
    let command = case.command(input, mode, deadline, limited);
    let mut command = match command {
        Ok(command) => command,
        Err(error) => {
            relay.control().close();
            let _finished = tokio::task::spawn_blocking(move ||
                relay.finish()).await;
            return Err(error);
        }
    };
    relay.configure_child(&mut command);
    let captured = child(command, Some(relay.control())).await;
    /*
     * 子の成否にかかわらずrelayをjoinしてから有限出力を照合する
     */
    let evidence = tokio::task::spawn_blocking(move || relay.finish()).await
        .map_err(|_| anyhow!("fault relay join"))??;
    let captured = captured?;
    evidence.check_output(&captured.stdout)?;
    evidence.check_output(&captured.stderr)?;
    private_output(&captured)?;
    let report = FaultReport::parse(&captured)?;
    eprintln!("{}", report.safe_record());
    evidence.verify()?;
    Ok((captured, evidence, report))
}

///
/// page番号から期待する操作行を作る。
///
/// # 引数
/// * `page` - 計画内page位置
/// * `operation` - 固定操作
///
/// # 戻り値
/// 任意URIを持たない行。
///
fn row(page: usize, operation: Op) -> Row { Row::new(page, operation) }

///
/// 完全JSONの確定ID応答を生成する。
///
/// # 引数
/// * `id` - 合成ページID
/// * `draft` - 新規draftの201か
///
/// # 戻り値
/// 通常lookupまたはlock付きPOST応答。
///
fn identity(id: &str, draft: bool) -> Reply {
    let body = format!("{{\"id\":\"{id}\"}}");
    if draft { Reply::new(201, body.as_bytes()).locked(LOCK) }
    else { Reply::new(200, body.as_bytes()) }
}

///
/// 固定path/revisionの完全metadataを生成する。
///
/// # 引数
/// * `case` - pathを所有するcase
/// * `page` - 対象位置
/// * `deleted` - 削除観測
/// * `locked` - lock観測
///
/// # 戻り値
/// 合成の完全metadata応答。
///
fn metadata(case: &Case, page: usize, deleted: bool, locked: bool) -> Reply {
    let kind = if deleted { "last_deleted" } else { "current" };
    Reply::new(200, format!("{{\"page_info\":{{\"path\":{{\"kind\":\"{kind}\",\
        \"value\":\"{}\"}},\"deleted\":{deleted},\"locked\":{locked}}},\
        \"revision_info\":{{\"revision\":7}}}}", case.path(page)).as_bytes())
}

///
/// 新規作成の完全列を応答列と同時に生成する。
///
/// # 引数
/// * `page` - 対象位置
/// * `id` - POSTで確定する合成ID
/// * `body` - bytesを変えない入力本文
///
/// # 戻り値
/// lookup/POST/renew/PUTの四要求と四応答。
///
fn creation(page: usize, id: &str, body: &[u8])
    -> (Vec<Row>, Vec<Reply>)
{
    (vec![row(page, Op::Lookup), row(page, Op::Create), row(page, Op::Renew),
        row(page, Op::Put).body(body)], vec![Reply::new(404, b"{}"),
        identity(id, true), Reply::new(204, b"").locked(LOCK2),
        Reply::new(204, b"")])
}

///
/// 既存変更直前までの完全列を生成する。
///
/// # 引数
/// * `case` - 合成pathの所有者
/// * `delete` - overwriteのDELETEかupdateのPUTか
/// * `body` - 更新時の期待本文
///
/// # 戻り値
/// 変更一回を含む十要求と通常応答。
///
fn existing(case: &Case, delete: bool, body: &[u8])
    -> (Vec<Row>, Vec<Reply>)
{
    let final_row = if delete { row(0, Op::Delete) }
        else { row(0, Op::Put).body(body) };
    (vec![row(0, Op::Lookup), row(0, Op::Meta), row(0, Op::Source),
        row(0, Op::Acquire), row(0, Op::Renew), row(0, Op::Meta),
        row(0, Op::Lookup), row(0, Op::Source), row(0, Op::Renew), final_row],
        vec![identity(OLD, false), metadata(case, 0, false, false),
            Reply::new(200, BODY0), Reply::new(204, b"").locked(LOCK),
            Reply::new(204, b"").locked(LOCK2),
            metadata(case, 0, false, true), identity(OLD, false),
            Reply::new(200, BODY0), Reply::new(204, b"").locked(LOCK),
            Reply::new(204, b"")])
}

///
/// 不確定PUTの目的一致観測を追加する。
///
/// # 引数
/// * `case` - path所有者
/// * `rows` - 既存操作列
/// * `replies` - 既存応答列
/// * `id` - PUT前から既知のID
///
/// # 戻り値
/// なし。lookup/meta/sourceを各一回だけ追加する。
///
fn put_observed(case: &Case, rows: &mut Vec<Row>, replies: &mut Vec<Reply>,
    id: &str)
{
    rows.extend([row(0, Op::Lookup), row(0, Op::Meta), row(0, Op::Source)]);
    replies.extend([identity(id, false), metadata(case, 0, false, false),
        Reply::new(200, BODY1)]);
}

///
/// 旧削除と有効path不存在の両観測を追加する。
///
/// # 引数
/// * `case` - path所有者
/// * `rows` - 既存操作列
/// * `replies` - 既存応答列
/// * `absence` - 404または410
///
/// # 戻り値
/// なし。片方の不存在で代用しない。
///
fn delete_observed(case: &Case, rows: &mut Vec<Row>,
    replies: &mut Vec<Reply>, absence: u16)
{
    rows.extend([row(0, Op::Meta), row(0, Op::Lookup)]);
    replies.extend([metadata(case, 0, true, false),
        Reply::new(absence, b"{}")]);
}

///
/// 再作成の新POST/renew/PUTだけを追加する。
///
/// # 引数
/// * `rows` - 旧削除確認までの要求列
/// * `replies` - 旧削除確認までの応答列
///
/// # 戻り値
/// なし。初回lookupを追加しない。
///
fn recreate(rows: &mut Vec<Row>, replies: &mut Vec<Reply>) {
    rows.extend([row(0, Op::Create), row(0, Op::Renew),
        row(0, Op::Put).body(BODY1)]);
    replies.extend([identity(NEW, true), Reply::new(204, b"").locked(LOCK2),
        Reply::new(204, b"")]);
}

///
/// IDをDebugせず元操作の詳細期待へ結合する。
///
/// # 引数
/// * `operation` - 詳細操作
/// * `kind` - 元kind
/// * `stage` - 元原因段階
/// * `status` - 明示Some/Noneの観測status
/// * `id` - 操作前から確定したID
///
/// # 戻り値
/// 送信開始済み単発操作の完全な期待。
///
fn detail(operation: &str, kind: &str, stage: &str, status: Option<u16>,
    id: Option<&str>) -> DetailExpectation
{
    let mut value = DetailExpectation::wiki(operation, kind, Some(stage),
        Some("Started"), status);
    value.known_id = id.map(str::to_owned);
    value
}

///
/// 計画内pageの基本期待を生成する。
///
/// # 引数
/// * `case` - path所有者
/// * `index` - page位置
/// * `outcome` - 有限な製品結果
/// * `stage` - 最後の試行段階
///
/// # 戻り値
/// 復旧情報と詳細は呼出側で明示する基本期待。
///
fn page(case: &Case, index: usize, outcome: &str, stage: &str)
    -> PageExpectation
{
    let source = format!("Zip:root/{}", ["a.md", "b.md", "c.md"][index]);
    PageExpectation::new(&case.path(index), &source, outcome, stage)
}

///
/// 正常な新規ページの復旧情報を明示する。
///
/// # 引数
/// * `case` - path所有者
/// * `index` - page位置
/// * `id` - 正常POSTで確定したID
///
/// # 戻り値
/// 観測値を補完しない正常完了期待。
///
fn created(case: &Case, index: usize, id: &str) -> PageExpectation {
    let mut value = page(case, index, "Created", "Finished");
    value.recovery = Some(RecoveryExpectation::new(None, None, Some(id),
        false, "Finished", "Applied"));
    value
}

///
/// 失敗結果の復旧案内を有限語彙で指定する。
///
/// # 引数
/// * `old` - 旧ID/revisionを持つか
/// * `new` - 確定新ID
/// * `deleted` - 旧削除確認
/// * `stage` - 元原因段階
/// * `observed` - 観測した状態
///
/// # 戻り値
/// 基本復旧情報。lock/pathは呼出側が成功観測時だけ設定する。
///
fn recovery(old: bool, new: Option<&str>, deleted: bool, stage: &str,
    observed: &str) -> RecoveryExpectation
{
    let mut value = RecoveryExpectation::new(if old { Some(OLD) }
        else { None }, if old { Some(7) } else { None }, new, deleted,
        stage, observed);
    value.advice = Some(if new.is_some() { "InspectDraft" }
        else if deleted { "RecreateManually" }
        else { "InspectBeforeRetry" }.to_owned());
    value
}

///
/// PUT警告と削除警告の完全期待を作る。
///
/// # 引数
/// * `delete` - 旧削除の警告か
/// * `status` - 元応答status
/// * `kind` - 元kind
/// * `id` - 元操作のID
///
/// # 戻り値
/// 発生順へ追加する一件の警告。
///
fn warning(delete: bool, status: Option<u16>, kind: &str, id: &str)
    -> WarningExpectation
{
    WarningExpectation::new(if delete { "Delete" } else { "Put" }, status,
        if delete { "Deleted" } else { "Applied" },
        detail(if delete { "DeletePage" } else { "PutSource" }, kind,
            if delete { "DeleteOld" } else { "PutSource" }, status, Some(id)))
}

///
/// 通常故障群を明示番号で実行する。
///
/// # 引数
/// * `group` - N01～N06の固定群番号
///
/// # 戻り値
/// 全小条件の成功。未設定をskipへ変換しない。
///
pub async fn normal(group: u8) -> Result<()> {
    match group {
        1 => invalid_inputs().await,
        2 => put_cases().await,
        3 => delete_cases().await,
        4 => post_cases().await,
        5 => independent_failures().await,
        6 => deadline_case().await,
        _ => Err(anyhow!("unknown fault group")),
    }
}

///
/// 不正ZIPをそれぞれ計画前・要求0で拒否する。
///
/// # 概要
/// 重複は合成ZIPのlocal/central双方の同長名を置換して作る。
///
/// # 戻り値
/// 破損、重複、件数超過の終了2。
///
async fn invalid_inputs() -> Result<()> {
    for variant in 0..3 {
        let case = Case::new("n01")?;
        let input = case.archive(&[BODY0, BODY1])?;
        /*
         * 外部入力を読まず生成済みZIPだけを小さく変造する
         */
        if variant == 0 {
            fs::write(&input, b"MS51-G6-broken-zip")
                .map_err(|_| anyhow!("fault corrupt ZIP"))?;
        } else if variant == 1 {
            let mut bytes = fs::read(&input)
                .map_err(|_| anyhow!("fault ZIP read"))?;
            let mut replaced = 0;
            for index in 0..bytes.len().saturating_sub(3) {
                if &bytes[index..index + 4] == b"b.md" {
                    bytes[index] = b'a';
                    replaced += 1;
                }
            }
            ensure!(replaced == 2, "fault duplicate ZIP structure");
            fs::write(&input, bytes)
                .map_err(|_| anyhow!("fault duplicate ZIP"))?;
        }
        let relay = Relay::scripted(case.plan(2, None, Vec::new(),
            Vec::new()), Vec::new())?;
        let (_, _, report) = run(&case, relay, &input, "error", false,
            variant == 2).await?;
        let mut expected = FaultExpectation::new(2, false, [0; 7]);
        expected.global_errors.push("入力検証に失敗しました".to_owned());
        report.expect(&expected)?;
    }
    Ok(())
}

///
/// 新規/updateの500/喪失を単回照合し後続へ進む。
///
/// # 概要
/// 四条件すべてで起点、目的状態、後続作成を完全要求列にする。
///
/// # 戻り値
/// 警告終了3と二重送信なし。
///
async fn put_cases() -> Result<()> {
    for old in [false, true] {
        for lost in [false, true] {
            let case = Case::new("n02")?;
            let input = case.archive(&[BODY1, BODY1])?;
            let (mut rows, mut replies) = if old {
                existing(&case, false, BODY1)
            } else { creation(0, NEW, BODY1) };
            let last = rows.len() - 1;
            rows[last] = row(0, Op::Put).body(BODY1).statuses(&[500]);
            replies[last] = if lost { Reply::disconnected() }
                else { Reply::new(500, b"{}") };
            let id = if old { OLD } else { NEW };
            put_observed(&case, &mut rows, &mut replies, id);
            let (later_rows, later_replies) = creation(1, LATER, BODY1);
            rows.extend(later_rows);
            replies.extend(later_replies);
            let relay = Relay::scripted(case.plan(2,
                if old { Some(OLD) } else { None }, rows, Vec::new()),
                replies)?;
            let (_, _, report) = run(&case, relay, &input, "update", false,
                false).await?;
            /*
             * 警告と目的確認の旧新情報を独立した完全期待へまとめる
             */
            let action = if old { "Update" } else { "Create" };
            let mut first = page(&case, 0, &format!(
                "AppliedWithWarning {{ action: {action} }}"), "Reconcile");
            let mut facts = RecoveryExpectation::new(if old { Some(OLD) }
                else { None }, if old { Some(7) } else { None },
                if old { None } else { Some(NEW) }, false, "Reconcile",
                "Applied");
            facts.old_lock = if old { Some(false) } else { None };
            facts.lock = Some(false);
            facts.path_state = "Found".to_owned();
            facts.path_id = Some(id.to_owned());
            first.recovery = Some(facts);
            first.warnings.push(warning(false, if lost { None }
                else { Some(500) }, if lost { "Transport" } else { "Http" },
                id));
            report.expect(&FaultExpectation::new(3, true,
                [if old { 1 } else { 2 }, 0, u64::from(old), 0, 1, 0, 0])
                .page(first).page(created(&case, 1, LATER)))?;
        }
    }
    Ok(())
}

///
/// 元原因を持つ失敗pageと解除未試行を生成する。
///
/// # 引数
/// * `case` - path所有者
/// * `category` - 有限失敗分類
/// * `cause` - 元原因段階
/// * `last` - 最後段階
/// * `primary` - 元詳細
///
/// # 戻り値
/// 復旧情報は呼出側が明示する失敗期待。
///
fn failed(case: &Case, category: &str, cause: &str, last: &str,
    primary: DetailExpectation) -> PageExpectation
{
    let mut value = page(case, 0, &format!(
        "Failed {{ category: {category}, stage: {cause} }}"), last);
    value.primary = Some(primary);
    value.execution = Some(ExecutionExpectation {
        cleanup_attempted: false, stop: false });
    value
}

///
/// first_failureと後続未実行を明示した完全期待を返す。
///
/// # 引数
/// * `case` - path所有者
/// * `first` - aの失敗期待
/// * `warnings` - 操作警告数
///
/// # 戻り値
/// 失敗一件とb未実行の集計・順序期待。
///
fn stopped(case: &Case, first: PageExpectation, warnings: u64)
    -> FaultExpectation
{
    let mut expected = FaultExpectation::new(1, true,
        [0, 0, 0, 0, warnings, 1, 1]).page(first)
        .page(page(case, 1, "NotExecuted", "Preflight"));
    expected.first_failure = Some(case.path(0));
    expected
}

///
/// 完全500だけに再作成を許すDELETE分類を確認する。
///
/// # 概要
/// 正常EOFの404/410両分岐と空・不完500・不正成功・喪失を比較する。
///
/// # 戻り値
/// 完全要求列、警告、旧情報と元kindの一致。
///
async fn delete_cases() -> Result<()> {
    for variant in 0..6 {
        let case = Case::new("n03")?;
        let input = case.archive(&[BODY1, BODY1])?;
        let (mut rows, mut replies) = existing(&case, true, BODY1);
        let (kind, status) = match variant {
            0 | 1 => ("Http", Some(500)),
            2 => ("Protocol", Some(500)),
            3 => ("Transport", Some(500)),
            4 => ("Protocol", Some(200)),
            _ => ("Transport", None),
        };
        rows[9] = row(0, Op::Delete).statuses(&[500, 200]);
        replies[9] = match variant {
            0 | 1 => Reply::new(500, b"{}"),
            2 => Reply::new(500, b""),
            3 => Reply::new(500, b"{}").incomplete(),
            4 => Reply::new(200, b""),
            _ => Reply::disconnected(),
        };
        let absence = if variant == 1 { 410 } else { 404 };
        delete_observed(&case, &mut rows, &mut replies, absence);
        /*
         * 完全500の二条件だけ新ID作成と通常後続を追加する
         */
        if variant < 2 {
            recreate(&mut rows, &mut replies);
            let (later_rows, later_replies) = creation(1, LATER, BODY1);
            rows.extend(later_rows);
            replies.extend(later_replies);
        }
        let relay = Relay::scripted(case.plan(2, Some(OLD), rows,
            Vec::new()), replies)?;
        let (_, _, report) = run(&case, relay, &input, "overwrite", false,
            false).await?;
        /*
         * 旧削除とpath不存在の観測を成功扱いと切り離して比較する
         */
        if variant < 2 {
            let mut first = page(&case, 0,
                "AppliedWithWarning { action: Recreate }", "Finished");
            let mut facts = RecoveryExpectation::new(Some(OLD), Some(7),
                Some(NEW), true, "Finished", "Applied");
            facts.old_lock = Some(false);
            facts.path_state = if absence == 410 { "Deleted" }
                else { "Absent" }.to_owned();
            first.recovery = Some(facts);
            first.warnings.push(warning(true, Some(500), "Http", OLD));
            report.expect(&FaultExpectation::new(3, true,
                [1, 0, 0, 1, 1, 0, 0]).page(first)
                .page(created(&case, 1, LATER)))?;
        } else {
            let mut first = failed(&case, "Uncertain", "DeleteOld",
                "Reconcile", detail("DeletePage", kind, "DeleteOld",
                    status, Some(OLD)));
            let mut facts = recovery(true, None, true, "DeleteOld", "Deleted");
            facts.old_lock = Some(false);
            facts.lock = Some(false);
            facts.path_state = "Absent".to_owned();
            first.recovery = Some(facts);
            first.warnings.push(warning(true, status, kind, OLD));
            report.expect(&stopped(&case, first, 1))?;
        }
    }
    Ok(())
}

///
/// 新規POSTの異常四条件を探索なしで確認する。
///
/// # 概要
/// 元HTTP/喪失と不正lock/不完JSONを独立した異常として期待する。
///
/// # 戻り値
/// ID確定範囲・元kind・全追加要求0。
///
async fn initial_post_cases() -> Result<()> {
    for variant in 0..4 {
        /*
         * POST一回で列を止め、完全確定IDと未知IDを分ける
         */
        let case = Case::new("n04-initial")?;
        let input = case.archive(&[BODY1, BODY1])?;
        let (reply, kind, status, known) = match variant {
            0 => (Reply::new(500, b"{}"), "Http", Some(500), false),
            1 => (Reply::disconnected(), "Transport", None, false),
            2 => (Reply::new(201,
                format!("{{\"id\":\"{NEW}\"}}").as_bytes()),
                "LockHeader", Some(201), true),
            _ => (Reply::new(201, b"{\"id\":\"partial").locked(LOCK),
                "Protocol", Some(201), false),
        };
        let rows = vec![row(0, Op::Lookup),
            row(0, Op::Create).statuses(&[201, 500])];
        let relay = Relay::scripted(case.plan(2, None, rows, Vec::new()),
            vec![Reply::new(404, b"{}"), reply])?;
        let (_, _, report) = run(&case, relay, &input, "error", false,
            false).await?;
        let id = if known { Some(NEW) } else { None };
        let mut first = failed(&case, "Uncertain", "CreateDraft",
            "CreateDraft", detail("CreateDraft", kind, "CreateDraft",
                status, id));
        first.recovery = Some(recovery(false, id, false, "CreateDraft",
            "Unknown"));
        report.expect(&stopped(&case, first, 0))?;
    }
    Ok(())
}

///
/// DELETE500後の再作成七分岐を個別に停止させる。
///
/// # 概要
/// POST拒否/喪失/不正lock/不完JSON、renew失敗、PUT400/照合失敗。
///
/// # 戻り値
/// 先行警告、旧情報、確定新ID範囲と未実行の一致。
///
async fn recreation_cases() -> Result<()> {
    for variant in 0..7 {
        let case = Case::new("n04-recreate")?;
        let input = case.archive(&[BODY1, BODY1])?;
        let (mut rows, mut replies) = existing(&case, true, BODY1);
        rows[9] = row(0, Op::Delete).statuses(&[500]);
        replies[9] = Reply::new(500, b"{}");
        delete_observed(&case, &mut rows, &mut replies, 404);
        let (operation, stage, kind, status, known) = match variant {
            /*
             * POST失敗では後続renew/PUTを定義しない
             */
            0..=3 => {
                rows.push(row(0, Op::Create).statuses(&[201, 403]));
                let values = match variant {
                    0 => (Reply::new(403, b"{}"), "Http", Some(403), false),
                    1 => (Reply::disconnected(), "Transport", None, false),
                    2 => (Reply::new(201,
                        format!("{{\"id\":\"{NEW}\"}}").as_bytes()),
                        "LockHeader", Some(201), true),
                    _ => (Reply::new(201, b"{\"id\":\"partial").locked(LOCK),
                        "Protocol",
                        Some(201), false),
                };
                replies.push(values.0);
                ("CreateDraft", "CreateDraft", values.1, values.2, values.3)
            }
            /*
             * 確定新IDのrenewとPUTを別原因として保持する
             */
            4 => {
                rows.extend([row(0, Op::Create),
                    row(0, Op::Renew).statuses(&[500])]);
                replies.extend([identity(NEW, true), Reply::new(500, b"{}")]);
                ("RenewLock", "PutSource", "Http", Some(500), true)
            }
            _ => {
                recreate(&mut rows, &mut replies);
                let status = if variant == 5 { 400 } else { 500 };
                rows[14] = row(0, Op::Put).body(BODY1).statuses(&[status]);
                replies[14] = Reply::new(status, b"{}");
                if variant == 6 {
                    rows.push(row(0, Op::Lookup).statuses(&[503]));
                    replies.push(Reply::new(503, b"{}"));
                }
                ("PutSource", "PutSource", "Http", Some(status), true)
            }
        };
        let relay = Relay::scripted(case.plan(2, Some(OLD), rows,
            Vec::new()), replies)?;
        let (_, _, report) = run(&case, relay, &input, "overwrite", false,
            false).await?;
        /*
         * 全分岐で元DELETE警告を先頭に保ち、主失敗を別に比較する
         */
        let category = if variant == 0 { "Permission" }
            else if variant == 4 { "Http(500)" }
            else if variant == 5 { "Rejected" } else { "Uncertain" };
        let id = if known { Some(NEW) } else { None };
        let last = if variant == 6 { "Reconcile" } else { stage };
        let mut first = failed(&case, category, stage, last,
            detail(operation, kind, stage, status, id));
        let mut facts = recovery(true, id, true, stage,
            if variant == 6 { "Unknown" } else { "Deleted" });
        facts.old_lock = Some(false);
        facts.path_state = "Absent".to_owned();
        first.recovery = Some(facts);
        first.warnings.push(warning(true, Some(500), "Http", OLD));
        if variant == 6 {
            first.auxiliary.push(detail("Lookup", "Http", "Reconcile",
                Some(503), None));
        }
        report.expect(&stopped(&case, first, 1))?;
    }
    Ok(())
}

///
/// 新規POST四条件と再作成七条件を同じ通常群へ接続する。
///
/// # 戻り値
/// 全十一条件の厳密要求列とReport期待の一致。
///
async fn post_cases() -> Result<()> {
    initial_post_cases().await?;
    recreation_cases().await
}

///
/// 先行成功の後の補助GET失敗と解除失敗を分離する。
///
/// # 概要
/// a成功/b失敗/c未実行を維持し、主原因の置換を拒否する。
///
/// # 戻り値
/// primary/auxiliary/cleanupの完全な独立期待。
///
async fn independent_failures() -> Result<()> {
    for cleanup in [false, true] {
        let case = Case::new("n05")?;
        let input = case.archive(&[BODY1, BODY1, BODY1])?;
        let (mut rows, mut replies) = creation(0, NEW, BODY1);
        let mut first = created(&case, 0, NEW);
        first.warnings.clear();
        /*
         * bの主失敗を先行aとは別の確定IDへ結合する
         */
        let mut middle = page(&case, 1, if cleanup {
            "Failed { category: Conflict, stage: Recheck }"
        } else { "Failed { category: Uncertain, stage: PutSource }" },
            if cleanup { "ReleaseLock" } else { "Reconcile" });
        if cleanup {
            rows.extend([row(1, Op::Lookup), row(1, Op::Meta),
                row(1, Op::Source), row(1, Op::Acquire), row(1, Op::Renew),
                row(1, Op::Meta), row(1, Op::Lookup), row(1, Op::Source),
                row(1, Op::Unlock).statuses(&[500])]);
            replies.extend([identity(OLD, false),
                metadata(&case, 1, false, false), Reply::new(200, BODY0),
                Reply::new(204, b"").locked(LOCK),
                Reply::new(204, b"").locked(LOCK2),
                metadata(&case, 1, false, true), identity(OLD, false),
                Reply::new(200, ACTOR), Reply::new(500, b"{}")]);
            middle.primary = Some(DetailExpectation::local("Conflict",
                Some("Recheck")));
            middle.cleanup = Some(detail("ReleaseLock", "Http",
                "ReleaseLock", Some(500), Some(OLD)));
            let mut facts = RecoveryExpectation::new(Some(OLD), Some(7),
                None, false, "Recheck", "Unknown");
            facts.old_lock = Some(true);
            facts.advice = Some("ResolveConflict".to_owned());
            middle.recovery = Some(facts);
        } else {
            let (mut next_rows, mut next_replies) = creation(1, OLD, BODY1);
            next_rows[3] = row(1, Op::Put).body(BODY1).statuses(&[500]);
            next_replies[3] = Reply::new(500, b"{}");
            rows.extend(next_rows);
            replies.extend(next_replies);
            rows.push(row(1, Op::Lookup).statuses(&[503]));
            replies.push(Reply::new(503, b"{}"));
            middle.primary = Some(detail("PutSource", "Http", "PutSource",
                Some(500), Some(OLD)));
            middle.auxiliary.push(detail("Lookup", "Http", "Reconcile",
                Some(503), None));
            middle.recovery = Some(recovery(false, Some(OLD), false,
                "PutSource", "Unknown"));
        }
        middle.execution = Some(ExecutionExpectation {
            cleanup_attempted: cleanup, stop: false });
        let mut plan = case.plan(3, None, rows, Vec::new());
        if cleanup { plan.pages[1].known_id = Some(OLD.to_owned()); }
        let relay = Relay::scripted(plan, replies)?;
        let (_, _, report) = run(&case, relay, &input, "update", false,
            false).await?;
        let mut expected = FaultExpectation::new(1, true,
            [1, 0, 0, 0, 0, 1, 1]).page(first).page(middle)
            .page(page(&case, 2, "NotExecuted", "Preflight"));
        expected.first_failure = Some(case.path(1));
        report.expect(&expected)?;
    }
    Ok(())
}

///
/// 共通期限到達時のDELETE元事実を完全に期待する。
///
/// # 引数
/// * `case` - path所有者
/// * `id` - 旧ID
/// * `revision` - 事前revision
///
/// # 戻り値
/// 期限後の追加HTTPを前提にしない失敗期待。
///
fn deadline_expectation(case: &Case, id: &str, revision: u64)
    -> FaultExpectation
{
    let mut first = failed(case, "Deadline", "DeleteOld", "DeleteOld",
        detail("DeletePage", "Deadline", "DeleteOld", None, Some(id)));
    let mut facts = RecoveryExpectation::new(Some(id), Some(revision), None,
        false, "DeleteOld", "Unknown");
    facts.old_lock = Some(true);
    facts.advice = Some("InspectBeforeRetry".to_owned());
    first.recovery = Some(facts);
    let mut expected = stopped(case, first, 0);
    expected.global_errors.push("category=Deadline stage=Preflight".to_owned());
    expected.global_details.push(DetailExpectation::local("Deadline",
        Some("Preflight")));
    expected
}

///
/// DELETE応答保持中に同じCLI期限へ到達させる。
///
/// # 戻り値
/// fault到達必須、追加HTTP0、強制終了なしの期限停止。
///
async fn deadline_case() -> Result<()> {
    let case = Case::new("n06")?;
    let input = case.archive(&[BODY1, BODY1])?;
    let (rows, replies) = existing(&case, true, BODY1);
    let relay = Relay::scripted(case.plan(2, Some(OLD), rows,
        vec![Fault { at: 9, kind: FaultKind::Hold }]), replies)?;
    let (_, _, report) = run(&case, relay, &input, "overwrite", true,
        false).await?;
    report.expect(&deadline_expectation(&case, OLD, 7))?;
    eprintln!("MS51 deadline expired_before_delete_inject=false \
        fault_reached=true followup_http=0 cancellation_evidence=unit_only");
    Ok(())
}

///
/// 独立seedで確認した旧IDとrevisionだけを所有する。
///
struct Seed {
    /// 合成pageへ結合した確定ID。
    id: String,
    /// 正常metadataから確認した旧revision。
    revision: u64,
}

///
/// 直結seed/actorと限定した実拒否の子を回収する。
///
/// # 引数
/// * `environment` - 承認済みの実HTTPS設定
/// * `case` - 合成入力の所有者
/// * `input` - caseが生成したZIP
/// * `mode` - 固定衝突mode
///
/// # 戻り値
/// 秘密非露出確認済みの有限報告。
///
async fn direct(environment: &Environment, case: &Case, input: &PathBuf,
    mode: &str) -> Result<FaultReport>
{
    let mut command = case.command(input, mode, false, false)?;
    environment.configure_child(&mut command, Auth::Bearer);
    let captured = child(command, None).await?;
    environment.check_output(&captured.stdout)?;
    environment.check_output(&captured.stderr)?;
    private_output(&captured)?;
    let report = FaultReport::parse(&captured)?;
    eprintln!("{}", report.safe_record());
    Ok(report)
}

///
/// 製品が正常POSTで知った新IDだけを期待値構築へ取り出す。
///
/// # 引数
/// * `report` - 有限解析済み報告
/// * `case` - 入力所有者
/// * `index` - 計画位置
///
/// # 戻り値
/// CLIが明示した確定新ID。relay専用IDは使わない。
///
fn draft(report: &FaultReport, case: &Case, index: usize) -> Result<String> {
    report.page(&case.path(index))?.recovery.as_ref()
        .and_then(|value| value.draft_id.clone())
        .ok_or_else(|| anyhow!("fault CLI draft ID missing"))
}

///
/// metadataの観測時刻と潜在cleanupを値非表示で記録する。
///
/// # 引数
/// * `observer` - 製品と別期限の独立観測者
/// * `case` - 合成領域
/// * `index` - 固定page位置
/// * `id` - 独立結合済みの確定ID
///
/// # 戻り値
/// 一回のmetadata応答。失敗を不存在へ変換しない。
///
async fn observed_meta(observer: &Observer, case: &Case, index: usize,
    id: &str) -> Result<Metadata>
{
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("fault observation clock"))?.as_millis();
    eprintln!("MS51 page=P{:02} observation=metadata phase=begin \
        unix_ms={elapsed} potential_expiry_cleanup=true", index + 1);
    let value = observer.metadata(id, &case.path(index)).await?;
    eprintln!("MS51 page=P{:02} observation=metadata phase=end \
        deleted={} locked={} revision={:?}", index + 1,
        value.page_info.deleted, value.page_info.locked,
        value.revision_info.as_ref().map(|value| value.revision));
    Ok(value)
}

///
/// 新caseの対象が有効pathを持たないことを一回確認する。
///
/// # 引数
/// * `observer` - 独立HTTPS観測者
/// * `case` - 合成領域
/// * `index` - 固定page位置
///
/// # 戻り値
/// 404/410の明示観測。
///
async fn missing(observer: &mut Observer, case: &Case, index: usize)
    -> Result<()>
{
    ensure!(matches!(observer.lookup(&case.path(index)).await?,
        Lookup::Missing(404 | 410)), "fault expected absent path");
    Ok(())
}

///
/// seed前に実対象全pageの不存在をそれぞれ一回確認する。
///
/// # 引数
/// * `environment` - 承認済み実HTTPS設定
/// * `name` - 固定caseラベル
/// * `count` - 計画に含む全page数
///
/// # 戻り値
/// 初期不存在を独立観測済みの新case。
///
async fn prepared(environment: &Environment, name: &str, count: usize)
    -> Result<Case>
{
    ensure!((1..=3).contains(&count), "fault prepared page count");
    let case = Case::new(name)?;
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    for index in 0..count { missing(&mut observer, &case, index).await?; }
    Ok(case)
}

///
/// 正常metadataの非draft/current/lockなしとrevisionを確認する。
///
/// # 引数
/// * `meta` - 一回の独立metadata観測
///
/// # 戻り値
/// 確認したrevision。未知を補完しない。
///
fn revision(meta: &Metadata) -> Result<u64> {
    ensure!(!meta.page_info.deleted && !meta.page_info.locked
        && meta.page_info.path.kind == "current", "fault active metadata");
    meta.revision_info.as_ref().map(|value| value.revision)
        .ok_or_else(|| anyhow!("fault normal revision missing"))
}

///
/// 有効path/ID、正常metadata、本文bytesを独立して比較する。
///
/// # 引数
/// * `observer` - 別期限の独立観測者
/// * `case` - 合成領域
/// * `index` - 固定page位置
/// * `id` - CLIまたはseedが確定したID
/// * `body` - 固定合成bytes
///
/// # 戻り値
/// 一回観測した正常revision。
///
async fn active_page(observer: &mut Observer, case: &Case, index: usize,
    id: &str, body: &[u8]) -> Result<u64>
{
    ensure!(matches!(observer.lookup(&case.path(index)).await?,
        Lookup::Found(value) if value == id), "fault active ID");
    let value = revision(&observed_meta(observer, case, index, id).await?)?;
    observer.source_equals(id, &case.path(index), body).await?;
    Ok(value)
}

///
/// 合成旧本文を直結CLIで作り、別HTTPSで旧事実を確定する。
///
/// # 引数
/// * `environment` - 承認済み実設定
/// * `case` - 新case
/// * `index` - seedする固定位置
///
/// # 戻り値
/// 独立確認済みの旧ID/revision。
///
async fn seed(environment: &Environment, case: &Case, index: usize)
    -> Result<Seed>
{
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    let input = case.archive_at(index, &[BODY0])?;
    let report = direct(environment, case, &input, "error").await?;
    let id = draft(&report, case, index)?;
    report.expect(&FaultExpectation::new(0, true, [1, 0, 0, 0, 0, 0, 0])
        .page(created(case, index, &id)))?;
    let revision = active_page(&mut observer, case, index, &id, BODY0).await?;
    Ok(Seed { id, revision })
}

///
/// 実planと初期IDを同じ完全path集合へ結合する。
///
/// # 引数
/// * `environment` - 承認済み実HTTPS設定
/// * `case` - 合成領域
/// * `count` - page数
/// * `old` - 初期既知page位置と旧ID
/// * `rows` - 完全要求列
/// * `faults` - 上流完了後の単回故障
///
/// # 戻り値
/// このinvocationだけの限定relay。
///
fn real_relay(environment: &Environment, case: &Case, count: usize,
    old: Option<(usize, &str)>, rows: Vec<Row>, faults: Vec<Fault>)
    -> Result<Relay>
{
    let mut plan = case.plan(count, None, rows, faults);
    let mut known = Vec::new();
    if let Some((index, id)) = old {
        plan.pages[index].known_id = Some(id.to_owned());
        known.push((id.to_owned(), case.path(index)));
    }
    let paths: Vec<String> = plan.pages.iter()
        .map(|value| value.path.clone()).collect();
    let grant = environment.relay_grant(&case.prefix, &paths, &known)?;
    Relay::real(grant, plan)
}

///
/// 独立旧事実へ合成期待の旧ID/revisionを置き換える。
///
/// # 引数
/// * `facts` - case固有の復旧期待
/// * `old` - 独立seedが確定した旧事実
///
/// # 戻り値
/// 旧事実を結合した完全期待。
///
fn with_old(mut facts: RecoveryExpectation, old: &Seed)
    -> RecoveryExpectation
{
    facts.old_id = Some(old.id.clone());
    facts.revision = Some(old.revision);
    facts
}

///
/// 上流のpath不存在statusだけから履歴期待を決める。
///
/// # 引数
/// * `evidence` - 回収済みの完全要求証拠
/// * `at` - 旧削除照合lookup位置
///
/// # 戻り値
/// 上流が確認したAbsent/Deleted。製品報告から期待を転記しない。
///
fn absent_history(evidence: &RelayEvidence, at: usize) -> Result<String> {
    match evidence.events().get(at).and_then(|value| value.status) {
        Some(404) => Ok("Absent".to_owned()),
        Some(410) => Ok("Deleted".to_owned()),
        _ => Err(anyhow!("fault upstream absence not confirmed")),
    }
}

///
/// 旧soft-deleteの履歴と元本文を別HTTPSで確認する。
///
/// # 引数
/// * `observer` - 新規作成先と別に旧IDを結合した観測者
/// * `case` - 合成領域
/// * `index` - 旧page位置
/// * `old` - 独立seedで確定した旧事実
///
/// # 戻り値
/// 旧削除/current不在とは独立した履歴の一致。
///
async fn deleted_page(observer: &mut Observer, case: &Case, index: usize,
    old: &Seed) -> Result<()>
{
    observer.remember(&old.id, &case.path(index))?;
    let meta = observed_meta(observer, case, index, &old.id).await?;
    ensure!(meta.page_info.deleted && !meta.page_info.locked
        && meta.page_info.path.kind == "last_deleted"
        && meta.revision_info.as_ref().is_some_and(|value|
            value.revision == old.revision), "fault old deletion facts");
    observer.source_equals(&old.id, &case.path(index), BODY0).await
}

///
/// 新draftを最後に一回だけ観測し、回収せず残す。
///
/// # 引数
/// * `environment` - 独立HTTPS設定
/// * `case` - 合成領域
/// * `index` - 新page位置
/// * `id` - CLI確定IDまたはrelay専用確定ID
/// * `relay_only` - POST喪失でCLIへ補完してはいけないIDか
///
/// # 戻り値
/// metadata一回だけで確認したdraft残存。探索・再POST・解除なし。
///
async fn draft_once(environment: &Environment, case: &Case, index: usize,
    id: &str, relay_only: bool) -> Result<()>
{
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    observer.remember(id, &case.path(index))?;
    eprintln!("MS51 draft_id_source={} metadata_only=true \
        automatic_recovery=false", if relay_only { "relay" } else { "cli" });
    let meta = observed_meta(&observer, case, index, id).await?;
    ensure!(!meta.page_info.deleted && meta.page_info.locked
        && meta.page_info.path.kind == "current"
        && meta.revision_info.is_none(), "fault draft observation");
    Ok(())
}

///
/// 実front matter拒否の固定有限診断を期待する。
///
/// # 引数
/// * `id` - 正常POSTでCLIが確定した新ID
///
/// # 戻り値
/// LuWikiの閉じdelimiter欠落: Syntax/line2/column未指定。
///
fn syntax_detail(id: &str) -> DetailExpectation {
    let mut value = detail("PutSource", "Http", "PutSource", Some(400),
        Some(id));
    value.diagnostics = Some(("Syntax".to_owned(), Some(2), None));
    value
}

///
/// 実入口を明示承認と既存5環境変数だけで選択する。
///
/// # 引数
/// * `number` - R01～R14の固定番号
///
/// # 戻り値
/// 代表実故障の独立観測と有限期待の一致。未設定をskipにしない。
///
pub async fn real(number: u8) -> Result<()> {
    ensure!(std::env::var("LUWIKI_MS51_FAULT_APPROVAL").ok().as_deref()
        == Some("REAL-FAULT-01"), "fault explicit approval required");
    let environment = Environment::load_ms51()?;
    match number {
        1 => real_frontmatter(&environment).await,
        2 => real_locked(&environment).await,
        3 => real_race(&environment).await,
        4 | 5 => real_put(&environment, number == 5).await,
        6..=8 | 10..=12 | 14 => real_delete(&environment, number).await,
        9 => real_post_lost(&environment).await,
        13 => real_recreation_stop(&environment).await,
        _ => Err(anyhow!("fault real case number")),
    }
}

///
/// 新規実PUT400と残存draftを確認する。
///
/// # 引数
/// * `environment` - 承認済み実設定
///
/// # 戻り値
/// 主Syntax診断・新ID確定・後続未実行。
///
async fn real_frontmatter(environment: &Environment) -> Result<()> {
    let case = prepared(environment, "r01", 2).await?;
    let input = case.archive(&[INVALID, BODY1])?;
    let report = direct(environment, &case, &input, "error").await?;
    let id = draft(&report, &case, 0)?;
    let mut first = failed(&case, "Rejected", "PutSource", "PutSource",
        syntax_detail(&id));
    first.recovery = Some(recovery(false, Some(&id), false, "PutSource",
        "Unknown"));
    report.expect(&stopped(&case, first, 0))?;
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    missing(&mut observer, &case, 1).await?;
    draft_once(environment, &case, 0, &id, false).await
}

///
/// 既存actor lockの予定409だけで限定解除する。
///
/// # 概要
/// 期待拒否と旧状態一致を確認した場合だけ同owner資源を解除する。
///
/// # 引数
/// * `environment` - 承認済み実設定
///
/// # 戻り値
/// 本文/旧revision維持と同ownerの単回解除。
///
async fn real_locked(environment: &Environment) -> Result<()> {
    /*
     * 全対象の初期不存在確認後、通常seedだけへactor lockを取得する
     */
    let case = prepared(environment, "r02", 2).await?;
    let old = seed(environment, &case, 0).await?;
    let mut actor = environment.observer(&case.prefix, Auth::Bearer)?;
    actor.remember(&old.id, &case.path(0))?;
    let mut lease = actor.acquire(&old.id, &case.path(0)).await?;
    let input = case.archive(&[BODY1, BODY1])?;
    let report = direct(environment, &case, &input, "update").await?;
    let mut first = failed(&case, "Conflict", "AcquireLock", "AcquireLock",
        detail("AcquireLock", "Http", "AcquireLock", Some(409),
            Some(&old.id)));
    let mut facts = with_old(recovery(true, None, false, "AcquireLock",
        "Unknown"), &old);
    facts.advice = Some("ResolveConflict".to_owned());
    first.recovery = Some(facts);
    report.expect(&stopped(&case, first, 0))?;
    /*
     * 期待拒否と旧状態一致が揃った場合だけactor自身のleaseを解除する
     */
    let meta = observed_meta(&actor, &case, 0, &old.id).await?;
    ensure!(meta.page_info.locked && !meta.page_info.deleted
        && meta.page_info.path.kind == "current"
        && meta.revision_info.as_ref().is_some_and(|value|
            value.revision == old.revision), "fault actor locked state");
    actor.source_equals(&old.id, &case.path(0), BODY0).await?;
    missing(&mut actor, &case, 1).await?;
    actor.release(&mut lease).await?;
    ensure!(active_page(&mut actor, &case, 0, &old.id, BODY0).await?
        == old.revision, "fault lock conflict revision");
    Ok(())
}

///
/// R03の初回source保持中だけ別の直接HTTPS actorを完了させる。
///
/// # 概要
/// actor完了と元CLI回収を別futureで待ち、例外を両方へ伝える。
///
/// # 引数
/// * `environment` - 承認済み実設定
///
/// # 戻り値
/// actor版維持、元CLI競合、自己既存lockの限定cleanup。
///
async fn real_race(environment: &Environment) -> Result<()> {
    /*
     * seed旧事実と初回source保留までの完全要求列を準備する
     */
    let case = prepared(environment, "r03", 2).await?;
    let old = seed(environment, &case, 0).await?;
    let input = case.archive(&[BODY1, BODY1])?;
    let actor_input = case.archive(&[ACTOR])?;
    let (mut rows, _) = existing(&case, false, BODY1);
    rows.truncate(8);
    rows.push(row(0, Op::Unlock));
    let relay = real_relay(environment, &case, 2, Some((0, &old.id)), rows,
        vec![Fault { at: 2, kind: FaultKind::Hold }])?;
    let mut command = match case.command(&input, "update", false, false) {
        Ok(command) => command,
        Err(error) => {
            relay.control().close();
            let _joined = tokio::task::spawn_blocking(move ||
                relay.finish()).await;
            return Err(error);
        }
    };
    relay.configure_child(&mut command);
    /*
     * actorの例外でも最初にgateを閉じ元CLIを回収してからrelayをjoinする
     */
    let actor = async {
        let result: Result<()> = async {
            /*
             * 元応答を保持したままactorの通常更新完了を待つ
             */
            relay.control().wait_reached(2).await?;
            let report = direct(environment, &case, &actor_input, "update")
                .await?;
            let mut changed = page(&case, 0, "Updated", "Finished");
            let mut facts = RecoveryExpectation::new(Some(&old.id),
                Some(old.revision), None, false, "Finished", "Applied");
            facts.old_lock = Some(true);
            changed.recovery = Some(facts);
            report.expect(&FaultExpectation::new(0, true,
                [0, 0, 1, 0, 0, 0, 0]).page(changed))?;
            relay.control().release_hold(2)?;
            Ok(())
        }.await;
        if result.is_err() { relay.control().abort(); }
        result
    };
    let (captured, actor_result) = tokio::join!(
        child(command, Some(relay.control())), actor);
    let evidence = tokio::task::spawn_blocking(move || relay.finish()).await
        .map_err(|_| anyhow!("fault race relay join"))??;
    let captured = captured?;
    evidence.check_output(&captured.stdout)?;
    evidence.check_output(&captured.stderr)?;
    environment.check_output(&captured.stdout)?;
    environment.check_output(&captured.stderr)?;
    private_output(&captured)?;
    actor_result?;
    evidence.verify()?;
    let report = FaultReport::parse(&captured)?;
    eprintln!("{}", report.safe_record());
    let mut first = failed(&case, "Conflict", "Recheck", "ReleaseLock",
        DetailExpectation::local("Conflict", Some("Recheck")));
    first.execution = Some(ExecutionExpectation {
        cleanup_attempted: true, stop: false });
    let mut facts = with_old(recovery(true, None, false, "Recheck",
        "Unknown"), &old);
    facts.old_lock = Some(true);
    facts.advice = Some("ResolveConflict".to_owned());
    first.recovery = Some(facts);
    report.expect(&stopped(&case, first, 0))?;
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    ensure!(active_page(&mut observer, &case, 0, &old.id, ACTOR).await?
        == old.revision.checked_add(1)
            .ok_or_else(|| anyhow!("fault revision overflow"))?,
        "fault actor revision");
    missing(&mut observer, &case, 1).await
}

///
/// 実PUT適用後の500/喪失と通常後続を確認する。
///
/// # 概要
/// 新規とupdateを分け、目的一致と後続正常適用を独立観測する。
///
/// # 引数
/// * `environment` - 承認済み実設定
/// * `update` - seed済みupdateで応答を喪失させるか
///
/// # 戻り値
/// 警告適用、単回照合と正常後続。
///
async fn real_put(environment: &Environment, update: bool) -> Result<()> {
    /*
     * seed有無と上流正常PUT完了後の故障位置を固定する
     */
    let case = prepared(environment, if update { "r05" } else { "r04" }, 2)
        .await?;
    let old = if update { Some(seed(environment, &case, 0).await?) }
        else { None };
    let input = case.archive(&[BODY1, BODY1])?;
    let (mut rows, _) = if update { existing(&case, false, BODY1) }
        else { creation(0, NEW, BODY1) };
    let at = rows.len() - 1;
    rows.extend([row(0, Op::Lookup), row(0, Op::Meta), row(0, Op::Source)]);
    rows.extend(creation(1, LATER, BODY1).0);
    let relay = real_relay(environment, &case, 2,
        old.as_ref().map(|value| (0, value.id.as_str())), rows,
        vec![Fault { at, kind: if update { FaultKind::Disconnect }
            else { FaultKind::Http500 } }])?;
    let (_, evidence, report) = run(&case, relay, &input, "update", false,
        false).await?;
    /*
     * 旧事実と正常POST確定IDだけから警告適用の完全期待を作る
     */
    let id = match &old { Some(value) => value.id.clone(),
        None => draft(&report, &case, 0)? };
    let later = draft(&report, &case, 1)?;
    ensure!(evidence.created_id(1) == Some(later.as_str())
        && (update || evidence.created_id(0) == Some(id.as_str())),
        "fault POST identity evidence");
    let mut first = page(&case, 0, if update {
        "AppliedWithWarning { action: Update }"
    } else { "AppliedWithWarning { action: Create }" }, "Reconcile");
    let mut facts = RecoveryExpectation::new(old.as_ref().map(|v|
        v.id.as_str()), old.as_ref().map(|v| v.revision),
        if update { None } else { Some(&id) }, false, "Reconcile", "Applied");
    facts.old_lock = if update { Some(false) } else { None };
    facts.lock = Some(false);
    facts.path_state = "Found".to_owned();
    facts.path_id = Some(id.clone());
    first.recovery = Some(facts);
    first.warnings.push(warning(false, if update { None }
        else { Some(500) }, if update { "Transport" } else { "Http" }, &id));
    report.expect(&FaultExpectation::new(3, true,
        [if update { 1 } else { 2 }, 0, u64::from(update), 0, 1, 0, 0])
        .page(first).page(created(&case, 1, &later)))?;
    /*
     * CLIとは別期限のHTTPSで両ページとupdateのrevision増分を確認する
     */
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    let after = active_page(&mut observer, &case, 0, &id, BODY1).await?;
    if let Some(old) = old {
        ensure!(after == old.revision.checked_add(1)
            .ok_or_else(|| anyhow!("fault revision overflow"))?,
            "fault update revision");
    }
    active_page(&mut observer, &case, 1, &later, BODY1).await?;
    Ok(())
}

///
/// 実新規POST喪失のIDをCLIに補完せず一回だけ観測する。
///
/// # 引数
/// * `environment` - 承認済み実設定
///
/// # 戻り値
/// CLI新ID未知、後続要求0、relay専用IDのmeta一回。
///
async fn real_post_lost(environment: &Environment) -> Result<()> {
    /*
     * POST上流201後だけ切断し、製品にはIDを補完しない
     */
    let case = prepared(environment, "r09", 2).await?;
    let input = case.archive(&[BODY1, BODY1])?;
    let relay = real_relay(environment, &case, 2, None,
        vec![row(0, Op::Lookup), row(0, Op::Create)],
        vec![Fault { at: 1, kind: FaultKind::Disconnect }])?;
    let (_, evidence, report) = run(&case, relay, &input, "error", false,
        false).await?;
    let mut first = failed(&case, "Uncertain", "CreateDraft", "CreateDraft",
        detail("CreateDraft", "Transport", "CreateDraft", None, None));
    first.recovery = Some(recovery(false, None, false, "CreateDraft",
        "Unknown"));
    report.expect(&stopped(&case, first, 0))?;
    let id = evidence.created_id(0)
        .ok_or_else(|| anyhow!("fault relay confirmed ID missing"))?;
    draft_once(environment, &case, 0, id, true).await
}

///
/// DELETE代表の完全要求列と実上流故障だけを構築する。
///
/// # 概要
/// 完全500だけに再作成列を許し、期限と喪失では後続列を持たせない。
///
/// # 引数
/// * `case` - 合成領域
/// * `number` - R06～R08/R10～R12/R14
///
/// # 戻り値
/// 実上流statusを変えない要求列と下流注入計画。
///
fn real_delete_plan(case: &Case, number: u8) -> (Vec<Row>, Vec<Fault>) {
    let body = if number == 12 { INVALID } else { BODY1 };
    let (mut rows, _) = existing(case, true, body);
    let kind = match number {
        7 => FaultKind::Empty500,
        8 => FaultKind::Disconnect,
        14 => FaultKind::Hold,
        _ => FaultKind::Http500,
    };
    let mut faults = vec![Fault { at: 9, kind }];
    /*
     * 期限代表だけは再照合を一件も許可しない
     */
    if number != 14 {
        rows.extend([row(0, Op::Meta), row(0, Op::Lookup)]);
    }
    match number {
        6 | 10..=12 => {
            /*
             * 再作成POST以後は故障した段階で要求列を打ち切る
             */
            rows.push(row(0, Op::Create));
            if number == 10 {
                faults.push(Fault { at: 12, kind: FaultKind::Disconnect });
            } else {
                rows.push(row(0, Op::Renew));
                if number == 11 {
                    faults.push(Fault { at: 13,
                        kind: FaultKind::Synthetic403 });
                } else {
                    rows.push(row(0, Op::Put).body(body)
                        .statuses(if number == 12 { &[400] } else { &[204] }));
                }
            }
            if number == 6 { rows.extend(creation(1, LATER, BODY1).0); }
        }
        _ => {}
    }
    (rows, faults)
}

///
/// 旧削除後の復旧事実を上流観測と独立seedから作る。
///
/// # 引数
/// * `old` - 独立seed旧事実
/// * `id` - 製品が知った場合だけの新ID
/// * `stage` - 元失敗段階
/// * `observed` - 期待した最後の観測状態
/// * `history` - 上流path不存在から決めた履歴
///
/// # 戻り値
/// DELETE照合後の失敗に共通する完全復旧期待。
///
fn deleted_recovery(old: &Seed, id: Option<&str>, stage: &str,
    observed: &str, history: String) -> RecoveryExpectation
{
    let mut facts = with_old(recovery(true, id, true, stage, observed), old);
    facts.old_lock = Some(false);
    facts.path_state = history;
    facts
}

///
/// DELETE500/不確定/再作成失敗/期限の実代表を確認する。
///
/// # 概要
/// 正常再作成、再作成不可、POST喪失、renew拒否、実PUT400を分ける。
///
/// # 引数
/// * `environment` - 承認済み実設定
/// * `number` - 固定実代表番号
///
/// # 戻り値
/// 上流適用、下流異常、復旧事実と残存状態の独立確認。
///
async fn real_delete(environment: &Environment, number: u8) -> Result<()> {
    let case = prepared(environment, &format!("r{number:02}"), 2).await?;
    let old = seed(environment, &case, 0).await?;
    let input = case.archive(&[if number == 12 { INVALID } else { BODY1 },
        BODY1])?;
    let (rows, faults) = real_delete_plan(&case, number);
    let relay = real_relay(environment, &case, 2, Some((0, &old.id)), rows,
        faults)?;
    let (_, evidence, report) = run(&case, relay, &input, "overwrite",
        number == 14, false).await?;
    /*
     * CLI未確定のPOST IDはこの期待構築へ渡さない
     */
    let new = if matches!(number, 6 | 11 | 12) {
        Some(draft(&report, &case, 0)?)
    } else { None };
    if let Some(id) = &new {
        ensure!(id != &old.id && evidence.created_id(0) == Some(id.as_str()),
            "fault recreation ID evidence");
    }
    if number == 14 {
        report.expect(&deadline_expectation(&case, &old.id, old.revision))?;
    } else {
        let history = absent_history(&evidence, 11)?;
        let expected = delete_result(&case, number, &old, new.as_deref(),
            history, &report)?;
        report.expect(&expected)?;
    }
    /*
     * relayを全回収した後に別300秒期限のobserverを作る
     */
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    deleted_page(&mut observer, &case, 0, &old).await?;
    if number == 6 {
        let id = new.as_deref().ok_or_else(|| anyhow!("fault new ID"))?;
        active_page(&mut observer, &case, 0, id, BODY1).await?;
        let later = draft(&report, &case, 1)?;
        ensure!(evidence.created_id(1) == Some(later.as_str()),
            "fault later ID evidence");
        active_page(&mut observer, &case, 1, &later, BODY1).await?;
    } else if number != 10 {
        missing(&mut observer, &case, 0).await?;
        missing(&mut observer, &case, 1).await?;
    }
    /*
     * POST喪失の新IDはmeta一回以外の観測・操作へ渡さない
     */
    if number == 10 {
        let id = evidence.created_id(0)
            .ok_or_else(|| anyhow!("fault relay confirmed ID missing"))?;
        draft_once(environment, &case, 0, id, true).await?;
    } else if let Some(id) = new.filter(|_| number != 6) {
        draft_once(environment, &case, 0, &id, false).await?;
    }
    Ok(())
}

///
/// 実DELETE代表の全結果を独立事実から完全に期待する。
///
/// # 概要
/// 正常再作成と停止を分け、元操作・先行警告・旧新情報を比較する。
///
/// # 引数
/// * `case` - 合成領域
/// * `number` - 固定代表番号
/// * `old` - 独立seed旧事実
/// * `new` - CLIが正常POSTで確定した場合だけの新ID
/// * `history` - 上流のpath不存在観測
/// * `report` - 後続の動的新ID取得だけに使う有限報告
///
/// # 戻り値
/// 七集計・警告・主原因・復旧・全page順序の完全期待。
///
fn delete_result(case: &Case, number: u8, old: &Seed, new: Option<&str>,
    history: String, report: &FaultReport) -> Result<FaultExpectation>
{
    /*
     * 完全500後の正常再作成だけは通常後続まで完了する
     */
    if number == 6 {
        let mut first = page(case, 0,
            "AppliedWithWarning { action: Recreate }", "Finished");
        let mut facts = RecoveryExpectation::new(Some(&old.id),
            Some(old.revision), new, true, "Finished", "Applied");
        facts.old_lock = Some(false);
        facts.path_state = history;
        first.recovery = Some(facts);
        first.warnings.push(warning(true, Some(500), "Http", &old.id));
        return Ok(FaultExpectation::new(3, true, [1, 0, 0, 1, 1, 0, 0])
            .page(first).page(created(case, 1, &draft(report, case, 1)?)));
    }
    /*
     * 停止代表の元操作と観測statusを固定し、未確定IDはNoneに保つ
     */
    let (category, stage, operation, kind, status, id) = match number {
        7 => ("Uncertain", "DeleteOld", "DeletePage", "Protocol", Some(500),
            Some(old.id.as_str())),
        8 => ("Uncertain", "DeleteOld", "DeletePage", "Transport", None,
            Some(old.id.as_str())),
        10 => ("Uncertain", "CreateDraft", "CreateDraft", "Transport", None,
            None),
        11 => ("Permission", "PutSource", "RenewLock", "Http", Some(403),
            new),
        12 => ("Rejected", "PutSource", "PutSource", "Http", Some(400),
            new),
        _ => return Err(anyhow!("fault delete expectation number")),
    };
    let primary = if number == 12 {
        syntax_detail(new.ok_or_else(|| anyhow!("fault syntax ID"))?)
    } else { detail(operation, kind, stage, status, id) };
    let mut first = failed(case, category, stage,
        if number < 9 { "Reconcile" } else { stage }, primary);
    let mut facts = deleted_recovery(old, new, stage, "Deleted", history);
    if number < 9 { facts.lock = Some(false); }
    first.recovery = Some(facts);
    first.warnings.push(warning(true, if number == 8 { None }
        else { Some(500) }, if number == 7 { "Protocol" }
        else if number == 8 { "Transport" } else { "Http" }, &old.id));
    Ok(stopped(case, first, 1))
}

///
/// 先行成功つき再作成PUT500を最終/後続ありで別caseにする。
///
/// # 概要
/// 独立停止をFailedへ換算せず、警告付き再作成と残作業を比較する。
///
/// # 引数
/// * `environment` - 承認済み実設定
///
/// # 戻り値
/// 目的一致後も安全停止exit1、元警告順と後続0。
///
async fn real_recreation_stop(environment: &Environment) -> Result<()> {
    for later in [false, true] {
        /*
         * a先行成功、b再作成、任意c未実行の全対象を事前確認する
         */
        let count = if later { 3 } else { 2 };
        let case = prepared(environment, if later { "r13-later" }
            else { "r13-final" }, count).await?;
        let old = seed(environment, &case, 1).await?;
        let input = case.archive(&vec![BODY1; count])?;
        let (mut rows, _) = creation(0, NEW, BODY1);
        let (mut middle, _) = existing(&case, true, BODY1);
        middle.extend([row(0, Op::Meta), row(0, Op::Lookup),
            row(0, Op::Create), row(0, Op::Renew),
            row(0, Op::Put).body(BODY1), row(0, Op::Lookup),
            row(0, Op::Meta), row(0, Op::Source)]);
        for row in &mut middle { row.page = 1; }
        rows.extend(middle);
        let relay = real_relay(environment, &case, count,
            Some((1, &old.id)), rows, vec![Fault { at: 13,
                kind: FaultKind::Http500 }, Fault { at: 18,
                kind: FaultKind::Http500 }])?;
        let (_, evidence, report) = run(&case, relay, &input, "overwrite",
            false, false).await?;
        let first = draft(&report, &case, 0)?;
        let new = draft(&report, &case, 1)?;
        ensure!(evidence.created_id(0) == Some(first.as_str())
            && evidence.created_id(1) == Some(new.as_str())
            && new != old.id, "fault recreation known IDs");
        /*
         * 二警告と独立停止を適用一件・失敗0として完全比較する
         */
        let mut middle = page(&case, 1,
            "AppliedWithWarning { action: Recreate }", "Reconcile");
        let mut facts = RecoveryExpectation::new(Some(&old.id),
            Some(old.revision), Some(&new), true, "Reconcile", "Applied");
        facts.old_lock = Some(false);
        facts.lock = Some(false);
        facts.path_state = "Found".to_owned();
        facts.path_id = Some(new.clone());
        facts.advice = Some("InspectBeforeRetry".to_owned());
        middle.recovery = Some(facts);
        middle.execution = Some(ExecutionExpectation {
            cleanup_attempted: false, stop: true });
        middle.warnings.extend([warning(true, Some(500), "Http", &old.id),
            warning(false, Some(500), "Http", &new)]);
        let mut expected = FaultExpectation::new(1, true,
            [1, 0, 0, 1, 2, 0, u64::from(later)])
            .page(created(&case, 0, &first)).page(middle);
        if later { expected.pages.push(page(&case, 2, "NotExecuted",
            "Preflight")); }
        report.expect(&expected)?;
        /*
         * 上流適用後の旧履歴・新本文・通常先行成功を別HTTPSで確認する
         */
        let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
        active_page(&mut observer, &case, 0, &first, BODY1).await?;
        deleted_page(&mut observer, &case, 1, &old).await?;
        active_page(&mut observer, &case, 1, &new, BODY1).await?;
        if later { missing(&mut observer, &case, 2).await?; }
    }
    Ok(())
}
