/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 子プロセスCLIの完全要求と順序を観察する専用loopback応答fixture
//!

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

///
/// 秘密値を使わない試験応答。
///
pub struct Reply {
    /// 確定したHTTP status。
    status: u16,
    /// 応答する合成本文。
    body: Vec<u8>,
    /// 任意の固定dummy lock token。
    token: Option<&'static str>,
}

impl Reply {
    ///
    /// 完全長の試験応答を作る。
    ///
    /// # 引数
    /// * `status` - fixtureのHTTP status
    /// * `body` - fixture本文bytes
    ///
    /// # 戻り値
    /// lockなしの合成応答
    ///
    pub fn new(status: u16, body: &[u8]) -> Self {
        Self { status, body: body.to_vec(), token: None }
    }

    ///
    /// lock付き応答へ固定dummy tokenを追加する。
    ///
    /// # 引数
    /// * `token` - この試験専用のULID
    ///
    /// # 戻り値
    /// lock取得・延長・draft作成用の応答
    ///
    pub fn locked(mut self, token: &'static str) -> Self {
        self.token = Some(token);
        self
    }

    ///
    /// 完全な要求本文を受信した接続にだけ応答する。
    ///
    /// # 概要
    /// 期限は試験時計から十分先にし、接続を閉じて一要求一応答とする。
    ///
    /// # 引数
    /// * `stream` - 完全要求を受信した専用接続
    ///
    /// # 戻り値
    /// なし。子の途中終了による切断はfixtureを停止しない。
    ///
    fn send(self, stream: &mut TcpStream) {
        /*
         * 完全長headerと任意lockを組み立てる
         */
        let mut header = format!("HTTP/1.1 {} Fixture\r\n\
            Content-Length: {}\r\nConnection: close\r\n", self.status,
            self.body.len());
        if let Some(token) = self.token {
            let expire: chrono::DateTime<chrono::Utc> =
                (SystemTime::now() + Duration::from_secs(600)).into();
            header.push_str(&format!("X-Page-Lock: expire={} token={token}\r\n",
                expire.to_rfc3339()));
        }
        /*
         * 全headerの後で宣言した本文を送信する
         */
        header.push_str("\r\n");
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&self.body);
    }
}

///
/// 全受信済みの要求。生の認証headerを保持しない。
///
pub struct Request {
    /// HTTP method。
    pub method: String,
    /// percent encodingを保持するrequest target。
    pub target: String,
    /// 宣言長を読み切った合成入力本文。
    pub body: Vec<u8>,
    /// 固定dummy Bearer認証の一致だけを保持する。
    pub authenticated: bool,
    /// 独立lock認証headerの存在だけを保持する。
    pub lock_auth: bool,
}

impl Request {
    ///
    /// headerとContent-Length分の本文を読み切る。
    ///
    /// # 概要
    /// PUT本文を受け取る前に成功応答を返さず、余分な要求も記録する。
    ///
    /// # 引数
    /// * `stream` - この試験がacceptしたloopback接続
    ///
    /// # 戻り値
    /// 完全要求。途中終了や不正fixture要求はNone。
    ///
    fn read(stream: &mut TcpStream,
        expected: Option<&reqwest::header::HeaderValue>)
        -> Option<Self>
    {
        let mut bytes = Vec::new();
        /*
         * header終端まで受信して必要な有限情報だけを取り出す
         */
        let end = loop {
            let mut chunk = [0; 4096];
            let size = stream.read(&mut chunk).ok()?;
            if size == 0 { return None; }
            bytes.extend_from_slice(&chunk[..size]);
            if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let header = std::str::from_utf8(&bytes[..end]).ok()?;
        let mut first = header.lines().next()?.split_ascii_whitespace();
        let method = first.next()?.to_owned();
        let target = first.next()?.to_owned();
        let mut length = 0;
        let mut authenticated = false;
        let mut lock_auth = false;
        /*
         * 生の認証値を保存せず合成認証の一致だけを記録する
         */
        for line in header.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().ok()?;
                }
                if name.eq_ignore_ascii_case("authorization") {
                    authenticated = matches!(value.trim(),
                        "Bearer unique-wiki-secret" | "Bearer unique-cli-secret"
                            | "Bearer unique-secret");
                    if let Some(expected) = expected {
                        authenticated = expected.as_bytes()
                            == value.trim().as_bytes();
                    }
                }
                if name.eq_ignore_ascii_case("x-lock-authentication") {
                    lock_auth = true;
                }
            }
        }
        /*
         * 同時受信分を含む全宣言bytesを読み切る
         */
        while bytes.len() < end + length {
            let mut chunk = [0; 4096];
            let size = stream.read(&mut chunk).ok()?;
            if size == 0 { return None; }
            bytes.extend_from_slice(&chunk[..size]);
        }
        Some(Self { method, target, body: bytes[end..end + length].to_vec(),
            authenticated, lock_auth })
    }
}

///
/// 専用socketと応答列、要求観測の寿命を所有する。
///
pub struct Server {
    /// 他のサービスへ接続しないloopback URL。
    pub url: String,
    /// 完全要求の発生順の列。
    requests: Arc<Mutex<Vec<Request>>>,
    /// 子終了後の監視停止flag。
    done: Arc<AtomicBool>,
    /// この試験だけのserver thread。
    task: Option<JoinHandle<()>>,
}

impl Server {
    ///
    /// 応答列が尽きた後も余分な要求を観察して503を返す。
    ///
    /// # 概要
    /// 子プロセスとは別threadで逐次応答し、外部接続や共有portを使わない。
    ///
    /// # 引数
    /// * `replies` - 発生順の完全応答
    ///
    /// # 戻り値
    /// drop時に専用threadを回収するserver
    ///
    pub fn new(replies: Vec<Reply>) -> Self {
        Self::prepare(replies, None)
    }

    ///
    /// 合成認証の一致だけを保存するloopbackを作る。
    ///
    /// # 引数
    /// * `replies` - 固定応答列
    /// * `authorization` - 合成header。生値を要求記録へ保存しない。
    ///
    /// # 戻り値
    /// 独立portと監視threadの所有者
    ///
    #[allow(dead_code)]
    pub fn with_auth(replies: Vec<Reply>,
        mut authorization: reqwest::header::HeaderValue) -> Self
    {
        authorization.set_sensitive(true);
        Self::prepare(replies, Some(authorization))
    }

    ///
    /// 既存dummyまたは指定合成認証でloopbackを準備する。
    ///
    /// # 概要
    /// 実資格情報を受ける入口ではなく、固定応答を一回ずつ送る。
    ///
    /// # 引数
    /// * `replies` - 固定応答列
    /// * `expected` - 合成認証の任意期待値
    ///
    /// # 戻り値
    /// drop時にthreadを回収するloopback
    ///
    fn prepare(replies: Vec<Reply>,
        expected: Option<reqwest::header::HeaderValue>) -> Self
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let received = Arc::clone(&requests);
        let finished = Arc::clone(&done);
        let mut replies = VecDeque::from(replies);
        /*
         * 完了後も待機済み接続を排出してから監視を閉じる
         */
        let task = std::thread::spawn(move || loop {
            /*
             * 完全要求だけを記録し、定義済み応答を一度ずつ返す
             */
            match listener.accept() {
                /*
                 * 子の実要求を完全受信した後だけ応答する
                 */
                Ok((mut stream, _)) => {
                    let timeout = Some(Duration::from_secs(5));
                    stream.set_read_timeout(timeout).unwrap();
                    stream.set_write_timeout(timeout).unwrap();
                    if let Some(request) = Request::read(&mut stream,
                        expected.as_ref())
                    {
                        received.lock().unwrap().push(request);
                        replies.pop_front().unwrap_or_else(||
                            Reply::new(503, b"{}")).send(&mut stream);
                    }
                }
                /*
                 * 子終了まで監視し、余分な要求の存在も検出する
                 */
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    if finished.load(Ordering::Relaxed) { break; }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        });
        Self { url, requests, done, task: Some(task) }
    }

    ///
    /// 完全要求列だけを短い同期参照で検査する。
    ///
    /// # 引数
    /// * `inspect` - 合成要求を照合する処理
    ///
    /// # 戻り値
    /// 検査処理の戻り値
    ///
    pub fn inspect<T>(&self, inspect: impl FnOnce(&[Request]) -> T) -> T {
        inspect(&self.requests.lock().unwrap())
    }
}

impl Drop for Server {
    ///
    /// この試験の監視threadだけを停止して回収する。
    ///
    /// # 戻り値
    /// なし。実サービスの接続を操作しない。
    ///
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() { task.join().unwrap(); }
    }
}
