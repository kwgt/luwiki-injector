/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! workerの停止・所有権・一時領域回収を検証する
//!

use std::future::Future;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tempfile::tempdir;

use crate::cmd_args::resolve::source::{GitSelector, LocalSelector, SourceSpec};
use crate::report::types::SourceKind;
use crate::source::store::Builder;
use crate::source::{EntryKind, SourceError, SourceSnapshot, StopContext};

use super::{GitFixture, body, limits, stop};

///
/// 原エラーの文字列を外へ出さないことを試すreader。
///
struct BrokenReader;

impl Read for BrokenReader {
    ///
    /// ダミー秘密値を含むI/O失敗を返す。
    ///
    /// # 引数
    /// * `_buffer` - 使用しない読込先
    ///
    /// # 戻り値
    /// 原因文字列を表示させてはいけないI/Oエラー
    ///
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("unique-secret-body-error"))
    }
}

///
/// 途中停止を通知するreader。
///
struct CancellingReader(StopContext);

impl Read for CancellingReader {
    ///
    /// 読込途中に停止を要求する。
    ///
    /// # 引数
    /// * `buffer` - 読込先
    ///
    /// # 戻り値
    /// 一バイトだけ返し、直後の停止確認を試す。
    ///
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        buffer[0] = b'x';
        self.0.cancel();
        Ok(1)
    }
}

///
/// 期限超過時もworker終了とlease回収を待つ。
///
/// # 概要
/// 途中abortできない同期操作を模し、返却前の回収を観察する。
///
/// # 注記
/// workerが領域を所有してsleepし、終了後のflagと領域不在を確認する。
///
/// # 戻り値
/// なし。期限失敗返却前のworker終了やlease回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn deadline_waits_for_worker_and_reclaims_lease() {
    let path = Arc::new(Mutex::new(None::<PathBuf>));
    let ended = Arc::new(AtomicBool::new(false));
    let worker_path = Arc::clone(&path);
    let worker_ended = Arc::clone(&ended);
    let context = StopContext::new(Instant::now() + Duration::from_millis(10));
    let result = crate::source::runtime::run(context, move |context| {
        let mut builder = Builder::new(SourceKind::Zip, 100, "size", 100)?;
        *worker_path.lock().expect("観察パス") = Some(builder.storage_path());
        /*
         * 即時中断できない同期操作中もworkerが領域を所有する
         */
        std::thread::sleep(Duration::from_millis(40));
        let result = builder.append(b"page.md", EntryKind::File,
            &mut Cursor::new(b"body"), 4, &context);
        drop(builder);
        worker_ended.store(true, Ordering::SeqCst);
        result
    }).await;
    assert_eq!(result, Err(SourceError::Deadline));
    assert!(ended.load(Ordering::SeqCst));
    let path = path.lock().expect("パス").clone().expect("作業領域");
    assert!(!path.exists());
}

///
/// 入力エラーと上限超過の両経路で一時領域を回収する。
///
/// # 概要
/// worker側の未公開領域がエラー返却時に残らないことを観察する。
///
/// # 注記
/// 原因文字列を含むreader失敗とtiny上限をworkerへ渡す。
///
/// # 戻り値
/// なし。worker失敗・上限拒否時の秘密非露出や領域回収が異なればassertで失敗する。
///
#[tokio::test]
async fn failure_and_limit_paths_drop_unpublished_storage() {
    /*
     * I/O失敗と宣言上限拒否の両経路を独立して実行する
     */
    for limited in [false, true] {
        let path = Arc::new(Mutex::new(None::<PathBuf>));
        let observed = Arc::clone(&path);
        let result = crate::source::runtime::run(stop(), move |context| {
            let mut builder = Builder::new(SourceKind::Zip,
                if limited { 1 } else { 100 }, "size", 100)?;
            *observed.lock().expect("観察パス") = Some(builder.storage_path());
            if limited {
                builder.append(b"page.md", EntryKind::File,
                    &mut Cursor::new(b"body"), 4, &context)
            } else {
                builder.append(b"page.md", EntryKind::File,
                    &mut BrokenReader, 4, &context)
            }
        }).await;
        let error = result.err().expect("取得失敗");
        assert!(!format!("{error:?} {error}")
            .contains("unique-secret-body-error"));
        let path = path.lock().expect("パス").clone().expect("領域");
        assert!(!path.exists());
    }
}

///
/// chunk読込後の停止点で途中snapshotを破棄する。
///
/// # 注記
/// readerが停止を通知し、成功結果を返さないことを確認する。
///
/// # 戻り値
/// なし。読込途中の取消分類や未公開領域回収が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn cooperative_cancel_during_read_reclaims_storage() {
    let path = Arc::new(Mutex::new(None::<PathBuf>));
    let observed = Arc::clone(&path);
    let result = crate::source::runtime::run(stop(), move |context| {
        let mut builder = Builder::new(SourceKind::Zip, 100, "size", 100)?;
        *observed.lock().expect("パス") = Some(builder.storage_path());
        builder.append(b"page.md", EntryKind::File,
            &mut CancellingReader(context.clone()), 1, &context)
    }).await;
    assert_eq!(result, Err(SourceError::Cancelled));
    let path = path.lock().expect("パス").clone().expect("領域");
    assert!(!path.exists());
}

///
/// 呼出しfutureの破棄もworkerへ協調停止を通知する。
///
/// # 概要
/// Futureのabortはworkerの即時abortではなく、worker側回収を誘発する。
///
/// # 注記
/// worker開始を待って呼出しをabortし、後続回収を観察する。
///
/// # 戻り値
/// なし。future破棄後の共有取消・worker終了・領域回収が異なればassertで失敗する。
///
#[tokio::test]
async fn dropped_caller_signals_worker_and_preserves_owned_cleanup() {
    let entered = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let path = Arc::new(Mutex::new(None::<PathBuf>));
    let worker_entered = Arc::clone(&entered);
    let worker_ended = Arc::clone(&ended);
    let worker_path = Arc::clone(&path);
    let task = crate::source::runtime::run(stop(), move |context| {
        let builder = Builder::new(SourceKind::Zip, 100, "size", 100)?;
        *worker_path.lock().expect("パス") = Some(builder.storage_path());
        worker_entered.store(true, Ordering::SeqCst);
        while context.checkpoint().is_ok() {
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(builder);
        worker_ended.store(true, Ordering::SeqCst);
        Err::<SourceSnapshot, _>(SourceError::Cancelled)
    });
    let future = tokio::spawn(task);
    /*
     * worker開始後にfutureだけを破棄する
     */
    tokio::time::timeout(Duration::from_secs(2), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await.expect("worker開始");
    future.abort();
    let _ = future.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ended.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await.expect("worker回収");
    let path = path.lock().expect("パス").clone().expect("領域");
    assert!(!path.exists());
}

///
/// 停止済み指定を元入力へアクセスする前に拒否する。
///
/// # 注記
/// 存在しないパスでもI/O失敗より協調停止を優先する。
///
/// # 戻り値
/// なし。入力アクセス前の取消拒否が異なる場合はassertで失敗する。
///
#[tokio::test]
async fn cancellation_before_acquisition_does_not_publish_snapshot() {
    let context = stop();
    context.cancel();
    let result = crate::source::acquire(
        SourceSpec::Zip(PathBuf::from("missing.zip")), None, limits(), context,
    ).await;
    assert_eq!(result.err(), Some(SourceError::Cancelled));
}

///
/// 非Sendのgix clone値が同じblocking closureに収まることを確認する。
///
/// # 概要
/// file transportは試験専用で、製品のHTTPS許容条件を変更しない。
///
/// # 注記
/// local fixtureのclone準備・fetch・checkoutを同じworker内で完結する。
///
/// # 戻り値
/// なし。同一worker内のclone・fetch・checkoutから確定IDを取得できなければ失敗する。
///
#[tokio::test]
async fn clone_prepare_fetch_and_checkout_stay_inside_one_worker() {
    let fixture = GitFixture::new();
    let origin = fixture.path().to_path_buf();
    /*
     * 試験専用のfile transportで非Send値の寿命を確認する
     */
    let result = crate::source::runtime::run(stop(), move |context| {
        let destination = tempdir().map_err(|_| SourceError::Io)?;
        let url = format!("file://{}", origin.display());
        let mut prepare = gix::clone::PrepareFetch::new(
            url.as_str(), destination.path().join("repo"),
            gix::create::Kind::WithWorktree, gix::create::Options::default(),
            gix::open::Options::isolated(),
        ).map_err(|_| SourceError::InvalidGit)?
            .with_revision(Some("refs/heads/main"))
            .map_err(|_| SourceError::RefNotFound)?;
        let (mut checkout, _) = prepare.fetch_then_checkout(
            gix::progress::Discard, context.cancelled.as_ref(),
        ).map_err(|_| SourceError::InvalidGit)?;
        context.checkpoint()?;
        let (repo, _) = checkout.main_worktree(
            gix::progress::Discard, context.cancelled.as_ref(),
        ).map_err(|_| SourceError::InvalidGit)?;
        Ok(repo.head_commit().map_err(|_| SourceError::InvalidGit)?
            .id.to_string())
    }).await.expect("同一worker clone");
    assert!(!result.is_empty());
}

///
/// ローカル取得も公開APIから成功し、本文を保管する。
///
/// # 注記
/// gixの値をworker外へ出さずにsnapshotだけを取得する。
///
/// # 戻り値
/// なし。公開取得入口から返る所有済みsnapshotの本文が異なればassertで失敗する。
///
#[tokio::test]
async fn local_snapshot_crosses_worker_as_owned_data_only() {
    let fixture = GitFixture::new();
    let snapshot = crate::source::acquire(SourceSpec::LocalGit {
        path: fixture.path().to_path_buf(),
        selector: LocalSelector::Committed(GitSelector::MainThenMaster),
    }, None, limits(), stop()).await.expect("公開取得境界");
    assert_eq!(body(&snapshot, "pages/page.md"), b"main\n");
}

///
/// 登録済み複数waiterへcloneからの取消を取りこぼさず通知する。
///
/// # 概要
/// 各futureの最初のpollを同期点にし、Notify登録後に取消する。
///
/// # 戻り値
/// なし。全waiterの完了と停止分類を検査する。
///
/// # 注記
/// 16waiterの登録を確認してから同じcontextをcancelし、全完了を待つ。
///
#[tokio::test]
async fn cancellation_wakes_all_registered_waiters() {
    let shared = stop();
    let mut tasks = Vec::new();
    /*
     * 最初のpoll後の通知で待機登録を確認する
     */
    for _ in 0..16 {
        let context = shared.clone();
        let (ready, registered) = tokio::sync::oneshot::channel();
        tasks.push(tokio::spawn(async move {
            let future = context.wait_stopped();
            tokio::pin!(future);
            let mut ready = Some(ready);
            std::future::poll_fn(|cx| {
                let polled = future.as_mut().poll(cx);
                if let Some(ready) = ready.take() { let _ = ready.send(()); }
                polled
            }).await
        }));
        registered.await.unwrap();
    }
    /*
     * 登録済み全waiterを共通flagと通知で起こす
     */
    shared.cancel();
    for task in tasks {
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), task)
            .await.unwrap().unwrap(), SourceError::Cancelled);
    }
}

///
/// 取消とfuture登録の競合でもsticky flagを見落とさない。
///
/// # 戻り値
/// なし。通知時期ごとの停止分類を検査する。
///
/// # 注記
/// task初回poll前と直後の取消を繰返し、未登録時の通知もflagで拾う。
///
#[tokio::test]
async fn cancellation_before_or_during_wait_registration_is_sticky() {
    /*
     * 通知時期の違いを同じclone待機で検査する
     */
    for before in [true, false] {
        for _ in 0..32 {
            let shared = stop();
            if before { shared.cancel(); }
            let context = shared.clone();
            let task = tokio::spawn(async move {
                context.wait_stopped().await
            });
            if !before { shared.cancel(); }
            assert_eq!(tokio::time::timeout(Duration::from_secs(2), task)
                .await.unwrap().unwrap(), SourceError::Cancelled);
        }
    }
}

///
/// async待機が同じ絶対期限を用い、同期cancelもruntimeなしで使える。
///
/// # 戻り値
/// なし。期限一致と同期取消の分類を検査する。
///
/// # 注記
/// accessor値の一致、過去期限の即時検出、同期sticky更新を分けて確認。
///
#[tokio::test]
async fn stop_wait_preserves_deadline_and_synchronous_cancel() {
    let deadline = Instant::now();
    let context = StopContext::new(deadline);
    assert_eq!(context.deadline(), deadline);
    assert_eq!(context.wait_stopped().await, SourceError::Deadline);
    let context = stop();
    std::thread::spawn(move || {
        context.cancel();
        assert_eq!(context.checkpoint(), Err(SourceError::Cancelled));
    }).join().unwrap();
}
