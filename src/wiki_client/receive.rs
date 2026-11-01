/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! Wiki受信だけのproducer/worker連携。通常失敗はglobal取消にしない
//!

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::report::types::{PageId, PagePath};
use crate::source::{CancelOnDrop, StopContext};

use super::json::{self, JsonValue, Purpose};
use super::store::{self, ReceivedBody};
use super::types::{Diagnostics, FailureKind};

pub(super) const WORK_CHUNK: usize = 64 * 1024;
const CHANNEL_CAPACITY: usize = 1;

///
/// workerの処理目的。本文はcaller既知IDと結び付ける。
///
pub(super) enum Job {
    Id, CreatedId, Metadata { id: PageId, expected: PagePath },
    Body(PageId), Error, Discard,
}

///
/// workerが全受信後に返す必要値。
///
pub(super) enum Received {
    Json(JsonValue), Body(ReceivedBody), Empty,
}

///
/// 正常EOFとchannel破棄を混同しないmessage。
///
enum Message { Data(Vec<u8>), End }

///
/// 現piece一つとchannelだけを持つ逐次reader。
///
struct ChannelReader {
    receiver: mpsc::Receiver<Message>,
    piece: Vec<u8>,
    position: usize,
    ended: bool,
    stop: StopContext,
}

impl Read for ChannelReader {
    ///
    /// 未処理pieceを返し、明示終端だけをEOFとする。
    ///
    /// # 概要
    /// sender破棄は成功EOFではなくローカル失敗としてworkerへ伝える。
    ///
    /// # 引数
    /// * `output` - workerの作業buffer
    ///
    /// # 戻り値
    /// 現pieceの読込量、または値を含めないI/O失敗
    ///
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() { return Ok(0); }
        /*
         * 停止を確認し、未処理pieceか明示終端を受け取る
         */
        while self.position == self.piece.len() {
            super::check(&self.stop).map_err(|_| io::ErrorKind::Interrupted)?;
            if self.ended { return Ok(0); }
            match self.receiver.blocking_recv() {
                Some(Message::Data(piece)) => {
                    self.piece = piece;
                    self.position = 0;
                }
                Some(Message::End) => self.ended = true,
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
        /*
         * pieceを重複せずworkerへ渡す
         */
        let count = output.len().min(self.piece.len() - self.position);
        output[..count].copy_from_slice(
            &self.piece[self.position..self.position + count],
        );
        self.position += count;
        Ok(count)
    }
}

///
/// HTTP chunkを有限pieceへ分割し、明示終端を渡す。
///
/// # 概要
/// channel送信中も停止へ競合させ、受信総量の上限は設けない。
///
/// # 引数
/// * `response` - 既にheaderを受け取った応答
/// * `sender` - 有界channel
/// * `stop` - 共有停止境界
/// * `transport_failed` - channel終了原因をworker失敗から区別する事実
///
/// # 戻り値
/// 正常EOFまたは安全な受信失敗
///
async fn pump(mut response: reqwest::Response, sender: mpsc::Sender<Message>,
    stop: StopContext, transport_failed: Arc<AtomicBool>)
    -> Result<(), FailureKind>
{
    /*
     * chunk受信と内部piece送信の双方で停止を待つ
     */
    loop {
        super::check(&stop)?;
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            _ = stop.wait_stopped() => return super::check(&stop),
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                transport_failed.store(true, Ordering::Release);
                return Err(FailureKind::Transport);
            }
        };
        #[cfg(not(test))]
        let piece_limit = WORK_CHUNK;
        #[cfg(test)]
        let piece_limit = super::tests::piece_limit();
        for piece in chunk.chunks(piece_limit) {
            super::check(&stop)?;
            let data = Message::Data(piece.to_vec());
            #[cfg(test)]
            super::tests::observe_piece(piece.len(), sender.max_capacity());
            tokio::select! {
                result = sender.send(data) =>
                    result.map_err(|_| FailureKind::Worker)?,
                _ = stop.wait_stopped() => return super::check(&stop),
            }
        }
    }
    /*
     * channel閉鎖をEOF扱いせず、完了messageを明示する
     */
    super::check(&stop)?;
    tokio::select! {
        result = sender.send(Message::End) =>
            result.map_err(|_| FailureKind::Worker),
        _ = stop.wait_stopped() => super::check(&stop),
    }
}

///
/// worker内の同期検証・保管を実行する。
///
/// # 概要
/// 処理目的に従い本文保管・JSON選択抽出・照会不存在の読み捨てを行う。
///
/// # 引数
/// * `input` - producerからの逐次reader
/// * `job` - 有限処理目的
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 完全受信済みの必要値、または安全な失敗
///
fn work(mut input: ChannelReader, job: Job, stop: StopContext)
    -> Result<Received, FailureKind>
{
    /*
     * 全量収集せず処理目的に対応するworkerへ逐次入力を渡す
     */
    match job {
        Job::Body(id) => store::receive(&mut input, id, &stop)
            .map(Received::Body),
        Job::Id => json::parse(input, Purpose::Id, stop).map(Received::Json),
        Job::CreatedId => json::parse(input, Purpose::CreatedId, stop)
            .map(Received::Json),
        Job::Metadata { id, expected } =>
            json::parse(input, Purpose::Metadata { id, expected }, stop)
                .map(Received::Json),
        Job::Error => json::parse(input, Purpose::Error, stop)
            .map(Received::Json),
        Job::Discard => {
            /*
             * lookupの404/410でもHTTP EOFと共有停止を検査する
             */
            let mut bytes = [0; WORK_CHUNK];
            while input.read(&mut bytes).map_err(|_| FailureKind::Io)? != 0 {
                super::check(&stop)?;
            }
            Ok(Received::Empty)
        }
    }
}

///
/// producerとworkerの終了を連携し、通常失敗では共通取消をしない。
///
/// # 概要
/// workerの早期拒否でもproducerを閉じる。期限ではworker終了を待つ。
///
/// # 引数
/// * `response` - 単発HTTP応答
/// * `job` - 受信目的
/// * `stop` - 同一期限と取消
/// * `permit` - worker終了まで共有所有するHTTP permit
///
/// # 戻り値
/// 全受信成功値、または型付き通常失敗
///
async fn coordinate(response: reqwest::Response, job: Job, stop: StopContext,
    permit: impl Into<crate::measurement::Permit>)
    -> Result<Received, FailureKind>
{
    super::check(&stop)?;
    #[cfg(test)]
    super::tests::notify_receive_started();
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let transport_failed = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let preserve_id = matches!(&job, Job::CreatedId);
    #[cfg(test)]
    let action = super::tests::take_worker_action();
    #[cfg(test)]
    let json_stop = super::tests::writes::take_json_stop();
    /*
     * workerがreader・保管・parser stateを一体で所有する
     */
    let permit = permit.into();
    let worker_id = crate::measurement::id();
    let context_id = stop.trace_id();
    crate::measurement::event(crate::measurement::Event::WorkerScheduled,
        worker_id, context_id, crate::measurement::Role::Receive, 0);
    let mut worker = tokio::task::spawn_blocking(move || {
        /*
         * workerの観測区間を開始し、permitを処理終了まで保持する
         */
        let _scope = crate::measurement::Span::new(
            crate::measurement::Event::WorkerStart,
            crate::measurement::Event::WorkerEnd, worker_id, context_id,
            crate::measurement::Role::Receive);
        let _permit = permit;
        #[cfg(test)]
        super::tests::writes::set_json_stop(json_stop);
        #[cfg(test)]
        super::tests::before_work(&action)?;
        /*
         * 受信readerと共有停止境界を渡して解析・保管を実行する
         */
        let result = work(ChannelReader {
            receiver, piece: Vec::new(), position: 0, ended: false,
            stop: worker_stop.clone(),
        }, job, worker_stop.clone());
        #[cfg(test)]
        super::tests::after_work(&result, action, &worker_stop);
        result
    });
    let mut producer = Box::pin(pump(response, sender, stop.clone(),
        Arc::clone(&transport_failed)));
    /*
     * 通常worker失敗をReadyとして受け、guardをglobal取消へ倒さない
     */
    crate::measurement::event(crate::measurement::Event::JoinStart,
        worker_id, context_id, crate::measurement::Role::Receive, 0);
    let result = tokio::select! {
        result = &mut worker => {
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                worker_id, context_id, crate::measurement::Role::Receive, 0);
            drop(producer);
            result.map_err(|_| FailureKind::Worker)?
        }
        produced = &mut producer => {
            drop(producer);
            /*
             * producer終了後もworkerを待ち、受信結果と失敗を対応付ける
             */
            let joined = worker.await;
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                worker_id, context_id, crate::measurement::Role::Receive, 0);
            let received = joined.map_err(|_| FailureKind::Worker)?;
            match produced {
                Ok(()) => received,
                Err(FailureKind::Worker) => received.and_then(|_|
                    Err(FailureKind::Worker)),
                Err(error) => Err(error),
            }
        }
        _ = stop.wait_stopped() => {
            stop.cancel();
            drop(producer);
            /*
             * 停止後のworker終了を待ち、作成済みIDだけは確定情報として保つ
             */
            let finished = worker.await;
            crate::measurement::event(crate::measurement::Event::JoinEnd,
                worker_id, context_id, crate::measurement::Role::Receive, 0);
            if preserve_id {
                if let Ok(Ok(value @ Received::Json(JsonValue::Id(_)))) =
                    finished
                { return Ok(value); }
            }
            super::check(&stop).and_then(|_| Err(FailureKind::Cancelled))
        }
    };
    if !preserve_id { super::check(&stop)?; }
    if transport_failed.load(Ordering::Acquire) {
        return Err(FailureKind::Transport);
    }
    result
}

///
/// actual future破棄だけをglobal取消へ写すWiki専用受信入口。
///
/// # 引数
/// * `response` - 確定headerを持つ応答
/// * `job` - 受信目的
/// * `stop` - 共通停止境界
/// * `permit` - future実破棄後もworkerが保持するHTTP permit
///
/// # 戻り値
/// 完全検証済み必要値、または通常失敗。どちらでもguardを解除する。
///
pub(super) async fn run(response: reqwest::Response, job: Job,
    stop: StopContext, permit: impl Into<crate::measurement::Permit>)
    -> Result<Received, FailureKind>
{
    let mut guard = CancelOnDrop::new(stop.clone());
    let result = coordinate(response, job, stop, permit).await;
    guard.disarm();
    result
}

///
/// error受信値から有限診断だけを取り出す。
///
/// # 引数
/// * `received` - 完全検証済み受信結果
///
/// # 戻り値
/// 任意の有限診断。本文は返さない。
///
pub(super) fn diagnostics(received: Received) -> Option<Diagnostics> {
    match received {
        Received::Json(JsonValue::Error(diagnostics)) => diagnostics,
        _ => None,
    }
}
