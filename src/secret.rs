/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 認証情報の値を表示経路から隔離するモジュール
//!

use std::fmt;

///
/// 認証情報の値を所有し、通常の表示では内容を隠す。
///
#[allow(dead_code)]
pub(crate) struct Secret(String);

impl Secret {
    ///
    /// 認証情報の値を所有する。
    ///
    /// # 引数
    /// * `value` - 認証情報の値
    ///
    /// # 戻り値
    /// 秘匿された値
    ///
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    ///
    /// 認証処理の境界で値を参照する。
    ///
    /// # 戻り値
    /// 認証情報の値
    ///
    #[allow(dead_code)]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    ///
    /// 秘密値を伏せて表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    ///
    /// 秘密値を伏せて表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}
