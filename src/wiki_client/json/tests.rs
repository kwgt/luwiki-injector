/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! JSON選択抽出の構文・巨大未知値・metadata path境界試験
//!

use std::io::Cursor;
use std::time::{Duration, Instant};

use crate::report::types::{PageId, PagePath};
use crate::source::StopContext;

use super::super::types::PathRelation;
use super::*;

const ID: &str = "00000000000000000000000000";

///
/// 単体試験の停止境界を作る。
///
/// # 戻り値
/// 期限内の共有context
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(60))
}

///
/// lookup JSONを製品parserへ渡す。
///
/// # 引数
/// * `body` - JSON fixture
///
/// # 戻り値
/// 完全検査結果
///
fn id(body: &[u8]) -> Result<JsonValue, FailureKind> {
    parse(Cursor::new(body), Purpose::Id, stop())
}

///
/// metadata fixtureを文字列連結で生成する。
///
/// # 引数
/// * `path` - JSON string内部の試験表記
/// * `extra` - 任意の追加field
///
/// # 戻り値
/// JSON fixture
///
fn metadata(path: &str, extra: &str) -> String {
    format!(r#"{{"page_info":{{"path":{{"kind":"current","value":"{path}"}},
        "deleted":false,"locked":true}},"revision_info":{{"revision":3}}
        {extra}}}"#)
}

///
/// metadata parserに期待pathと既知IDを渡す。
///
/// # 引数
/// * `body` - JSON fixture
/// * `expected` - 検証済み期待path
///
/// # 戻り値
/// 完全検査されたmetadata
///
fn meta(body: &str, expected: &str) -> Result<PageMetadata, FailureKind> {
    match parse(Cursor::new(body), Purpose::Metadata {
        id: PageId::new(ID).unwrap(),
        expected: PagePath::new(expected).unwrap(),
    }, stop())? {
        JsonValue::Metadata(value) => Ok(value),
        _ => Err(FailureKind::Protocol),
    }
}

///
/// escaped同名keyと各必要containerの重複・欠落を拒否する。
///
/// # 概要
/// 別objectから必要fieldを合成して成功する経路を作らない。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 復号後duplicate、型不正、末尾と未完EOFを明示fixtureで照合する。
///
#[test]
fn rejects_required_duplicates_missing_types_and_trailing_data() {
    /*
     * IDの曖昧さと完了前の部分成功を拒否する
     */
    for body in [
        format!(r#"{{"id":"{ID}","\u0069d":"{ID}"}}"#),
        format!(r#"{{"id":"{ID}"}} false"#),
        format!(r#"{{"id":"{ID}","#),
        r#"{"id":null}"#.into(), r#"{"id":42}"#.into(), "{}".into(),
    ] {
        assert_eq!(id(body.as_bytes()).unwrap_err(), FailureKind::Protocol);
    }
    /*
     * metadataの必要containerとfieldはそれぞれ一意にする
     */
    let valid = metadata("/a", "");
    for body in [
        valid.replace(r#""page_info":"#, r#""page_info":{},"page_info":"#),
        valid.replace(r#""revision_info":"#,
            r#""revision_info":{},"revision_info":"#),
        valid.replace(r#""path":"#, r#""path":{},"path":"#),
        valid.replace(r#""revision":3"#, r#""revision":3,"revision":4"#),
        valid.replace(r#""revision":3"#, r#""revision":null"#),
        valid.replace(r#","locked":true"#, ""),
        valid.replace(r#""deleted":false"#, r#""deleted":"false""#),
    ] {
        assert_eq!(meta(&body, "/a").unwrap_err(), FailureKind::Protocol);
    }
}

///
/// 未知の巨大key/string/number/depthを保持せず構文だけ検査する。
///
/// # 概要
/// 通常の再帰parserのdepthを超えるfixtureをfile stackで処理する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// unknown内の偽idを無視し、任意精度数値へ変換せず既知IDを返す。
///
#[test]
fn handles_large_unknown_values_without_collecting_or_schema_leakage() {
    let depth = 4096;
    let key = "k".repeat(100_000);
    let string = "s".repeat(100_000);
    let number = "9".repeat(100_000);
    let nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    let body = format!(r#"{{"{key}":"{string}","nested":{nested},
        "number":1e{number},"other":{{"id":"invalid","path":null}},
        "id":"{ID}"}}"#);
    assert!(matches!(id(body.as_bytes()), Ok(JsonValue::Id(value))
        if value.as_str() == ID));
}

///
/// JSONのnumber・escape・Unicodeと区切りを厳格に検査する。
///
/// # 概要
/// 未知値でも不正構文を読み飛ばさず、値の数値変換は要求しない。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 無効literal・number・surrogate・raw制御・UTF-8を全体parserへ通す。
///
#[test]
fn validates_unknown_grammar_unicode_and_eof() {
    /*
     * 未知scalarの正常表記と境界を確認する
     */
    for value in ["null", "true", "false", "-0", "1.25e-99",
        r#""\ud83d\ude00""#, r#""\u0061\n\t\\\"""#]
    {
        let body = format!(r#"{{"u":{value},"id":"{ID}"}}"#);
        assert!(id(body.as_bytes()).is_ok(), "valid scalar rejected");
    }
    /*
     * 未知値の不正も全体成功を拒否する
     */
    for value in ["00", "+1", ".1", "1.", "1e", "--1", "NaN", "nul",
        "[1,]", "{\"a\":1,}", r#""\ud800""#, r#""\udc00""#,
        r#""\ud800\u0041""#, r#""\x20""#, "\"raw\ncontrol\""]
    {
        let body = format!(r#"{{"u":{value},"id":"{ID}"}}"#);
        assert_eq!(id(body.as_bytes()).unwrap_err(), FailureKind::Protocol);
    }
    let mut body = format!(r#"{{"u":""#).into_bytes();
    body.extend_from_slice(b"\xff");
    body.extend_from_slice(format!(r#"","id":"{ID}"}}"#).as_bytes());
    assert_eq!(id(&body).unwrap_err(), FailureKind::Protocol);
}

///
/// decoded pathの相違は事実として返し、危険pathだけを拒否する。
///
/// # 概要
/// Unicode表記を正規化せず、escaped同一値と違う表記を区別する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// root、禁止segment、Unicode空白・制御・置換文字も確認する。
///
#[test]
fn compares_paths_without_collecting_normalizing_or_merging_fields() {
    let value = meta(&metadata(r#"/\u0061"#, ""), "/a").unwrap();
    assert_eq!(value.id().as_str(), ID);
    assert_eq!(value.path_relation(), PathRelation::MatchesExpected);
    assert!(meta(&metadata("/", ""), "/").is_ok());
    assert_eq!(meta(&metadata("/e\u{301}", ""), "/é").unwrap()
        .path_relation(), PathRelation::Different);
    /*
     * ASCIIとUnicodeの禁止要素を同じdecoded文字境界で拒否する
     */
    for path in ["/a//b", "/a/", "/.", "/a/..", "/a\\\\b",
        "/a b", "/a\u{3000}b", r#"/a\u0000b"#, r#"/a\ufffdb"#, "relative"]
    {
        assert_eq!(meta(&metadata(path, ""), "/a").unwrap_err(),
            FailureKind::Protocol);
    }
}

///
/// error診断は有限分類と数値だけを取り出す。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 任意message/property_pathを未知値として捨て、巨大値でも表示しない。
///
#[test]
fn selects_only_finite_error_diagnostics() {
    let body = format!(r#"{{"kind":"front_matter","detail":{{
        "type":"syntax","line":2,"column":7,"message":"{}",
        "property_path":"private-path"}}}}"#, "secret".repeat(20_000));
    let value = parse(Cursor::new(body), Purpose::Error, stop()).unwrap();
    assert!(matches!(value, JsonValue::Error(Some(Diagnostics {
        kind: FrontMatterKind::Syntax, line: Some(2), column: Some(7),
    }))));
}

///
/// file-backed stackは復帰後も最大depth分だけのfileでstateを保持する。
///
/// # 戻り値
/// なし。fixtureと確定事実をassertで検査する。
///
/// # 注記
/// 異なるframeを積み戻し、top一つの状態と固定record量を照合する。
///
#[test]
fn file_stack_roundtrips_frames_without_memory_depth_stack() {
    let mut stack = Stack::new().unwrap();
    /*
     * frameの保存順と復帰順を異なる値で確認する
     */
    for i in 0..1000 {
        let mut frame = Frame::new((i % 8) as u8, false);
        frame.seen = i;
        stack.push(frame).unwrap();
    }
    for i in (0..1000).rev() {
        assert_eq!(stack.top.unwrap().seen, i);
        stack.pop().unwrap();
    }
    assert!(stack.top.is_none());
}
