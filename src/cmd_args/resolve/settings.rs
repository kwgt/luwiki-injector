/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 採用された接続・ページ・上限設定を検証するモジュール
//!

use url::Url;

use crate::cmd_args::UnresolvedRunConfig;

use super::{Mode, RunLimits, RunSettings, ValidationError};

///
/// 優先順位で採用した値だけを意味検証する。
///
/// # 概要
/// 各入力元の候補を上書きせず、採用値を検証済み設定へ移す。
///
/// # 引数
/// * `input` - 収集済みの全候補
///
/// # 戻り値
/// 検証済みの通常設定
///
pub(super) fn resolve(input: &UnresolvedRunConfig)
    -> Result<RunSettings, ValidationError>
{
    /*
     * 項目ごとの優先順位で候補を選ぶ
     */
    let server = input.config.values.server.as_ref();
    let page = input.config.values.page.as_ref();
    let target = input.cli.target_server.as_deref()
        .or(input.environment.url.as_deref())
        .or(server.and_then(|v| v.url.as_deref()))
        .unwrap_or(input.defaults.server_url);
    let prefix = input.cli.prefix.as_deref()
        .or(page.and_then(|v| v.prefix.as_deref()))
        .unwrap_or(input.defaults.prefix);
    let mode = input.cli.mode.as_deref()
        .or(page.and_then(|v| v.mode.as_deref()))
        .unwrap_or(input.defaults.mode);

    /*
     * 選択された値だけを検証する
     */
    validate_prefix(prefix)?;
    Ok(RunSettings {
        target: validate_target(target)?,
        prefix: prefix.to_owned(),
        mode: validate_mode(mode)?,
        limits: resolve_limits(input)?,
    })
}

///
/// Wikiの接続先をHTTP/HTTPSのoriginに限定する。
///
/// # 概要
/// 元の文字列と解析結果を照合し、暗黙のパス補正を拒否する。
///
/// # 引数
/// * `value` - 採用されたURL文字列
///
/// # 戻り値
/// ホストを持ち、資格情報・query・fragment・非rootパスがないURL
///
fn validate_target(value: &str) -> Result<Url, ValidationError> {
    /*
     * URLパーサによる空白・パスの暗黙補正を許さない
     */
    let (_, rest) = value.split_once("://").ok_or(ValidationError::Url)?;
    let authority = rest.split('/').next().unwrap_or("");
    if value.chars().any(|v| v.is_whitespace() || v.is_control())
        || authority.contains('@')
        || rest.find('/').is_some_and(|i| &rest[i..] != "/")
    {
        return Err(ValidationError::Url);
    }
    let url = Url::parse(value).map_err(|_| ValidationError::Url)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(ValidationError::Url);
    }
    Ok(url)
}

///
/// 絶対Wikiプレフィクスの禁止要素を検証する。
///
/// # 引数
/// * `value` - 採用されたプレフィクス
///
/// # 戻り値
/// rootまたは禁止要素を含まない絶対パスなら成功
///
fn validate_prefix(value: &str) -> Result<(), ValidationError> {
    if !value.starts_with('/')
        || value.contains('\\')
        || value.contains('\u{fffd}')
        || value.chars().any(|v| v.is_whitespace() || v.is_control())
    {
        return Err(ValidationError::Prefix);
    }
    if value != "/" && value[1..].split('/')
        .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(ValidationError::Prefix);
    }
    Ok(())
}

///
/// 衝突モードを排他的な列挙値へ変換する。
///
/// # 引数
/// * `value` - 採用されたモード
///
/// # 戻り値
/// 仕様上の三つのモード、または不正値エラー
///
fn validate_mode(value: &str) -> Result<Mode, ValidationError> {
    match value {
        "error" => Ok(Mode::Error),
        "update" => Ok(Mode::Update),
        "overwrite" => Ok(Mode::Overwrite),
        _ => Err(ValidationError::Mode),
    }
}

///
/// 一つの正整数上限を検証する。
///
/// # 引数
/// * `value` - 採用値
/// * `maximum` - 許容最大値
/// * `name` - 仕様で定義されたキー名
///
/// # 戻り値
/// 検証済み正整数
///
fn bounded(value: i64, maximum: i64, name: &'static str)
    -> Result<u64, ValidationError>
{
    if !(1..=maximum).contains(&value) {
        return Err(ValidationError::Limit(name));
    }
    Ok(value as u64)
}

///
/// 設定値または既定値から八つの上限を解決する。
///
/// # 概要
/// 各設定キーの省略時だけ既定値を採用し、八つの上限を範囲検証する。
///
/// # 引数
/// * `input` - 全入力と独立した既定値
///
/// # 戻り値
/// 範囲検証済みの上限
///
fn resolve_limits(input: &UnresolvedRunConfig)
    -> Result<RunLimits, ValidationError>
{
    /*
     * 各キーの省略時だけ既定値を使う
     */
    let config = input.config.values.limits.as_ref();
    let defaults = &input.defaults.limits;
    macro_rules! limit {
        ($field:ident, $max:expr) => {
            bounded(
                config.and_then(|v| v.$field).unwrap_or(defaults.$field),
                $max,
                stringify!($field),
            )?
        };
    }
    Ok(RunLimits {
        max_pages: limit!(max_pages, 4_000),
        max_page_bytes: limit!(max_page_bytes, 4_194_304),
        max_zip_entries: limit!(max_zip_entries, 20_000),
        max_zip_compressed_bytes: limit!(max_zip_compressed_bytes, 536_870_912),
        max_zip_uncompressed_bytes:
            limit!(max_zip_uncompressed_bytes, 1_073_741_824),
        max_git_bytes: limit!(max_git_bytes, 2_147_483_648),
        max_processing_seconds: limit!(max_processing_seconds, 1_200),
        max_http_concurrency: limit!(max_http_concurrency, 4),
    })
}

#[cfg(test)]
mod tests {
    use super::{bounded, validate_mode, validate_prefix, validate_target};

    ///
    /// originだけを許し、暗黙のパス補正と資格情報を拒否する。
    ///
    /// # 概要
    /// 正常なHTTP/HTTPS originと禁止要素を含むURLを分けて検証する。
    ///
    /// # 注記
    /// HTTP/HTTPSの正常値と各禁止要素を表で照合する。
    ///
    /// # 戻り値
    /// なし。接続先URLの許容・拒否が仕様と異なる場合はassertで失敗する。
    ///
    #[test]
    fn validates_target_origins() {
        /*
         * 許容するHTTPとHTTPSの接続先を確認する
         */
        for value in ["http://localhost:8080", "https://wiki.example/"] {
            assert!(validate_target(value).is_ok());
        }
        /*
         * 認証埋込・非rootパス・query等の禁止要素を拒否する
         */
        for value in [
            "ftp://wiki.example", "https://user:secret@wiki.example",
            "https://@wiki.example", "https://wiki.example/wiki",
            "https://wiki.example/a/..", "https://wiki.example?token=x",
            "https://wiki.example#x", "https://wiki.example/ ",
            "https:///", "https:wiki.example",
        ] {
            assert!(validate_target(value).is_err());
        }
    }

    ///
    /// prefixの破損・危険要素とmodeの許容集合を確認する。
    ///
    /// # 概要
    /// 正常なprefixと禁止要素、modeの許容値と大文字表記の拒否を確認する。
    ///
    /// # 注記
    /// root・日本語パスと、禁止要素を含む例を照合する。
    ///
    /// # 戻り値
    /// なし。prefixやmodeの許容・拒否が仕様と異なる場合はassertで失敗する。
    ///
    #[test]
    fn validates_page_settings() {
        /*
         * rootと日本語prefixの正常値を確認する
         */
        assert!(validate_prefix("/").is_ok());
        assert!(validate_prefix("/ワークエリア/docs").is_ok());
        /*
         * prefixの空白・特殊segment・不正文字を拒否する
         */
        for value in [
            "", "relative", "/a/", "/a//b", "/a/../b", "/./b",
            "/a b", "/a\\b", "/a\0b", "/a\u{fffd}",
        ] {
            assert!(validate_prefix(value).is_err());
        }
        /*
         * modeの許容集合と大文字表記の拒否を照合する
         */
        for value in ["error", "update", "overwrite"] {
            assert!(validate_mode(value).is_ok());
        }
        assert!(validate_mode("UPDATE").is_err());
    }

    ///
    /// 八つの上限の両端と超過を確認する。
    ///
    /// # 注記
    /// 各許容最大値に対し負数・0・上下端・最大超過を照合する。
    ///
    /// # 戻り値
    /// なし。各上限の許容端点と範囲外を区別できなければassertで失敗する。
    ///
    #[test]
    fn validates_limit_boundaries() {
        for max in [
            4_000, 4_194_304, 20_000, 536_870_912,
            1_073_741_824, 2_147_483_648, 1_200, 4,
        ] {
            assert!(bounded(-1, max, "limit").is_err());
            assert!(bounded(0, max, "limit").is_err());
            assert_eq!(bounded(1, max, "limit"), Ok(1));
            assert_eq!(bounded(max, max, "limit"), Ok(max as u64));
            assert!(bounded(max + 1, max, "limit").is_err());
        }
    }
}
