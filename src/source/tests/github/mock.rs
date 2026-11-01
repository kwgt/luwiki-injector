/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 実tokenを記録しないloopback専用GitHub模擬API
//!

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

///
/// mockの固定応答と任意の遅延。
///
#[derive(Clone)]
pub(super) struct Reply {
    status: u16,
    body: String,
    location: Option<String>,
    delay: Duration,
}

impl Reply {
    ///
    /// JSONの固定応答を構成する。
    ///
    /// # 引数
    /// * `status` - 応答status
    /// * `body` - テスト専用JSON
    ///
    /// # 戻り値
    /// 固定応答
    ///
    pub(super) fn json(status: u16, body: String) -> Self {
        Self { status, body, location: None, delay: Duration::ZERO }
    }

    ///
    /// redirect禁止試験の応答を構成する。
    ///
    /// # 引数
    /// * `location` - loopback内の遷移先
    ///
    /// # 戻り値
    /// 302応答
    ///
    pub(super) fn redirect(location: String) -> Self {
        Self { location: Some(location), ..Self::json(302,
            "{\"message\":\"dummy-secret\"}".into()) }
    }

    ///
    /// 停止検査用に応答を遅延させる。
    ///
    /// # 引数
    /// * `delay` - テストの待機時間
    ///
    /// # 戻り値
    /// 遅延したrepo応答
    ///
    pub(super) fn delayed(delay: Duration) -> Self {
        Self { delay, ..Self::json(200, "{}".into()) }
    }
}

///
/// 要求pathとダミー認証一致の真偽だけを記録する。
///
struct Requests { paths: Vec<String>, authenticated: bool }

///
/// テスト所有のAPI serverと静的応答・安全な要求記録。
///
pub(super) struct MockApi {
    base: String,
    replies: Arc<Mutex<HashMap<String, Reply>>>,
    requests: Arc<Mutex<Requests>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockApi {
    ///
    /// loopbackの空きportに固定応答serverを作る。
    ///
    /// # 概要
    /// 全要求を直列処理し、header本文は保存しない。
    ///
    /// # 引数
    /// * `initial` - pathごとの応答
    ///
    /// # 戻り値
    /// Dropで終了するmock
    ///
    pub(super) fn new(initial: Vec<(String, Reply)>) -> Self {
        /*
         * 独立したloopback listenerと記録領域を用意する
         */
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        listener.set_nonblocking(true).expect("mock nonblocking");
        let replies = Arc::new(Mutex::new(initial.into_iter().collect()));
        let requests = Arc::new(Mutex::new(Requests {
            paths: Vec::new(), authenticated: false,
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_replies = replies.clone();
        let worker_requests = requests.clone();
        let worker_stop = stopped.clone();
        /*
         * 実tokenを持たないserverを開始し、終了通知を定期確認する
         */
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &worker_replies,
                        &worker_requests),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
                        => std::thread::sleep(Duration::from_millis(2)),
                    Err(_) => break,
                }
            }
        });
        Self { base: format!("http://{address}"), replies, requests, stopped,
            worker: Some(worker) }
    }

    ///
    /// 固定応答を追加・変更する。
    ///
    /// # 引数
    /// * `path` - mock path
    /// * `reply` - mock応答
    ///
    /// # 戻り値
    /// なし
    ///
    pub(super) fn reply(&self, path: &str, reply: Reply) {
        self.replies.lock().expect("mock replies")
            .insert(path.to_owned(), reply);
    }

    ///
    /// test専用originを取得する。
    ///
    /// # 戻り値
    /// loopback HTTP origin
    ///
    pub(super) fn base(&self) -> String { self.base.clone() }

    ///
    /// 秘密情報を含まない要求pathを取得する。
    ///
    /// # 戻り値
    /// 到達順のpath
    ///
    pub(super) fn paths(&self) -> Vec<String> {
        self.requests.lock().expect("mock requests").paths.clone()
    }

    ///
    /// ダミーbearerに一致した要求の存在を取得する。
    ///
    /// # 戻り値
    /// 一度でもダミー認証が確認できればtrue
    ///
    pub(super) fn authenticated(&self) -> bool {
        self.requests.lock().expect("mock requests").authenticated
    }
}

impl Drop for MockApi {
    ///
    /// server終了とthread回収を行う。
    ///
    /// # 戻り値
    /// なし。失敗したclientの接続も読込期限で終了する。
    ///
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("mock worker終了");
        }
    }
}

///
/// 一件のGETを上限付きで読み、秘密値を記録せず固定応答を返す。
///
/// # 概要
/// 受信したheaderはその場で真偽へ写し、保存・表示しない。
///
/// # 引数
/// * `stream` - loopback接続
/// * `replies` - 固定応答表
/// * `requests` - pathと認証有無だけの記録
///
/// # 戻り値
/// なし
///
fn serve(mut stream: TcpStream, replies: &Mutex<HashMap<String, Reply>>,
    requests: &Mutex<Requests>)
{
    /*
     * 受信上限と期限を設け、headerまで読み込む
     */
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while bytes.len() < 16 * 1024 {
        let count = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|v| v == b"\r\n\r\n") { break; }
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return,
    };
    let path = text.lines().next().unwrap_or("").split_whitespace()
        .nth(1).unwrap_or("").to_owned();
    /*
     * 認証headerそのものは保存せず、ダミー値との一致だけを記録する
     */
    let authenticated = text.lines().any(|line| {
        let Some((key, value)) = line.split_once(':') else { return false; };
        key.eq_ignore_ascii_case("authorization")
            && value.trim() == "Bearer dummy-github-secret"
    });
    {
        let mut record = requests.lock().expect("mock requests");
        record.paths.push(path.clone());
        record.authenticated |= authenticated;
    }
    let reply = replies.lock().expect("mock replies").get(&path).cloned()
        .unwrap_or_else(|| Reply::json(404,
            "{\"message\":\"missing\"}".into()));
    /*
     * 接続を閉じる固定応答と任意の遅延だけを処理する
     */
    std::thread::sleep(reply.delay);
    let location = reply.location.map(|v| format!("Location: {v}\r\n"))
        .unwrap_or_default();
    let response = format!("HTTP/1.1 {} Fixture\r\nContent-Type: \
        application/json\r\nContent-Length: {}\r\nConnection: close\r\n\
        {location}\r\n{}", reply.status, reply.body.len(), reply.body);
    let _ = stream.write_all(response.as_bytes());
}
