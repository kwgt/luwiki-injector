/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 四方式の入力取得と所有されたsnapshot・安全な報告の共通境界
//!

mod git;
mod github;
mod remote;
mod runtime;
mod store;
mod zip;

pub(crate) use runtime::{CancelOnDrop, run as run_blocking};
pub(crate) use store::validate_name as validate_entry_name;
#[cfg(test)]
pub(crate) use tests::integration::validation_adapter_fixtures;

use std::fmt;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::Notify;

use crate::cmd_args::ResolvedRunConfig;
use crate::cmd_args::resolve::RunLimits;
use crate::cmd_args::resolve::source::SourceSpec;
use crate::measurement::Area as TempDir;
use crate::report::Failure;
use crate::report::types::{FailureCategory, ProcessingStage, SourceReference};
use crate::secret::Secret;

/// ページ候補を収める、ソース最上位の固定directory名。
pub(crate) const PAGE_ROOT: &str = "root";

///
/// 固定root配下の名前から、先頭の一段だけを除く。
///
/// # 引数
/// * `name` - 安全検査済みのソース相対名
///
/// # 戻り値
/// root配下ならroot基準の相対名。それ以外はNone。
///
pub(crate) fn root_relative_name(name: &str) -> Option<&str> {
    name.strip_prefix(PAGE_ROOT)?.strip_prefix('/')
}

///
/// root配下の小文字Markdown名かを判定する。
///
/// # 引数
/// * `name` - 安全検査済みのソース相対名
///
/// # 戻り値
/// ページ本文上限の対象となる名前ならtrue。
///
pub(super) fn is_page_name(name: &str) -> bool {
    root_relative_name(name).is_some_and(|relative| relative.ends_with(".md"))
}

///
/// 本文や生の外部エラーを持たない取得失敗分類。
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceError {
    Io, InvalidPath, InvalidUtf8, Symlink, Submodule, InvalidGit,
    RefNotFound, GitFetch, GitCheckout, ZipStructure, ZipUnsupported,
    GitHubApi, GitHubAuth, GitHubDenied, GitHubMissing, GitHubIdentity,
    Duplicate, Changed,
    Limit(&'static str), Cancelled, Deadline, Worker, NotAvailable,
}

impl SourceError {
    ///
    /// 入力不正と取得処理失敗を既存のReport契約へ写す。
    ///
    /// # 概要
    /// 入力検証失敗は2、停止・取得処理失敗は1の終了状態へ対応する。
    /// TransportはI/O・Git取得・APIプロトコル失敗をまとめる分類であり、
    /// 生のmessageやHTTP応答コードから障害原因を推測しない。
    ///
    /// # 戻り値
    /// 秘密値・本文・上限の生名を保持しない全体失敗
    ///
    pub(crate) fn into_failure(self) -> Failure {
        /*
         * 入力の不正はWiki変更前の入力エラーへ写す
         */
        let category = match self {
            Self::InvalidPath | Self::InvalidUtf8 | Self::Symlink
                | Self::Submodule | Self::InvalidGit | Self::RefNotFound
                | Self::ZipStructure | Self::ZipUnsupported | Self::Duplicate
                | Self::Limit(_) | Self::GitHubMissing | Self::GitHubIdentity
                => return Failure::Input,
            /*
             * 処理中の失敗と停止は実行失敗として分類する
             */
            Self::Io | Self::GitFetch | Self::GitHubApi =>
                FailureCategory::Transport,
            Self::GitCheckout => FailureCategory::Rejected,
            Self::GitHubAuth => FailureCategory::Authentication,
            // 403/429だけから権限不足かrate limitかを断定しない。
            Self::GitHubDenied => FailureCategory::Rejected,
            Self::Changed => FailureCategory::Conflict,
            Self::Cancelled => FailureCategory::NotApplied,
            Self::Deadline => FailureCategory::Deadline,
            Self::Worker => FailureCategory::NotApplied,
            Self::NotAvailable => FailureCategory::ExecutionUnavailable,
        };
        Failure::Execution { category, stage: ProcessingStage::Preflight }
    }
}

impl fmt::Display for SourceError {
    ///
    /// 入力値を含めない失敗分類を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 公開可能な分類
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(name) => write!(formatter, "入力上限を超えました: {name}"),
            _ => write!(formatter, "入力取得に失敗しました: {self:?}"),
        }
    }
}

impl std::error::Error for SourceError {}

///
/// 取得時に必要な上限。検証済みRunLimitsからだけ生成する。
///
#[derive(Clone, Copy)]
pub(crate) struct AcquisitionLimits {
    zip_bytes: u64,
    zip_entries: u64,
    zip_expanded: u64,
    git_bytes: u64,
    page_bytes: u64,
}

impl From<&RunLimits> for AcquisitionLimits {
    ///
    /// 検証済み設定の取得上限を取り出す。
    ///
    /// # 引数
    /// * `limits` - OP2で検証済みの上限
    ///
    /// # 戻り値
    /// 取得境界用の上限
    ///
    fn from(limits: &RunLimits) -> Self {
        Self {
            zip_bytes: limits.max_zip_compressed_bytes,
            zip_entries: limits.max_zip_entries,
            zip_expanded: limits.max_zip_uncompressed_bytes,
            git_bytes: limits.max_git_bytes,
            page_bytes: limits.max_page_bytes,
        }
    }
}

///
/// blocking作業も共有する処理期限と協調停止フラグ。
///
#[derive(Clone)]
pub(crate) struct StopContext {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    /// clone間で共有し、contextの所有者を増やさない匿名観測ID。
    #[cfg(feature = "measurement")]
    trace_id: u64,
}

impl StopContext {
    ///
    /// 設定済み期限から停止境界を作る。
    ///
    /// # 引数
    /// * `deadline` - 後続でも共有する期限
    ///
    /// # 戻り値
    /// 未停止の境界
    ///
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline, cancelled: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            #[cfg(feature = "measurement")]
            trace_id: crate::measurement::context(deadline),
        }
    }

    ///
    /// 協調停止を通知する。
    ///
    /// # 戻り値
    /// なし。共有フラグを更新する。
    ///
    pub(crate) fn cancel(&self) {
        crate::measurement::event(crate::measurement::Event::Cancel,
            self.trace_id(), 0, crate::measurement::Role::Other, 0);
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    ///
    /// 読込やphaseの境界で停止を確認する。
    ///
    /// # 戻り値
    /// 期限内で未停止なら成功
    ///
    pub(crate) fn checkpoint(&self) -> Result<(), SourceError> {
        /*
         * 同じ絶対期限を先に検査し、観測で再起算しない
         */
        if Instant::now() >= self.deadline {
            crate::measurement::event(
                crate::measurement::Event::DeadlineSeen, self.trace_id(),
                0, crate::measurement::Role::Other, 0);
            return Err(SourceError::Deadline);
        }
        if self.cancelled.load(Ordering::Acquire) {
            crate::measurement::event(crate::measurement::Event::CancelSeen,
                self.trace_id(), 0, crate::measurement::Role::Other, 0);
            return Err(SourceError::Cancelled);
        }
        Ok(())
    }

    ///
    /// 再起算せず、共有している絶対期限を参照する。
    ///
    /// # 戻り値
    /// 設定解決時からの絶対期限
    ///
    pub(crate) fn deadline(&self) -> Instant { self.deadline }

    ///
    /// 同contextを結ぶ匿名観測IDを返す。
    ///
    /// # 戻り値
    /// 観測無効時0、feature有効時はcloneで共有するID
    ///
    pub(crate) fn trace_id(&self) -> u64 {
        #[cfg(feature = "measurement")]
        { self.trace_id }
        #[cfg(not(feature = "measurement"))]
        { 0 }
    }

    ///
    /// 登録と状態確認を競合安全に行い、期限または取消を待つ。
    ///
    /// # 概要
    /// 通知前の取消もsticky flagで検出し、全cloneで同じ期限を用いる。
    ///
    /// # 戻り値
    /// 到達した期限または取消の安全な分類
    ///
    pub(crate) async fn wait_stopped(&self) -> SourceError {
        loop {
            /*
             * 待機を登録してから状態を確認し、通知の取りこぼしを防ぐ
             */
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Err(error) = self.checkpoint() { return error; }
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(
                    tokio::time::Instant::from_std(self.deadline),
                ) => {},
            }
        }
    }
}

///
/// 方式固有の安全検査後のentry種別。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind { File, Directory }

///
/// 生相対名と取得済み本文の参照。共通本文検証済みとは表明しない。
///
pub(crate) struct SourceEntry {
    raw_name: Vec<u8>,
    kind: EntryKind,
    body: Option<usize>,
    byte_len: u64,
    reference: SourceReference,
}

impl SourceEntry {
    ///
    /// 生のroot相対名を取得する。
    ///
    /// # 戻り値
    /// UTF-8へlossy変換していない名前
    ///
    pub(crate) fn name_raw(&self) -> &[u8] { &self.raw_name }

    ///
    /// entry種別を取得する。
    ///
    /// # 戻り値
    /// ファイルまたはディレクトリ
    ///
    pub(crate) fn kind(&self) -> EntryKind { self.kind }

    ///
    /// 実読込した本文サイズを取得する。
    ///
    /// # 戻り値
    /// バイト数
    ///
    pub(crate) fn byte_len(&self) -> u64 { self.byte_len }

    ///
    /// 安全な報告用参照を取得する。
    ///
    /// # 戻り値
    /// URLと資格情報を持たない入力参照
    ///
    pub(crate) fn reference(&self) -> &SourceReference { &self.reference }
}

impl fmt::Debug for SourceEntry {
    ///
    /// entryの内容や保管パスを表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 種別とサイズだけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SourceEntry")
            .field("kind", &self.kind).field("bytes", &self.byte_len).finish()
    }
}

///
/// 取得データまたは本文の専用一時領域を所有するlease。
///
struct SourceLease(TempDir);

///
/// entryと本文取得元の寿命を一体で保持するsnapshot。
///
pub(crate) struct SourceSnapshot {
    entries: Vec<SourceEntry>,
    lease: SourceLease,
    // 本文leaseを先に破棄し、その親の取得領域を最後に回収する。
    acquisition: Option<SourceLease>,
}

impl SourceSnapshot {
    ///
    /// 入力entry群を参照する。
    ///
    /// # 戻り値
    /// 共通投影・本文検証前のentry群
    ///
    pub(crate) fn entries(&self) -> &[SourceEntry] { &self.entries }

    ///
    /// 元入力ではなく所有された本文保管先を開く。
    ///
    /// # 引数
    /// * `index` - entryの位置
    ///
    /// # 戻り値
    /// ファイルの読込handle。directoryや不正indexは拒否する。
    ///
    pub(crate) fn open_body(&self, index: usize) -> Result<File, SourceError> {
        let ordinal = self.entries.get(index).and_then(|v| v.body)
            .ok_or(SourceError::InvalidPath)?;
        File::open(self.lease.0.path().join(ordinal.to_string()))
            .map_err(|_| SourceError::Io)
    }
}

// 不正入力の生成は試験だけに限定し、製品のbuilderを弱めない。
#[cfg(test)]
impl SourceSnapshot {
    ///
    /// root基準の相対名を、取込directoryを持つ試験入力へ包む。
    ///
    /// # 概要
    /// 生名を検査するfixtureとは区別し、呼出し側が指定したentry位置を保つ。
    /// 空集合でもrootのdirectory entryを末尾へ置く。
    ///
    /// # 引数
    /// * `entries` - root基準の相対名・種別・試験本文
    ///
    /// # 戻り値
    /// root内に指定entryを持つ試験専用snapshot。
    ///
    pub(crate) fn validation_root_fixture(
        entries: &[(&[u8], EntryKind, &[u8])],
    ) -> Result<Self, SourceError> {
        /*
         * 元の生名を正規化せず、先頭へfixtureのrootだけを付ける
         */
        let names: Vec<Vec<u8>> = entries.iter().map(|(raw, _, _)|
            [b"root/".as_slice(), *raw].concat()).collect();
        let mut rooted: Vec<_> = entries.iter().zip(&names)
            .map(|((_, kind, bytes), name)|
                (name.as_slice(), *kind, *bytes)).collect();
        /*
         * root自体を末尾に置き、既存entryの元位置を変えない
         */
        rooted.push((b"root".as_slice(), EntryKind::Directory, b"".as_slice()));
        Self::validation_fixture(&rooted)
    }

    ///
    /// 共通検証を試すため、未検査の生名を持つSnapshotを生成する。
    ///
    /// # 概要
    /// 本文は番号付きfileへ保管し、不正名はfilesystem pathへ使用しない。
    /// 安全な参照を作れない生名には試験専用の参照を付ける。
    ///
    /// # 引数
    /// * `entries` - 生相対名・種別・試験本文
    ///
    /// # 戻り値
    /// 試験だけで使用する未検査Snapshot
    ///
    pub(crate) fn validation_fixture(
        entries: &[(&[u8], EntryKind, &[u8])],
    ) -> Result<Self, SourceError> {
        let area = tempfile::tempdir().map_err(|_| SourceError::Io)?;
        let mut result = Vec::new();
        /*
         * raw名を保持し、参照と本文は安全な内部名で構成する
         */
        for (index, (raw, kind, bytes)) in entries.iter().enumerate() {
            let reference = validate_entry_name(raw, *kind).ok()
                .and_then(|name| SourceReference::new(
                    crate::report::types::SourceKind::Zip, name,
                ).ok()).unwrap_or_else(|| SourceReference::new(
                    crate::report::types::SourceKind::Zip, "fixture",
                ).expect("static fixture reference"));
            if *kind == EntryKind::File {
                std::fs::write(area.path().join(index.to_string()), bytes)
                    .map_err(|_| SourceError::Io)?;
            }
            result.push(SourceEntry {
                raw_name: raw.to_vec(), kind: *kind,
                body: if *kind == EntryKind::File { Some(index) } else { None },
                byte_len: bytes.len() as u64, reference,
            });
        }
        Ok(Self {
            entries: result, lease: SourceLease(area.into()), acquisition: None,
        })
    }
}

impl fmt::Debug for SourceSnapshot {
    ///
    /// 本文や元入力・一時領域パスを表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 件数だけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SourceSnapshot")
            .field("entries", &self.entries.len()).finish()
    }
}

///
/// ZIP/ローカルGitの取得全体を一つのblocking作業で実行する。
///
/// # 引数
/// * `spec` - 検証済み入力指定
/// * `limits` - 取得用上限
/// * `stop` - 期限と協調停止境界
///
/// # 戻り値
/// 全取得成功時だけ返すsnapshot。四方式の呼び分けはacquireが行う。
///
pub(crate) async fn acquire_local(
    spec: SourceSpec, limits: AcquisitionLimits, stop: StopContext,
) -> Result<SourceSnapshot, SourceError> {
    runtime::run(stop, move |stop| match spec {
        SourceSpec::Zip(path) => zip::acquire(&path, limits, &stop),
        SourceSpec::LocalGit { path, selector } =>
            git::acquire(&path, selector, limits, &stop),
        _ => Err(SourceError::NotAvailable),
    }).await
}

///
/// SourceSpecの四方式を方式固有の取得境界へ渡す。
///
/// # 概要
/// 未完成snapshotを返さず、全entryと本文leaseの所有権を一体で返す。
/// tokenはGitHub指定だけへ渡し、通常HTTPS Gitへの認証に流用しない。
///
/// # 引数
/// * `spec` - 検証済み入力方式と参照方針
/// * `token` - GITHUB_TOKEN由来の任意の秘密値
/// * `limits` - 取得用上限
/// * `stop` - 全体と共有する期限と取消
///
/// # 戻り値
/// 共通本文検証・投影前の所有されたsnapshot
///
pub(crate) async fn acquire(spec: SourceSpec, token: Option<&Secret>,
    limits: AcquisitionLimits, stop: StopContext)
    -> Result<SourceSnapshot, SourceError>
{
    stop.checkpoint()?;
    /*
     * 方式固有の同期/非同期境界を維持して呼び分ける
     */
    match spec {
        SourceSpec::Zip(_) | SourceSpec::LocalGit { .. } =>
            acquire_local(spec, limits, stop).await,
        SourceSpec::HttpsGit { url, selector } =>
            acquire_https(url, selector, limits, stop).await,
        SourceSpec::GitHub { owner, repo, selector } =>
            github::acquire(owner, repo, selector, token, limits, stop).await,
    }
}

///
/// 検証済み設定を共通取得入口へ渡し、失敗をReport用分類へ変換する。
///
/// # 引数
/// * `config` - OP2で検証済みの設定・上限・期限・認証
///
/// # 戻り値
/// 成功snapshotまたはReport::failへ渡せる失敗
///
pub(crate) async fn acquire_for_report(config: &ResolvedRunConfig)
    -> Result<SourceSnapshot, Failure>
{
    acquire(config.source().clone(), config.github_token(),
        AcquisitionLimits::from(&config.settings().limits),
        StopContext::new(config.deadline())).await
        .map_err(SourceError::into_failure)
}

///
/// HTTPS Gitの取得全体を一つのblocking作業で実行する。
///
/// # 引数
/// * `url` - 資格情報を含まないHTTPS入力URL
/// * `selector` - コミット参照方針
/// * `limits` - 取得用上限
/// * `stop` - 共有期限と協調停止境界
///
/// # 戻り値
/// 全取得成功時だけ返す所有されたsnapshot
///
pub(crate) async fn acquire_https(
    url: url::Url, selector: crate::cmd_args::resolve::source::GitSelector,
    limits: AcquisitionLimits, stop: StopContext,
) -> Result<SourceSnapshot, SourceError> {
    runtime::run(stop, move |stop| {
        remote::acquire(&url, selector, limits, &stop)
    }).await
}

#[cfg(test)]
mod tests;
