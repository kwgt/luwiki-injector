/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 通常は全本文受領後に応答し、指定PUTだけ部分切断できるfixture
//!

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

// 固定dummy新token。製品時計/資格情報には接続しない。
pub(in crate::executor) const TOKEN2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
pub(in crate::executor) const TOKEN3: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAY";

///
/// 完全応答またはheader前喪失を指定する試験応答。
///
pub(in crate::executor) struct Reply {
    /// 応答するstatus。Noneは応答なし切断。
    pub(in crate::executor) status: Option<u16>,
    /// dummy応答の本文。
    pub(in crate::executor) body: Vec<u8>,
    /// dummy lock headerの有無。
    pub(in crate::executor) lock: bool,
    /// このdummy応答に含めるtoken。実値を使用しない。
    pub(in crate::executor) token: &'static str,
}

impl Reply {
    ///
    /// 完全204と指定dummy新tokenを返す。
    ///
    /// # 引数
    /// * `token` - 固定のdummy ULID
    ///
    /// # 戻り値
    /// 延長/取得の正常応答
    ///
    pub(in crate::executor) fn locked(token: &'static str) -> Self {
        Self { status: Some(204), body: Vec::new(), lock: true, token }
    }

    ///
    /// 完全長の単発応答を作る。
    ///
    /// # 引数
    /// * `status` - fixture status
    /// * `body` - dummy本文
    ///
    /// # 戻り値
    /// lock headerのない応答
    ///
    pub(in crate::executor) fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status: Some(status), body, lock: false, token: super::TOKEN }
    }
}

///
/// 試験内だけで保持する要求の有限構成とdummy bytes。
///
pub(in crate::executor) struct Request {
    /// HTTP method。
    pub(in crate::executor) method: String,
    /// percent encodingを保持したtarget。
    pub(in crate::executor) target: String,
    /// Content-Length分を受信した全dummy本文。
    pub(in crate::executor) body: Vec<u8>,
    /// 独立lock認証headerの存在だけを記録する。
    pub(in crate::executor) lock_auth: bool,
    /// 既知dummy tokenだけの識別。生header値は保持しない。
    pub(in crate::executor) lock_version: Option<u8>,
}

impl Request {
    ///
    /// headerと宣言本文量の双方を受信する。
    ///
    /// # 概要
    /// PUT全bytesを受信する前に204を返さずproducerの早期成功を作らない。
    ///
    /// # 引数
    /// * `stream` - 専用loopback接続
    ///
    /// # 戻り値
    /// 完全要求。途中接続終了はNone。
    ///
    async fn read(stream: &mut TcpStream) -> Option<Self> {
        let mut bytes = Vec::new();
        /*
         * dummy要求headerの末尾が揃うまで逐次受信する
         */
        let end = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 { return None; }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let header = std::str::from_utf8(&bytes[..end]).ok()?;
        let mut first = header.lines().next()?.split_ascii_whitespace();
        let method = first.next()?.to_owned();
        let target = first.next()?.to_owned();
        let mut length = 0;
        let mut lock_auth = false;
        let mut lock_version = None;
        /*
         * 秘密header値を保持せず本文量とlock header存在だけを読む
         */
        for line in header.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().ok()?;
                }
                if name.eq_ignore_ascii_case("x-lock-authentication") {
                    lock_auth = true;
                    lock_version = match value.trim() {
                        v if v == format!("token={}", super::TOKEN) => Some(1),
                        v if v == format!("token={TOKEN2}") => Some(2),
                        v if v == format!("token={TOKEN3}") => Some(3),
                        _ => None,
                    };
                }
            }
        }
        /*
         * headerと同じreadに入った本文を含め全宣言bytesを待つ
         */
        while bytes.len() < end + length {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 { return None; }
            bytes.extend_from_slice(&chunk[..count]);
        }
        Some(Self { method, target, body: bytes[end..end + length].to_vec(),
            lock_auth, lock_version })
    }
}

///
/// 部分要求の有限観測。本文やheader原文を保持しない。
///
#[derive(Clone, Copy)]
pub(in crate::executor) struct PartialRequest {
    /// 一始まりの要求番号。
    pub(in crate::executor) ordinal: usize,
    /// headerが宣言した本文量（byte）。
    pub(in crate::executor) declared: usize,
    /// fixtureアプリが読んだ本文量（byte）。
    pub(in crate::executor) received: usize,
}

impl PartialRequest {
    ///
    /// headerを過読せずPUT本文一byteだけを受領する。
    ///
    /// # 概要
    /// 対象slotの合成要求だけを読み、応答せずcloseできる観測を返す。
    ///
    /// # 引数
    /// * `stream` - 試験専用socket
    /// * `ordinal` - 対象要求番号
    ///
    /// # 戻り値
    /// 全量要求と混同しない有限観測と本文を持たない要求
    ///
    async fn read(stream: &mut TcpStream, ordinal: usize)
        -> Option<(Request, Self)>
    {
        let mut header = Vec::new();
        /*
         * 本文まで先読みしないよう一byteずつheaderだけを受ける
         */
        while !header.ends_with(b"\r\n\r\n") {
            if header.len() == 65_536 { return None; }
            let mut byte = [0];
            stream.read_exact(&mut byte).await.ok()?;
            header.push(byte[0]);
        }
        let text = std::str::from_utf8(&header).ok()?;
        let mut first = text.lines().next()?.split_ascii_whitespace();
        let method = first.next()?.to_owned();
        let target = first.next()?.to_owned();
        if method != "PUT" || !target.ends_with("/source") { return None; }
        let mut lengths = text.lines().filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then_some(value)
        });
        let declared = lengths.next()?.trim().parse::<usize>().ok()?;
        if lengths.next().is_some() || !(2..=1_048_576).contains(&declared) {
            return None;
        }
        /*
         * 本文原文を保存せず受領量だけを確定する
         */
        let mut byte = [0];
        stream.read_exact(&mut byte).await.ok()?;
        let request = Request { method, target, body: Vec::new(),
            lock_auth: text.lines().any(|line|
                line.to_ascii_lowercase().starts_with(
                    "x-lock-authentication:")), lock_version: None };
        Some((request, Self { ordinal, declared, received: 1 }))
    }
}

///
/// 要求順と余分なHTTPを観察する専用script server。
///
pub(in crate::executor) struct Server {
    /// 外部へ接続しない専用URL。
    pub(in crate::executor) url: Url,
    /// 通常要求と、専用観測へ分離した部分要求の順序列。
    requests: Arc<Mutex<Vec<Request>>>,
    /// 部分受領した一要求だけの有限観測。
    partial: Arc<Mutex<Option<PartialRequest>>>,
    /// このfixtureだけのserver task。
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    ///
    /// script終了後も余分な要求を記録して500で拒否する。
    ///
    /// # 概要
    /// fixtureごとに独立したsocket/列を持ち、並列試験で共有しない。
    ///
    /// # 引数
    /// * `replies` - 発生順のdummy応答
    ///
    /// # 戻り値
    /// 所有者dropまで観察するserver
    ///
    pub(in crate::executor) async fn new(replies: Vec<Reply>) -> Self {
        Self::start(replies, None, None).await
    }

    ///
    /// 指定要求の応答本文だけを宣言長より一byte短く切断する。
    ///
    /// # 引数
    /// * `replies` - 各要求への応答
    /// * `request` - 切断する要求の一始まり番号
    ///
    /// # 戻り値
    /// 元statusと受信Transportを区別できるfixture
    ///
    pub(in crate::executor) async fn truncated(replies: Vec<Reply>,
        request: usize) -> Self
    {
        Self::start(replies, Some(request), None).await
    }

    ///
    /// 指定PUTだけを部分受領して無応答で切断する。
    ///
    /// # 引数
    /// * `replies` - 切断slotも含む固定応答列
    /// * `request` - 部分受領する一始まり番号
    ///
    /// # 戻り値
    /// 部分観測と後続再照合を別々に記録するserver
    ///
    pub(in crate::executor) async fn partial_put(replies: Vec<Reply>,
        request: usize) -> Self
    {
        Self::start(replies, None, Some(request)).await
    }

    ///
    /// 専用socketで応答列と任意の本文切断を処理する。
    ///
    /// # 概要
    /// 通常は全量受信後だけ応答し、指定部分切断も要求数に残す。
    ///
    /// # 引数
    /// * `replies` - 発生順のdummy応答
    /// * `truncated` - 応答本文を切断する一始まり番号
    /// * `cut` - 要求本文を部分受領する一始まり番号
    ///
    /// # 戻り値
    /// 所有者dropまで要求を観察するserver
    ///
    async fn start(replies: Vec<Reply>, truncated: Option<usize>,
        cut: Option<usize>) -> Self
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}",
            listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&requests);
        let partial = Arc::new(Mutex::new(None));
        let partial_observed = Arc::clone(&partial);
        let mut replies: VecDeque<_> = replies.into();
        let task = tokio::spawn(async move {
            /*
             * 通常要求は全量受領後に記録し指定slotだけ部分切断する
             */
            while let Ok((mut stream, _)) = listener.accept().await {
                let next = observed.lock().unwrap().len() + 1;
                if cut == Some(next) {
                    /*
                     * slotを消費してから切断し再送を別要求として検出する
                     */
                    if let Some((request, evidence)) =
                        PartialRequest::read(&mut stream, next).await
                    {
                        observed.lock().unwrap().push(request);
                        *partial_observed.lock().unwrap() = Some(evidence);
                        let _ = replies.pop_front();
                    }
                    continue;
                }
                let Some(request) = Request::read(&mut stream).await else {
                    continue;
                };
                let number = {
                    let mut requests = observed.lock().unwrap();
                    requests.push(request);
                    requests.len()
                };
                let reply = replies.pop_front().unwrap_or_else(||
                    Reply::new(500, b"{}".to_vec()));
                if let Some(status) = reply.status {
                    let declared = reply.body.len()
                        + usize::from(truncated == Some(number));
                    let mut bytes = format!("HTTP/1.1 {status} Reply\r\n\
                        Content-Length: {}\r\nConnection: close\r\n",
                        declared).into_bytes();
                    /*
                     * 期限はdummy応答として生成し製品の5分仮定にしない
                     */
                    if reply.lock {
                        let expire: chrono::DateTime<chrono::Utc> =
                            (std::time::SystemTime::now()
                                + std::time::Duration::from_secs(600)).into();
                        bytes.extend_from_slice(format!("X-Page-Lock: \
                            expire={} token={}\r\n", expire.to_rfc3339(),
                            reply.token).as_bytes());
                    }
                    bytes.extend_from_slice(b"\r\n");
                    bytes.extend_from_slice(&reply.body);
                    let _ = stream.write_all(&bytes).await;
                }
            }
        });
        Self { url, requests, partial, task }
    }

    ///
    /// 要求列を短い同期参照で照合する。部分量はpartialへ分離する。
    ///
    /// # 引数
    /// * `inspect` - dummy列だけを確認する試験処理
    ///
    /// # 戻り値
    /// なし。socket処理中にmutexをawait越しに持たない。
    ///
    pub(in crate::executor) fn inspect(&self,
        inspect: impl FnOnce(&[Request]))
    {
        inspect(&self.requests.lock().unwrap());
    }

    ///
    /// 本文や原文を含まない部分受領事実を取得する。
    ///
    /// # 戻り値
    /// 対象slotへ到達した場合だけ確定した観測
    ///
    pub(in crate::executor) fn partial(&self) -> Option<PartialRequest> {
        *self.partial.lock().unwrap()
    }
}

impl Drop for Server {
    ///
    /// 自分の試験socketだけを閉じる。
    ///
    /// # 戻り値
    /// なし。余ったscript待機を残さない。
    ///
    fn drop(&mut self) { self.task.abort(); }
}
