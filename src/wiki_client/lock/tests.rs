/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! ロックheader・期限・非露出・資源状態の単体契約試験
//!

use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::*;

thread_local! {
    static NOW: RefCell<Option<DateTime<Utc>>> = const { RefCell::new(None) };
}

///
/// 試験threadだけで現在時刻を取得する。
///
/// # 戻り値
/// 注入値または実時計指定
///
pub(in crate::wiki_client) fn now() -> Option<DateTime<Utc>> {
    NOW.with(|now| *now.borrow())
}

///
/// 試験threadだけで時刻を固定する。
///
/// # 引数
/// * `value` - 注入RFC3339、Noneなら実時計
///
/// # 戻り値
/// なし
///
pub(in crate::wiki_client) fn set_now(value: Option<&str>) {
    NOW.with(|now| *now.borrow_mut() = value.map(|value|
        DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)));
}

///
/// ダミーheaderを構築する。
///
/// # 引数
/// * `value` - ダミーheader
///
/// # 戻り値
/// 単一header
///
fn headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-page-lock", HeaderValue::from_str(value).unwrap());
    headers
}

///
/// 単一headerだけを読み、秘密をPageIdに写さず期限を解析する。
///
/// # 注記
/// 固定現在時刻でUTC/offset/小数秒とtoken大小文字を照合する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[test]
fn parses_rfc3339_offsets_without_clock_compensation() {
    set_now(Some("2026-10-04T00:00:00Z"));
    let data = parse(&headers("expire=2026-10-04T09:05:00.123+09:00 \
        token=01m42hx77w4m8akw7b31h8qajx")).unwrap();
    assert!(data.valid().is_ok());
    set_now(Some("2026-10-04T00:05:00.123Z"));
    assert_eq!(data.valid(), Err(FailureKind::LockExpired));
    set_now(None);
}

///
/// 重複・欠落・ULID不正・時刻不正を成功資源へしない。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// header値を診断へ出さず有限failureだけを比較する。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[test]
fn rejects_ambiguous_or_invalid_headers() {
    assert!(matches!(parse(&HeaderMap::new()), Err(FailureKind::LockHeader)));
    let valid = "expire=2999-01-01T00:00:00Z \
        token=00000000000000000000000000";
    let mut duplicate = headers(valid);
    duplicate.append("x-page-lock", HeaderValue::from_str(valid).unwrap());
    assert!(matches!(parse(&duplicate), Err(FailureKind::LockHeader)));
    /*
     * field構文と形式固有の不正を別々に拒否する
     */
    for value in ["", "token=00000000000000000000000000",
        "expire=invalid token=00000000000000000000000000",
        "expire=2999-01-01T00:00:00Z token=80000000000000000000000000",
        "expire=2999-01-01T00:00:00Z token=0000000000000000000000000I",
        "expire=2999-01-01T00:00:00Z token=00000000000000000000000000 x=1",
        "expire=2999-01-01T00:00:00Z token=00000000000000000000000000 \
            token=00000000000000000000000001",
        "expire=2999-01-01T00:00:00Z, token=00000000000000000000000000"]
    {
        assert!(matches!(parse(&headers(value)), Err(FailureKind::LockHeader)));
    }
}

///
/// 送信開始/保持/延長とclient/ID結合を独立に検査する。
///
/// # 概要
/// 模擬操作と資源状態を照合し、注記に示す境界を確認する。
///
/// # 注記
/// 秘匿headerはsensitive、Debugはtoken/ID/期限を出さないことも確認。
///
/// # 戻り値
/// なし。契約との差異があればassertで失敗する。
///
#[test]
fn binds_client_and_id_and_replaces_token_exclusively() {
    let old = "expire=2999-01-01T00:00:00Z \
        token=00000000000000000000000001";
    let new = "expire=2999-01-01T00:00:00Z \
        token=00000000000000000000000002";
    let client = Arc::new(());
    let id = PageId::new("00000000000000000000000000").unwrap();
    let mut lease = LockLease::new(parse(&headers(old)).unwrap(), &client,
        id.clone(), Origin::Acquired).unwrap();
    assert!(lease.authentication().unwrap().is_sensitive());
    assert_eq!(lease.check(&Arc::new(()), &id),
        Err(FailureKind::LeaseMismatch));
    let other = PageId::new("00000000000000000000000003").unwrap();
    assert_eq!(lease.check(&client, &other), Err(FailureKind::LeaseMismatch));
    /*
     * 開始後は同じclientでも使用不可、新tokenだけで復帰する
     */
    lease.begin();
    assert_eq!(lease.check(&client, &id), Err(FailureKind::LeaseUnavailable));
    assert_eq!(lease.renew(parse(&headers(old)).unwrap()),
        Err(FailureKind::LockHeader));
    lease.renew(parse(&headers(new)).unwrap()).unwrap();
    assert_eq!(lease.origin(), Origin::Acquired);
    assert!(lease.authentication().unwrap().as_bytes()
        == b"token=00000000000000000000000002");
    let rendered = format!("{lease:?}");
    assert!(!rendered.contains("000000000000000000000000"));
    assert!(!rendered.contains("2999"));
    lease.begin();
    let stop = StopContext::new(Instant::now() + Duration::from_secs(60));
    stop.cancel();
    assert_eq!(lease.retain(&stop), Err(FailureKind::Cancelled));
    assert!(!lease.usable());
}
