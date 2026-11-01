/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 検証済み計画とreadonly Fileをworkerに所有させる単発送信stream
//!

use std::future::Future;
use std::io::{self, Read};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll};

use futures_core::Stream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::projection::PlannedBody;
use crate::source::StopContext;

use super::types::FailureKind;

type Piece = Result<Vec<u8>, FailureKind>;

///
/// 同時read要求1個と終了通知だけの内部workerプロトコル。
///
enum Command { Read(oneshot::Sender<Piece>), Stop }

///
/// workerとcoordinatorとstreamが共有するoperation内終了状態。
///
struct State {
    ended: AtomicBool,
    delivered: AtomicU64,
    sender: mpsc::Sender<Command>,
}

impl State {
    ///
    /// 停止通知を一度だけ送りblocking recvを解放する。
    ///
    /// # 戻り値
    /// なし。共通取消を発生させない。
    ///
    fn finish(&self) {
        if !self.ended.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(Command::Stop);
        }
    }
}

///
/// 正常結果でもworker終了を確認するcoordinator所有者。
///
pub(super) struct Upload {
    state: Arc<State>,
    worker: Option<JoinHandle<Result<(), FailureKind>>>,
    bytes: u64,
    /// workerのjoinと実終了を対応付ける匿名観測ID。
    worker_id: u64,
}

impl Upload {
    ///
    /// 全量を収集せずworker内のreadonly準備完了を待つ。
    ///
    /// # 引数
    /// * `body` - 計画に結び付いた検証済み本文
    /// * `stop` - 同じ期限と取消
    /// * `permit` - worker終了まで所有するHTTP permit
    ///
    /// # 戻り値
    /// worker所有者と要求body、または安全な準備失敗
    ///
    pub(super) async fn prepare(body: PlannedBody, stop: StopContext,
        permit: impl Into<crate::measurement::Permit>)
        -> Result<(Self, reqwest::Body), FailureKind>
    {
        let (owner, stream) = Self::prepare_stream(body, stop, permit).await?;
        Ok((owner, reqwest::Body::wrap_stream(stream)))
    }

    ///
    /// workerを準備し、同時read一つのstreamへ接続する。
    ///
    /// # 概要
    /// blocking workerへ計画・File・permitを所有させて準備を待つ。
    ///
    /// # 引数
    /// * `body` - 検証済み本文所有者
    /// * `stop` - 共通停止
    /// * `permit` - worker所有permit
    ///
    /// # 戻り値
    /// coordinatorと内部stream
    ///
    async fn prepare_stream(body: PlannedBody, stop: StopContext,
        permit: impl Into<crate::measurement::Permit>)
        -> Result<(Self, BodyStream), FailureKind>
    {
        let permit = permit.into();
        super::check(&stop)?;
        let bytes = body.byte_len();
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(State { ended: AtomicBool::new(false),
            delivered: AtomicU64::new(0), sender });
        let (prepared, ready) = oneshot::channel();
        let worker_state = Arc::clone(&state);
        let worker_stop = stop.clone();
        /*
         * workerがplan・File・permitを実I/O終了まで一体で所有する
         */
        #[cfg(test)]
        let action = tests::take_action();
        let worker_id = crate::measurement::id();
        let context_id = worker_stop.trace_id();
        crate::measurement::event(crate::measurement::Event::WorkerScheduled,
            worker_id, context_id, crate::measurement::Role::Upload, 0);
        let worker = tokio::task::spawn_blocking(move || {
            let _scope = crate::measurement::Span::new(
                crate::measurement::Event::WorkerStart,
                crate::measurement::Event::WorkerEnd, worker_id, context_id,
                crate::measurement::Role::Upload);
            let _permit = permit;
            let result = super::check(&worker_stop)
                .and_then(|_| body.open().map_err(|_| FailureKind::Io));
            let file = match result {
                Ok(file) => { let _ = prepared.send(Ok(())); file }
                Err(kind) => {
                    let _ = prepared.send(Err(kind));
                    return Err(kind);
                }
            };
            let mut file = crate::measurement::file(file,
                crate::measurement::Role::Payload);
            #[cfg(test)]
            let action = tests::pause(action, &file);
            #[cfg(test)]
            tests::before_read(&action, &file)?;
            let mut read = 0_u64;
            /*
             * demandごとに一つだけ読み、結果送信でblocking待機しない
             */
            while let Ok(command) = receiver.recv() {
                let Command::Read(reply) = command else { break; };
                if worker_state.ended.load(Ordering::Acquire) { break; }
                let result = super::check(&worker_stop).and_then(|_| {
                    if worker_state.ended.load(Ordering::Acquire) {
                        return Ok(Vec::new());
                    }
                    #[cfg(not(test))]
                    let width = super::receive::WORK_CHUNK;
                    #[cfg(test)]
                    let width = tests::width(&action);
                    let mut piece = vec![0; width];
                    let count = file.read(&mut piece)
                        .map_err(|_| FailureKind::Io)?;
                    super::check(&worker_stop)?;
                    if worker_state.ended.load(Ordering::Acquire) {
                        return Ok(Vec::new());
                    }
                    read = read.checked_add(count as u64)
                        .ok_or(FailureKind::Io)?;
                    if read > bytes || (count == 0 && read != bytes) {
                        return Err(FailureKind::Io);
                    }
                    piece.truncate(count);
                    Ok(piece)
                });
                let failure = result.as_ref().err().copied();
                let done = !matches!(&result, Ok(piece) if !piece.is_empty());
                let sent = reply.send(result).is_ok();
                if let Some(kind) = failure { return Err(kind); }
                if !sent || done { break; }
            }
            #[cfg(test)]
            tests::after_loop(action, &file)?;
            drop(file);
            drop(body);
            Ok(())
        });
        let mut upload = Self { state: Arc::clone(&state),
            worker: Some(worker), bytes, worker_id };
        /*
         * 共有停止と準備を競合させ、通常失敗でもworkerを回収する
         */
        let result = tokio::select! {
            result = ready => result.map_err(|_| FailureKind::Worker)
                .and_then(|result| result),
            _ = stop.wait_stopped() => super::check(&stop),
        }.and_then(|_| super::check(&stop));
        if let Err(kind) = result {
            let _ = upload.join().await;
            return Err(kind);
        }
        let stream = BodyStream { state, stop, pending: None, done: false };
        Ok((upload, stream))
    }

    ///
    /// 全byteをStream呼出し側へ返した事実だけを確認する。
    ///
    /// # 戻り値
    /// 期待量を引き渡したならtrue。wire送信/サーバー受領は推測しない。
    ///
    pub(super) fn complete(&self) -> bool {
        self.state.delivered.load(Ordering::Acquire) == self.bytes
    }

    ///
    /// operationを終えてworkerの実I/O終了を待つ。
    ///
    /// # 戻り値
    /// 終了確認または安全なworker失敗
    ///
    pub(super) async fn join(&mut self) -> Result<(), FailureKind> {
        self.state.finish();
        if let Some(worker) = self.worker.take() {
            crate::measurement::event(crate::measurement::Event::JoinStart,
                self.worker_id, 0, crate::measurement::Role::Upload, 0);
            let joined = worker.await;
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                self.worker_id, 0, crate::measurement::Role::Upload, 0);
            joined.map_err(|_| FailureKind::Worker)?
        } else { Ok(()) }
    }
}

impl Drop for Upload {
    ///
    /// future破棄でも待機を解放しworkerの所有物を先に削除しない。
    ///
    /// # 戻り値
    /// なし。JoinHandle破棄後もworkerは所有物を保持する。
    ///
    fn drop(&mut self) { self.state.finish(); }
}

///
/// 一つのread結果だけを待ち、prefetchしない要求body。
///
struct BodyStream {
    state: Arc<State>,
    stop: StopContext,
    pending: Option<oneshot::Receiver<Piece>>,
    done: bool,
}

impl Stream for BodyStream {
    type Item = Result<Vec<u8>, io::Error>;

    ///
    /// worker結果を返す時点で引渡し量を記録する。
    ///
    /// # 概要
    /// 一つのread結果だけを待ち、停止後の追加readや引渡しを拒否する。
    ///
    /// # 引数
    /// * `context` - 起床登録先
    ///
    /// # 戻り値
    /// 一つのpiece、EOF、または値なしのstream失敗
    ///
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>)
        -> Poll<Option<Self::Item>>
    {
        let this = self.get_mut();
        if this.done { return Poll::Ready(None); }
        /*
         * 停止後はworker要求も新しいHTTP用byte引渡しも行わない
         */
        if this.state.ended.load(Ordering::Acquire)
            || super::check(&this.stop).is_err()
        {
            this.done = true;
            this.state.finish();
            return Poll::Ready(Some(Err(io::ErrorKind::Interrupted.into())));
        }
        if this.pending.is_none() {
            let (sender, receiver) = oneshot::channel();
            if this.state.sender.send(Command::Read(sender)).is_err() {
                this.done = true;
                return Poll::Ready(Some(Err(io::ErrorKind::BrokenPipe.into())));
            }
            this.pending = Some(receiver);
        }
        /*
         * 完了済みworker結果でも未消費pieceを完了量に数えない
         */
        let result = match Pin::new(this.pending.as_mut().unwrap())
            .poll(context)
        {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        this.pending = None;
        match result {
            Ok(Ok(piece)) if piece.is_empty() => {
                this.done = true;
                Poll::Ready(None)
            }
            Ok(Ok(piece)) => {
                this.state.delivered.fetch_add(piece.len() as u64,
                    Ordering::Release);
                Poll::Ready(Some(Ok(piece)))
            }
            _ => {
                this.done = true;
                Poll::Ready(Some(Err(io::ErrorKind::Other.into())))
            }
        }
    }
}

impl Drop for BodyStream {
    ///
    /// Content-LengthによるEOF未pollも正常にworker終了へ接続する。
    ///
    /// # 戻り値
    /// なし。完了はEOFでなく引渡し量で判定する。
    ///
    fn drop(&mut self) { self.state.finish(); }
}

#[cfg(test)]
pub(in crate::wiki_client) mod tests;
