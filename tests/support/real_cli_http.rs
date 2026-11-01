/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 明示実CLI試験だけの環境隔離・有限REST観測と既存lock actor
//!

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client, Method, Response};
use serde::Deserialize;
use url::Url;

/// 許可済み領域。新しいrun subtreeだけを操作する。
pub const ROOT: &str = "/Test/luwiki-injector-ms42";

/// MS51は既存許可root内の専用subtreeだけを使用する。
pub const MS51_ROOT: &str = "/Test/luwiki-injector-ms42/ms51";

///
/// 任意の接続先やrootを受け付けない試験系列。
///
#[derive(Clone, Copy)]
enum Series {
    /// 既存MS43の許可条件と観測表示。
    Ms43,
    /// MS51の狭いcase領域とalias表示。
    Ms51,
}


///
/// 選択する認証方式。秘密値を列挙値へ入れない。
///
#[derive(Clone, Copy)]
pub enum Auth {
    /// 既存の専用Basic設定。
    Basic,
    /// 既存の専用Bearer設定。
    Bearer,
}

///
/// 値をDebug/表示へ渡さない専用環境。
///
pub struct Environment {
    /// 検査済みの公開URL。
    target: Url,
    /// 内部固定の許可領域と表示方針。
    series: Series,
    /// Basicユーザー名。argvへ渡さない。
    user: String,
    /// Basic秘密値。
    password: String,
    /// Bearer秘密値。
    token: String,
}

impl Environment {
    ///
    /// 同じ新case/pathでnegative資格情報の拒否を単回確認する。
    ///
    /// # 概要
    /// 成功や別statusなら製品を起動せず、本文を取得/記録しない。
    ///
    /// # 引数
    /// * `path` - 新ms51 case内の最初のpath
    /// * `token` - probeとCLIで共有する所有資格情報
    /// * `expected` - 401または403
    ///
    /// # 戻り値
    /// 同pathへ単回使用する拒否確認済み資格情報
    ///
    #[allow(dead_code)]
    pub async fn probe_rejection(&self, path: &str, token: String,
        expected: u16) -> Result<RejectedCredential>
    {
        ensure!(matches!(self.series, Series::Ms51)
            && within(path, MS51_ROOT) && matches!(expected, 401 | 403),
            "negative probe scope");
        let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow!("negative credential format"))?;
        header.set_sensitive(true);
        /*
         * 認証/targetを固定し、redirect/retry/proxyを使用しない
         */
        let client = Client::builder().no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none()).build()
            .map_err(|_| anyhow!("negative probe preparation"))?;
        let mut url = self.target.clone();
        url.set_path("/api/pages/id");
        url.query_pairs_mut().append_pair("path", path);
        let response = client.get(url).header(AUTHORIZATION, header)
            .timeout(Duration::from_secs(30)).send().await
            .map_err(|_| anyhow!("negative probe transport"))?;
        let status = response.status().as_u16();
        eprintln!("MS51 role=negative_probe observation=lookup \
            status={status}");
        ensure!(status == expected, "negative probe rejected expectation");
        Ok(RejectedCredential { token, path: path.to_owned(), status })
    }

    ///
    /// ユーザー実行時だけ専用環境を読み、HTTP前に対象を検証する。
    ///
    /// # 概要
    /// 通常試験は本入口を呼ばず、欠落値そのものを診断へ含めない。
    ///
    /// # 戻り値
    /// 承認済み環境、または変数名だけの固定失敗
    ///
    pub fn load() -> Result<Self> { Self::load_series(Series::Ms43) }

    ///
    /// 既存5変数を再利用しMS51の内部scopeだけを狭める。
    ///
    /// # 戻り値
    /// ROOTを確認し、ROOT/ms51配下だけへ観測を許す実行環境
    ///
    #[allow(dead_code)]
    pub fn load_ms51() -> Result<Self> { Self::load_series(Series::Ms51) }

    ///
    /// 実環境を読まない合成資格情報で子設定だけを検査する。
    ///
    /// # 戻り値
    /// 固定対象/固定合成値。HTTPには使用しない。
    ///
    #[allow(dead_code)]
    pub fn synthetic_ms51() -> Result<Self> {
        Ok(Self { target: Url::parse("https://localhost:8040")
            .map_err(|_| anyhow!("synthetic URL"))?, series: Series::Ms51,
            user: "synthetic-user".to_owned(),
            password: "synthetic-password-only".to_owned(),
            token: "synthetic-bearer-only".to_owned() })
    }

    ///
    /// 固定系列に対応する実環境を値を表示せず読み取る。
    ///
    /// # 概要
    /// 外部prefixは既存ROOTと一致させ、系列はコード内でのみ選ぶ。
    ///
    /// # 引数
    /// * `series` - 内部固定の試験系列
    ///
    /// # 戻り値
    /// 対象と資格情報を秘匿保持する環境
    ///
    fn load_series(series: Series) -> Result<Self> {
        /*
         * URL/prefixも値をechoせず固定の許可条件へ照合する
         */
        let target = required("LUWIKI_MS42_TEST_URL")?;
        let prefix = required("LUWIKI_MS42_TEST_PREFIX")?;
        ensure!(approved_target(&target) && prefix == ROOT,
            "real CLI approved environment mismatch");
        let target = Url::parse(&target).map_err(|_| anyhow!("test URL"))?;
        /*
         * 秘密はハーネスと選択した子envだけへ保持する
         */
        Ok(Self { target, series,
            user: required("LUWIKI_MS42_TEST_BASIC_USER")?,
            password: required("LUWIKI_MS42_TEST_BASIC_PASSWORD")?,
            token: required("LUWIKI_MS42_TEST_BEARER_TOKEN")? })
    }

    ///
    /// 認証方式を混在させず子の環境を隔離する。
    ///
    /// # 概要
    /// 秘密をargv/設定へ書かず、他方式・専用env・proxyを子へ残さない。
    ///
    /// # 引数
    /// * `command` - 実製品子プロセス
    /// * `auth` - 今回の認証方式
    ///
    /// # 戻り値
    /// なし。子だけの環境を変更する。
    ///
    pub fn configure_child(&self, command: &mut Command, auth: Auth) {
        if matches!(self.series, Series::Ms51) { isolate_child(command); }
        /*
         * 既存の認証・取得・proxy環境を選択前に除く
         */
        for name in ["LUWIKI_URL", "LUWIKI_USER", "LUWIKI_PASSWORD",
            "LUWIKI_TOKEN", "LUWIKI_MS42_TEST_BASIC_USER",
            "LUWIKI_MS42_TEST_BASIC_PASSWORD", "LUWIKI_MS42_TEST_BEARER_TOKEN",
            "GITHUB_TOKEN", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR",
            "GIT_INDEX_FILE", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
            "http_proxy", "https_proxy", "all_proxy"]
        { command.env_remove(name); }
        /*
         * 通常の製品envへ選択方式だけを設定する
         */
        command.env("LUWIKI_URL", self.target.as_str());
        match auth {
            Auth::Basic => {
                command.env("LUWIKI_USER", &self.user)
                    .env("LUWIKI_PASSWORD", &self.password);
            }
            Auth::Bearer => { command.env("LUWIKI_TOKEN", &self.token); }
        }
    }

    ///
    /// 生出力を表示する前に、明確な認証header/長い秘密の漏出を拒否する。
    ///
    /// # 概要
    /// 短いユーザー名等と固定文/ULIDの偶然一致は秘密漏出と断定しない。
    /// 生出力自体は成功/失敗のどちらでも端末へ渡さない。
    ///
    /// # 引数
    /// * `text` - 子から回収した出力
    ///
    /// # 戻り値
    /// 値を含めない検査結果
    ///
    pub fn check_output(&self, text: &str) -> Result<()> {
        /*
         * 完全な認証表現と十分長い秘密値だけを明示検査する
         */
        for auth in [Auth::Basic, Auth::Bearer] {
            let header = self.authorization(auth)?;
            let value = header.to_str().map_err(|_| anyhow!("auth format"))?;
            ensure!(!text.contains(value), "credential output rejected");
        }
        for value in [&self.user, &self.password, &self.token] {
            if value.len() >= 16 {
                ensure!(!text.contains(value), "credential output rejected");
            }
        }
        Ok(())
    }

    ///
    /// 同じ実行設定から独立observer/actorを作る。
    ///
    /// # 概要
    /// 製品CLIとは別invocationの期限で、同じ対象/認証を検証する。
    ///
    /// # 引数
    /// * `prefix` - 新run内の専用case
    /// * `auth` - 認証選択
    ///
    /// # 戻り値
    /// 単発要求だけを発行する観測者
    ///
    pub fn observer(&self, prefix: &str, auth: Auth) -> Result<Observer> {
        let root = match self.series {
            Series::Ms43 => ROOT,
            Series::Ms51 => MS51_ROOT,
        };
        ensure!(within(prefix, root), "observer region rejected");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(AUTHORIZATION, self.authorization(auth)?);
        /*
         * TLS検証を維持しlocalhost直通・再送/追従なしにする
         */
        let client = Client::builder().default_headers(headers).no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none()).build()
            .map_err(|_| anyhow!("observer preparation"))?;
        Ok(Observer { client, target: self.target.clone(),
            prefix: prefix.to_owned(), known: BTreeMap::new(),
            raw_records: matches!(self.series, Series::Ms43),
            owner: Arc::new(()), deadline: Instant::now()
                + Duration::from_secs(300) })
    }

    ///
    /// G6の完全path集合だけへBearer付き単発送信を許可する。
    ///
    /// # 引数
    /// * `prefix` - 新MS51 caseの真子孫領域
    /// * `paths` - 合成入力が所有する完全path集合
    /// * `known` - 独立確認済みのIDとpathの結合
    ///
    /// # 戻り値
    /// 任意URLや秘密getterを持たない非Debugの送信権限
    ///
    #[allow(dead_code)]
    pub fn relay_grant(&self, prefix: &str, paths: &[String],
        known: &[(String, String)]) -> Result<RelayGrant>
    {
        /*
         * G6実入口だけが承認markerと固定targetを再確認する
         */
        ensure!(matches!(self.series, Series::Ms51)
            && std::env::var("LUWIKI_MS51_FAULT_APPROVAL").ok().as_deref()
                == Some("REAL-FAULT-01")
            && within(prefix, MS51_ROOT)
            && approved_target(self.target.as_str()), "relay approval");
        ensure!(!paths.is_empty() && paths.iter()
            .all(|path| within(path, prefix)), "relay paths");
        let allowed: std::collections::BTreeSet<_> =
            paths.iter().cloned().collect();
        ensure!(allowed.len() == paths.len(), "relay duplicate path");
        let authorization = self.authorization(Auth::Bearer)?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization.clone());
        let client = Client::builder().default_headers(headers).no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none()).build()
            .map_err(|_| anyhow!("relay client"))?;
        let mut grant = RelayGrant { client, target: self.target.clone(),
            prefix: prefix.to_owned(), paths: allowed,
            known: BTreeMap::new(), authorization };
        /*
         * 既知IDも同じ完全path集合へ結び付ける
         */
        for (id, path) in known { grant.bind(id, path)?; }
        Ok(grant)
    }

    ///
    /// reqwestの正規生成を使い認証headerを秘匿値として得る。
    ///
    /// # 引数
    /// * `auth` - 選択方式
    ///
    /// # 戻り値
    /// sensitive指定済みheader。Debugへ渡さない。
    ///
    fn authorization(&self, auth: Auth) -> Result<HeaderValue> {
        let client = Client::builder().no_proxy().build()
            .map_err(|_| anyhow!("test authentication preparation"))?;
        let request = client.get(self.target.clone());
        let request = match auth {
            Auth::Basic => request.basic_auth(&self.user, Some(&self.password)),
            Auth::Bearer => request.bearer_auth(&self.token),
        }.build().map_err(|_| anyhow!("test authentication"))?;
        let mut value = request.headers().get(AUTHORIZATION)
            .ok_or_else(|| anyhow!("test authentication"))?.clone();
        value.set_sensitive(true);
        Ok(value)
    }
}

///
/// G6専用の固定HTTPS送信能力。認証値を表示・直列化・複製しない。
///
#[allow(dead_code)]
pub struct RelayGrant {
    /// 通常TLS検証、再送/追従/proxyなしの専用client。
    client: Client,
    /// 固定の承認済みHTTPS対象。
    target: Url,
    /// 所有するcaseの真子孫領域。
    prefix: String,
    /// 完全一致で許可した合成path。
    paths: std::collections::BTreeSet<String>,
    /// 正常応答で確定したIDと許可path。
    known: BTreeMap<String, String>,
    /// capture検査にも使用する非公開認証header。
    authorization: HeaderValue,
}

// G4/G5はG6能力を使用しないが共有ファイルの通常契約を保持する。
#[allow(dead_code)]
impl RelayGrant {
    ///
    /// relay計画がこのgrantの初期権限と一致するか確認する。
    ///
    /// # 引数
    /// * `prefix` - 計画のcase領域
    /// * `paths` - 計画の完全path集合
    /// * `known` - 計画開始時のID/path結合
    ///
    /// # 戻り値
    /// 同一権限、またはHTTPなしの拒否
    ///
    pub fn verify(&self, prefix: &str, paths: &[String],
        known: &[(String, String)]) -> Result<()>
    {
        let paths: std::collections::BTreeSet<_> =
            paths.iter().cloned().collect();
        let known: BTreeMap<_, _> = known.iter().cloned().collect();
        ensure!(self.prefix == prefix && self.paths == paths
            && self.known == known, "relay grant mismatch");
        Ok(())
    }

    ///
    /// 完全201等で検証済みのIDを同じ合成pathへ結び付ける。
    ///
    /// # 引数
    /// * `id` - canonical ULID
    /// * `path` - 既に許可した完全path
    ///
    /// # 戻り値
    /// 衝突のない結合、または値を含めない拒否
    ///
    pub fn bind(&mut self, id: &str, path: &str) -> Result<()> {
        ensure!(valid_id(id) && id.bytes().all(|v| !v.is_ascii_lowercase())
            && self.paths.contains(path), "relay identity");
        ensure!(self.known.get(id).is_none_or(|prior| prior == path),
            "relay identity collision");
        self.known.insert(id.to_owned(), path.to_owned());
        Ok(())
    }

    ///
    /// 固定操作を単回送信し、外部エラー本文/URLを診断へ渡さない。
    ///
    /// # 概要
    /// methodと固定suffixを限定し、任意URL/追加queryを受け付けない。
    /// 結合済みIDと所有pathを送信前に照合する。
    ///
    /// # 引数
    /// * `method` - 許可したREST method
    /// * `path` - 合成pageの完全path
    /// * `id` - ID操作だけの結合済みID
    /// * `tail` - source/meta/lock、またはページ本体
    /// * `lock` - 同IDでrelayが検査した独立lock認証
    /// * `body` - 検査済み合成PUT bytes
    ///
    /// # 戻り値
    /// 単発応答、または値を含まない通信失敗
    ///
    pub async fn send(&self, method: Method, path: &str, id: Option<&str>,
        tail: Option<&str>, lock: Option<HeaderValue>, body: Option<Vec<u8>>)
        -> Result<Response>
    {
        ensure!(self.paths.contains(path), "relay path capability");
        let locked = id.is_some() && (method == Method::PUT
            || method == Method::DELETE);
        ensure!(lock.is_some() == locked
            && body.is_some() == (method == Method::PUT
                && tail == Some("source")), "relay payload capability");
        let mut url = self.target.clone();
        /*
         * API segmentとmethodの組を固定し、外部URLを継承しない
         */
        if let Some(id) = id {
            ensure!(self.known.get(id).is_some_and(|v| v == path),
                "relay ID capability");
            let allowed = match tail {
                Some("meta") => method == Method::GET,
                Some("source") => method == Method::GET
                    || method == Method::PUT,
                Some("lock") => method == Method::POST
                    || method == Method::PUT || method == Method::DELETE,
                None => method == Method::DELETE,
                _ => false,
            };
            ensure!(allowed, "relay operation capability");
            let mut segments = url.path_segments_mut()
                .map_err(|_| anyhow!("relay URL"))?;
            segments.clear().extend(["api", "pages", id]);
            if let Some(tail) = tail { segments.push(tail); }
        } else {
            ensure!(tail.is_none() && (method == Method::GET
                || method == Method::POST), "relay path operation");
            url.set_path(if method == Method::GET {
                "/api/pages/id"
            } else { "/api/pages" });
            url.query_pairs_mut().append_pair("path", path);
        }
        /*
         * 必要な独立認証と合成本文だけを再構築する
         */
        let mut request = self.client.request(method, url)
            .timeout(Duration::from_secs(60));
        if let Some(lock) = lock {
            request = request.header("x-lock-authentication", lock);
        }
        if let Some(body) = body {
            request = request.header("content-type", "text/markdown")
                .header("content-length", body.len()).body(body);
        }
        request.send().await.map_err(|_| anyhow!("relay upstream transport"))
    }

    ///
    /// captureへ実Bearerが露出していないことを値非表示で検査する。
    ///
    /// # 引数
    /// * `text` - 非公開の子stream
    ///
    /// # 戻り値
    /// 非露出、または固定診断
    ///
    pub fn check_output(&self, text: &str) -> Result<()> {
        let value = self.authorization.to_str()
            .map_err(|_| anyhow!("relay authorization"))?;
        let token = value.strip_prefix("Bearer ")
            .ok_or_else(|| anyhow!("relay authorization"))?;
        ensure!(!text.contains(value) && !text.contains(token),
            "relay credential output");
        Ok(())
    }
}

///
/// 単回lookupで拒否された同じ資格情報。再読取/複製しない。
///
pub struct RejectedCredential {
    /// 非表示の同一Bearer値。
    token: String,
    /// probeした新caseの最初のpath。
    path: String,
    /// 観測した401または403。
    status: u16,
}

impl RejectedCredential {
    ///
    /// 確認済み同pathのCLIへ一度だけ資格情報を移す。
    ///
    /// # 引数
    /// * `command` - 認証隔離済みのCLI
    /// * `path` - 製品が最初にlookupするpath
    ///
    /// # 戻り値
    /// 固定status。元資格情報は出力しない。
    ///
    #[allow(dead_code)]
    pub fn configure(&self, command: &mut Command, path: &str) -> Result<u16> {
        ensure!(self.path == path && within(path, MS51_ROOT),
            "negative credential binding");
        command.env_remove("LUWIKI_USER").env_remove("LUWIKI_PASSWORD")
            .env("LUWIKI_TOKEN", &self.token);
        Ok(self.status)
    }

    ///
    /// negative資格情報の漏出を生表示せず検査する。
    ///
    /// # 引数
    /// * `text` - 生表示しない子stream
    ///
    /// # 戻り値
    /// 完全認証表現/長い固有値の非露出
    ///
    #[allow(dead_code)]
    pub fn check_output(&self, text: &str) -> Result<()> {
        ensure!(!text.contains(&format!("Bearer {}", self.token))
            && (self.token.len() < 16 || !text.contains(&self.token)),
            "negative credential output rejected");
        Ok(())
    }
}

///
/// 固定schemaのmetadata観測。未知値の全量JSON Valueを保持しない。
/// 試験内の比較用であり、fieldの変更を実Wikiへ反映するAPIは持たない。
///
#[derive(Deserialize)]
pub struct Metadata {
    /// 対象の状態観測。
    pub page_info: PageInfo,
    /// draftには存在しない。未確認をrevision0へ補完しない。
    pub revision_info: Option<RevisionInfo>,
}

///
/// 合成caseのページ状態。
///
#[derive(Deserialize)]
pub struct PageInfo {
    /// 保存pathの分類と値。
    pub path: MetaPath,
    /// 削除済みという観測。
    pub deleted: bool,
    /// lockがあるという観測。自己保持の証明には使わない。
    pub locked: bool,
}

///
/// metadataの保存path。
///
#[derive(Deserialize)]
pub struct MetaPath {
    /// current/last_deletedの観測文字列。
    pub kind: String,
    /// 固定caseの期待pathと照合する値。
    pub value: String,
}

///
/// 完全metadataから確認したrevision。
///
#[derive(Deserialize)]
pub struct RevisionInfo {
    /// サーバー観測値。
    pub revision: u64,
}

///
/// path照会の有限結果。404/410を合成しない。
///
pub enum Lookup {
    /// 有効ID。
    Found(
        /// 形式検査済みの照会ID。
        String,
    ),
    /// 観測した不存在/削除status。
    Missing(
        /// その要求で観測した404または410。
        u16,
    ),
}

///
/// path照会の小さい固定JSON schema。
///
#[derive(Deserialize)]
struct Identity {
    /// サーバーが返したID。使用前に形式検査する。
    id: String,
}

///
/// 同じobserver/ID/pathに閉じる複製不可のactor既存lock。
///
pub struct Lease {
    /// 取得したobserverの識別。
    owner: Arc<()>,
    /// 確認済み対象ID。
    id: String,
    /// 取得時のcase path。
    path: String,
    /// 出力しない独立lock token。
    token: HeaderValue,
    /// サーバーのRFC3339期限。補正/猶予なし。
    expires: DateTime<Utc>,
    /// 解除を一度開始したか。再送を許さない。
    used: bool,
}

///
/// 許可caseだけを読み取り/既存lock操作する独立観測者。
///
pub struct Observer {
    /// TLS検証/認証済みclient。
    client: Client,
    /// 固定対象。
    target: Url,
    /// この観測者のcase。
    prefix: String,
    /// 確認したIDとpathの結合。
    known: BTreeMap<String, String>,
    /// lease所有の識別。
    owner: Arc<()>,
    /// この独立invocationの絶対期限。
    deadline: Instant,
    /// MS43だけが既存のpath付き観測記録を出す。
    raw_records: bool,
}

impl Observer {
    ///
    /// CLIが報告した確定IDをcase pathへ関連付ける。
    ///
    /// # 引数
    /// * `id` - 形式検証する復旧ID
    /// * `path` - このcaseの期待path
    ///
    /// # 戻り値
    /// 有効な結合、または値を含めない拒否
    ///
    pub fn remember(&mut self, id: &str, path: &str) -> Result<()> {
        ensure!(valid_id(id) && within(path, &self.prefix),
            "identity rejected");
        if let Some(prior) = self.known.get(id) {
            ensure!(prior == path, "identity binding rejected");
        }
        self.known.insert(id.to_owned(), path.to_owned());
        Ok(())
    }

    ///
    /// 版やprefix証明とは別の認証疎通を確認する。
    ///
    /// # 戻り値
    /// hello200確認。エラー本文は読み取らない。
    ///
    pub async fn hello(&self) -> Result<()> {
        let response = self.send(Method::GET, &["api", "hello"], None).await?;
        ensure!(response.status().as_u16() == 200,
            "hello authentication failed");
        Ok(())
    }

    ///
    /// 有効IDまたは404/410を独立して確認する。
    ///
    /// # 概要
    /// JSONは制御した合成caseの小さいID schemaだけをdeserializeする。
    ///
    /// # 引数
    /// * `path` - 許可case内path
    ///
    /// # 戻り値
    /// 実際の照会結果。通信失敗を不存在へ変換しない。
    ///
    pub async fn lookup(&mut self, path: &str) -> Result<Lookup> {
        ensure!(within(path, &self.prefix), "lookup region rejected");
        let mut url = self.url(&["api", "pages", "id"])?;
        url.query_pairs_mut().append_pair("path", path);
        let response = self.client.get(url).timeout(self.remaining()?).send()
            .await.map_err(|_| anyhow!("lookup transport"))?;
        /*
         * 成功だけ固定schemaを読み、未知errorの本文を取得しない
         */
        match response.status().as_u16() {
            200 => {
                let value = response.json::<Identity>().await
                    .map_err(|_| anyhow!("lookup schema"))?;
                self.remember(&value.id, path)?;
                if self.raw_records {
                    eprintln!("MS43 observation=lookup path={path} id={}",
                        value.id);
                }
                Ok(Lookup::Found(value.id))
            }
            code @ (404 | 410) => Ok(Lookup::Missing(code)),
            code => Err(anyhow!("lookup HTTP {code}")),
        }
    }

    ///
    /// 既知IDのmetadataを一回だけ観測する。
    ///
    /// # 概要
    /// GETが期限切れlock/draft cleanupを伴い得ることを明示記録する。
    /// 製品の一般JSON受信とは別の、小さい合成metadata観測である。
    ///
    /// # 引数
    /// * `id` - 登録済みID
    /// * `path` - 結合した期待path
    ///
    /// # 戻り値
    /// 固定schemaの実観測。draft/期限切れ不存在を補完しない。
    ///
    pub async fn metadata(&self, id: &str, path: &str) -> Result<Metadata> {
        self.bound(id, path)?;
        if self.raw_records {
            eprintln!("MS43 observation=metadata \
                potential_expiry_cleanup=true");
        }
        let response = self.send(Method::GET, &["api", "pages", id, "meta"],
            None).await?;
        ensure!(response.status().as_u16() == 200,
            "metadata HTTP {} ; preserve observed state",
            response.status().as_u16());
        /*
         * 合成caseの固定schemaだけを保持し、raw JSONを表示しない
         */
        let value = response.json::<Metadata>().await
            .map_err(|_| anyhow!("metadata schema"))?;
        ensure!(value.page_info.path.value == path, "metadata path mismatch");
        ensure!(matches!(value.page_info.path.kind.as_str(),
            "current" | "last_deleted"), "metadata kind mismatch");
        if self.raw_records {
            eprintln!("MS43 observation=metadata path={path} id={id} kind={} \
                deleted={} locked={} revision={:?}", value.page_info.path.kind,
                value.page_info.deleted, value.page_info.locked,
                value.revision_info.as_ref().map(|v| v.revision));
        }
        Ok(value)
    }

    ///
    /// 本文を全量収集せず合成入力bytesへ逐次比較する。
    ///
    /// # 概要
    /// 読取statusと全chunk/EOFを検査し、本文を診断へ載せない。
    ///
    /// # 引数
    /// * `id` - 登録済みID
    /// * `path` - 期待case path
    /// * `expected` - 合成入力だけのbytes
    ///
    /// # 戻り値
    /// 完全一致、または秘密/本文を含まない失敗
    ///
    pub async fn source_equals(&self, id: &str, path: &str,
        expected: &[u8]) -> Result<()>
    {
        self.source_equals_reader(id, path, Cursor::new(expected)).await
    }

    ///
    /// 保護期待file/readerを全量常駐させずWiki本文と比較する。
    ///
    /// # 概要
    /// HTTP chunkと期待readerを64KiB以下の比較bufferでEOFまで照合する。
    ///
    /// # 引数
    /// * `id` - 登録済みID
    /// * `path` - 結合した期待path
    /// * `expected` - case所有者が生存する期待本文
    ///
    /// # 戻り値
    /// 完全bytes一致。入力本文上限をWiki応答へ流用しない。
    ///
    #[allow(dead_code)]
    pub async fn source_equals_reader(&self, id: &str, path: &str,
        mut expected: impl Read) -> Result<()>
    {
        self.bound(id, path)?;
        let mut response = self.send(Method::GET,
            &["api", "pages", id, "source"], None).await?;
        ensure!(response.status().as_u16() == 200, "source HTTP {}",
            response.status().as_u16());
        let mut buffer = [0_u8; 65_536];
        /*
         * chunkの幅に依存せず期待bytesとEOFまで照合する
         */
        while let Some(chunk) = response.chunk().await
            .map_err(|_| anyhow!("source receive"))?
        {
            for piece in chunk.chunks(buffer.len()) {
                expected.read_exact(&mut buffer[..piece.len()])
                    .map_err(|_| anyhow!("expected source read"))?;
                ensure!(buffer[..piece.len()] == *piece,
                    "source bytes mismatch");
            }
        }
        ensure!(expected.read(&mut buffer[..1])
            .map_err(|_| anyhow!("expected source EOF"))? == 0,
            "source EOF mismatch");
        Ok(())
    }

    ///
    /// 確認済みの通常ページでactorの既存lockを取得する。
    ///
    /// # 概要
    /// client/ID/path/期限へ結び付け、資源やheaderを表示しない。
    ///
    /// # 引数
    /// * `id` - 通常metadata確認済みID
    /// * `path` - 同じcase path
    ///
    /// # 戻り値
    /// 一度だけ正常解除を検討できるactor所有lock
    ///
    pub async fn acquire(&self, id: &str, path: &str) -> Result<Lease> {
        self.bound(id, path)?;
        let response = self.send(Method::POST, &["api", "pages", id, "lock"],
            None).await?;
        ensure!(response.status().as_u16() == 204, "actor acquire HTTP {}",
            response.status().as_u16());
        /*
         * 独立lock headerを検証し取得元の所有者へ閉じる
         */
        let values = response.headers().get_all("x-page-lock");
        let mut headers = values.iter();
        let header = headers.next()
            .ok_or_else(|| anyhow!("actor lock header missing"))?;
        ensure!(headers.next().is_none(), "actor lock header ambiguous");
        let (token, expires) = parse_lock(header, SystemTime::now().into())?;
        Ok(Lease { owner: Arc::clone(&self.owner), id: id.to_owned(),
            path: path.to_owned(), token, expires, used: false })
    }

    ///
    /// 期待した409拒否後にだけ呼ぶactor自身の既存lock解除。
    ///
    /// # 概要
    /// 有効/同所有/同対象を再確認し、一度開始した解除は再送しない。
    ///
    /// # 引数
    /// * `lease` - actorが取得した複製不可の資源
    ///
    /// # 戻り値
    /// 確定204、または元状態を保持すべき失敗
    ///
    pub async fn release(&self, lease: &mut Lease) -> Result<()> {
        self.bound(&lease.id, &lease.path)?;
        ensure!(Arc::ptr_eq(&self.owner, &lease.owner) && !lease.used
            && lease.expires > DateTime::<Utc>::from(SystemTime::now()),
            "actor lease unavailable");
        self.remaining()?;
        lease.used = true;
        /*
         * 函数呼出後の不確定でもtokenを再使用せず一回だけ送る
         */
        let response = self.send(Method::DELETE,
            &["api", "pages", &lease.id, "lock"], Some(&lease.token)).await?;
        ensure!(response.status().as_u16() == 204, "actor release HTTP {}",
            response.status().as_u16());
        Ok(())
    }

    ///
    /// 登録済みID/path結合を要求前に確認する。
    ///
    /// # 引数
    /// * `id` - 操作ID
    /// * `path` - 期待path
    ///
    /// # 戻り値
    /// 一致、またはHTTPなしの拒否
    ///
    fn bound(&self, id: &str, path: &str) -> Result<()> {
        ensure!(valid_id(id) && within(path, &self.prefix)
            && self.known.get(id).is_some_and(|v| v == path),
            "observer identity binding rejected");
        Ok(())
    }

    ///
    /// 同じ絶対期限までの残時間を得る。
    ///
    /// # 戻り値
    /// 正の残時間。期限を作り直さない。
    ///
    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "observer deadline reached");
        Ok(remaining)
    }

    ///
    /// 固定segmentから同じサーバー内URLだけを組み立てる。
    ///
    /// # 引数
    /// * `segments` - 検証済み操作の固定segmentとID
    ///
    /// # 戻り値
    /// 既知対象内URL
    ///
    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.target.clone();
        url.path_segments_mut().map_err(|_| anyhow!("observer URL"))?
            .clear().extend(segments);
        Ok(url)
    }

    ///
    /// 自動再送/追従なしで単発要求を送る。
    ///
    /// # 引数
    /// * `method` - 固定操作
    /// * `segments` - 検証済みsegment
    /// * `lock` - actor所有資源だけの独立認証
    ///
    /// # 戻り値
    /// 単発応答。生transport errorは保持しない。
    ///
    async fn send(&self, method: Method, segments: &[&str],
        lock: Option<&HeaderValue>) -> Result<Response>
    {
        let mut request = self.client.request(method, self.url(segments)?)
            .timeout(self.remaining()?);
        if let Some(value) = lock {
            request = request.header("x-lock-authentication", value.clone());
        }
        request.send().await.map_err(|_| anyhow!("observer transport"))
    }
}

///
/// 必須専用envを値を含まない失敗へ変換する。
///
/// # 引数
/// * `name` - 固定の環境変数名
///
/// # 戻り値
/// 実行時の値。値をerrorへ含めない。
///
fn required(name: &str) -> Result<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("required test environment: {name}"))
}

///
/// MS51の子から親の取得/認証/設定注入環境を除去する。
///
/// # 概要
/// 値は表示せず、未設定の既知変数も明示除去する。TLS検証は変更しない。
///
/// # 引数
/// * `command` - fixtureまたは製品の子だけを設定するコマンド
///
/// # 戻り値
/// なし。親プロセスの環境は変更しない。
///
pub fn isolate_child(command: &mut Command) {
    /*
     * 可変個数のGit設定や試験専用認証も名前だけで除去する
     */
    let names: Vec<_> = std::env::vars_os().map(|(name, _)| name)
        .chain(command.get_envs().map(|(name, _)| name.to_os_string()))
        .collect();
    for name in names {
        let text = name.to_string_lossy();
        if ["GIT_", "GITHUB_", "LUWIKI_"].iter()
            .any(|prefix| text.starts_with(prefix))
        { command.env_remove(name); }
    }
    /*
     * 未設定値とproxyも明示的に除き、合成検査可能にする
     */
    for name in ["GITHUB_TOKEN", "LUWIKI_URL", "LUWIKI_USER",
        "LUWIKI_PASSWORD", "LUWIKI_TOKEN", "GIT_DIR", "GIT_WORK_TREE",
        "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
        "http_proxy", "https_proxy", "all_proxy"]
    { command.env_remove(name); }
}

///
/// HTTPS localhostの承認済み対象だけを許可する。
///
/// # 引数
/// * `target` - 公開URL
///
/// # 戻り値
/// 許可済み固定表記ならtrue
///
pub fn approved_target(target: &str) -> bool {
    matches!(target, "https://localhost:8040" | "https://localhost:8040/")
}

///
/// root自身や隣接prefixを許さずsegment境界を検査する。
///
/// # 引数
/// * `path` - 対象path
/// * `root` - 許可された親領域
///
/// # 戻り値
/// 空白/制御/dot等がない真の子孫ならtrue
///
pub fn within(path: &str, root: &str) -> bool {
    path.strip_prefix(root).is_some_and(|rest| rest.starts_with('/')
        && rest.len() > 1 && !path.contains('\\')
        && !path.contains('\u{fffd}')
        && !path.chars().any(|v| v.is_whitespace() || v.is_control())
        && path.split('/').skip(1).all(|v| !matches!(v, "" | "." | "..")))
}

///
/// tokenとpage IDの共通形式だけを検査する。
///
/// # 引数
/// * `value` - 表示しない候補
///
/// # 戻り値
/// ULID形式ならtrue。資源種別は呼出側で分離する。
///
pub fn valid_id(value: &str) -> bool {
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    value.len() == 26 && matches!(value.as_bytes()[0], b'0'..=b'7')
        && value.bytes().all(|v| alphabet.contains(&v.to_ascii_uppercase()))
}

///
/// 独立lock headerの期限とtokenを原文を残さず検査する。
///
/// # 概要
/// 重複/未知項目・不正期限・期限切れを拒否し、時計猶予を置かない。
///
/// # 引数
/// * `header` - 取得応答のsensitive候補
/// * `now` - 今回の観測時刻
///
/// # 戻り値
/// sensitiveな認証値と検証済み期限
///
pub fn parse_lock(header: &HeaderValue, now: DateTime<Utc>)
    -> Result<(HeaderValue, DateTime<Utc>)>
{
    let text = header.to_str().map_err(|_| anyhow!("actor lock format"))?;
    let mut expires = None;
    let mut token = None;
    /*
     * 既知項目を一度だけ受け取り元headerを診断へ保存しない
     */
    for part in text.split_ascii_whitespace() {
        let (key, value) = part.split_once('=')
            .ok_or_else(|| anyhow!("actor lock format"))?;
        match key {
            "expire" if expires.is_none() => {
                expires = Some(DateTime::parse_from_rfc3339(value)
                    .map_err(|_| anyhow!("actor lock expiry"))?
                    .with_timezone(&Utc));
            }
            "token" if token.is_none() && valid_id(value) =>
                token = Some(value),
            _ => return Err(anyhow!("actor lock format")),
        }
    }
    /*
     * 有効期限と独立認証表現だけを資源へ渡す
     */
    let expires = expires.ok_or_else(|| anyhow!("actor lock expiry missing"))?;
    ensure!(expires > now, "actor lock expired");
    let mut token = HeaderValue::from_str(&format!("token={}",
        token.ok_or_else(|| anyhow!("actor lock token missing"))?))
        .map_err(|_| anyhow!("actor lock authentication"))?;
    token.set_sensitive(true);
    Ok((token, expires))
}
