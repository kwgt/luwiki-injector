/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 未検証設定を検証済み実行設定へ変換する境界
//!

mod auth;
mod settings;
pub(crate) mod source;
mod warnings;

use std::fmt;
use std::time::{Duration, Instant};

use url::Url;

use crate::cmd_args::UnresolvedRunConfig;
use crate::secret::Secret;

///
/// 入力値を含めない設定検証エラー。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ValidationError {
    Url,
    Prefix,
    Mode,
    Limit(&'static str),
    AuthMissing,
    AuthConflict,
    EmptyCredential(&'static str),
    PasswordUnavailable,
    PromptFailed,
    Source,
    SourceType,
    RefConflict,
    RefValue,
    Worktree,
    Deadline,
}

impl fmt::Display for ValidationError {
    ///
    /// 安全な分類だけを表示する。
    ///
    /// # 概要
    /// 項目名が必要な失敗と固定分類の失敗を分け、入力値や認証値を表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        /*
         * 許可された項目名を伴う失敗を固定文へ写す
         */
        match self {
            Self::Limit(name) => write!(formatter, "{name}が許容範囲外です"),
            Self::EmptyCredential(name) => {
                write!(formatter, "{name}が空です")
            }
            _ => formatter.write_str(match self {
                /*
                 * その他の失敗を入力値のない固定診断へ写す
                 */
                Self::Url => "対象Wiki URLが不正です",
                Self::Prefix => "ページプレフィクスが不正です",
                Self::Mode => "衝突モードが不正です",
                Self::AuthMissing => "Wiki認証情報が不足しています",
                Self::AuthConflict => "BearerとBasicが同時指定されています",
                Self::PasswordUnavailable => "Basicパスワードが不足しています",
                Self::PromptFailed => "パスワードを読み取れません",
                Self::Source => "ページソースが不正です",
                Self::SourceType => "入力種別とページソースが一致しません",
                Self::RefConflict => "Git参照指定が競合しています",
                Self::RefValue => "Git参照指定が空です",
                Self::Worktree => "worktreeはローカルGit専用です",
                Self::Deadline => "処理期限を構築できません",
                _ => unreachable!(),
            }),
        }
    }
}

///
/// 既存ページとの衝突時の動作。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Error,
    Update,
    Overwrite,
}

///
/// 検証済みの入力上限。数値はバイト数・秒・件数を表す。
///
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct RunLimits {
    pub(crate) max_pages: u64,
    pub(crate) max_page_bytes: u64,
    pub(crate) max_zip_entries: u64,
    pub(crate) max_zip_compressed_bytes: u64,
    pub(crate) max_zip_uncompressed_bytes: u64,
    pub(crate) max_git_bytes: u64,
    pub(crate) max_processing_seconds: u64,
    pub(crate) max_http_concurrency: u64,
}

///
/// 検証済みのページ・接続・上限設定。
///
#[allow(dead_code)]
pub(crate) struct RunSettings {
    pub(crate) target: Url,
    pub(crate) prefix: String,
    pub(crate) mode: Mode,
    pub(crate) limits: RunLimits,
}

///
/// 同時指定できないWiki認証方式。
///
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum WikiAuth {
    Bearer(Secret),
    Basic { user: Secret, password: Secret },
}

///
/// 値を含めない起動時の警告。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Warning {
    CliSecret,
    ConfigPermissions,
}

impl fmt::Display for Warning {
    ///
    /// 資格情報を含めない警告を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CliSecret =>
                "CLI認証情報はプロセス一覧や履歴へ露出する可能性があります",
            Self::ConfigPermissions =>
                "認証情報を含む設定ファイルがgroup/otherから読取可能です",
        })
    }
}

///
/// 全項目の検証成功時だけ構築される実行設定。
///
#[allow(dead_code)]
pub(crate) struct ResolvedRunConfig {
    settings: RunSettings,
    auth: WikiAuth,
    github_token: Option<Secret>,
    source: source::SourceSpec,
    warnings: Vec<Warning>,
    deadline: Instant,
}

impl ResolvedRunConfig {
    ///
    /// 検証済みの通常設定を参照する。
    ///
    /// # 戻り値
    /// 接続・ページ・上限設定
    ///
    #[allow(dead_code)]
    pub(crate) fn settings(&self) -> &RunSettings { &self.settings }

    ///
    /// Wiki認証を認証処理の境界へ渡す。
    ///
    /// # 戻り値
    /// 検証済み認証
    ///
    #[allow(dead_code)]
    pub(crate) fn auth(&self) -> &WikiAuth { &self.auth }

    ///
    /// 入力取得へ検証済みソース指定を渡す。
    ///
    /// # 戻り値
    /// 入力ソース指定
    ///
    #[allow(dead_code)]
    pub(crate) fn source(&self) -> &source::SourceSpec { &self.source }

    ///
    /// GitHub認証を取得処理の境界へ渡す。
    ///
    /// # 戻り値
    /// 環境変数由来の任意のGitHub資格情報
    ///
    #[allow(dead_code)]
    pub(crate) fn github_token(&self) -> Option<&Secret> {
        self.github_token.as_ref()
    }

    ///
    /// 起動時の警告を取得する。
    ///
    /// # 戻り値
    /// 秘密値を含めない警告
    ///
    pub(crate) fn warnings(&self) -> &[Warning] { &self.warnings }

    ///
    /// 後続処理で共有する期限を取得する。
    ///
    /// # 戻り値
    /// 設定解決完了後からの処理期限
    ///
    #[allow(dead_code)]
    pub(crate) fn deadline(&self) -> Instant { self.deadline }
}

impl fmt::Debug for ResolvedRunConfig {
    ///
    /// 入力値を表示せず検証状態を示す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ResolvedRunConfig")
            .field("settings", &"[validated]")
            .field("auth", &"[REDACTED]")
            .field("github_token", &self.github_token.is_some())
            .field("source", &self.source)
            .field("warnings", &self.warnings)
            .finish()
    }
}

///
/// 実際のTTYを使って収集済み設定を解決する。
///
/// # 引数
/// * `input` - 入力経路を独立保持する未検証設定
///
/// # 戻り値
/// 全検証成功時だけ公開される実行設定
///
pub(super) fn resolve(input: UnresolvedRunConfig)
    -> Result<ResolvedRunConfig, ValidationError>
{
    resolve_with(input, &mut auth::TerminalPrompt)
}

///
/// 対話境界を注入して実行設定を解決する。
///
/// # 概要
/// 設定と入力種別を検証した後で必要な認証だけを対話収集する。
///
/// # 引数
/// * `input` - 未検証設定
/// * `prompt` - パスワードの取得境界
///
/// # 戻り値
/// 検証済み設定、または値を含めない検証エラー
///
fn resolve_with(
    mut input: UnresolvedRunConfig,
    prompt: &mut impl auth::PasswordPrompt,
) -> Result<ResolvedRunConfig, ValidationError> {
    /*
     * TTY入力より先に通常設定と入力種別を検証する
     */
    let warnings = warnings::collect(&input);
    let settings = settings::resolve(&input)?;
    let source = source::resolve(&input.cli)?;

    /*
     * 採用した資格情報だけを検証し、秘匿型のまま移す
     */
    auth::validate_secret(
        input.environment.github_token.as_ref(), "GitHub token",
    )?;
    let auth = auth::resolve(&mut input, prompt)?;
    let github_token = input.environment.github_token.take();
    let deadline = Instant::now().checked_add(Duration::from_secs(
        settings.limits.max_processing_seconds,
    )).ok_or(ValidationError::Deadline)?;

    Ok(ResolvedRunConfig {
        settings, auth, github_token, source, warnings, deadline,
    })
}

#[cfg(test)]
mod tests;

///
/// 共通検証入口を試す設定を実際のCLI/設定解決で構成する。
///
/// # 概要
/// 製品のprivate生成境界を維持し、期限だけを試験用の値へ置き換える。
/// 実環境の資格情報を読み込まず、CLI秘密指定警告を一件持つ。
///
/// # 引数
/// * `source` - 試験所有の実在するZIP
/// * `deadline` - 試験で共有する絶対期限
/// * `page_bytes` - 仕様範囲内の本文上限（bytes）
///
/// # 戻り値
/// cfg(test)だけで利用する検証済み設定
///
#[cfg(test)]
pub(crate) fn validation_config_fixture(source: std::path::PathBuf,
    deadline: Instant, page_bytes: u64) -> ResolvedRunConfig
{
    use std::ffi::OsString;

    use crate::cmd_args::{ParseOutcome, collect_with, parse_from};

    /*
     * テスト所有の設定とダミー認証だけを構成する
     */
    let area = tempfile::tempdir().expect("validation config area");
    let path = area.path().join("config.toml");
    std::fs::write(&path,
        format!("[limits]\nmax_page_bytes={page_bytes}\n"))
        .expect("validation config file");
    let args = vec![OsString::from("luwiki-injector"),
        OsString::from("--config-path"), path.clone().into_os_string(),
        OsString::from("--token"), OsString::from("dummy-wiki-secret"),
        source.into_os_string()];
    let ParseOutcome::Input(cli) = parse_from(args) else {
        panic!("validation fixture CLI");
    };
    /*
     * 製品と同じ意味検証後に、期限の試験条件だけを注入する
     */
    let input = collect_with(cli, |_| Ok(None), &path)
        .expect("validation fixture collection");
    let mut config = resolve(input).expect("validation fixture resolution");
    config.deadline = deadline;
    config
}

///
/// 実環境を読まず、製品設定解決を通すWiki HTTP試験設定を作る。
///
/// # 概要
/// ダミー認証とloopback対象だけをTOMLへ書き、期限条件を最後に注入する。
///
/// # 引数
/// * `target` - 試験専用の接続先
/// * `bearer` - BearerかBasicか
/// * `deadline` - StopContextと同じ試験期限
///
/// # 戻り値
/// cfg(test)だけの検証済み設定
///
#[cfg(test)]
pub(crate) fn wiki_config_fixture(target: &url::Url, bearer: bool,
    deadline: Instant) -> ResolvedRunConfig
{
    use std::ffi::OsString;

    use crate::cmd_args::{ParseOutcome, collect_with, parse_from};

    let area = tempfile::tempdir().expect("wiki config area");
    let path = area.path().join("config.toml");
    let source = area.path().join("input.zip");
    std::fs::write(&source, b"fixture source").expect("fixture source");
    /*
     * 秘密はダミーだけを設定し、収集・意味検証の製品経路へ通す
     */
    let auth = if bearer { "token='dummy-wiki-secret'" }
        else { "user='dummy-user'\npassword='dummy-password'" };
    std::fs::write(&path, format!("[server]\nurl='{target}'\n{auth}\n"))
        .expect("wiki fixture config");
    let args = vec![OsString::from("luwiki-injector"),
        OsString::from("--config-path"), path.clone().into_os_string(),
        source.into_os_string()];
    let ParseOutcome::Input(cli) = parse_from(args) else {
        panic!("wiki fixture CLI");
    };
    let input = collect_with(cli, |_| Ok(None), &path)
        .expect("wiki fixture collection");
    let mut config = resolve(input).expect("wiki fixture resolution");
    config.deadline = deadline;
    config
}

///
/// 検証失敗時にも安全な起動警告を保持する。
///
/// # 引数
/// * `input` - 収集済みの未検証設定
///
/// # 戻り値
/// 資格情報を含まない警告
///
pub(super) fn startup_warnings(input: &UnresolvedRunConfig) -> Vec<Warning> {
    warnings::collect(input)
}

///
/// 専用環境だけから実Wiki試験の製品設定を解決する。
///
/// # 概要
/// 資格情報をargv/TOMLへ書かず選択した方式だけを収集する。
/// prefixの許可領域は呼出側で全計画とともに検査する。
///
/// # 引数
/// * `prefix` - 試験で許可・検査済みの投入先
/// * `mode` - 製品の衝突モード名
/// * `bearer` - Bearer方式の選択
///
/// # 戻り値
/// 実設定または秘密値を含まない固定失敗分類
///
#[cfg(test)]
pub(crate) fn real_wiki_config(prefix: &str, mode: &str, bearer: bool)
    -> anyhow::Result<ResolvedRunConfig>
{
    use std::ffi::OsString;

    use crate::cmd_args::{ParseOutcome, collect_with, parse_from};

    /*
     * 空設定と合成source指定だけを試験領域へ置く
     */
    let area = tempfile::tempdir().map_err(|_| anyhow::anyhow!("config area"))?;
    let path = area.path().join("config.toml");
    let source = area.path().join("input.zip");
    std::fs::write(&path, b"").map_err(|_| anyhow::anyhow!("config file"))?;
    std::fs::write(&source, b"fixture")
        .map_err(|_| anyhow::anyhow!("source file"))?;
    let args = vec![OsString::from("luwiki-injector"),
        OsString::from("--config-path"), path.clone().into_os_string(),
        OsString::from("--prefix"), OsString::from(prefix),
        OsString::from("--mode"), OsString::from(mode),
        source.into_os_string()];
    let ParseOutcome::Input(cli) = parse_from(args) else {
        return Err(anyhow::anyhow!("config arguments"));
    };
    /*
     * 未選択認証や通常プロセス設定を読まず専用envだけへ対応する
     */
    let input = collect_with(cli, |name| {
        let selected = match name {
            "LUWIKI_URL" => Some("LUWIKI_MS42_TEST_URL"),
            "LUWIKI_TOKEN" if bearer => Some("LUWIKI_MS42_TEST_BEARER_TOKEN"),
            "LUWIKI_USER" if !bearer => Some("LUWIKI_MS42_TEST_BASIC_USER"),
            "LUWIKI_PASSWORD" if !bearer =>
                Some("LUWIKI_MS42_TEST_BASIC_PASSWORD"),
            _ => None,
        };
        let Some(selected) = selected else { return Ok(None); };
        match std::env::var(selected) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            _ => Err(crate::cmd_args::env::EnvironmentError::NotUnicode(
                "MS42 required environment")),
        }
    }, &path).map_err(|_| anyhow::anyhow!("required test environment"))?;
    resolve(input).map_err(|_| anyhow::anyhow!("test configuration"))
}
