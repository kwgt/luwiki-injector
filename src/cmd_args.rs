/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! コマンドライン引数を未検証の入力として収集するモジュール
//!

mod config;
mod env;
pub(crate) mod resolve;

pub(crate) use resolve::ResolvedRunConfig;
#[cfg(test)]
pub(crate) use resolve::validation_config_fixture;

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use clap::{error::ErrorKind, ArgAction, CommandFactory, Parser};

use crate::cmd_args::config::{ConfigError, LoadedConfig};
use crate::cmd_args::env::{EnvironmentError, EnvironmentInput};
use crate::secret::Secret;

///
/// clapが解釈するコマンドラインの構文。
///
#[derive(Parser)]
#[command(
    name = "luwiki-injector",
    version,
    about = "kwgt/luwikiへMarkdownページを投入する",
    long_about = None,
    disable_version_flag = true
)]
#[allow(dead_code)]
struct RawArgs {
    /// root/配下を投入するページソース
    #[arg(value_name = "PAGE-SOURCE")]
    page_source: String,

    /// 対象Wikiサーバー
    #[arg(value_name = "TARGET-SERVER")]
    target_server: Option<String>,

    /// TOML設定ファイル
    #[arg(short = 'c', long = "config-path", value_name = "FILE")]
    config_path: Option<PathBuf>,

    /// Wikiページのプレフィクス
    #[arg(short = 'p', long, value_name = "PAGE-PATH")]
    prefix: Option<String>,

    /// 既存ページとの衝突時の動作
    #[arg(short = 'm', long, value_name = "MODE")]
    mode: Option<String>,

    /// ページソースの種別
    #[arg(short = 's', long, value_name = "TYPE")]
    source_type: Option<String>,

    /// Wiki認証用Bearerトークン
    #[arg(short = 't', long, value_name = "TOKEN")]
    token: Option<String>,

    /// Wiki Basic認証のユーザー名
    #[arg(short = 'u', long, value_name = "USER")]
    user: Option<String>,

    /// Wiki Basic認証のパスワード
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,

    /// Gitブランチ
    #[arg(long, value_name = "BRANCH")]
    branch: Option<String>,

    /// Gitタグ
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,

    /// GitコミットID
    #[arg(long, value_name = "COMMIT")]
    commit: Option<String>,

    /// ローカルGit作業ツリーを使う指定
    #[arg(long)]
    worktree: bool,

    /// プログラムのバージョンを表示
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    version: Option<bool>,
}

///
/// 収集済みで、優先順位や意味をまだ検証していないCLI入力。
///
#[allow(dead_code)]
pub(crate) struct CliInput {
    pub(crate) page_source: String,
    pub(crate) target_server: Option<String>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) prefix: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) source_type: Option<String>,
    pub(crate) token: Option<Secret>,
    pub(crate) user: Option<Secret>,
    pub(crate) password: Option<Secret>,
    pub(crate) branch: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) worktree: bool,
}

impl From<RawArgs> for CliInput {
    ///
    /// 引数値を未検証入力へ移し、認証値を秘匿型に包む。
    ///
    /// # 引数
    /// * `args` - clapが解釈した引数
    ///
    /// # 戻り値
    /// 未検証のCLI入力
    ///
    fn from(args: RawArgs) -> Self {
        /*
         * 各指定値を保持し、認証値だけを秘匿型へ移す
         */
        Self {
            page_source: args.page_source,
            target_server: args.target_server,
            config_path: args.config_path,
            prefix: args.prefix,
            mode: args.mode,
            source_type: args.source_type,
            token: args.token.map(Secret::new),
            user: args.user.map(Secret::new),
            password: args.password.map(Secret::new),
            branch: args.branch,
            tag: args.tag,
            commit: args.commit,
            worktree: args.worktree,
        }
    }
}

impl fmt::Debug for CliInput {
    ///
    /// 値を出さずに各入力の指定有無を表示する。
    ///
    /// # 概要
    /// 各候補値を出さず、位置引数の固定表示・任意項目の指定有無・worktree指定を返す。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        /*
         * 未検証値を表示せず、指定有無だけを並べる
         */
        formatter
            .debug_struct("CliInput")
            .field("page_source", &"[specified]")
            .field("target_server", &self.target_server.is_some())
            .field("config_path", &self.config_path.is_some())
            .field("prefix", &self.prefix.is_some())
            .field("mode", &self.mode.is_some())
            .field("source_type", &self.source_type.is_some())
            .field("token", &self.token.is_some())
            .field("user", &self.user.is_some())
            .field("password", &self.password.is_some())
            .field("branch", &self.branch.is_some())
            .field("tag", &self.tag.is_some())
            .field("commit", &self.commit.is_some())
            .field("worktree", &self.worktree)
            .finish()
    }
}

///
/// 仕様で定められた八つの入力上限の既定値。
///
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct DefaultLimits {
    pub(crate) max_pages: i64,
    pub(crate) max_page_bytes: i64,
    pub(crate) max_zip_entries: i64,
    pub(crate) max_zip_compressed_bytes: i64,
    pub(crate) max_zip_uncompressed_bytes: i64,
    pub(crate) max_git_bytes: i64,
    pub(crate) max_processing_seconds: i64,
    pub(crate) max_http_concurrency: i64,
}

impl Default for DefaultLimits {
    ///
    /// 設定仕様書の暫定既定値を構築する。
    ///
    /// # 戻り値
    /// 八つの入力上限の既定値
    ///
    fn default() -> Self {
        Self {
            max_pages: 1_000,
            max_page_bytes: 1_048_576,
            max_zip_entries: 5_000,
            max_zip_compressed_bytes: 134_217_728,
            max_zip_uncompressed_bytes: 268_435_456,
            max_git_bytes: 536_870_912,
            max_processing_seconds: 300,
            max_http_concurrency: 1,
        }
    }
}

///
/// CLIや設定値とは区別して保持する既定値。
///
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct DefaultValues {
    pub(crate) server_url: &'static str,
    pub(crate) prefix: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) source_type: &'static str,
    pub(crate) limits: DefaultLimits,
}

impl Default for DefaultValues {
    ///
    /// CLIと設定の仕様で定められた既定値を構築する。
    ///
    /// # 戻り値
    /// 入力値から独立した既定値
    ///
    fn default() -> Self {
        Self {
            server_url: "https://localhost:8080",
            prefix: "/",
            mode: "error",
            source_type: "auto",
            limits: DefaultLimits::default(),
        }
    }
}

///
/// 全入力経路を混ぜずに所有する未検証設定。
///
#[allow(dead_code)]
pub(crate) struct UnresolvedRunConfig {
    cli: CliInput,
    environment: EnvironmentInput,
    config: LoadedConfig,
    defaults: DefaultValues,
}

impl UnresolvedRunConfig {
    ///
    /// 独立した収集結果を未検証設定へまとめる。
    ///
    /// # 引数
    /// * `cli` - CLI引数
    /// * `environment` - 環境変数
    /// * `config` - TOML設定ファイル
    ///
    /// # 戻り値
    /// 入力元ごとの候補値と既定値を保持する設定
    ///
    fn new(
        cli: CliInput,
        environment: EnvironmentInput,
        config: LoadedConfig,
    ) -> Self {
        Self {
            cli,
            environment,
            config,
            defaults: DefaultValues::default(),
        }
    }
}

impl fmt::Debug for UnresolvedRunConfig {
    ///
    /// 収集状態だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnresolvedRunConfig")
            .field("cli", &self.cli)
            .field("environment", &self.environment)
            .field("config", &self.config)
            .field("defaults", &"[specified]")
            .finish()
    }
}

///
/// CLI解析後の収集失敗を安全な分類で表す。
///
#[derive(Debug)]
pub(crate) enum CollectionError {
    Environment(EnvironmentError),
    Config(ConfigError),
    Validation(resolve::ValidationError),
}

impl fmt::Display for CollectionError {
    ///
    /// 未検証値を含めずにエラーを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(error) => write!(formatter, "{error}"),
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Validation(error) => write!(formatter, "{error}"),
        }
    }
}

///
/// エントリポイントへ返す引数収集の結果。
///
pub(crate) enum CollectedOutcome {
    Input(ResolvedRunConfig),
    Help(String),
    Version(String),
    Invalid,
    Error { error: CollectionError, warnings: Vec<resolve::Warning> },
}

///
/// 引数解析の結果。helpとversionは実行処理へ渡さない。
///
pub(crate) enum ParseOutcome {
    Input(CliInput),
    Help(String),
    Version(String),
    Invalid,
}

///
/// 実際のプロセス引数を解析する。
///
/// # 概要
/// 正常表示を先に返し、収集・検証失敗時も安全な警告を残す。
///
/// # 戻り値
/// 未検証入力、正常な表示要求、または値を含まない構文エラー
///
pub(crate) fn parse() -> CollectedOutcome {
    /*
     * helpとversionは環境・設定を読む前に返す
     */
    match parse_from(std::env::args_os()) {
        ParseOutcome::Input(cli) => {
            /*
             * 収集失敗時もCLI秘密指定という事実だけを残す
             */
            let cli_secret = cli.token.is_some() || cli.user.is_some()
                || cli.password.is_some();
            match collect(cli) {
                Ok(input) => {
                    let warnings = resolve::startup_warnings(&input);
                    match resolve::resolve(input) {
                        Ok(input) => CollectedOutcome::Input(input),
                        Err(error) => CollectedOutcome::Error {
                            error: CollectionError::Validation(error), warnings,
                        },
                    }
                }
                Err(error) => CollectedOutcome::Error {
                    error, warnings: if cli_secret {
                        vec![resolve::Warning::CliSecret]
                    } else { Vec::new() },
                },
            }
        }
        ParseOutcome::Help(text) => CollectedOutcome::Help(text),
        ParseOutcome::Version(text) => CollectedOutcome::Version(text),
        ParseOutcome::Invalid => CollectedOutcome::Invalid,
    }
}

///
/// CLI、環境、TOML、既定値を独立した入力として集める。
///
/// # 引数
/// * `cli` - 解析済みの未検証CLI入力
///
/// # 戻り値
/// 全候補と出所を保持する未検証設定
///
fn collect(cli: CliInput) -> Result<UnresolvedRunConfig, CollectionError> {
    /*
     * 候補値を選択せず、それぞれの取得元ごとに保持する
     */
    let environment = EnvironmentInput::collect()
        .map_err(CollectionError::Environment)?;
    let config = config::load(cli.config_path.as_deref())
        .map_err(CollectionError::Config)?;

    Ok(UnresolvedRunConfig::new(cli, environment, config))
}

///
/// テスト用の入力源から候補値を収集する。
///
/// # 概要
/// 擬似環境と指定パスから候補を収集し、入力元の分離と既定値を未検証設定へ保持する。
///
/// # 引数
/// * `cli` - CLI引数
/// * `get` - 環境変数の取得関数
/// * `default_path` - 既定の設定ファイルパス
///
/// # 戻り値
/// 入力元ごとの候補値と既定値を保持する設定
///
#[cfg(test)]
fn collect_with<F>(
    cli: CliInput,
    get: F,
    default_path: &std::path::Path,
) -> Result<UnresolvedRunConfig, CollectionError>
where
    F: FnMut(&'static str) -> Result<Option<String>, EnvironmentError>,
{
    /*
     * 実プロセスの環境を変更せず、各経路の読込を検証する
     */
    let environment = EnvironmentInput::collect_with(get)
        .map_err(CollectionError::Environment)?;
    let config = config::load_with_default_path(
        cli.config_path.as_deref(),
        default_path,
    )
    .map_err(CollectionError::Config)?;

    Ok(UnresolvedRunConfig::new(cli, environment, config))
}

///
/// 指定された引数列を解析する。
///
/// # 概要
/// clap解析の結果を未検証入力・help・version・安全な構文不正の四種類へ写す。
///
/// # 引数
/// * `args` - プログラム名を含む引数列
///
/// # 戻り値
/// 未検証入力、正常な表示要求、または値を含まない構文エラー
///
fn parse_from<I, T>(args: I) -> ParseOutcome
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    /*
     * clapのエラー本文を公開せず、安全な結果へ変換する
     */
    match RawArgs::try_parse_from(args) {
        Ok(raw) => ParseOutcome::Input(raw.into()),
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let help = RawArgs::command().render_help().to_string();
            ParseOutcome::Help(help)
        }
        Err(error) if error.kind() == ErrorKind::DisplayVersion => {
            let version = format!(
                "luwiki-injector {}\n",
                env!("CARGO_PKG_VERSION")
            );
            ParseOutcome::Version(version)
        }
        Err(_) => ParseOutcome::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{collect_with, parse_from, ParseOutcome};

    ///
    /// 位置引数と短縮オプションが仕様どおり収集されることを確認する。
    ///
    /// # 概要
    /// 両位置引数と短縮オプションをまとめて解析し、意味検証前の指定値を照合する。
    ///
    /// # 注記
    /// 両位置引数と短縮オプションを入力し、指定値を照合する。
    ///
    /// # 戻り値
    /// なし。位置引数や短縮オプションの収集値が異なる場合はassertで失敗する。
    ///
    #[test]
    fn collects_positionals_and_short_options() {
        /*
         * 位置引数と短縮オプションを同時に指定する
         */
        let args = [
            "luwiki-injector", "-c", "a.toml", "-p", "/docs",
            "-m", "update", "-s", "git", "-t", "token-value",
            "-u", "user-value", "source", "https://localhost:8080",
        ];
        let ParseOutcome::Input(input) = parse_from(args) else {
            panic!("CLI入力が収集されなかった");
        };

        /*
         * 収集された未検証値を照合する
         */
        assert_eq!(input.page_source, "source");
        assert_eq!(
            input.target_server.as_deref(),
            Some("https://localhost:8080")
        );
        assert_eq!(
            input.config_path.as_deref(),
            Some(std::path::Path::new("a.toml"))
        );
        assert_eq!(input.prefix.as_deref(), Some("/docs"));
        assert_eq!(input.mode.as_deref(), Some("update"));
        assert_eq!(input.source_type.as_deref(), Some("git"));
        assert_eq!(
            input.token.as_ref().map(|v| v.expose()),
            Some("token-value")
        );
        assert_eq!(input.user.as_ref().map(|v| v.expose()), Some("user-value"));
    }

    ///
    /// 省略と明示auto、およびref指定を区別できることを確認する。
    ///
    /// # 概要
    /// 入力種別の省略と明示autoを比較し、解決前のGit参照・worktree指定の保持を確認する。
    ///
    /// # 注記
    /// 省略時と全長オプション指定時を別々に解析する。
    ///
    /// # 戻り値
    /// なし。省略と明示指定の区別や指定値の保持が異なる場合はassertで失敗する。
    ///
    #[test]
    fn preserves_option_presence() {
        /*
         * 省略と明示指定を別々に解析する
         */
        let ParseOutcome::Input(omitted) =
            parse_from(["luwiki-injector", "source"])
        else {
            panic!("省略時のCLI入力が収集されなかった");
        };
        let ParseOutcome::Input(explicit) = parse_from([
            "luwiki-injector", "--source-type", "auto", "--branch",
            "main", "--worktree", "source",
        ]) else {
            panic!("明示指定時のCLI入力が収集されなかった");
        };

        /*
         * 既定値への解決前に指定有無が残ることを確認する
         */
        assert!(omitted.source_type.is_none());
        assert_eq!(explicit.source_type.as_deref(), Some("auto"));
        assert_eq!(explicit.branch.as_deref(), Some("main"));
        assert!(explicit.worktree);
    }

    ///
    /// 残りのGit参照とBasicパスワードが収集されることを確認する。
    ///
    /// # 概要
    /// BasicパスワードとGitのtag・commitを全長指定し、未検証の収集値を照合する。
    ///
    /// # 注記
    /// 各オプションを全長指定し、未検証のまま保持される値を照合する。
    ///
    /// # 戻り値
    /// なし。秘密値やGit参照の収集値が異なる場合はassertで失敗する。
    ///
    #[test]
    fn collects_remaining_options() {
        /*
         * 未検証の段階ではref指定の組合せを保持する
         */
        let ParseOutcome::Input(input) = parse_from([
            "luwiki-injector", "--password", "password-value",
            "--tag", "v1", "--commit", "abc123", "source",
        ]) else {
            panic!("CLI入力が収集されなかった");
        };

        /*
         * 秘密値とGit参照の指定値を照合する
         */
        assert_eq!(
            input.password.as_ref().map(|v| v.expose()),
            Some("password-value")
        );
        assert_eq!(input.tag.as_deref(), Some("v1"));
        assert_eq!(input.commit.as_deref(), Some("abc123"));
    }

    ///
    /// helpとversionが正常な表示要求として扱われることを確認する。
    ///
    /// # 概要
    /// 必須位置引数なしでhelp・versionを要求し、表示種別と引数名・版を照合する。
    ///
    /// # 注記
    /// 必須位置引数を与えず、両標準オプションを解析する。
    ///
    /// # 戻り値
    /// なし。正常表示要求の分類や表示内容が異なる場合はassertで失敗する。
    ///
    #[test]
    fn returns_help_and_version() {
        /*
         * 必須位置引数なしで正常表示を要求する
         */
        let ParseOutcome::Help(help) =
            parse_from(["luwiki-injector", "--help"])
        else {
            panic!("helpが返されなかった");
        };
        let ParseOutcome::Version(version) =
            parse_from(["luwiki-injector", "-v"])
        else {
            panic!("versionが返されなかった");
        };

        /*
         * 表示に必要な引数名と版を確認する
         */
        assert!(help.contains("PAGE-SOURCE"));
        assert!(help.contains("TARGET-SERVER"));
        assert!(help.contains("--password"));
        assert!(!help.contains("--target "));
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
    }

    ///
    /// 不正な引数と秘密値が表示経路へ漏れないことを確認する。
    ///
    /// # 概要
    /// 構文不正を値のない結果にし、正常収集された秘密値もDebugへ出さないことを確認する。
    ///
    /// # 注記
    /// 必須値不足と未知オプションを試し、Debug表示を照合する。
    ///
    /// # 戻り値
    /// なし。構文不正の分類や秘密非露出の契約が異なる場合はassertで失敗する。
    ///
    #[test]
    fn rejects_invalid_arguments_without_exposing_values() {
        /*
         * 不正な構文は値を含まない結果になる
         */
        assert!(matches!(
            parse_from(["luwiki-injector"]),
            ParseOutcome::Invalid
        ));
        assert!(matches!(
            parse_from(["luwiki-injector", "--target", "server", "source"]),
            ParseOutcome::Invalid
        ));
        assert!(matches!(
            parse_from(["luwiki-injector", "--token"]),
            ParseOutcome::Invalid
        ));

        /*
         * 収集済み入力のDebugから秘密値を排除する
         */
        let ParseOutcome::Input(input) = parse_from([
            "luwiki-injector", "--token", "unique-secret-value",
            "--password", "unique-password-value", "source",
        ]) else {
            panic!("秘密値を含むCLI入力が収集されなかった");
        };
        let debug = format!("{input:?}");
        assert!(!debug.contains("unique-secret-value"));
        assert!(!debug.contains("unique-password-value"));
    }

    ///
    /// 四つの入力経路が上書きされず保持されることを確認する。
    ///
    /// # 概要
    /// CLI・環境・TOML・既定値を独立した候補として保持し、統合結果の秘密非露出を確認する。
    ///
    /// # 注記
    /// CLI、環境、TOMLに別々のURLと認証値を与える。
    ///
    /// # 戻り値
    /// なし。入力経路の保持や秘密非露出の契約が異なる場合はassertで失敗する。
    ///
    #[test]
    fn combines_sources_without_resolving_priority() {
        /*
         * TOMLとCLI引数を別々に用意する
         */
        let temporary = tempdir().expect("一時ディレクトリ");
        let path = temporary.path().join("settings.toml");
        fs::write(
            &path,
            "[server]\nurl='https://config.example'\ntoken='config-secret'\n",
        )
        .expect("設定ファイル作成");
        let args = vec![
            "luwiki-injector".to_owned(),
            "--config-path".to_owned(),
            path.to_string_lossy().into_owned(),
            "--token".to_owned(),
            "cli-secret".to_owned(),
            "source".to_owned(),
            "https://cli.example".to_owned(),
        ];
        let ParseOutcome::Input(cli) = parse_from(args) else {
            panic!("CLI入力が収集されなかった");
        };

        /*
         * 環境候補も与えて未検証設定へ統合する
         */
        let input = collect_with(
            cli,
            |name| match name {
                "LUWIKI_URL" => Ok(Some("https://env.example".to_owned())),
                "LUWIKI_TOKEN" => Ok(Some("env-secret".to_owned())),
                _ => Ok(None),
            },
            &temporary.path().join("unused.toml"),
        )
        .expect("全経路の収集");
        let server = input.config.values.server.as_ref().expect("server設定");

        assert_eq!(
            input.cli.target_server.as_deref(),
            Some("https://cli.example")
        );
        assert_eq!(
            input.environment.url.as_deref(),
            Some("https://env.example")
        );
        assert_eq!(server.url.as_deref(), Some("https://config.example"));
        assert_eq!(input.defaults.server_url, "https://localhost:8080");
        assert!(input.config.explicit);
        assert_eq!(input.config.path, path);
        let debug = format!("{input:?}");
        for value in ["cli-secret", "env-secret", "config-secret"] {
            assert!(!debug.contains(value));
        }
    }

    ///
    /// 収集失敗時のエラーに未検証値を含めないことを確認する。
    ///
    /// # 概要
    /// 秘密値を含む不正TOMLを収集し、エラーのDisplayとDebugが内容を含まないことを確認する。
    ///
    /// # 注記
    /// 秘密値を含む不正TOMLから安全な分類だけが返ることを調べる。
    ///
    /// # 戻り値
    /// なし。収集エラーの表示が秘密値を含む場合はassertで失敗する。
    ///
    #[test]
    fn hides_values_in_collection_errors() {
        /*
         * 不正なTOMLを明示設定パスから読み取る
         */
        let temporary = tempdir().expect("一時ディレクトリ");
        let path = temporary.path().join("broken.toml");
        fs::write(&path, "[server]\ntoken=[private-value\n")
            .expect("不正TOML作成");
        let args = vec![
            "luwiki-injector".to_owned(),
            "--config-path".to_owned(),
            path.to_string_lossy().into_owned(),
            "source".to_owned(),
        ];
        let ParseOutcome::Input(cli) = parse_from(args) else {
            panic!("CLI入力が収集されなかった");
        };
        let error = collect_with(cli, |_| Ok(None), &path)
            .err()
            .expect("設定エラー");

        /*
         * 表示とDebugの両方で秘密値がないことを確認する
         */
        assert!(!error.to_string().contains("private-value"));
        assert!(!format!("{error:?}").contains("private-value"));
    }
}
