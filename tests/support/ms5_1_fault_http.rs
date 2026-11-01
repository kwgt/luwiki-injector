/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! G6の完全要求列に閉じたloopback relayと有限の障害証拠
//!

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::future::{Future, poll_fn};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener as StdListener, TcpStream as StdStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, ensure};
use reqwest::Method;
use reqwest::header::HeaderValue;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::real_cli_http::{MS51_ROOT, RelayGrant, isolate_child, parse_lock,
    valid_id, within};

///
/// 外部文字列を保持しない固定REST操作。
///
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    /// pathからの有効ID照会。
    Lookup,
    /// 新draftの作成。
    Create,
    /// 既知IDのmetadata。
    Meta,
    /// 既知IDの本文取得。
    Source,
    /// 既存lockの取得。
    Acquire,
    /// 同IDのlock延長。
    Renew,
    /// 合成本文の単回登録。
    Put,
    /// 既知pageの非再帰削除。
    Delete,
    /// 自己既存lockの明示解除。
    Unlock,
}

impl Op {
    ///
    /// 固定操作を上流methodとsuffixへ変換する。
    ///
    /// # 戻り値
    /// 固定method、suffix、ID操作の有無
    ///
    fn route(self) -> (Method, Option<&'static str>, bool) {
        match self {
            Self::Lookup => (Method::GET, None, false),
            Self::Create => (Method::POST, None, false),
            Self::Meta => (Method::GET, Some("meta"), true),
            Self::Source => (Method::GET, Some("source"), true),
            Self::Acquire => (Method::POST, Some("lock"), true),
            Self::Renew => (Method::PUT, Some("lock"), true),
            Self::Put => (Method::PUT, Some("source"), true),
            Self::Delete => (Method::DELETE, None, true),
            Self::Unlock => (Method::DELETE, Some("lock"), true),
        }
    }

    ///
    /// 独立lock認証を必要とする操作を判定する。
    ///
    /// # 戻り値
    /// 自己leaseでの操作ならtrue
    ///
    fn locked(self) -> bool {
        matches!(self, Self::Renew | Self::Put | Self::Delete | Self::Unlock)
    }
}

///
/// caseが所有するpageと独立事前確認済みID。
///
pub struct RelayPage {
    /// case真子孫の完全path。表示しない。
    pub path: String,
    /// 事前に確定したID。新規pageはNone。
    pub known_id: Option<String>,
}

///
/// 一回だけ許可する完全要求のslot。
///
pub struct Row {
    /// 計画内のpage index。
    pub page: usize,
    /// そのpageに許可する固定操作。
    pub op: Op,
    /// Putだけの既知合成本文。
    body: Option<Vec<u8>>,
    /// 実上流で許可するstatus。下流注入statusとは別。
    statuses: Vec<u16>,
}

impl Row {
    ///
    /// 正常statusを期待する単回slotを作る。
    ///
    /// # 引数
    /// * `page` - page index
    /// * `op` - 固定操作
    ///
    /// # 戻り値
    /// Put本文はbodyで明示設定するslot
    ///
    pub fn new(page: usize, op: Op) -> Self {
        let statuses = match op {
            Op::Lookup => vec![200, 404, 410],
            Op::Create => vec![201],
            Op::Meta | Op::Source => vec![200],
            _ => vec![204],
        };
        Self { page, op, body: None, statuses }
    }

    ///
    /// Putの期待bytesをそのまま所有する。
    ///
    /// # 引数
    /// * `body` - 試験所有の合成bytes
    ///
    /// # 戻り値
    /// 本文付きslot
    ///
    pub fn body(mut self, body: &[u8]) -> Self {
        self.body = Some(body.to_vec());
        self
    }

    ///
    /// case固有の上流status集合を明示する。
    ///
    /// # 引数
    /// * `statuses` - caseが予定する数値status
    ///
    /// # 戻り値
    /// status集合を限定したslot
    ///
    pub fn statuses(mut self, statuses: &[u16]) -> Self {
        self.statuses = statuses.to_vec();
        self
    }
}

///
/// 下流だけへ一度注入する障害。外部値を保持しない。
///
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultKind {
    /// 完全JSONのHTTP500。
    Http500,
    /// 空本文のHTTP500。
    Empty500,
    /// statusを送らず切断。
    Disconnect,
    /// 完全上流応答を保持し、明示解放または停止を待つ。
    Hold,
    /// Renewだけを上流へ送らず合成403。
    Synthetic403,
}

///
/// 0始まりslotへ結合した一回だけの障害。
///
pub struct Fault {
    /// 計画のrow index。
    pub at: usize,
    /// 注入する固定障害。
    pub kind: FaultKind,
}

///
/// 任意proxyを許さないcase単位の完全計画。
///
pub struct RelayPlan {
    /// MS51配下のcase真子孫領域。
    pub prefix: String,
    /// 完全一致で所有するpage集合。
    pub pages: Vec<RelayPage>,
    /// 許可する要求の完全な順序。
    pub rows: Vec<Row>,
    /// 同じrowへ二重に設定しない注入点。
    pub faults: Vec<Fault>,
}

impl RelayPlan {
    ///
    /// listener作成前に計画の完全性を検査する。
    ///
    /// # 戻り値
    /// 有効な合成計画、またはHTTPなしの拒否
    ///
    fn validate(&self) -> Result<()> {
        ensure!(within(&self.prefix, MS51_ROOT), "fault case region");
        let mut paths = BTreeSet::new();
        let mut ids = BTreeSet::new();
        /*
         * 隣接prefixや重複page/IDを計画開始前に拒否する
         */
        for page in &self.pages {
            ensure!(within(&page.path, &self.prefix)
                && paths.insert(&page.path), "fault page region");
            if let Some(id) = &page.known_id {
                ensure!(canonical_id(id) && ids.insert(id), "fault page ID");
            }
        }
        for row in &self.rows {
            ensure!(row.page < self.pages.len()
                && (row.op == Op::Put) == row.body.is_some()
                && !row.statuses.is_empty()
                && row.statuses.iter().all(|v| (100..=599).contains(v)),
                "fault row");
        }
        /*
         * 転送前拒否と上流適用後の障害を操作段階へ閉じる
         */
        let mut faults = BTreeSet::new();
        for fault in &self.faults {
            let row = self.rows.get(fault.at)
                .ok_or_else(|| anyhow!("fault index"))?;
            ensure!(faults.insert(fault.at), "fault duplicate index");
            ensure!(match fault.kind {
                FaultKind::Synthetic403 => row.op == Op::Renew,
                FaultKind::Hold => matches!(row.op,
                    Op::Source | Op::Create | Op::Put | Op::Delete),
                _ => matches!(row.op, Op::Create | Op::Put | Op::Delete),
            }, "fault operation");
        }
        Ok(())
    }
}

///
/// 合成応答。実資格情報を受け取るAPIではない。
///
pub struct Reply {
    /// 数値status。
    status: u16,
    /// 試験所有の合成応答bytes。
    body: Vec<u8>,
    /// 合成lock token。生値は記録しない。
    token: Option<String>,
    /// 宣言長を1増やして切断するか。
    incomplete: bool,
    /// headerを送らず切断するか。
    disconnect: bool,
}

impl Reply {
    ///
    /// 完全な合成応答を作る。
    ///
    /// # 引数
    /// * `status` - 固定数値status
    /// * `body` - 固定合成bytes
    ///
    /// # 戻り値
    /// tokenを持たない合成応答
    ///
    pub fn new(status: u16, body: &[u8]) -> Self {
        Self { status, body: body.to_vec(), token: None,
            incomplete: false, disconnect: false }
    }

    ///
    /// 合成lock headerを付ける。
    ///
    /// # 引数
    /// * `token` - 通常試験だけの固定dummy値
    ///
    /// # 戻り値
    /// 有効期限を実行時に設定する応答
    ///
    pub fn locked(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }

    ///
    /// 宣言した長さの前に切断する。
    ///
    /// # 戻り値
    /// 不完全応答
    ///
    pub fn incomplete(mut self) -> Self {
        self.incomplete = true;
        self
    }

    ///
    /// statusなしで切断する合成応答を作る。
    ///
    /// # 戻り値
    /// 上流へ接続しない合成切断
    ///
    pub fn disconnected() -> Self {
        Self { disconnect: true, ..Self::new(0, b"") }
    }
}

///
/// 秘密や任意pathを保持しない一要求分の有限証拠。
///
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event {
    /// 0始まりの許可row。
    pub ordinal: usize,
    /// 0始まりのpage alias。
    pub page: usize,
    /// 固定REST操作。
    pub op: Op,
    /// 実/合成上流のsendを開始したか。wire量ではない。
    pub forwarded: bool,
    /// 上流で観測したstatus。header未観測/転送前拒否はNone。
    pub status: Option<u16>,
    /// 正常上流完了後に注入した障害。
    pub fault: Option<FaultKind>,
}

///
/// 同期停止gateとasync待合を共有する制御値。
///
#[derive(Clone)]
pub struct SharedControl {
    /// 秘密を持たないgateの所有者。
    inner: Arc<Control>,
}

///
/// 転送開始の直前検査と待合通知を一つに所有する。
///
struct Control {
    /// 停止と注入点の有限状態。
    state: Mutex<Gate>,
    /// 停止/到達/解放の通知。
    changed: Notify,
}

///
/// mutex下で確定する停止・到達状態。
///
struct Gate {
    /// 新しい転送を開始してよいか。
    open: bool,
    /// 期待外や異常監視により失敗したか。
    failed: bool,
    /// 完全上流受信後に到達したrow。
    reached: BTreeSet<usize>,
    /// actor完了後に解放した保留row。
    released: BTreeSet<usize>,
}

impl SharedControl {
    ///
    /// 開いたgateを生成する。
    ///
    /// # 戻り値
    /// listenerとrunnerだけが共有する停止制御
    ///
    fn new() -> Self {
        Self { inner: Arc::new(Control { state: Mutex::new(Gate {
            open: true, failed: false, reached: BTreeSet::new(),
            released: BTreeSet::new() }), changed: Notify::new() }) }
    }

    ///
    /// 最初に新規forwardを禁止し、全非同期待機を起こす。
    ///
    /// # 戻り値
    /// なし。回収の完了はRelay::finishで待つ。
    ///
    pub fn close(&self) {
        self.inner.state.lock().unwrap_or_else(|v| v.into_inner()).open =
            false;
        self.inner.changed.notify_waiters();
    }

    ///
    /// 期待外をstickyに記録してgateを閉じる。
    ///
    /// # 戻り値
    /// なし。既存上流の未適用を意味しない。
    ///
    fn fail(&self) {
        let mut gate = self.inner.state.lock()
            .unwrap_or_else(|v| v.into_inner());
        gate.failed = true;
        gate.open = false;
        drop(gate);
        self.inner.changed.notify_waiters();
    }

    ///
    /// actor/runner失敗をstickyにし、captureの即時回収を促す。
    ///
    /// # 戻り値
    /// なし。既存上流の未適用を意味しない。
    ///
    pub fn abort(&self) { self.fail(); }

    ///
    /// runnerの監視用に失敗の有無だけを返す。
    ///
    /// # 戻り値
    /// 期待外を一度でも観測したならtrue
    ///
    pub fn failed(&self) -> bool {
        self.inner.state.lock().unwrap_or_else(|v| v.into_inner()).failed
    }

    ///
    /// 同期した新規send直前のgateを確認する。
    ///
    /// # 戻り値
    /// 転送を開始できるならtrue
    ///
    fn open(&self) -> bool {
        self.inner.state.lock().unwrap_or_else(|v| v.into_inner()).open
    }

    ///
    /// gate閉鎖と排他に、次の単回送信開始を確定する。
    ///
    /// # 引数
    /// * `event` - 完全検査済みslotの有限証拠
    ///
    /// # 戻り値
    /// 停止前の送信開始、または転送0の拒否
    ///
    fn begin(&self, event: &mut Event) -> Result<()> {
        let gate = self.inner.state.lock()
            .unwrap_or_else(|v| v.into_inner());
        ensure!(gate.open && !gate.failed, "relay closed at send");
        event.forwarded = true;
        Ok(())
    }

    ///
    /// reqwest futureのpollをgate閉鎖と排他にして送信する。
    ///
    /// # 概要
    /// listenerでのslot検査後にも同期spoolやrequest準備があるため、
    /// 最初のsend pollまで停止gateを再確認する。以後のpollも閉鎖を
    /// 拒否してinflightを取消し、futureやresponseを残さない。
    ///
    /// # 引数
    /// * `event` - 単回slotの有限送信証拠
    /// * `request` - 固定HTTPSだけを呼ぶ未pollのfuture
    ///
    /// # 戻り値
    /// 単発応答、または上流futureを破棄する固定停止
    ///
    async fn forward<T>(&self, event: &mut Event,
        request: impl Future<Output = Result<T>>) -> Result<T>
    {
        tokio::pin!(request);
        poll_fn(|context| {
            let gate = self.inner.state.lock()
                .unwrap_or_else(|v| v.into_inner());
            if !gate.open || gate.failed {
                return Poll::Ready(Err(anyhow!("relay stopped at poll")));
            }
            event.forwarded = true;
            let result = request.as_mut().poll(context);
            drop(gate);
            result
        }).await
    }

    ///
    /// 完全受信した注入点を記録する。
    ///
    /// # 引数
    /// * `at` - row index
    ///
    /// # 戻り値
    /// なし。raw応答を共有しない。
    ///
    fn reached(&self, at: usize) {
        self.inner.state.lock().unwrap_or_else(|v| v.into_inner())
            .reached.insert(at);
        self.inner.changed.notify_waiters();
    }

    ///
    /// 完全上流受信後の指定点を有限の試験期限内で待つ。
    ///
    /// # 引数
    /// * `at` - row index
    ///
    /// # 戻り値
    /// 到達確認、または値を含まない停止/試験期限
    ///
    pub async fn wait_reached(&self, at: usize) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        /*
         * 通知を登録してから状態を確認し取りこぼしを防ぐ
         */
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let gate = self.inner.state.lock()
                    .unwrap_or_else(|v| v.into_inner());
                if gate.reached.contains(&at) { return Ok(()); }
                ensure!(gate.open && !gate.failed, "relay point unavailable");
            }
            tokio::time::timeout_at(deadline, changed).await
                .map_err(|_| anyhow!("relay point deadline"))?;
        }
    }

    ///
    /// 正常なactor完了後に指定保留応答だけを解放する。
    ///
    /// # 引数
    /// * `at` - 到達済みrow index
    ///
    /// # 戻り値
    /// 一回の解放、または停止/重複拒否
    ///
    pub fn release_hold(&self, at: usize) -> Result<()> {
        let mut gate = self.inner.state.lock()
            .unwrap_or_else(|v| v.into_inner());
        ensure!(gate.open && gate.reached.contains(&at)
            && gate.released.insert(at), "relay hold release");
        drop(gate);
        self.inner.changed.notify_waiters();
        Ok(())
    }

    ///
    /// 停止、または指定保留の解放を待つ。
    ///
    /// # 引数
    /// * `release` - 解放も待つ場合のrow index
    ///
    /// # 戻り値
    /// 解放時true、停止時false
    ///
    async fn changed_until(&self, release: Option<usize>) -> bool {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let gate = self.inner.state.lock()
                    .unwrap_or_else(|v| v.into_inner());
                if !gate.open { return false; }
                if release.is_some_and(|at| gate.released.contains(&at)) {
                    return true;
                }
            }
            changed.await;
        }
    }
}

///
/// 専用listener/runtime/threadをRAIIで所有するrelay。
///
pub struct Relay {
    /// このlistenerだけの公開loopback URL。
    url: String,
    /// 非公開のrun専用合成Bearer。
    bearer: String,
    /// capture監視と共有する停止gate。
    control: SharedControl,
    /// 全I/Oと秘密所有者を回収する専用thread。
    task: Option<JoinHandle<Result<State>>>,
}

impl Relay {
    ///
    /// 実上流なしで同じ要求gateを使用する。
    ///
    /// # 引数
    /// * `plan` - 合成caseの完全計画
    /// * `replies` - rowごとの合成応答
    ///
    /// # 戻り値
    /// 独立runtimeの合成relay
    ///
    pub fn scripted(plan: RelayPlan, replies: Vec<Reply>) -> Result<Self> {
        ensure!(plan.rows.len() == replies.len(), "script reply count");
        Self::start(plan, Backend::Script(VecDeque::from(replies)))
    }

    ///
    /// 既存固定HTTPSへの限定grantを一回だけ消費する。
    ///
    /// # 引数
    /// * `grant` - 実Bearerを外部へ公開しない固定権限
    /// * `plan` - 合成入力と完全要求列
    ///
    /// # 戻り値
    /// 上流だけに実Bearerを設定するrelay
    ///
    pub fn real(grant: RelayGrant, plan: RelayPlan) -> Result<Self> {
        let paths: Vec<_> = plan.pages.iter().map(|v| v.path.clone())
            .collect();
        let known: Vec<_> = plan.pages.iter().filter_map(|v|
            v.known_id.as_ref().map(|id| (id.clone(), v.path.clone())))
            .collect();
        grant.verify(&plan.prefix, &paths, &known)?;
        Self::start(plan, Backend::Real(grant))
    }

    ///
    /// 合成認証・listener・専用runtimeを順番に準備する。
    ///
    /// # 概要
    /// 失敗時は子を起動せず、作成済みlistener/秘密を所有者へ戻す。
    /// threadの外部エラーは固定分類だけにする。
    ///
    /// # 引数
    /// * `plan` - 検証するcase計画
    /// * `backend` - 合成または限定HTTPS上流
    ///
    /// # 戻り値
    /// listenerとthreadを所有するrelay
    ///
    fn start(plan: RelayPlan, backend: Backend) -> Result<Self> {
        plan.validate()?;
        let bearer = random_bearer()?;
        let listener = StdListener::bind("127.0.0.1:0")
            .map_err(|_| anyhow!("relay listener"))?;
        listener.set_nonblocking(true)
            .map_err(|_| anyhow!("relay listener mode"))?;
        let address = listener.local_addr()
            .map_err(|_| anyhow!("relay listener address"))?;
        let control = SharedControl::new();
        let runtime_control = control.clone();
        let state = State::new(plan, backend, bearer.clone(),
            address.to_string());
        /*
         * current-thread製品試験から独立してI/Oを進める
         */
        let task = thread::Builder::new().spawn(move || {
            /*
             * runtime準備失敗もcaptureへstickyに通知する
             */
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all().build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    runtime_control.fail();
                    return Err(anyhow!("relay runtime"));
                }
            };
            /*
             * listener失敗を通知し、全所有状態をjoin先へ返す
             */
            let result = runtime.block_on(run(listener, state,
                runtime_control.clone()));
            if result.is_err() { runtime_control.fail(); }
            result
        }).map_err(|_| anyhow!("relay thread"))?;
        Ok(Self { url: format!("http://{address}"), bearer, control,
            task: Some(task) })
    }

    ///
    /// 公開loopback URLだけを返す。
    ///
    /// # 戻り値
    /// 実Bearerを含まないURL
    ///
    pub fn url(&self) -> &str { &self.url }

    ///
    /// 製品子を合成Bearerだけのloopbackへ限定する。
    ///
    /// # 引数
    /// * `command` - まだ起動していない製品子
    ///
    /// # 戻り値
    /// なし。実Bearerや他認証を子へ渡さない。
    ///
    pub fn configure_child(&self, command: &mut Command) {
        isolate_child(command);
        command.env("LUWIKI_URL", &self.url)
            .env("LUWIKI_TOKEN", &self.bearer);
    }

    ///
    /// capture監視用の制御能力だけを複製する。
    ///
    /// # 戻り値
    /// 秘密getterを持たない停止制御
    ///
    pub fn control(&self) -> SharedControl { self.control.clone() }

    ///
    /// 停止gateを先に閉じて全taskを回収する。
    ///
    /// # 引数
    /// * `self` - このlistener/threadの唯一の所有者
    ///
    /// # 戻り値
    /// 完全回収後だけ解析可能な有限証拠
    ///
    pub fn finish(mut self) -> Result<RelayEvidence> {
        self.control.close();
        let state = self.task.take().ok_or_else(|| anyhow!("relay joined"))?
            .join().map_err(|_| anyhow!("relay worker"))??;
        Ok(RelayEvidence { state, failed: self.control.failed() })
    }
}

impl Drop for Relay {
    ///
    /// 例外経路でも新規forwardを禁止して専用threadを回収する。
    ///
    /// # 戻り値
    /// なし。panic内容や秘密値を表示しない。
    ///
    fn drop(&mut self) {
        self.control.close();
        if let Some(task) = self.task.take() { let _joined = task.join(); }
    }
}

///
/// 実行完了後の有限証拠とprivate認証検査資源。
///
pub struct RelayEvidence {
    /// 秘密を持つためDebug/Serializeを実装しない。
    state: State,
    /// runtimeのsticky失敗。
    failed: bool,
}

impl RelayEvidence {
    ///
    /// 余剰/不足/不成立注入がなく完全列と一致したか検査する。
    ///
    /// # 戻り値
    /// 全slot成立、または固定診断
    ///
    pub fn verify(&self) -> Result<()> {
        ensure!(!self.failed && self.state.rejected == 0
            && self.state.closed_arrivals == 0
            && self.state.events.len() == self.state.plan.rows.len()
            && self.state.completed == self.state.plan.rows.len(),
            "relay sequence incomplete or rejected");
        for fault in &self.state.plan.faults {
            ensure!(self.state.events.get(fault.at)
                .is_some_and(|event| event.fault == Some(fault.kind)),
                "relay injection not reached");
        }
        Ok(())
    }

    ///
    /// 秘密/pathなしの完全要求証拠を借用する。
    ///
    /// # 戻り値
    /// ingressとforwardを区別する有限列
    ///
    pub fn events(&self) -> &[Event] { &self.state.events }

    ///
    /// relayだけが正常201で確定した最新IDを返す。
    ///
    /// # 引数
    /// * `page` - 計画内page index
    ///
    /// # 戻り値
    /// observer専用ID。CLIへ補完しない。
    ///
    pub fn created_id(&self, page: usize) -> Option<&str> {
        self.state.created.get(&page).map(String::as_str)
    }

    ///
    /// 全Bearer/取得延長lock値がcaptureへ露出していないか検査する。
    ///
    /// # 引数
    /// * `text` - 非公開の子stream
    ///
    /// # 戻り値
    /// 非露出、または生断片なしの固定診断
    ///
    pub fn check_output(&self, text: &str) -> Result<()> {
        ensure!(!text.contains(&self.state.bearer), "relay dummy output");
        if let Backend::Real(grant) = &self.state.backend {
            grant.check_output(text)?;
        }
        for value in &self.state.private_values {
            ensure!(value.is_empty() || !text.contains(value),
                "relay lock output");
        }
        Ok(())
    }
}

///
/// 任意HTTP接続先を持たない二種類の上流。
///
enum Backend {
    /// 通常試験だけの合成応答列。
    Script(VecDeque<Reply>),
    /// 固定HTTPSへの非複製能力。
    Real(RelayGrant),
}

///
/// 専用runtime内だけで変更するcase状態。
///
struct State {
    /// 完全許可列。
    plan: RelayPlan,
    /// 実認証を持つ唯一の上流所有者。
    backend: Backend,
    /// ingress認証の比較値。
    bearer: String,
    /// このlistenerの固定Host表記。
    host: String,
    /// 各pageで現在操作できるID。
    current: BTreeMap<usize, String>,
    /// 全既知IDのpath index結合。
    known: BTreeMap<String, usize>,
    /// 正常201で確定したIDだけ。
    created: BTreeMap<usize, String>,
    /// 同IDに最後に返した有効lock認証。
    locks: BTreeMap<String, HeaderValue>,
    /// 全新旧token/認証表現。表示・file保存禁止。
    private_values: Vec<String>,
    /// 完全検証済みingressごとの有限証拠。
    events: Vec<Event>,
    /// 完全上流処理まで到達したrow数。
    completed: usize,
    /// 不正/未知要求の有限件数。
    rejected: usize,
    /// 停止gate閉鎖時に残っていた追加接続。
    closed_arrivals: usize,
}

impl State {
    ///
    /// 事前既知IDだけで実行状態を初期化する。
    ///
    /// # 引数
    /// * `plan` - 検証済み完全計画
    /// * `backend` - 合成/実上流
    /// * `bearer` - run専用合成値
    /// * `host` - listenerのHost表記
    ///
    /// # 戻り値
    /// 未送信の所有状態
    ///
    fn new(plan: RelayPlan, backend: Backend, bearer: String, host: String)
        -> Self
    {
        let current: BTreeMap<_, _> = plan.pages.iter().enumerate()
            .filter_map(|(index, page)| page.known_id.as_ref()
                .map(|id| (index, id.clone()))).collect();
        let known = current.iter().map(|(index, id)| (id.clone(), *index))
            .collect();
        Self { plan, backend, bearer, host, current, known,
            created: BTreeMap::new(), locks: BTreeMap::new(),
            private_values: Vec::new(), events: Vec::new(), completed: 0,
            rejected: 0, closed_arrivals: 0 }
    }

    ///
    /// 完全応答で確定したIDだけを同じpageへ結び付ける。
    ///
    /// # 引数
    /// * `page` - 検証済みpage index
    /// * `id` - canonical ULID
    /// * `created` - POST201由来か
    ///
    /// # 戻り値
    /// 矛盾のない確定結合
    ///
    fn remember(&mut self, page: usize, id: String, created: bool)
        -> Result<()>
    {
        ensure!(canonical_id(&id) && self.known.get(&id)
            .is_none_or(|prior| *prior == page), "relay response ID");
        if created {
            ensure!(!self.known.contains_key(&id), "relay reused created ID");
        } else if let Some(current) = self.current.get(&page) {
            ensure!(current == &id, "relay lookup ID changed");
        }
        if let Backend::Real(grant) = &mut self.backend {
            grant.bind(&id, &self.plan.pages[page].path)?;
        }
        self.known.insert(id.clone(), page);
        self.current.insert(page, id.clone());
        if created { self.created.insert(page, id); }
        Ok(())
    }
}

///
/// 固定bufferで保存する応答body。header/tokenはfileへ書かない。
///
struct Body {
    /// 0700の所有領域。fileより長く生存する。
    _area: TempDir,
    /// 0600の一時body。raw dumpとして外部へ渡さない。
    file: File,
    /// 完全受信したbody長。
    length: u64,
}

impl Body {
    ///
    /// 応答bodyだけの保護spoolを作る。
    ///
    /// # 戻り値
    /// EOFまで逐次書き込める所有body
    ///
    fn new() -> Result<Self> {
        let area = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700)).tempdir()
            .map_err(|_| anyhow!("relay body area"))?;
        let file = OpenOptions::new().read(true).write(true).create_new(true)
            .mode(0o600).open(area.path().join("body"))
            .map_err(|_| anyhow!("relay body file"))?;
        Ok(Self { _area: area, file, length: 0 })
    }

    ///
    /// 受信chunkを固定幅で一時bodyへ追加する。
    ///
    /// # 引数
    /// * `bytes` - 表示しない応答chunk
    ///
    /// # 戻り値
    /// 完全な書込み、または固定I/O診断
    ///
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        for piece in bytes.chunks(16_384) {
            self.file.write_all(piece).map_err(|_| anyhow!("relay spool"))?;
            self.length = self.length.checked_add(piece.len() as u64)
                .ok_or_else(|| anyhow!("relay body length"))?;
        }
        Ok(())
    }

    ///
    /// 固定schemaのIDだけをEOFまで読んで検証する。
    ///
    /// # 戻り値
    /// canonical ID。未知JSONを成功値へ補完しない。
    ///
    fn identity(&mut self) -> Result<String> {
        self.file.seek(SeekFrom::Start(0))
            .map_err(|_| anyhow!("relay identity seek"))?;
        read_identity(&mut BufReader::new(&mut self.file))
    }
}

///
/// 完全受信した上流応答と、合成不完全化の指示。
///
struct Answer {
    /// 上流の観測済みstatus。
    status: u16,
    /// tokenを含み得るためprivate/sensitiveに保持するheader。
    lock: Option<HeaderValue>,
    /// 完全受信済みbodyだけ。
    body: Body,
    /// 合成試験だけの不完全化。
    incomplete: bool,
    /// 合成試験だけのstatusなし切断。
    disconnect: bool,
}

///
/// gate通過前に完全受信・検査するingress。
///
struct Incoming {
    /// 固定methodの原文。記録しない。
    method: String,
    /// 検証前のtarget。記録しない。
    target: String,
    /// 単一の非公開lock認証。
    lock: Option<HeaderValue>,
    /// 合成PUTの完全bytes。
    body: Vec<u8>,
}

///
/// 独立runtimeでacceptし、停止後の追加接続も失敗記録する。
///
/// # 概要
/// 直列slotの全I/Oを停止gateと競合させ、閉鎖後はforwardせず
/// backlogの到着数だけを保持してlistenerを回収する。
///
/// # 引数
/// * `listener` - このcaseだけのnonblocking listener
/// * `state` - 秘密を含む唯一の実行所有状態
/// * `control` - 同期停止gate
///
/// # 戻り値
/// 全inflight futureを破棄/回収した所有状態
///
async fn run(listener: StdListener, mut state: State,
    control: SharedControl) -> Result<State>
{
    let pending = listener.try_clone().map_err(|_| anyhow!("relay clone"))?;
    let listener = TcpListener::from_std(listener)
        .map_err(|_| anyhow!("relay async listener"))?;
    /*
     * 一つの許可slotを完了してから次の要求を受ける
     */
    while control.open() {
        let accepted = tokio::select! {
            biased;
            _ = control.changed_until(None) => break,
            result = listener.accept() => result,
        };
        let (mut stream, peer) = match accepted {
            Ok(value) => value,
            Err(_) => { control.fail(); break; }
        };
        if !peer.ip().is_loopback() {
            state.rejected += 1;
            control.fail();
            break;
        }
        let result = tokio::select! {
            biased;
            _ = control.changed_until(None) => None,
            result = serve(&mut stream, &mut state, &control) => Some(result),
        };
        if result.is_some_and(|result| result.is_err()) {
            state.rejected += 1;
            control.fail();
        }
    }
    /*
     * 停止後は送信せず待機済み追加接続の存在だけを記録する
     */
    while let Ok((stream, _)) = pending.accept() {
        state.closed_arrivals += 1;
        drop(stream);
    }
    Ok(state)
}

///
/// 完全要求を照合し、送信slotを消費して一回だけ応答する。
///
/// # 概要
/// 上流正常EOFと下流障害を別証拠にし、停止時は再送しない。
/// 不正要求の値はerrorへ入れない。
///
/// # 引数
/// * `stream` - loopbackの一要求接続
/// * `state` - 完全要求列と限定権限
/// * `control` - send直前に再確認する停止gate
///
/// # 戻り値
/// 一回の完了、またはterminalにする固定失敗
///
async fn serve(stream: &mut TcpStream, state: &mut State,
    control: &SharedControl) -> Result<()>
{
    let at = state.events.len();
    let row = state.plan.rows.get(at).ok_or_else(|| anyhow!("relay extra"))?;
    let max_body = row.body.as_ref().map_or(0, Vec::len);
    let incoming = read_request(stream, &state.host, &state.bearer, max_body)
        .await?;
    let id = validate_request(&incoming, row, state)?;
    let page = row.page;
    let op = row.op;
    let expected = row.statuses.clone();
    let fault = state.plan.faults.iter().find(|v| v.at == at).map(|v| v.kind);
    /*
     * 完全検証後にslotを消費し、喪失しても同じ変更を再送しない
     */
    ensure!(control.open(), "relay closed before send");
    state.events.push(Event { ordinal: at, page, op, forwarded: false,
        status: None, fault: None });
    if fault == Some(FaultKind::Synthetic403) {
        state.events[at].fault = fault;
        state.completed += 1;
        control.reached(at);
        return send_bytes(stream, 403, b"{}").await;
    }
    let mut answer = upstream(state, at, op, page, id.as_deref(),
        incoming.lock, incoming.body, control).await?;
    if let Some(lock) = &answer.lock {
        remember_private(&mut state.private_values, lock)?;
    }
    let real = matches!(&state.backend, Backend::Real(_));
    if real {
        ensure!(expected.contains(&answer.status), "relay upstream status");
    }
    /*
     * 不完全合成応答は製品へ渡し、未知IDを補完しない
     */
    let mut create_identity = false;
    if !answer.disconnect && !answer.incomplete {
        if (op == Op::Create && answer.status == 201)
            || (op == Op::Lookup && answer.status == 200)
        {
            match answer.body.identity() {
                Ok(id) => {
                    state.remember(page, id, op == Op::Create)?;
                    create_identity = op == Op::Create;
                }
                Err(_) if !real => (),
                Err(_) => return Err(anyhow!("relay upstream identity")),
            }
        }
        if op != Op::Create || create_identity {
            capture_lock(state, op, page, answer.lock.as_ref(), real)?;
        }
    }
    if let Some(kind) = fault {
        ensure!(!answer.disconnect && !answer.incomplete
            && answer.status == if op == Op::Create { 201 }
                else if op == Op::Source { 200 } else { 204 },
            "relay injection upstream incomplete");
        state.events[at].fault = Some(kind);
        control.reached(at);
    }
    state.completed += 1;
    /*
     * 正常上流を完全受信した後だけ下流の故障を作る
     */
    match fault {
        Some(FaultKind::Http500) => send_bytes(stream, 500, b"{}").await,
        Some(FaultKind::Empty500) => send_bytes(stream, 500, b"").await,
        Some(FaultKind::Disconnect) => Ok(()),
        Some(FaultKind::Hold) => {
            if control.changed_until(Some(at)).await {
                send_answer(stream, answer).await
            } else { Ok(()) }
        }
        _ => send_answer(stream, answer).await,
    }
}

///
/// 完全許可列に対応する上流を一回だけ呼ぶ。
///
/// # 概要
/// statusはheader受信時点で残し、body完了や注入成立と分離する。
/// headerの秘密はbody受信の成否によらずRAM検査へ保持する。
///
/// # 引数
/// * `state` - 限定上流と秘密所有者
/// * `at` - header観測を残す消費済みrow
/// * `op` - 固定操作
/// * `page` - 許可page index
/// * `id` - ID操作だけの確定ID
/// * `lock` - 同ID検査済みlock認証
/// * `body` - 検査済み合成bytes
/// * `control` - send初回pollと閉鎖を排他にするgate
///
/// # 戻り値
/// 完全受信した応答。受信量上限は追加しない。
///
async fn upstream(state: &mut State, at: usize, op: Op, page: usize,
    id: Option<&str>, lock: Option<HeaderValue>, body: Vec<u8>,
    control: &SharedControl) -> Result<Answer>
{
    let mut stored = Body::new()?;
    match &mut state.backend {
        Backend::Script(replies) => {
            control.begin(&mut state.events[at])?;
            let reply = replies.pop_front().ok_or_else(|| anyhow!("script"))?;
            if !reply.disconnect {
                state.events[at].status = Some(reply.status);
            }
            stored.append(&reply.body)?;
            let lock = if let Some(token) = reply.token {
                let expire: chrono::DateTime<chrono::Utc> =
                    (SystemTime::now() + Duration::from_secs(600)).into();
                let mut value = HeaderValue::from_str(&format!(
                    "expire={} token={token}", expire.to_rfc3339()))
                    .map_err(|_| anyhow!("script lock"))?;
                value.set_sensitive(true);
                Some(value)
            } else { None };
            Ok(Answer { status: reply.status, lock, body: stored,
                incomplete: reply.incomplete, disconnect: reply.disconnect })
        }
        Backend::Real(grant) => {
            let (method, tail, _) = op.route();
            let body = if op == Op::Put { Some(body) } else { None };
            let request = grant.send(method, &state.plan.pages[page].path,
                id, tail, lock, body);
            let mut response = control.forward(&mut state.events[at], request)
                .await?;
            let status = response.status().as_u16();
            state.events[at].status = Some(status);
            let values: Vec<_> = response.headers().get_all("x-page-lock")
                .iter().collect();
            ensure!(values.len() <= 1, "relay lock duplicate");
            let lock = values.first().map(|value| {
                let mut value = (*value).clone();
                value.set_sensitive(true);
                value
            });
            if let Some(header) = &lock {
                remember_private(&mut state.private_values, header)?;
            }
            /*
             * 全chunkと正常EOFまで固定幅で保護spoolへ保存する
             */
            while let Some(chunk) = response.chunk().await
                .map_err(|_| anyhow!("relay upstream receive"))?
            { stored.append(&chunk)?; }
            Ok(Answer { status, lock, body: stored, incomplete: false,
                disconnect: false })
        }
    }
}

///
/// 全取得/延長lockをRAMだけに追跡し、同IDへ結び付ける。
///
/// # 概要
/// canonical IDが確定した応答だけに呼び、期限を検査してから
/// 同じIDのleaseを更新する。未知IDへの補完は行わない。
///
/// # 引数
/// * `state` - 非公開lock所有者
/// * `op` - 応答を得た操作
/// * `page` - 許可page
/// * `header` - 非公開の取得/延長header
/// * `real` - 実上流の厳密検証を要求するか
///
/// # 戻り値
/// 検証/追跡済み、または秘密を含まない拒否
///
fn capture_lock(state: &mut State, op: Op, page: usize,
    header: Option<&HeaderValue>, real: bool) -> Result<()>
{
    if let Some(header) = header {
        remember_private(&mut state.private_values, header)?;
        match parse_lock(header, SystemTime::now().into()) {
            Ok((authentication, _)) => {
                let id = state.current.get(&page)
                    .ok_or_else(|| anyhow!("relay lock ID"))?;
                state.locks.insert(id.clone(), authentication);
            }
            Err(_) if !real => (),
            Err(_) => return Err(anyhow!("relay upstream lock")),
        }
    } else if real && matches!(op, Op::Create | Op::Acquire | Op::Renew) {
        return Err(anyhow!("relay upstream lock missing"));
    }
    Ok(())
}

///
/// body受信失敗時もheaderで見えた全tokenをRAM検査へ残す。
///
/// # 引数
/// * `values` - file/logへ公開しない秘密集合
/// * `header` - 取得/延長時のsensitive header
///
/// # 戻り値
/// 固定header/token表現の保持、または値なしの形式拒否
///
fn remember_private(values: &mut Vec<String>, header: &HeaderValue)
    -> Result<()>
{
    let text = header.to_str().map_err(|_| anyhow!("relay lock format"))?;
    values.push(text.to_owned());
    /*
     * 旧tokenも上書きせず両captureの検査まで保持する
     */
    for part in text.split_ascii_whitespace() {
        if let Some(token) = part.strip_prefix("token=") {
            values.push(token.to_owned());
            values.push(part.to_owned());
        }
    }
    Ok(())
}

///
/// 完全HTTP/1.1要求のframingと認証を上流前に検証する。
///
/// # 概要
/// headerだけに有限構文上限を置き、bodyはcaseが所有する既知長と
/// 一致させる。上流のWiki応答へこの上限を適用しない。
///
/// # 引数
/// * `stream` - 専用loopback接続
/// * `host` - 所有listenerの固定Host
/// * `bearer` - run専用合成認証
/// * `max_body` - このslotが所有する合成bytes長
///
/// # 戻り値
/// 完全要求、またはraw値なしの拒否
///
async fn read_request(stream: &mut TcpStream, host: &str, bearer: &str,
    max_body: usize) -> Result<Incoming>
{
    let mut bytes = Vec::new();
    /*
     * raw headerはこのstackだけに保持し、eventへ渡さない
     */
    let end = loop {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).await
            .map_err(|_| anyhow!("relay ingress read"))?;
        ensure!(count != 0, "relay ingress incomplete");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            ensure!(end + 4 <= 65_536, "relay header length");
            break end + 4;
        }
        ensure!(bytes.len() <= 65_536, "relay header length");
    };
    let parsed = parse_header(&bytes[..end], host, bearer)?;
    ensure!(parsed.length <= max_body
        && bytes.len() - end <= parsed.length, "relay framing");
    let mut body = bytes.split_off(end);
    /*
     * 宣言長だけを受信し、同時到着した余剰/pipelineは拒否する
     */
    while body.len() < parsed.length {
        let mut buffer = [0_u8; 4096];
        let limit = buffer.len().min(parsed.length - body.len());
        let count = stream.read(&mut buffer[..limit]).await
            .map_err(|_| anyhow!("relay ingress body"))?;
        ensure!(count != 0, "relay ingress body incomplete");
        body.extend_from_slice(&buffer[..count]);
    }
    Ok(Incoming { method: parsed.method, target: parsed.target,
        lock: parsed.lock, body })
}

///
/// header原文を保持しない選択結果。
///
struct Header {
    /// 検証する単一method。
    method: String,
    /// 検証するorigin-form target。
    target: String,
    /// 単一の独立認証。
    lock: Option<HeaderValue>,
    /// 単一Content-Length。省略時0。
    length: usize,
}

///
/// 純粋なheader境界を検査する。
///
/// # 引数
/// * `bytes` - CRLF終端までの非公開header
/// * `host` - listenerの固定Host
/// * `bearer` - run専用合成値
///
/// # 戻り値
/// 非曖昧な選択結果、または固定拒否
///
fn parse_header(bytes: &[u8], host: &str, bearer: &str) -> Result<Header> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow!("relay header encoding"))?;
    ensure!(text.ends_with("\r\n\r\n")
        && !text.replace("\r\n", "").contains(['\r', '\n']),
        "relay header lines");
    let mut lines = text[..text.len() - 4].split("\r\n");
    let first: Vec<_> = lines.next().unwrap_or("").split(' ').collect();
    ensure!(first.len() == 3 && first[2] == "HTTP/1.1"
        && matches!(first[0], "GET" | "POST" | "PUT" | "DELETE")
        && first[1].starts_with('/') && !first[1].starts_with("//")
        && !first[1].contains('#'), "relay request line");
    let mut seen = BTreeSet::new();
    let mut length = None;
    let mut lock = None;
    let mut auth = false;
    let mut authority = false;
    /*
     * 未知headerもdrop転送せず拒否し、hopごとに既知値を再構築する
     */
    for line in lines {
        ensure!(!line.starts_with([' ', '\t']), "relay header folding");
        let (name, value) = line.split_once(':')
            .ok_or_else(|| anyhow!("relay header syntax"))?;
        ensure!(!name.is_empty() && name.bytes().all(|v|
            v.is_ascii_alphanumeric() || v == b'-'), "relay header name");
        let name = name.to_ascii_lowercase();
        ensure!(seen.insert(name.clone()), "relay header duplicate");
        let value = value.trim_matches([' ', '\t']);
        ensure!(!value.chars().any(char::is_control), "relay header value");
        match name.as_str() {
            "host" => authority = value == host,
            "authorization" => auth = value == format!("Bearer {bearer}"),
            "content-length" => {
                ensure!(!value.is_empty() && value.bytes()
                    .all(|v| v.is_ascii_digit()), "relay length syntax");
                length = Some(value.parse::<usize>()
                    .map_err(|_| anyhow!("relay length"))?);
            }
            "x-lock-authentication" => {
                let mut header = HeaderValue::from_str(value)
                    .map_err(|_| anyhow!("relay lock authentication"))?;
                header.set_sensitive(true);
                lock = Some(header);
            }
            "content-type" => ensure!(value == "text/markdown",
                "relay content type"),
            "accept" => ensure!(value == "*/*", "relay accept"),
            "connection" => ensure!(value.eq_ignore_ascii_case("close"),
                "relay connection"),
            _ => return Err(anyhow!("relay unsupported header")),
        }
    }
    ensure!(auth && authority, "relay ingress authentication");
    let body_put = first[0] == "PUT" && first[1].ends_with("/source");
    ensure!(!body_put || length.is_some(), "relay PUT length missing");
    ensure!(body_put || length.unwrap_or(0) == 0, "relay unexpected body");
    ensure!(body_put == seen.contains("content-type"),
        "relay content type presence");
    Ok(Header { method: first[0].to_owned(), target: first[1].to_owned(),
        lock, length: length.unwrap_or(0) })
}

///
/// request target、完全path、現在ID、lock、本文をslotへ照合する。
///
/// # 引数
/// * `incoming` - 完全受信済み要求
/// * `row` - 次の単回slot
/// * `state` - 所有path/ID/lease
///
/// # 戻り値
/// ID操作の確定ID、または上流0の拒否
///
fn validate_request(incoming: &Incoming, row: &Row, state: &State)
    -> Result<Option<String>>
{
    let (method, tail, by_id) = row.op.route();
    ensure!(incoming.method == method.as_str(), "relay method slot");
    let id = if by_id {
        let id = state.current.get(&row.page)
            .ok_or_else(|| anyhow!("relay unknown ID"))?;
        ensure!(state.known.get(id) == Some(&row.page), "relay ID binding");
        let mut target = format!("/api/pages/{id}");
        if let Some(tail) = tail { target.push('/'); target.push_str(tail); }
        ensure!(incoming.target == target, "relay ID target");
        Some(id.clone())
    } else {
        let route = if row.op == Op::Lookup { "/api/pages/id?" }
            else { "/api/pages?" };
        let query = incoming.target.strip_prefix(route)
            .ok_or_else(|| anyhow!("relay path target"))?;
        validate_query(query, &state.plan.pages[row.page].path)?;
        None
    };
    /*
     * 任意lockを中継せず、同IDで最後に返した認証だけを受ける
     */
    if row.op.locked() {
        let known = id.as_ref().and_then(|id| state.locks.get(id))
            .ok_or_else(|| anyhow!("relay lease unknown"))?;
        ensure!(incoming.lock.as_ref() == Some(known), "relay lease binding");
    } else {
        ensure!(incoming.lock.is_none(), "relay unexpected lease");
    }
    ensure!(incoming.body.as_slice() == row.body.as_deref().unwrap_or(b""),
        "relay body slot");
    Ok(id)
}

///
/// path queryを一回だけdecodeして完全所有pathと比較する。
///
/// # 引数
/// * `query` - 非公開query原文
/// * `expected` - case所有の完全path
///
/// # 戻り値
/// 単一path keyの完全一致
///
fn validate_query(query: &str, expected: &str) -> Result<()> {
    let bytes = query.as_bytes();
    let mut offset = 0;
    /*
     * URL parserの寛容な不正percent/UTF8置換を前もって拒否する
     */
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            ensure!(offset + 2 < bytes.len()
                && bytes[offset + 1].is_ascii_hexdigit()
                && bytes[offset + 2].is_ascii_hexdigit(), "relay query escape");
            offset += 3;
        } else { offset += 1; }
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(bytes).collect();
    ensure!(query.starts_with("path=") && !query.contains('&')
        && pairs.len() == 1
        && pairs[0].0 == "path" && pairs[0].1 == expected
        && !pairs[0].1.contains('\u{fffd}'), "relay path query");
    Ok(())
}

///
/// 完全spoolだけから一接続一応答を送る。
///
/// # 引数
/// * `stream` - 検証済み要求の接続
/// * `answer` - 完全受信した上流または合成応答
///
/// # 戻り値
/// 完全送信、または下流切断の固定分類
///
async fn send_answer(stream: &mut TcpStream, mut answer: Answer) -> Result<()> {
    if answer.disconnect { return Ok(()); }
    let length = answer.body.length + u64::from(answer.incomplete);
    let mut header = format!("HTTP/1.1 {} G6\r\nContent-Length: {length}\r\n\
        Connection: close\r\n", answer.status);
    if let Some(lock) = answer.lock {
        header.push_str("X-Page-Lock: ");
        header.push_str(lock.to_str().map_err(|_| anyhow!("relay lock wire"))?);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes()).await
        .map_err(|_| anyhow!("relay downstream header"))?;
    answer.body.file.seek(SeekFrom::Start(0))
        .map_err(|_| anyhow!("relay body seek"))?;
    /*
     * body全体を再収集せず固定bufferで送信する
     */
    let mut buffer = [0_u8; 16_384];
    loop {
        let count = answer.body.file.read(&mut buffer)
            .map_err(|_| anyhow!("relay body read"))?;
        if count == 0 { break; }
        stream.write_all(&buffer[..count]).await
            .map_err(|_| anyhow!("relay downstream body"))?;
    }
    stream.shutdown().await.map_err(|_| anyhow!("relay downstream close"))?;
    Ok(())
}

///
/// 注入用の小さい固定本文だけを応答する。
///
/// # 引数
/// * `stream` - 下流接続
/// * `status` - 固定注入status
/// * `body` - 固定JSONまたは空bytes
///
/// # 戻り値
/// 完全応答、または固定送信失敗
///
async fn send_bytes(stream: &mut TcpStream, status: u16, body: &[u8])
    -> Result<()>
{
    let mut stored = Body::new()?;
    stored.append(body)?;
    send_answer(stream, Answer { status, lock: None, body: stored,
        incomplete: false, disconnect: false }).await
}

///
/// 引用符外のJSON空白だけを読み飛ばす。
///
/// # 引数
/// * `reader` - 完全spoolのreader
///
/// # 戻り値
/// 次の非空白byte、または正常EOF
///
fn json_byte(reader: &mut impl Read) -> Result<Option<u8>> {
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) if matches!(byte[0], b' ' | b'\t' | b'\r' | b'\n') => (),
            Ok(_) => return Ok(Some(byte[0])),
            Err(_) => return Err(anyhow!("relay identity read")),
        }
    }
}

///
/// 引用符内の文字を変換せず、固定ID schemaを完全EOFまで読む。
///
/// # 概要
/// key/IDの原文と構造記号を分けて読み、引用符内の空白を
/// 読み飛ばさない。schema外のfieldや末尾bytesは受け付けない。
///
/// # 引数
/// * `reader` - 全量収集しない応答reader
///
/// # 戻り値
/// canonical ID、または未知field/重複/不正文字列の拒否
///
fn read_identity(reader: &mut impl Read) -> Result<String> {
    /*
     * 構造記号の外だけ空白を許し、keyとIDは原文で一致させる
     */
    ensure!(json_byte(reader)? == Some(b'{'), "relay identity schema");
    ensure!(json_byte(reader)? == Some(b'"'), "relay identity schema");
    let mut key = [0_u8; 3];
    reader.read_exact(&mut key).map_err(|_| anyhow!("relay identity"))?;
    ensure!(&key == b"id\"", "relay identity key");
    ensure!(json_byte(reader)? == Some(b':'), "relay identity schema");
    ensure!(json_byte(reader)? == Some(b'"'), "relay identity schema");
    let mut id = [0_u8; 26];
    reader.read_exact(&mut id).map_err(|_| anyhow!("relay identity"))?;
    let mut quote = [0_u8; 1];
    reader.read_exact(&mut quote).map_err(|_| anyhow!("relay identity"))?;
    ensure!(quote == [b'"'], "relay identity string");
    /*
     * 未知/重複field、末尾値、非canonical IDは結合前に拒否する
     */
    ensure!(json_byte(reader)? == Some(b'}'), "relay identity schema");
    ensure!(json_byte(reader)?.is_none(), "relay identity EOF");
    let id = std::str::from_utf8(&id)
        .map_err(|_| anyhow!("relay identity"))?;
    ensure!(canonical_id(id), "relay identity format");
    Ok(id.to_owned())
}

///
/// canonical大文字ULIDだけを許可する。
///
/// # 引数
/// * `id` - 候補ID
///
/// # 戻り値
/// 一意表記ならtrue
///
fn canonical_id(id: &str) -> bool {
    valid_id(id) && id.bytes().all(|v| !v.is_ascii_lowercase())
}

///
/// OS乱数からrun専用の合成Bearerを生成する。
///
/// # 戻り値
/// argv/file/logへ出さない256bit相当の合成値
///
fn random_bearer() -> Result<String> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| anyhow!("relay OS randomness"))?;
    let mut value = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}")
            .map_err(|_| anyhow!("relay dummy encoding"))?;
    }
    Ok(value)
}

///
/// N07から呼ぶ合成framing/path/gate/loopback停止検査。
///
/// # 概要
/// 純粋parserと合成socketの停止を同じ公開検査へまとめ、
/// 実上流の権限や製品子を用いず有限境界を確認する。
///
/// # 注記
/// 合成headerと専用loopback socketだけを使い、実上流へ接続しない。
///
/// # 戻り値
/// 非曖昧framing、完全path、停止gateの代表一致
///
pub fn verify_boundary() -> Result<()> {
    let head = "GET /api/pages/id?path=%2Fcase%2Fp HTTP/1.1\r\n\
        Host: 127.0.0.1:1\r\nAuthorization: Bearer dummy\r\n\r\n";
    parse_header(head.as_bytes(), "127.0.0.1:1", "dummy")?;
    /*
     * 曖昧framing/認証/authorityは上流前に拒否する
     */
    for extra in ["Content-Length: 0\r\nContent-Length: 0\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Transfer-Encoding: chunked\r\nContent-Length: 0\r\n",
        "Authorization: Bearer dummy\r\n",
        "X-Lock-Authentication: a\r\nX-Lock-Authentication: b\r\n",
        " folded: value\r\n", "Proxy-Authorization: value\r\n"]
    {
        let bad = head.replace("\r\n\r\n", &format!("\r\n{extra}\r\n"));
        ensure!(parse_header(bad.as_bytes(), "127.0.0.1:1", "dummy").is_err(),
            "boundary header accepted");
    }
    validate_query("path=%2Fcase%2Fp", "/case/p")?;
    for query in ["path=%2Fcase%2Fp&path=%2Fcase%2Fp", "path=%2Fcase2%2Fp",
        "path=%2Fcase%2Fp&", "path=%2Fcase%2Fp&&",
        "path=%252Fcase%252Fp", "path=%2Fcase%2Fp&recursive=true", "path=%q0"]
    {
        ensure!(validate_query(query, "/case/p").is_err(),
            "boundary path accepted");
    }
    let control = SharedControl::new();
    ensure!(control.open() && !control.failed(), "boundary initial gate");
    control.fail();
    ensure!(!control.open() && control.failed(), "boundary terminal gate");
    verify_identity()?;
    verify_bindings()?;
    verify_socket_stop()?;
    verify_malformed_create()?;
    Ok(())
}

///
/// ID応答の空白・未知field・重複・EOFを合成bytesで検査する。
///
/// # 概要
/// 引用符外の空白だけを許し、不正keyや重複/未知fieldを
/// canonical IDの成功応答へ変換しないことを確認する。
///
/// # 戻り値
/// 厳密schemaだけの受理
///
fn verify_identity() -> Result<()> {
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let good = format!(" \n {{ \"id\" : \"{id}\" }} \t");
    ensure!(read_identity(&mut Cursor::new(good.as_bytes()))? == id,
        "boundary identity valid");
    /*
     * 文字列内空白/escapeと未知schemaはIDを補完せず拒否する
     */
    for bad in [format!("{{\"i d\":\"{id}\"}}"),
        format!("{{\"id \":\"{id}\"}}"),
        format!("{{\"id\":\" {id}\"}}"),
        format!("{{\"id\":\"{id} \"}}"),
        format!("{{\"id\":\"{id}\",\"id\":\"{id}\"}}"),
        format!("{{\"id\":\"{id}\",\"unknown\":0}}"),
        format!("{{\"id\":\"{id}\"}}null"),
        format!("{{\"id\":\"{id}\""),
        format!("{{\"id\":\"{}\"}}", id.to_ascii_lowercase()),
        "{\"i\\u0064\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\"}".to_owned()]
    {
        ensure!(read_identity(&mut Cursor::new(bad.as_bytes())).is_err(),
            "boundary identity accepted");
    }
    Ok(())
}

///
/// 合成caseの完全pathとIDを初期化する。
///
/// # 戻り値
/// 実資格と無関係な二pageの計画
///
fn boundary_plan() -> RelayPlan {
    let prefix = format!("{MS51_ROOT}/boundary");
    RelayPlan { pages: vec![RelayPage { path: format!("{prefix}/a"),
        known_id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()) },
        RelayPage { path: format!("{prefix}/b"),
            known_id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned()) }],
        prefix, rows: vec![Row::new(0, Op::Source)], faults: Vec::new() }
}

///
/// 次slotと完全ID/path/lockの結合を上流なしで検査する。
///
/// # 概要
/// 合成状態へ二pageを設定し、method/target/leaseと応答IDが
/// 次slotの既知pageだけに結び付くことを確認する。
///
/// # 戻り値
/// 未知IDと異page/異操作の転送0
///
fn verify_bindings() -> Result<()> {
    let plan = boundary_plan();
    plan.validate()?;
    let mut state = State::new(plan, Backend::Script(VecDeque::new()),
        "dummy".to_owned(), "127.0.0.1:1".to_owned());
    let row = Row::new(0, Op::Source);
    let mut incoming = Incoming { method: "GET".to_owned(),
        target: "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/source".to_owned(),
        lock: None, body: Vec::new() };
    validate_request(&incoming, &row, &state)?;
    incoming.method = "DELETE".to_owned();
    ensure!(validate_request(&incoming, &row, &state).is_err(),
        "boundary operation slot accepted");
    incoming.method = "GET".to_owned();
    /*
     * pathが同じでも別ID、順序違い、余分queryは受け付けない
     */
    for target in ["/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAW/source",
        "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/source?amend=true",
        "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/meta"]
    {
        incoming.target = target.to_owned();
        ensure!(validate_request(&incoming, &row, &state).is_err(),
            "boundary ID slot accepted");
    }
    incoming.target = "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/lock"
        .to_owned();
    incoming.method = "PUT".to_owned();
    incoming.lock = Some(HeaderValue::from_static("token=dummy-b"));
    state.locks.insert("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        HeaderValue::from_static("token=dummy-a"));
    let renew = Row::new(0, Op::Renew);
    ensure!(validate_request(&incoming, &renew, &state).is_err(),
        "boundary foreign lease accepted");
    incoming.lock = Some(HeaderValue::from_static("token=dummy-a"));
    validate_request(&incoming, &renew, &state)?;
    /*
     * 未確定IDや他pageのIDを応答から再結合しない
     */
    ensure!(state.remember(0, "01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned(),
        false).is_err(), "boundary response ID collision");
    ensure!(state.remember(0, "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        true).is_err(), "boundary created ID reuse");
    state.current.remove(&0);
    ensure!(validate_request(&incoming, &renew, &state).is_err(),
        "boundary unknown ID accepted");
    Ok(())
}

///
/// このrelayだけへ有限timeout付きの合成socketを接続する。
///
/// # 引数
/// * `relay` - run専用listenerと合成Bearerの所有者
/// * `method` - この検査の固定method
/// * `target` - この検査が固定したrequest target
///
/// # 戻り値
/// 送信済みsocket。実URL/実認証を受け付けない。
///
fn boundary_socket(relay: &Relay, method: &str, target: &str)
    -> Result<StdStream>
{
    let address = relay.url().strip_prefix("http://")
        .ok_or_else(|| anyhow!("boundary URL"))?;
    let mut stream = StdStream::connect(address)
        .map_err(|_| anyhow!("boundary socket"))?;
    let timeout = Some(Duration::from_secs(5));
    stream.set_read_timeout(timeout)
        .map_err(|_| anyhow!("boundary read timeout"))?;
    stream.set_write_timeout(timeout)
        .map_err(|_| anyhow!("boundary write timeout"))?;
    let request = format!("{method} {target} HTTP/1.1\r\nHost: {address}\r\n\
        Authorization: Bearer {}\r\n\r\n", relay.bearer);
    stream.write_all(request.as_bytes())
        .map_err(|_| anyhow!("boundary socket write"))?;
    Ok(stream)
}

///
/// 保留socketの停止回収と未知要求後のsticky転送0を検査する。
///
/// # 概要
/// 専用relay runtimeだけで合成応答を処理し、保留到達後にgateを
/// 閉じてinflightとlistenerをjoinする。製品子は起動しない。
///
/// # 戻り値
/// 停止済みsocket、完全join、未知要求のterminal拒否
///
fn verify_socket_stop() -> Result<()> {
    let mut plan = boundary_plan();
    plan.faults.push(Fault { at: 0, kind: FaultKind::Hold });
    let relay = Relay::scripted(plan, vec![Reply::new(200, b"synthetic")])?;
    let mut socket = boundary_socket(&relay, "GET",
        "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/source")?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all()
        .build().map_err(|_| anyhow!("boundary wait runtime"))?;
    let control = relay.control();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), control.wait_reached(0))
            .await.map_err(|_| anyhow!("boundary hold timeout"))?
    })?;
    let _queued = boundary_socket(&relay, "GET",
        "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/source")?;
    /*
     * 保留応答を送らず停止し、既に完了した上流証拠だけ残す
     */
    control.close();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes)
        .map_err(|_| anyhow!("boundary socket stop"))?;
    ensure!(bytes.is_empty(), "boundary hold leaked response");
    let evidence = relay.finish()?;
    ensure!(!evidence.failed && evidence.state.rejected == 0
        && evidence.state.closed_arrivals == 1
        && evidence.state.completed == 1
        && evidence.events().len() == 1
        && evidence.events()[0].status == Some(200)
        && evidence.verify().is_err(), "boundary hold event");
    /*
     * 未知要求はslotを消費せず、後の要求にも再開しない
     */
    let relay = Relay::scripted(boundary_plan(),
        vec![Reply::new(200, b"synthetic")])?;
    let mut socket = boundary_socket(&relay, "GET",
        "/api/pages/unknown/source")?;
    bytes.clear();
    socket.read_to_end(&mut bytes)
        .map_err(|_| anyhow!("boundary reject socket"))?;
    let control = relay.control();
    ensure!(control.failed(), "boundary reject not terminal");
    let evidence = relay.finish()?;
    ensure!(evidence.failed && evidence.state.rejected == 1
        && evidence.events().is_empty()
        && evidence.state.completed == 0, "boundary rejection forwarded");
    let mut event = Event { ordinal: 0, page: 0, op: Op::Source,
        forwarded: false, status: None, fault: None };
    ensure!(control.begin(&mut event).is_err() && !event.forwarded,
        "boundary terminal resumed");
    let mut polled = false;
    let request = async { polled = true; Ok(()) };
    ensure!(runtime.block_on(control.forward(&mut event, request)).is_err()
        && !polled && !event.forwarded, "boundary stopped future polled");
    /*
     * 正常完了したslotも再利用せず、追加要求を転送0で止める
     */
    let relay = Relay::scripted(boundary_plan(),
        vec![Reply::new(200, b"synthetic")])?;
    let target = "/api/pages/01ARZ3NDEKTSV4RRFFQ69G5FAV/source";
    let mut socket = boundary_socket(&relay, "GET", target)?;
    bytes.clear();
    socket.read_to_end(&mut bytes)
        .map_err(|_| anyhow!("boundary complete socket"))?;
    ensure!(bytes.starts_with(b"HTTP/1.1 200 "), "boundary first response");
    let address = relay.url().strip_prefix("http://")
        .ok_or_else(|| anyhow!("boundary extra URL"))?;
    let mut socket = StdStream::connect(address)
        .map_err(|_| anyhow!("boundary extra connect"))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| anyhow!("boundary extra timeout"))?;
    bytes.clear();
    match socket.read_to_end(&mut bytes) {
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
        Err(_) => return Err(anyhow!("boundary extra socket")),
    }
    let evidence = relay.finish()?;
    ensure!(evidence.failed && evidence.state.rejected == 1
        && evidence.state.completed == 1 && evidence.events().len() == 1,
        "boundary consumed slot reused");
    Ok(())
}

///
/// 合成POST不正IDを透過しつつ新旧IDへlockを誤結合しない。
///
/// # 概要
/// 新規と再作成の両方で不正IDと有効な合成lockを返す。
/// tokenはcapture検査へ保持し、未確定のIDへleaseを補完しない。
///
/// # 戻り値
/// 不正応答の下流到達と秘密registry、ID/lease未結合
///
fn verify_malformed_create() -> Result<()> {
    /*
     * 再作成でも既知old IDはnew lockの結合先へ流用しない
     */
    for old in [false, true] {
        let mut plan = boundary_plan();
        if !old { plan.pages[0].known_id = None; }
        plan.rows = vec![Row::new(0, Op::Create)];
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("path", &plan.pages[0].path).finish();
        let token = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
        let relay = Relay::scripted(plan,
            vec![Reply::new(201, b"{}").locked(token)])?;
        let mut socket = boundary_socket(&relay, "POST",
            &format!("/api/pages?{query}"))?;
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes)
            .map_err(|_| anyhow!("boundary malformed create socket"))?;
        let evidence = relay.finish()?;
        evidence.verify()?;
        ensure!(bytes.starts_with(b"HTTP/1.1 201 ")
            && bytes.windows(token.len()).any(|v| v == token.as_bytes())
            && evidence.created_id(0).is_none()
            && evidence.state.locks.is_empty()
            && evidence.check_output(token).is_err(),
            "boundary malformed create binding");
    }
    Ok(())
}
