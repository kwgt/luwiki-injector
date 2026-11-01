/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 認証の優先順位と条件付きTTY入力を扱う境界
//!

use std::io::IsTerminal;

use crate::cmd_args::UnresolvedRunConfig;
use crate::secret::Secret;

use super::{ValidationError, WikiAuth};

///
/// 対話入力をテストから制御する境界。
///
pub(super) trait PasswordPrompt {
    ///
    /// 対話を許容するTTYかを調べる。
    ///
    /// # 戻り値
    /// 標準入力がTTYならtrue
    ///
    fn is_tty(&self) -> bool;

    ///
    /// 秘密値としてパスワードを読む。
    ///
    /// # 戻り値
    /// パスワード、または値を含まない読込エラー
    ///
    fn read(&mut self) -> Result<Secret, ValidationError>;
}

///
/// プロセスのTTYから非表示でパスワードを収集する。
///
pub(super) struct TerminalPrompt;

impl PasswordPrompt for TerminalPrompt {
    ///
    /// 標準入力のTTY状態を返す。
    ///
    /// # 戻り値
    /// 対話入力を許容する場合true
    ///
    fn is_tty(&self) -> bool { std::io::stdin().is_terminal() }

    ///
    /// rpasswordでパスワードを読む。
    ///
    /// # 戻り値
    /// Secretに移した入力値、または安全なエラー
    ///
    fn read(&mut self) -> Result<Secret, ValidationError> {
        rpassword::prompt_password("Wiki Basic password: ")
            .map(Secret::new)
            .map_err(|_| ValidationError::PromptFailed)
    }
}

///
/// 任意の採用資格情報が空でないことを検証する。
///
/// # 引数
/// * `value` - 採用された秘匿値
/// * `name` - 値を含めない項目名
///
/// # 戻り値
/// 未指定または非空なら成功
///
pub(super) fn validate_secret(value: Option<&Secret>, name: &'static str)
    -> Result<(), ValidationError>
{
    if value.is_some_and(|v| v.expose().is_empty()) {
        return Err(ValidationError::EmptyCredential(name));
    }
    Ok(())
}

///
/// 認証を項目ごとの優先順位で解決する。
///
/// # 概要
/// CLI、環境、TOMLの採用値を秘匿型のまま移し、必要時だけ対話する。
///
/// # 引数
/// * `input` - 消費する未検証設定
/// * `prompt` - TTY状態とパスワード取得境界
///
/// # 戻り値
/// 排他的な認証方式、または値を含めないエラー
///
pub(super) fn resolve(
    input: &mut UnresolvedRunConfig,
    prompt: &mut impl PasswordPrompt,
) -> Result<WikiAuth, ValidationError> {
    /*
     * 項目ごとに採用する候補だけを移す
     */
    let token = input.cli.token.take()
        .or_else(|| input.environment.token.take())
        .or_else(|| input.config.values.server.as_mut()
            .and_then(|v| v.token.take()));
    let user = input.cli.user.take()
        .or_else(|| input.environment.user.take())
        .or_else(|| input.config.values.server.as_mut()
            .and_then(|v| v.user.take()));
    let password = input.cli.password.take()
        .or_else(|| input.environment.password.take())
        .or_else(|| input.config.values.server.as_mut()
            .and_then(|v| v.password.take()));

    /*
     * 方式の同時指定と空の採用値を対話前に拒否する
     */
    if token.is_some() && (user.is_some() || password.is_some()) {
        return Err(ValidationError::AuthConflict);
    }
    validate_secret(token.as_ref(), "Wiki token")?;
    validate_secret(user.as_ref(), "Basic user")?;
    validate_secret(password.as_ref(), "Basic password")?;
    if let Some(token) = token {
        return Ok(WikiAuth::Bearer(token));
    }
    let user = user.ok_or(ValidationError::AuthMissing)?;

    /*
     * ユーザー名確定・パスワード不足・TTY時だけ取得する
     */
    let password = match password {
        Some(password) => password,
        None if prompt.is_tty() => prompt.read()?,
        None => return Err(ValidationError::PasswordUnavailable),
    };
    validate_secret(Some(&password), "Basic password")?;
    Ok(WikiAuth::Basic { user, password })
}
