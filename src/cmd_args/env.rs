/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 設定に使用する環境変数を収集するモジュール
//!

use std::fmt;

use crate::secret::Secret;

///
/// 環境変数の読込失敗を安全な分類で表す。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnvironmentError {
    NotUnicode(&'static str),
}

impl fmt::Display for EnvironmentError {
    ///
    /// 環境変数の値を含まずにエラーを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUnicode(name) => {
                write!(formatter, "{name}の文字列を読み取れません")
            }
        }
    }
}

///
/// 環境変数から得た未検証の値。
///
#[allow(dead_code)]
pub(super) struct EnvironmentInput {
    pub(super) url: Option<String>,
    pub(super) token: Option<Secret>,
    pub(super) user: Option<Secret>,
    pub(super) password: Option<Secret>,
    pub(super) github_token: Option<Secret>,
}

impl EnvironmentInput {
    ///
    /// プロセスの環境変数を収集する。
    ///
    /// # 戻り値
    /// 未検証の環境入力。非Unicode値は安全なエラーになる。
    ///
    pub(super) fn collect() -> Result<Self, EnvironmentError> {
        Self::collect_with(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(EnvironmentError::NotUnicode(name))
            }
        })
    }

    ///
    /// 指定した取得関数から環境入力を収集する。
    ///
    /// # 引数
    /// * `get` - 環境変数名から値を取得する関数
    ///
    /// # 戻り値
    /// 各項目の指定有無を保持する未検証入力
    ///
    pub(super) fn collect_with<F>(mut get: F) -> Result<Self, EnvironmentError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, EnvironmentError>,
    {
        /*
         * Wiki認証とGitHub認証を別の値として保持する
         */
        Ok(Self {
            url: get("LUWIKI_URL")?,
            token: get("LUWIKI_TOKEN")?.map(Secret::new),
            user: get("LUWIKI_USER")?.map(Secret::new),
            password: get("LUWIKI_PASSWORD")?.map(Secret::new),
            github_token: get("GITHUB_TOKEN")?.map(Secret::new),
        })
    }
}

impl fmt::Debug for EnvironmentInput {
    ///
    /// 値を伏せて指定有無を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentInput")
            .field("url", &self.url.is_some())
            .field("token", &self.token.is_some())
            .field("user", &self.user.is_some())
            .field("password", &self.password.is_some())
            .field("github_token", &self.github_token.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::EnvironmentInput;

    ///
    /// 指定有無とGitHub認証の独立性を確認する。
    ///
    /// # 概要
    /// 対応する環境変数を擬似入力し、WikiとGitHubの認証分離・保持・表示を確認する。
    ///
    /// # 注記
    /// 五つの環境変数を擬似的に与え、各欄を照合する。
    ///
    /// # 戻り値
    /// なし。収集値や認証分離・表示境界が異なる場合はassertで失敗する。
    ///
    #[test]
    fn collects_only_supported_variables() {
        /*
         * 対応変数と無関係な変数を擬似入力して収集する
         */
        let vars = HashMap::from([
            ("LUWIKI_URL", "https://wiki.example"),
            ("LUWIKI_TOKEN", "wiki-token"),
            ("LUWIKI_USER", "wiki-user"),
            ("LUWIKI_PASSWORD", "wiki-password"),
            ("GITHUB_TOKEN", "github-token"),
            ("UNRELATED", "ignored"),
        ]);
        let input = EnvironmentInput::collect_with(|name| {
            Ok(vars.get(name).map(|value| (*value).to_owned()))
        })
        .expect("環境入力の収集");

        /*
         * 採用した各値とWiki・GitHub認証の独立性を照合する
         */
        assert_eq!(input.url.as_deref(), Some("https://wiki.example"));
        assert_eq!(
            input.token.as_ref().map(|v| v.expose()),
            Some("wiki-token")
        );
        assert_eq!(
            input.github_token.as_ref().map(|v| v.expose()),
            Some("github-token")
        );
        assert_eq!(
            input.user.as_ref().map(|v| v.expose()),
            Some("wiki-user")
        );
        assert_eq!(
            input.password.as_ref().map(|v| v.expose()),
            Some("wiki-password")
        );
        assert!(!format!("{input:?}").contains("wiki-password"));
    }

    ///
    /// 未設定と空文字列指定を区別できることを確認する。
    ///
    /// # 注記
    /// トークンだけを空文字列で与え、他項目を省略する。
    ///
    /// # 戻り値
    /// なし。未設定と空文字列を区別できなければassertで失敗する。
    ///
    #[test]
    fn preserves_empty_value_as_present() {
        let input = EnvironmentInput::collect_with(|name| {
            Ok((name == "LUWIKI_TOKEN").then(String::new))
        })
        .expect("環境入力の収集");

        assert!(input.url.is_none());
        assert_eq!(input.token.as_ref().map(|v| v.expose()), Some(""));
    }
}
