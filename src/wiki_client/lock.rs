/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 秘匿ロックtoken・RFC3339期限・client結合と排他的な使用状態
//!

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue};

use crate::report::types::PageId;
use crate::secret::Secret;
use crate::source::StopContext;

use super::types::FailureKind;

///
/// 確定作成と、ページ種別が未確定の取得を区別する。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin { CreatedDraft, Acquired }

///
/// 解除204で確定した作用。取得由来のdraft削除可能性を隠さない。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseEffect {
    DraftDeleted, ReleasedWithPossibleDraftDeletion,
}

///
/// 正常headerからだけ作る独立秘匿tokenと解析済み期限。
///
pub(super) struct LockData { token: Secret, expire: DateTime<Utc> }

impl LockData {
    ///
    /// 現在時刻と補正なしで有効期限を比較する。
    ///
    /// # 戻り値
    /// 有効なら成功
    ///
    pub(super) fn valid(&self) -> Result<(), FailureKind> {
        if self.expire <= now() { Err(FailureKind::LockExpired) }
        else { Ok(()) }
    }
}

///
/// client/IDに結び付いた複製不可のロック資源。
///
pub(crate) struct LockLease {
    data: LockData,
    client: Arc<()>,
    id: PageId,
    origin: Origin,
    usable: bool,
}

impl LockLease {
    ///
    /// 検証済みheaderを同じclientと対象へ結び付ける。
    ///
    /// # 引数
    /// * `data` - 検証済み秘匿header
    /// * `client` - 内部client識別
    /// * `id` - 確定対象ID
    /// * `origin` - 作成/取得の確定した由来
    ///
    /// # 戻り値
    /// 成功公開直前にも期限を検査した資源
    ///
    pub(super) fn new(data: LockData, client: &Arc<()>, id: PageId,
        origin: Origin) -> Result<Self, FailureKind>
    {
        data.valid()?;
        Ok(Self { data, client: Arc::clone(client), id, origin,
            usable: true })
    }

    ///
    /// 対象IDだけを安全な復旧情報として取得する。
    ///
    /// # 戻り値
    /// 確定ID
    ///
    pub(crate) fn id(&self) -> &PageId { &self.id }

    ///
    /// 資源の由来を取得する。
    ///
    /// # 戻り値
    /// 作成済みdraftまたは種別未確定の取得
    ///
    pub(crate) fn origin(&self) -> Origin { self.origin }

    ///
    /// 解析済み期限を取得する。
    ///
    /// # 戻り値
    /// 補正していないサーバー期限
    ///
    pub(crate) fn expire(&self) -> DateTime<Utc> { self.data.expire }

    ///
    /// 期限も含め現在使用可能かを安全な有限値として取得する。
    ///
    /// # 戻り値
    /// 使用可能ならtrue
    ///
    pub(crate) fn usable(&self) -> bool {
        self.usable && self.data.valid().is_ok()
    }

    ///
    /// client/対象/状態/期限を送信前に検査する。
    ///
    /// # 引数
    /// * `client` - 要求を送る内部識別
    /// * `id` - 要求対象
    ///
    /// # 戻り値
    /// 同じ有効資源なら成功
    ///
    pub(super) fn check(&self, client: &Arc<()>, id: &PageId)
        -> Result<(), FailureKind>
    {
        if !Arc::ptr_eq(&self.client, client) || self.id != *id {
            return Err(FailureKind::LeaseMismatch);
        }
        if !self.usable { return Err(FailureKind::LeaseUnavailable); }
        self.data.valid()
    }

    ///
    /// 要求境界だけへ秘匿認証headerを生成する。
    ///
    /// # 戻り値
    /// sensitive指定済みheader
    ///
    pub(super) fn authentication(&self) -> Result<HeaderValue, FailureKind> {
        let mut value = HeaderValue::from_str(&format!("token={}",
            self.data.token.expose())).map_err(|_| FailureKind::LockHeader)?;
        value.set_sensitive(true);
        Ok(value)
    }

    ///
    /// send初回pollで旧資源を使用不可へ移す。
    ///
    /// # 戻り値
    /// なし。future破棄でもこの状態を維持する。
    ///
    pub(super) fn begin(&mut self) { self.usable = false; }

    ///
    /// 完全400拒否後だけ、停止と期限を検査して保持へ戻す。
    ///
    /// # 引数
    /// * `stop` - 同じ絶対期限と取消
    ///
    /// # 戻り値
    /// 境界内かつ期限内なら保持成功
    ///
    pub(super) fn retain(&mut self, stop: &StopContext)
        -> Result<(), FailureKind>
    {
        super::check(stop)?;
        self.data.valid()?;
        self.usable = true;
        Ok(())
    }

    ///
    /// 延長正常応答だけで新tokenへ切り替える。
    ///
    /// # 引数
    /// * `data` - 正常headerの新しい秘匿資源
    ///
    /// # 戻り値
    /// 新tokenを公開時に再検査した結果
    ///
    pub(super) fn renew(&mut self, data: LockData)
        -> Result<(), FailureKind>
    {
        data.valid()?;
        if data.token.expose().eq_ignore_ascii_case(self.data.token.expose()) {
            return Err(FailureKind::LockHeader);
        }
        self.data = data;
        self.usable = true;
        Ok(())
    }
}

impl fmt::Debug for LockLease {
    ///
    /// token/期限/ID/client結合を表示せず状態だけを返す。
    ///
    /// # 引数
    /// * `formatter` - 表示先
    ///
    /// # 戻り値
    /// 安全な表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("LockLease").field("origin", &self.origin)
            .field("usable", &self.usable).finish()
    }
}

///
/// ドラフト作成の確定IDと秘匿資源。
///
pub(crate) struct DraftCreated {
    pub(super) id: PageId,
    pub(super) lease: LockLease,
}

impl DraftCreated {
    ///
    /// 正常応答で確定した作成IDを取得する。
    ///
    /// # 戻り値
    /// 確定ID
    ///
    pub(crate) fn id(&self) -> &PageId { &self.id }

    ///
    /// 検証済み対応を保ったままIDと秘匿資源を所有者へ渡す。
    ///
    /// # 戻り値
    /// 対応するIDと複製不可lease
    ///
    pub(crate) fn into_parts(self) -> (PageId, LockLease) {
        (self.id, self.lease)
    }
}

impl fmt::Debug for DraftCreated {
    ///
    /// IDとtokenを表示しない。
    ///
    /// # 引数
    /// * `formatter` - 表示先
    ///
    /// # 戻り値
    /// 状態だけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("DraftCreated")
            .field("lease", &self.lease).finish()
    }
}

///
/// 重複/欠落を拒否してheaderを必要な秘匿値だけへ写す。
///
/// # 概要
/// 単一headerとfield形式、独立token、RFC3339期限を順に検査する。
///
/// # 引数
/// * `headers` - 受信直後のheader
///
/// # 戻り値
/// 単一の正しいlock、または値なしの失敗
///
pub(super) fn parse(headers: &HeaderMap) -> Result<LockData, FailureKind> {
    let invalid = FailureKind::LockHeader;
    let values = headers.get_all("x-page-lock");
    let mut values = values.iter();
    let value = values.next().ok_or(invalid)?;
    if values.next().is_some() { return Err(invalid); }
    /*
     * 任意fieldや重複を受け付けず、生headerを収集しない
     */
    let text = value.to_str().map_err(|_| invalid)?;
    let mut expire = None;
    let mut token = None;
    for part in text.split_ascii_whitespace() {
        if let Some(value) = part.strip_prefix("expire=") {
            if expire.replace(value).is_some() { return Err(invalid); }
        } else if let Some(value) = part.strip_prefix("token=") {
            if token.replace(value).is_some() { return Err(invalid); }
        } else { return Err(invalid); }
    }
    /*
     * PageIdにtokenを変換せず、形式とRFC3339期限だけを検査する
     */
    let token = token.ok_or(invalid)?;
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if token.len() != 26 || !matches!(token.as_bytes()[0], b'0'..=b'7')
        || !token.bytes().all(|byte|
            alphabet.contains(&byte.to_ascii_uppercase()))
    { return Err(invalid); }
    let expire = DateTime::parse_from_rfc3339(expire.ok_or(invalid)?)
        .map_err(|_| invalid)?.with_timezone(&Utc);
    let data = LockData { token: Secret::new(token.to_owned()), expire };
    data.valid()?;
    Ok(data)
}

///
/// 時計補正せず現在のUTC時刻を取得する。
///
/// # 戻り値
/// 現在時刻。試験の注入はこの関数に閉じる。
///
fn now() -> DateTime<Utc> {
    #[cfg(test)]
    if let Some(now) = tests::now() { return now; }
    SystemTime::now().into()
}

#[cfg(test)]
pub(in crate::wiki_client) mod tests;
