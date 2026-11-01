/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 認証値を含めない起動時警告を生成するモジュール
//!

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::cmd_args::UnresolvedRunConfig;

use super::Warning;

///
/// CLI指定と設定ファイルの権限から警告を作る。
///
/// # 概要
/// CLIへの秘密値直接指定と、認証キーを含む設定の読取権限を独立して警告する。
///
/// # 引数
/// * `input` - 収集済みの入力と読込ファイルのmetadata
///
/// # 戻り値
/// 秘密値を含まない警告。警告だけでは処理を拒否しない。
///
pub(super) fn collect(input: &UnresolvedRunConfig) -> Vec<Warning> {
    /*
     * 不採用でもCLIに秘密値を直接指定した事実を警告する
     */
    let mut warnings = Vec::new();
    if input.cli.token.is_some() || input.cli.user.is_some()
        || input.cli.password.is_some()
    {
        warnings.push(Warning::CliSecret);
    }

    /*
     * 内容と同じファイルの権限に基づいて判定する
     */
    let has_auth = input.config.values.server.as_ref().is_some_and(|v| {
        v.token.is_some() || v.user.is_some() || v.password.is_some()
    });
    #[cfg(unix)]
    let readable = input.config.permissions.as_ref()
        .is_some_and(|v| v.mode() & 0o044 != 0);
    #[cfg(not(unix))]
    let readable = false;
    if has_auth && readable { warnings.push(Warning::ConfigPermissions); }
    warnings
}
