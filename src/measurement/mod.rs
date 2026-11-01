/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 既定では無効な測定用の有限イベントと所有境界の観測
//!
//! Cargo feature `measurement` でだけ観測を有効にする。
//! `LUWIKI_MEASURE_DIR` と `TMPDIR` は既存の絶対/非symlink/0700領域。
//! CLI引数や認証設定は通常と同じで、観測設定に資格情報を入れない。
//! `events.tsv` はschema2の単調ns/event/id/parent/role/value、
//! `resources.tsv` はローカルだけの匿名IDとhex符号化pathの対応。
//! 有効な終端と `complete` の両方がない観測を成功としない。
//! 型/enumの定義が列の意味を定め、外部の作業記録を前提にしない。
//! File eventはsnapshot書込、検証I/O、upload読込、受信書込、JSONを対象とする。
//! 既存readonly APIが返す通常Fileのcloseは変更せず、FD samplingと区別する。
//!

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "measurement")]
use std::time::Instant;

use tokio::sync::OwnedSemaphorePermit;

///
/// 一時領域とworkerを識別する有限分類。
///
// 無効featureでも同じ呼出境界を保ち、未使用の有限分類だけを局所許容する。
#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Role {
    /// 未分類の資源。
    Other,
    /// 取得元の隔離領域。
    Source,
    /// 検証済み本文。
    Payload,
    /// Wiki受信本文。
    Wiki,
    /// JSON補助領域。
    Json,
    /// upload worker。
    Upload,
    /// receive worker。
    Receive,
}

///
/// 値を持つ文字列を受け付けない観測種別。
///
// feature無効時だけ未使用となる観測種別を許容する。
#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Event {
    /// 同contextの期限。valueは観測起点からの相対ns。
    Deadline = 1,
    /// 同contextへの取消呼出。
    Cancel,
    /// checkpointで期限到達を検知した。
    DeadlineSeen,
    /// checkpointで取消を検知した。
    CancelSeen,
    /// guardのDrop通知。実future abortとは別の事実。
    GuardDrop,
    /// workerが実行を開始した。
    WorkerStart,
    /// workerのscopeが終了した。
    WorkerEnd,
    /// coordinatorがjoinを開始した。
    JoinStart,
    /// coordinatorがjoinを完了した。
    JoinEnd,
    /// 初回execute poll直前。
    HttpStart,
    /// transportの要求scope終了。
    HttpEnd,
    /// permitを最初に観測した。
    PermitAcquire,
    /// 実permitの最後の強参照が解放された。
    PermitRelease,
    /// 一時領域が生成された。
    AreaCreate,
    /// 一時領域のdestructorを開始する。
    AreaDropStart,
    /// destructor完了。削除の成功は外部で別に観測する。
    AreaDropEnd,
    /// 所有されたfileを観測した。
    FileOpen,
    /// 所有fileのdrop完了。closeエラーの成功保証ではない。
    FileDropEnd,
    /// 計画本文の共有所有を開始した。
    PlanBodyCreate,
    /// 計画本文の共有所有者を解放し始める。
    PlanBodyDrop,
    /// blocking workerを予約した。実開始とは別。
    WorkerScheduled,
    /// 制御主体が実future abortを行う直前の通知。
    FutureDrop,
    /// 解放直前の容量走査開始。idはsnapshot、parentは解放領域。
    StorageStart,
    /// 同snapshot/roleの論理容量（bytes）。
    StorageLogical,
    /// 同snapshot/roleの割当容量（bytes、st_blocks * 512）。
    StorageAllocated,
    /// 同snapshot/roleの重複排除した通常file数。
    StorageFiles,
    /// 容量走査終了。valueは読取失敗/競合/打切り件数。
    StorageEnd,
    /// 同snapshot/roleのmetadataを確認した登録領域数。
    StorageAreas,
}

#[cfg(not(feature = "measurement"))]
pub(crate) type Area = tempfile::TempDir;
#[cfg(not(feature = "measurement"))]
pub(crate) type File = fs::File;
#[cfg(not(feature = "measurement"))]
pub(crate) type Permit = Arc<OwnedSemaphorePermit>;
#[cfg(feature = "measurement")]
pub(crate) use active::{Area, File, Permit};

///
/// feature有効時だけ保護された観測出力を準備する。
///
/// # 戻り値
/// なし。観測不能は製品の結果を変更しない。
///
pub(crate) fn init() {
    #[cfg(feature = "measurement")]
    active::init();
}

///
/// 製品の終了後に観測writerを閉じる。
///
/// # 戻り値
/// なし。欠落は観測出力のvalidで判定する。
///
pub(crate) fn finish() {
    #[cfg(feature = "measurement")]
    active::finish();
}

///
/// 匿名IDを割り当てる。
///
/// # 戻り値
/// 観測無効時は0、それ以外はprocess内だけのID
///
pub(crate) fn id() -> u64 {
    #[cfg(feature = "measurement")]
    { active::id() }
    #[cfg(not(feature = "measurement"))]
    { 0 }
}

///
/// 有限eventと数値だけを非待機受付へ渡す。
///
/// # 引数
/// * `kind` - 有限の種別
/// * `identity` - 匿名対象ID
/// * `parent` - 匿名の親ID
/// * `role` - 有限の資源分類
/// * `value` - 種別で意味が固定された数値
///
/// # 戻り値
/// なし。欠落は観測を無効にするだけ。
///
pub(crate) fn event(kind: Event, identity: u64, parent: u64,
    role: Role, value: i128)
{
    #[cfg(feature = "measurement")]
    active::event(kind, identity, parent, role, value);
    #[cfg(not(feature = "measurement"))]
    let _ = (kind, identity, parent, role, value);
}

///
/// 既存deadlineを作り直さず相対時刻で登録する。
///
/// # 引数
/// * `deadline` - 設定解決時に確定した期限
///
/// # 戻り値
/// cloneで共有する匿名context ID
///
#[cfg(feature = "measurement")]
pub(crate) fn context(deadline: Instant) -> u64 {
    let identity = id();
    #[cfg(feature = "measurement")]
    active::deadline(identity, deadline);
    #[cfg(not(feature = "measurement"))]
    let _ = deadline;
    identity
}

///
/// root一時領域を通常と同じ方法で生成する。
///
/// # 引数
/// * `role` - 容量の有限分類
///
/// # 戻り値
/// 所有された領域、または通常のI/O失敗
///
pub(crate) fn area(role: Role) -> io::Result<Area> {
    #[cfg(feature = "measurement")]
    let directory = active::new_directory()?;
    #[cfg(not(feature = "measurement"))]
    let directory = tempfile::tempdir()?;
    Ok(wrap_area(directory, role))
}

///
/// acquisition内に領域を生成する。
///
/// # 引数
/// * `parent` - 既存の取得領域
/// * `role` - 有限分類
///
/// # 戻り値
/// 所有された子領域、または通常のI/O失敗
///
pub(crate) fn area_in(parent: &Path, role: Role) -> io::Result<Area> {
    Ok(wrap_area(tempfile::tempdir_in(parent)?, role))
}

///
/// 既存領域を所有数を増やさず観測へ接続する。
///
/// # 引数
/// * `directory` - 移譲される一時領域
/// * `role` - 有限分類
///
/// # 戻り値
/// 同じ領域を所有する値
///
fn wrap_area(directory: tempfile::TempDir, role: Role) -> Area {
    #[cfg(feature = "measurement")]
    { active::Area::new(directory, role) }
    #[cfg(not(feature = "measurement"))]
    { let _ = role; directory }
}

///
/// 所有fileを複製せず観測へ接続する。
///
/// # 引数
/// * `file` - 移譲されるhandle
/// * `role` - 有限分類
///
/// # 戻り値
/// 同じhandleを所有する値
///
pub(crate) fn file(file: fs::File, role: Role) -> File {
    #[cfg(feature = "measurement")]
    { active::File::new(file, role) }
    #[cfg(not(feature = "measurement"))]
    { let _ = role; file }
}

///
/// 既存permitの強参照を増やさず観測へ移す。
///
/// # 引数
/// * `permit` - 既存の共有所有値
///
/// # 戻り値
/// 最後の強参照解放を観測する値
///
pub(crate) fn permit(permit: Arc<OwnedSemaphorePermit>) -> Permit {
    permit.into()
}

///
/// 従来と同じ一つの強参照だけを共有する。
///
/// # 引数
/// * `permit` - 既存の所有値
///
/// # 戻り値
/// 送受信workerへ渡す一つの共有値
///
pub(crate) fn share(permit: &Permit) -> Arc<OwnedSemaphorePermit> {
    Arc::clone(permit)
}

///
/// workerやjoinのscopeを退出時にも観測するguard。
///
pub(crate) struct Span {
    /// 同じ処理を結ぶ匿名ID。
    identity: u64,
    /// cloneしない親context ID。
    parent: u64,
    /// 有限分類。
    role: Role,
    /// scope退出時の有限event。
    end: Event,
}

impl Span {
    ///
    /// scope開始を記録し終了guardを返す。
    ///
    /// # 引数
    /// * `start` - 開始event
    /// * `end` - 退出event
    /// * `identity` - 同じscopeの匿名ID
    /// * `parent` - 所有context ID
    /// * `role` - 有限分類
    ///
    /// # 戻り値
    /// 所有物やcontextの強参照を持たないguard
    ///
    pub(crate) fn new(start: Event, end: Event, identity: u64,
        parent: u64, role: Role) -> Self
    {
        event(start, identity, parent, role, 0);
        Self { identity, parent, role, end }
    }
}

impl Drop for Span {
    ///
    /// scope退出を記録する。workerの成功/失敗とは区別する。
    ///
    /// # 戻り値
    /// なし。
    ///
    fn drop(&mut self) {
        event(self.end, self.identity, self.parent, self.role, 0);
    }
}

#[cfg(feature = "measurement")]
mod active {
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::ops::{Deref, DerefMut};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use tokio::sync::OwnedSemaphorePermit;

    use super::{Event, Role};

    ///
    /// writerへ渡す有限行または保護されたpath対応。
    ///
    enum Message {
        /// 時刻、event、匿名ID、親ID、分類、数値。
        Row(u128, u8, u64, u64, u8, i128),
        /// ローカルだけに保存する資源対応。
        Resource(u64, u64, u8, PathBuf),
        /// 全producer終了後の終端。
        Finish,
    }

    ///
    /// underlying permitを延命しない共有観測状態。
    ///
    struct PermitState {
        /// 匿名ID。
        identity: u64,
        /// 実permitの弱参照だけ。
        weak: Weak<OwnedSemaphorePermit>,
        /// 並行drop後も解放eventを一度だけにする。
        released: AtomicBool,
    }

    ///
    /// processごとに一度だけ起動する観測器。
    ///
    struct Recorder {
        /// Rust内部event共通の単調起点。
        origin: Instant,
        /// try_sendでproducerを容量待機させない送信口。
        sender: mpsc::SyncSender<Message>,
        /// 観測失敗を製品結果と分離するflag。
        valid: Arc<AtomicBool>,
        /// 終端後の追加eventを検出する。
        closed: AtomicBool,
        /// 最終flushだけを待つwriter。
        writer: Mutex<Option<JoinHandle<()>>>,
        /// resource親子のローカル対応。
        resources: Mutex<Vec<(u64, PathBuf, Role)>>,
        /// 強参照を含まないpermit対応。
        permits: Mutex<HashMap<usize, Arc<PermitState>>>,
        /// 製品子にだけ渡した専用TMPDIR。
        temporary: PathBuf,
        /// 終端の保護出力先。
        directory: PathBuf,
        /// 開始前のworker予約も含めた生存資源数。
        live: AtomicU64,
    }

    static RECORDER: OnceLock<Recorder> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(1);
    #[cfg(test)]
    static WORKER_END_EMITTED: AtomicU64 = AtomicU64::new(0);
    #[cfg(test)]
    static WORKER_END_WRITTEN: AtomicU64 = AtomicU64::new(0);


    ///
    /// 観測設定と同じ一時領域を使い、環境の書換を必要にしない。
    ///
    /// # 戻り値
    /// 専用領域、または通常のI/O失敗
    ///
    pub(super) fn new_directory() -> io::Result<tempfile::TempDir> {
        match RECORDER.get() {
            Some(recorder) => tempfile::tempdir_in(&recorder.temporary),
            None => tempfile::tempdir(),
        }
    }

    ///
    /// 保護された新fileを排他的に開く。
    ///
    /// # 引数
    /// * `path` - 既知の出力先
    ///
    /// # 戻り値
    /// file、またはI/O失敗
    ///
    fn output(path: &Path) -> io::Result<fs::File> {
        OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(path)
    }

    ///
    /// 既存の保護directoryを検査する。
    ///
    /// # 引数
    /// * `path` - 変更しないdirectory
    ///
    /// # 戻り値
    /// 非symlink/絶対/0700相当/canonical一致ならtrue
    ///
    fn protected(path: &Path) -> bool {
        path.is_absolute() && fs::symlink_metadata(path).is_ok_and(|m|
            m.is_dir() && m.permissions().mode() & 0o077 == 0)
            && path.canonicalize().is_ok_and(|p| p == path)
    }

    ///
    /// typedな設定からwriterを用意する。試験も同じ入口を使う。
    ///
    /// # 概要
    /// 型付きの保護設定を検査し、上限付きwriterを一つだけ作る。
    ///
    /// # 引数
    /// * `directory` - 保護出力directory
    /// * `temporary` - 製品だけの一時領域
    ///
    /// # 戻り値
    /// なし。設定不能時は有効な終端を生成しない。
    ///
    pub(super) fn init_at(directory: &Path, temporary: &Path) {
        /*
         * 出力先を検査し、既存fileを上書きしない
         */
        if !protected(directory) || !temporary.is_absolute() { return; }
        let Ok(events) = output(&directory.join("events.tsv")) else {
            return;
        };
        let Ok(resources) = output(&directory.join("resources.tsv")) else {
            return;
        };
        let (sender, receiver) = mpsc::sync_channel(16384);
        let valid = Arc::new(AtomicBool::new(true));
        let writer_valid = Arc::clone(&valid);
        let Ok(thread) = std::thread::Builder::new().spawn(move || {
            write_trace(events, resources, receiver, writer_valid);
        }) else { return; };
        /*
         * writerと記録先を一度だけprocessへ接続する
         */
        let recorder = Recorder { origin: Instant::now(), sender, valid,
            closed: AtomicBool::new(false), writer: Mutex::new(Some(thread)),
            resources: Mutex::new(Vec::new()),
            permits: Mutex::new(HashMap::new()),
            temporary: temporary.to_owned(),
            directory: directory.to_owned(), live: AtomicU64::new(0) };
        if let Err(unused) = RECORDER.set(recorder) {
            let _ = unused.sender.send(Message::Finish);
            if let Ok(mut handle) = unused.writer.lock() {
                if let Some(handle) = handle.take() { let _ = handle.join(); }
            }
        }
    }

    ///
    /// feature有効時だけ有限の観測設定を読む。
    ///
    /// # 戻り値
    /// なし。環境値を出力しない。
    ///
    pub(super) fn init() {
        let Some(directory) = std::env::var_os("LUWIKI_MEASURE_DIR")
            else { return; };
        let Some(temporary) = std::env::var_os("TMPDIR") else { return; };
        if !protected(Path::new(&temporary)) { return; }
        init_at(Path::new(&directory), Path::new(&temporary));
    }

    ///
    /// path対応と有限eventを別fileへ書く。
    ///
    /// # 概要
    /// 有限eventとpath対応を別fileへ保存し、欠落のない終端を判定する。
    ///
    /// # 引数
    /// * `events` - 値を持たないevent出力
    /// * `resources` - 保護されたローカル対応
    /// * `receiver` - 上限付き受付
    /// * `valid` - 出力/受付失敗のflag
    ///
    /// # 戻り値
    /// なし。失敗したtraceを有効な終端にしない。
    ///
    fn write_trace(mut events: fs::File, mut resources: fs::File,
        receiver: mpsc::Receiver<Message>, valid: Arc<AtomicBool>)
    {
        let result = (|| -> io::Result<()> {
            writeln!(events, "LUWIKI_TRACE\t2")?;
            writeln!(resources, "LUWIKI_RESOURCES\t1")?;
            /*
             * 秘密を含まないeventとpath対応を混ぜず逐次保存する
             */
            while let Ok(message) = receiver.recv() {
                match message {
                    /*
                     * 値を持たないeventだけを共通列へ書く
                     */
                    Message::Row(t, kind, id, parent, role, value) => {
                        writeln!(events,
                            "{t}\t{kind}\t{id}\t{parent}\t{role}\t{value}")?;
                        #[cfg(test)]
                        if kind == Event::WorkerEnd as u8 {
                            WORKER_END_WRITTEN.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Message::Resource(id, parent, role, path) => {
                        write!(resources, "{id}\t{parent}\t{role}\t")?;
                        for byte in path.as_os_str().as_bytes() {
                            write!(resources, "{byte:02x}")?;
                        }
                        writeln!(resources)?;
                    }
                    Message::Finish => {
                        resources.flush()?;
                        writeln!(events, "END\t{}",
                            u8::from(valid.load(Ordering::SeqCst)))?;
                        return events.flush();
                    }
                }
            }
            Err(io::Error::other("incomplete measurement"))
        })();
        if result.is_err() { valid.store(false, Ordering::SeqCst); }
    }

    ///
    /// 容量待機なしで一行を受け付ける。
    ///
    /// # 引数
    /// * `recorder` - processの観測器
    /// * `message` - 有限の一行
    ///
    /// # 戻り値
    /// なし。欠落時はinvalid。
    ///
    fn send(recorder: &Recorder, message: Message) {
        if recorder.closed.load(Ordering::Acquire)
            || recorder.sender.try_send(message).is_err()
        { recorder.valid.store(false, Ordering::SeqCst); }
    }

    ///
    /// 匿名IDを割り当てる。
    ///
    /// # 戻り値
    /// 観測器がない場合0
    ///
    pub(super) fn id() -> u64 {
        if RECORDER.get().is_none() { return 0; }
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    ///
    /// 数値eventを同一時計で記録する。
    ///
    /// # 概要
    /// 所有の生存数と時刻を記録し、欠落を製品結果へ混ぜない。
    ///
    /// # 引数
    /// * `kind` - 有限種別
    /// * `identity` - 匿名ID
    /// * `parent` - 親ID
    /// * `role` - 有限分類
    /// * `value` - 種別で定義した数値
    ///
    /// # 戻り値
    /// なし。
    ///
    pub(super) fn event(kind: Event, identity: u64, parent: u64,
        role: Role, value: i128)
    {
        let Some(recorder) = RECORDER.get() else { return; };
        /*
         * 未開始workerも含めた生存資源数を独立に保持する
         */
        match kind {
            Event::WorkerScheduled | Event::AreaCreate | Event::FileOpen
                | Event::PermitAcquire | Event::PlanBodyCreate
                | Event::HttpStart => {
                recorder.live.fetch_add(1, Ordering::SeqCst);
            }
            Event::WorkerEnd | Event::AreaDropEnd | Event::FileDropEnd
                | Event::PermitRelease | Event::PlanBodyDrop
                | Event::HttpEnd => {
                if recorder.live.fetch_update(Ordering::SeqCst,
                    Ordering::SeqCst, |n| n.checked_sub(1)).is_err()
                { recorder.valid.store(false, Ordering::SeqCst); }
            }
            _ => {}
        }
        /*
         * 上限付き受付に失敗した場合は測定を無効にする
         */
        #[cfg(test)]
        if matches!(kind, Event::WorkerEnd) {
            WORKER_END_EMITTED.fetch_add(1, Ordering::SeqCst);
        }
        send(recorder, Message::Row(recorder.origin.elapsed().as_nanos(),
            kind as u8, identity, parent, role as u8, value));
    }

    ///
    /// deadlineを同じ起点からの符号付きnsにする。
    ///
    /// # 引数
    /// * `identity` - context ID
    /// * `deadline` - 既存期限
    ///
    /// # 戻り値
    /// なし。
    ///
    pub(super) fn deadline(identity: u64, deadline: Instant) {
        let Some(recorder) = RECORDER.get() else { return; };
        let value = if deadline >= recorder.origin {
            deadline.duration_since(recorder.origin).as_nanos() as i128
        } else {
            -(recorder.origin.duration_since(deadline).as_nanos() as i128)
        };
        event(Event::Deadline, identity, 0, Role::Other, value);
    }

    ///
    /// 終端を送りwriterのflushを待つ。
    ///
    /// # 概要
    /// producerの生存とwriterの結果を検査し、有効な終端だけを残す。
    ///
    /// # 戻り値
    /// なし。製品のReport/終了値を変えない。
    ///
    pub(super) fn finish() {
        let Some(recorder) = RECORDER.get() else { return; };
        if recorder.closed.swap(true, Ordering::AcqRel) { return; }
        /*
         * 生存資源が残るtraceを完全な終了としない
         */
        if recorder.live.load(Ordering::SeqCst) != 0 {
            recorder.valid.store(false, Ordering::SeqCst);
        }
        let _ = recorder.sender.send(Message::Finish);
        if let Ok(mut handle) = recorder.writer.lock() {
            if let Some(handle) = handle.take() {
                if handle.join().is_err() {
                    recorder.valid.store(false, Ordering::SeqCst);
                }
            }
        }
        /*
         * writer完了後だけに独立した終端markerを残す
         */
        if recorder.valid.load(Ordering::SeqCst) {
            if let Ok(mut file) = output(&recorder.directory.join("complete")) {
                let _ = file.write_all(b"complete\n");
            }
        }
    }

    #[cfg(test)]
    ///
    /// 終了待ち失敗をworker/event受付/writerへ切り分ける。
    ///
    /// # 戻り値
    /// 終了event発行/書出件数、trace有効flag、生存数、writer終了flag
    ///
    pub(super) fn control_state() -> (u64, u64, bool, u64, bool) {
        let Some(recorder) = RECORDER.get() else {
            return (0, 0, false, 0, true);
        };
        let finished = recorder.writer.try_lock().is_ok_and(|writer|
            writer.as_ref().is_none_or(|thread| thread.is_finished()));
        (WORKER_END_EMITTED.load(Ordering::SeqCst),
            WORKER_END_WRITTEN.load(Ordering::SeqCst),
            recorder.valid.load(Ordering::SeqCst),
            recorder.live.load(Ordering::SeqCst), finished)
    }

    #[cfg(test)]
    ///
    /// 上限付き受付の欠落を同期的に検査する。
    ///
    /// # 戻り値
    /// consumerが進まない時にinvalidとなるならtrue
    ///
    pub(super) fn overflow_control() -> bool {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let recorder = Recorder { origin: Instant::now(), sender,
            valid: Arc::new(AtomicBool::new(true)),
            closed: AtomicBool::new(false), writer: Mutex::new(None),
            resources: Mutex::new(Vec::new()),
            permits: Mutex::new(HashMap::new()), temporary: PathBuf::new(),
            directory: PathBuf::new(), live: AtomicU64::new(0) };
        send(&recorder, Message::Row(0, 1, 1, 0, 0, 0));
        send(&recorder, Message::Row(0, 1, 1, 0, 0, 0));
        !recorder.valid.load(Ordering::SeqCst)
    }

    ///
    /// 登録領域のmetadataを有限時間・件数で走査する。
    ///
    /// # 概要
    /// 最内側分類とinode単位で集計し、競合や打切りを不完全として返す。
    /// symlinkを辿らず、未リンクfileと未登録領域は対象外とする。
    ///
    /// # 引数
    /// * `paths` - 所有handleを含まない生存登録のコピー
    ///
    /// # 戻り値
    /// 分類ごとの論理bytes/割当bytes/file数/領域数と不完全件数
    ///
    pub(super) fn storage_scan(paths: &[(u64, PathBuf, Role)])
        -> ([[u64; 4]; 5], u64)
    {
        let begin = Instant::now();
        let mut totals = [[0_u64; 4]; 5];
        let mut errors = 0;
        let mut roots = Vec::new();
        /*
         * 実在する登録領域だけを選び、内包rootを二度走査しない
         */
        for (_, path, role) in paths {
            match fs::symlink_metadata(path) {
                Ok(info) if info.is_dir() && (*role as usize) < 5 => {
                    totals[*role as usize][3] += 1;
                    roots.push(path.clone());
                }
                _ => errors += 1,
            }
        }
        let mut stack: Vec<_> = roots.iter().filter(|path|
            !roots.iter().any(|other| *path != other
                && path.starts_with(other))).cloned().collect();
        let mut inodes = HashMap::new();
        let mut visited = 0;
        /*
         * 50ms/65536entryで協調打切りする。単一OS呼出の時間は保証しない
         */
        'walk: while let Some(directory) = stack.pop() {
            /*
             * directoryを開けない場合は不完全件数へ加え、次の領域を走査する
             */
            let Ok(children) = fs::read_dir(directory) else {
                errors += 1;
                continue;
            };
            for child in children {
                /*
                 * 走査件数と経過時間を検査し、上限では全走査を打ち切る
                 */
                visited += 1;
                if visited > 65536 || begin.elapsed().as_millis() >= 50 {
                    errors += 1;
                    break 'walk;
                }
                /*
                 * 各entryのmetadataを検査し、directoryは後続の走査へ回す
                 */
                let Ok(child) = child else { errors += 1; continue; };
                let path = child.path();
                let Ok(info) = fs::symlink_metadata(&path) else {
                    errors += 1;
                    continue;
                };
                if info.is_dir() { stack.push(path); continue; }
                if !info.is_file() { errors += 1; continue; }
                /*
                 * 最内側の登録分類を選び、同じinodeの容量変化を不完全として保持する
                 */
                let role = paths.iter().filter(|(_, root, _)|
                    path.starts_with(root)).max_by_key(|(_, root, _)|
                        root.components().count()).map_or(0,
                            |(_, _, role)| *role as usize);
                let role = if role < 5 { role } else { 0 };
                let key = (info.dev(), info.ino());
                let value = (role, info.len(), info.blocks() * 512);
                let prior = inodes.entry(key).or_insert(value);
                if prior.1 != value.1 || prior.2 != value.2 {
                    errors += 1;
                    prior.1 = prior.1.max(value.1);
                    prior.2 = prior.2.max(value.2);
                }
                if prior.0 != role { prior.0 = 0; }
            }
        }
        /*
         * 各inodeを同じ走査区間の一分類にだけ加算する
         */
        for (_, (role, logical, allocated)) in inodes {
            totals[role][0] += logical;
            totals[role][1] += allocated;
            totals[role][2] += 1;
        }
        (totals, errors)
    }

    ///
    /// 解放直前の容量をRust時計の独立snapshotへ記録する。
    ///
    /// # 概要
    /// pathだけを複写してlockを解放し、製品所有を延命せず走査する。
    /// 読取不能はsnapshotの不完全値とし製品結果を変更しない。
    ///
    /// # 引数
    /// * `area` - 解放を開始する匿名領域ID
    ///
    /// # 戻り値
    /// なし。受付欠落は従来どおりtrace全体を無効にする。
    ///
    fn storage_snapshot(area: u64) {
        let Some(recorder) = RECORDER.get() else { return; };
        let identity = id();
        event(Event::StorageStart, identity, area, Role::Other, 0);
        let (totals, errors) = match recorder.resources.try_lock() {
            Ok(paths) => {
                let copy = paths.clone();
                drop(paths);
                let (totals, mut errors) = storage_scan(&copy);
                match recorder.resources.try_lock() {
                    Ok(current) if current.iter().map(|p| p.0).eq(
                        copy.iter().map(|p| p.0)) => {}
                    _ => errors += 1,
                }
                (totals, errors)
            }
            Err(_) => ([[0; 4]; 5], 1),
        };
        /*
         * 独立snapshotの全分類を出し、存在と非零容量を分ける
         */
        for (role, values) in [Role::Other, Role::Source, Role::Payload,
            Role::Wiki, Role::Json].iter().zip(totals)
        {
            for (kind, value) in [Event::StorageLogical,
                Event::StorageAllocated, Event::StorageFiles,
                Event::StorageAreas].iter().zip(values)
            { event(*kind, identity, area, *role, i128::from(value)); }
        }
        event(Event::StorageEnd, identity, area, Role::Other,
            i128::from(errors));
    }

    ///
    /// 通常のTempDirを同じ所有数で包む。
    ///
    pub(crate) struct Area {
        /// 実領域。drop時に先に消費する。
        inner: Option<tempfile::TempDir>,
        /// 匿名ID。
        identity: u64,
        /// 有限分類。
        role: Role,
    }

    impl Area {
        ///
        /// 所有領域を登録する。
        ///
        /// # 概要
        /// 既存領域の親子だけを登録し、所有者を増やさず保持する。
        ///
        /// # 引数
        /// * `inner` - 実領域
        /// * `role` - 有限分類
        ///
        /// # 戻り値
        /// 強参照を増やさない所有値
        ///
        pub(super) fn new(inner: tempfile::TempDir, role: Role) -> Self {
            let identity = id();
            if let Some(recorder) = RECORDER.get() {
                /*
                 * 専用領域内だけを登録し、実pathは保護対応へ分離する
                 */
                if let Ok(mut paths) = recorder.resources.try_lock() {
                    if inner.path().starts_with(&recorder.temporary)
                        && paths.len() < 1024
                        && !paths.iter().any(|(_, p, _)| p == inner.path())
                    {
                        /*
                         * 最も内側の親を対応付けて一回だけ登録する
                         */
                        let parent = paths.iter().filter(|(_, path, _)|
                            inner.path().starts_with(path)).max_by_key(
                                |(_, path, _)| path.components().count())
                            .map_or(0, |(id, _, _)| *id);
                        paths.push((identity, inner.path().to_owned(), role));
                        send(recorder, Message::Resource(identity, parent,
                            role as u8, inner.path().to_owned()));
                        event(Event::AreaCreate, identity, parent, role, 0);
                    } else { recorder.valid.store(false, Ordering::SeqCst); }
                } else { recorder.valid.store(false, Ordering::SeqCst); }
            }
            Self { inner: Some(inner), identity, role }
        }
    }

    impl From<tempfile::TempDir> for Area {
        ///
        /// 試験fixture等の既存領域をotherとして包む。
        ///
        /// # 引数
        /// * `value` - 移譲する領域
        ///
        /// # 戻り値
        /// 同じ領域の所有値
        ///
        fn from(value: tempfile::TempDir) -> Self {
            Self::new(value, Role::Other)
        }
    }

    impl Deref for Area {
        type Target = tempfile::TempDir;
        ///
        /// 元のpath APIへ参照を委譲する。
        ///
        /// # 戻り値
        /// 生存中の領域参照
        ///
        fn deref(&self) -> &Self::Target { self.inner.as_ref().unwrap() }
    }

    impl Drop for Area {
        ///
        /// 領域destructorの前後を記録する。
        ///
        /// # 戻り値
        /// なし。削除成功は外部scanで判定する。
        ///
        fn drop(&mut self) {
            storage_snapshot(self.identity);
            event(Event::AreaDropStart, self.identity, 0, self.role, 0);
            drop(self.inner.take());
            if let Some(recorder) = RECORDER.get() {
                if let Ok(mut paths) = recorder.resources.try_lock() {
                    paths.retain(|(id, _, _)| *id != self.identity);
                } else { recorder.valid.store(false, Ordering::SeqCst); }
            }
            event(Event::AreaDropEnd, self.identity, 0, self.role, 0);
        }
    }

    ///
    /// 通常fileを複製せずdropまで所有する。
    ///
    pub(crate) struct File {
        /// 実handle。
        inner: Option<fs::File>,
        /// 匿名ID。
        identity: u64,
        /// 有限分類。
        role: Role,
    }

    impl File {
        ///
        /// 所有handleの観測を開始する。
        ///
        /// # 引数
        /// * `inner` - 実handle
        /// * `role` - 有限分類
        ///
        /// # 戻り値
        /// 同じhandleの所有値
        ///
        pub(super) fn new(inner: fs::File, role: Role) -> Self {
            let identity = id();
            event(Event::FileOpen, identity, 0, role, 0);
            Self { inner: Some(inner), identity, role }
        }
    }

    impl Deref for File {
        type Target = fs::File;
        ///
        /// 既存metadata等のAPIへ参照を委譲する。
        ///
        /// # 戻り値
        /// 生存中のhandle参照
        ///
        fn deref(&self) -> &Self::Target { self.inner.as_ref().unwrap() }
    }

    impl DerefMut for File {
        ///
        /// 既存試験のFile注入へ可変参照を委譲する。
        ///
        /// # 戻り値
        /// 生存中のhandle可変参照
        ///
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.inner.as_mut().unwrap()
        }
    }

    impl Read for File {
        ///
        /// 通常readをそのまま行う。
        ///
        /// # 引数
        /// * `buffer` - 呼出元のbuffer
        ///
        /// # 戻り値
        /// 通常のread結果
        ///
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.deref_mut().read(buffer)
        }
    }

    impl Write for File {
        ///
        /// 通常writeをそのまま行う。
        ///
        /// # 引数
        /// * `buffer` - 書込元buffer
        ///
        /// # 戻り値
        /// 通常のwrite結果
        ///
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.deref_mut().write(buffer)
        }

        ///
        /// 通常flushをそのまま行う。
        ///
        /// # 戻り値
        /// 通常のflush結果
        ///
        fn flush(&mut self) -> io::Result<()> { self.deref_mut().flush() }
    }

    impl Seek for File {
        ///
        /// 通常seekをそのまま行う。
        ///
        /// # 引数
        /// * `position` - seek先
        ///
        /// # 戻り値
        /// 通常のseek結果
        ///
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.deref_mut().seek(position)
        }
    }

    impl Drop for File {
        ///
        /// handle drop後だけに終了eventを出す。
        ///
        /// # 戻り値
        /// なし。closeエラーがないという保証ではない。
        ///
        fn drop(&mut self) {
            drop(self.inner.take());
            event(Event::FileDropEnd, self.identity, 0, self.role, 0);
        }
    }

    ///
    /// underlying Arcの強参照をそのまま一つ所有する。
    ///
    pub(crate) struct Permit {
        /// 実共有所有値。
        inner: Option<Arc<OwnedSemaphorePermit>>,
        /// 弱参照だけを含む観測状態。
        state: Option<Arc<PermitState>>,
    }

    impl From<Arc<OwnedSemaphorePermit>> for Permit {
        ///
        /// 既存の強参照を増やさず観測へ移す。
        ///
        /// # 概要
        /// 実permitの弱参照だけを共有し、並行解放の観測を一度にまとめる。
        ///
        /// # 引数
        /// * `value` - 移譲するArc
        ///
        /// # 戻り値
        /// 同じ強参照を保持する値
        ///
        fn from(value: Arc<OwnedSemaphorePermit>) -> Self {
            let mut state = None;
            if let Some(recorder) = RECORDER.get() {
                if let Ok(mut permits) = recorder.permits.try_lock() {
                    /*
                     * pointerは内部keyだけに使い、外部へは匿名IDを出す
                     */
                    let key = Arc::as_ptr(&value) as usize;
                    if permits.get(&key).is_some_and(|p|
                        p.weak.strong_count() == 0)
                    { permits.remove(&key); }
                    if permits.len() < 4096 {
                        state = Some(Arc::clone(permits.entry(key)
                            .or_insert_with(|| {
                                let identity = id();
                                event(Event::PermitAcquire, identity, 0,
                                    Role::Other, 0);
                                Arc::new(PermitState { identity,
                                    weak: Arc::downgrade(&value),
                                    released: AtomicBool::new(false) })
                            })));
                    } else { recorder.valid.store(false, Ordering::SeqCst); }
                } else { recorder.valid.store(false, Ordering::SeqCst); }
            }
            Self { inner: Some(value), state }
        }
    }

    impl Deref for Permit {
        type Target = Arc<OwnedSemaphorePermit>;
        ///
        /// 既存のArc::clone境界へ参照を渡す。
        ///
        /// # 戻り値
        /// 実強参照の借用
        ///
        fn deref(&self) -> &Self::Target { self.inner.as_ref().unwrap() }
    }

    impl Drop for Permit {
        ///
        /// 実所有を解放し最後のdrop後だけにeventを出す。
        ///
        /// # 戻り値
        /// なし。弱参照は実permitを延命しない。
        ///
        fn drop(&mut self) {
            drop(self.inner.take());
            if let Some(state) = &self.state {
                if state.weak.strong_count() == 0
                    && !state.released.swap(true, Ordering::AcqRel)
                {
                    event(Event::PermitRelease, state.identity, 0,
                        Role::Other, 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
