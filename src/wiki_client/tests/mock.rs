/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 外部へ接続しないloopback応答・分割・同期点fixture
//!

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use url::Url;

///
/// ダミー要求だけを観測する試験用記録。
///
#[derive(Clone)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) authorization: String,
    pub(super) lock: String,
    pub(super) content_type: String,
    pub(super) length: usize,
    pub(super) body: Vec<u8>,
}

///
/// 応答fixture。正常EOF・通信切断・body待機を区別する。
///
pub(super) struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
    truncated: bool,
    headers: Vec<(String, String)>,
    early: bool,
    no_headers: bool,
}

impl Reply {
    ///
    /// 固定statusとbodyを用意する。
    ///
    /// # 引数
    /// * `status` - 試験status
    /// * `body` - ダミー本文
    ///
    /// # 戻り値
    /// 固定応答fixture
    ///
    pub(super) fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status, body, location: None, gate: None, truncated: false,
            headers: Vec::new(), early: false, no_headers: false }
    }

    ///
    /// ダミー応答headerを追加し、重複fixtureも作る。
    ///
    /// # 引数
    /// * `name` - 固定header名
    /// * `value` - ダミー値
    ///
    /// # 戻り値
    /// header追加済みfixture
    ///
    pub(super) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    ///
    /// 要求本文を待たずに応答するfixtureへ変える。
    ///
    /// # 戻り値
    /// 早期応答fixture
    ///
    pub(super) fn early(mut self) -> Self { self.early = true; self }

    ///
    /// 要求を記録後、応答headerを送らず接続を閉じる。
    ///
    /// # 戻り値
    /// status未観測のtransport失敗fixture
    ///
    pub(super) fn without_headers(mut self) -> Self {
        self.no_headers = true;
        self
    }

    ///
    /// GET redirectを観察するfixtureを作る。
    ///
    /// # 引数
    /// * `location` - 同じloopbackへの相対先
    ///
    /// # 戻り値
    /// 302応答
    ///
    pub(super) fn redirect(location: &str) -> Self {
        let mut reply = Self::new(302, b"{}".to_vec());
        reply.location = Some(location.into());
        reply
    }

    ///
    /// 宣言量を満たさない通信切断へ変える。
    ///
    /// # 戻り値
    /// JSON構文の完結とHTTP EOFを区別するfixture
    ///
    pub(super) fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    ///
    /// header後のbody送信を同期点で止める。
    ///
    /// # 戻り値
    /// 応答と明示解除用sender
    ///
    pub(super) fn held(mut self) -> (Self, std::sync::mpsc::Sender<()>) {
        let (release, gate) = std::sync::mpsc::channel();
        self.gate = Some(gate);
        (self, release)
    }
}

///
/// reply queueとダミー要求の記録。
///
struct State { replies: VecDeque<Reply>, requests: Vec<Request> }

///
/// 終了時に全fixture threadを回収するloopbackサーバー。
///
pub(super) struct Server {
    url: Url,
    state: Arc<Mutex<State>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    ///
    /// 小規模loopbackを起動する。
    ///
    /// # 概要
    /// 完了通知後も待機済み接続を排出し、handlerをすべて回収する。
    ///
    /// # 引数
    /// * `replies` - 消費順のfixture
    ///
    /// # 戻り値
    /// 外部へ接続しないサーバー所有者
    ///
    pub(super) fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!("http://{}",
            listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(State {
            replies: replies.into(), requests: Vec::new(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let state_copy = Arc::clone(&state);
        let completed = Arc::clone(&done);
        /*
         * handlerは並行に受け、製品permitの直列化を独立に観察する
         */
        let thread = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            /*
             * 完了後もqueue内の接続を排出して取りこぼしを防ぐ
             */
            loop {
                match listener.accept() {
                    Ok((connection, _)) => {
                        let state = Arc::clone(&state_copy);
                        let done = Arc::clone(&completed);
                        handlers.push(std::thread::spawn(move ||
                            serve(connection, state, done)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
                    => {
                        if completed.load(Ordering::Acquire) { break; }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
            for handler in handlers { let _ = handler.join(); }
        });
        Self { url, state, done, thread: Some(thread) }
    }

    ///
    /// 認証を含まないloopback URLを返す。
    ///
    /// # 戻り値
    /// 試験接続先
    ///
    pub(super) fn url(&self) -> Url { self.url.clone() }

    ///
    /// 観測済み要求数を返す。
    ///
    /// # 戻り値
    /// 要求数
    ///
    pub(super) fn count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    ///
    /// ダミー要求の観測を返す。
    ///
    /// # 戻り値
    /// 試験内部だけで使う記録
    ///
    pub(super) fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for Server {
    ///
    /// サーバーとすべてのhandlerを終了する。
    ///
    /// # 戻り値
    /// なし。fixture threadを回収する。
    ///
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

///
/// ダミーGETを読み、header/bodyを別の時点に送る。
///
/// # 概要
/// bodyは小分割し、期限・取消中のreadとUTF-8分割を試せるようにする。
///
/// # 引数
/// * `connection` - loopback接続
/// * `state` - fixtureの所有queue
/// * `done` - サーバー所有者の終了通知
///
/// # 戻り値
/// なし。client破棄によるI/O失敗はfixture終了として扱う。
///
fn serve(mut connection: TcpStream, state: Arc<Mutex<State>>,
    done: Arc<AtomicBool>)
{
    let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = connection.set_write_timeout(Some(Duration::from_secs(2)));
    let mut headers = Vec::new();
    let mut piece = [0; 1024];
    /*
     * 小さい製品GETだけを記録し、ユーザー環境の認証を読まない
     */
    while !headers.windows(4).any(|value| value == b"\r\n\r\n") {
        let count = match connection.read(&mut piece) {
            Ok(0) | Err(_) => return, Ok(count) => count,
        };
        headers.extend_from_slice(&piece[..count]);
    }
    let boundary = headers.windows(4).position(|value|
        value == b"\r\n\r\n").unwrap() + 4;
    let text = String::from_utf8(headers[..boundary].to_vec()).unwrap();
    let mut first_line = text.lines().next().unwrap().split_whitespace();
    let method = first_line.next().unwrap().to_owned();
    let target = first_line.next().unwrap().to_owned();
    let authorization = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    }).unwrap_or_default();
    let field = |expected: &str| text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected).then(|| value.trim().to_owned())
    }).unwrap_or_default();
    let lock = field("x-lock-authentication");
    let content_type = field("content-type");
    let length = field("content-length").parse::<usize>().unwrap_or(0);
    let reply = {
        let mut state = state.lock().unwrap();
        state.replies.pop_front().unwrap_or_else(||
            Reply::new(503, b"{}".to_vec()))
    };
    let mut body = headers[boundary..].to_vec();
    /*
     * 固定Content-Lengthのダミー要求bytesを変換せず記録する
     */
    if !reply.early {
        while body.len() < length {
            let count = match connection.read(&mut piece) {
                Ok(0) | Err(_) => return, Ok(count) => count,
            };
            body.extend_from_slice(&piece[..count]);
        }
    }
    state.lock().unwrap().requests.push(Request { method, target,
        authorization, lock, content_type, length, body });
    /*
     * 確定headerと、未到着または途中切断bodyを区別する
     */
    if reply.no_headers { return; }
    let length = reply.body.len() + if reply.truncated { 17 } else { 0 };
    let location = reply.location.as_ref().map(|value|
        format!("Location: {value}\r\n")).unwrap_or_default();
    let extra: String = reply.headers.iter().map(|(name, value)|
        format!("{name}: {value}\r\n")).collect();
    let header = format!("HTTP/1.1 {} Fixture\r\nContent-Length: {length}\r\n\
        Content-Type: application/json\r\n{location}{extra}\
        Connection: close\r\n\r\n",
        reply.status);
    if connection.write_all(header.as_bytes()).is_err() { return; }
    if let Some(gate) = reply.gate {
        /*
         * 時間経過ではbodyを解放せず、明示解除か所有者終了だけを待つ
         */
        loop {
            if done.load(Ordering::Acquire) { return; }
            match gate.recv_timeout(Duration::from_millis(10)) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    for piece in reply.body.chunks(8191) {
        if connection.write_all(piece).is_err() { return; }
    }
}
