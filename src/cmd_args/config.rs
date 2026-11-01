/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! TOML設定ファイルを未検証入力として収集するモジュール
//!

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use serde::Deserialize;

use crate::secret::Secret;

///
/// 設定ファイルの収集エラーを安全な分類で表す。
///
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    DefaultPathUnavailable,
    Read,
    Syntax,
    UnknownKey,
    TypeMismatch,
}

impl fmt::Display for ConfigError {
    ///
    /// 設定本文やファイルパスを含まずにエラーを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::DefaultPathUnavailable => "既定の設定パスを特定できません",
            Self::Read => "設定ファイルを読み取れません",
            Self::Syntax => "設定ファイルのTOML構文が不正です",
            Self::UnknownKey => "設定ファイルに未知のキーがあります",
            Self::TypeMismatch => "設定ファイルの値の型が不正です",
        };
        formatter.write_str(message)
    }
}

///
/// 設定ファイルの所在と内容を保持する。
///
#[allow(dead_code)]
pub(super) struct LoadedConfig {
    pub(super) path: PathBuf,
    pub(super) explicit: bool,
    pub(super) present: bool,
    pub(super) permissions: Option<fs::Permissions>,
    pub(super) values: ConfigInput,
}

impl fmt::Debug for LoadedConfig {
    ///
    /// パスと設定値を伏せ、収集状態だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedConfig")
            .field("explicit", &self.explicit)
            .field("present", &self.present)
            .field("values", &self.values)
            .finish()
    }
}

///
/// TOML内の各テーブルを未検証値として保持する。
///
#[derive(Default)]
#[allow(dead_code)]
pub(super) struct ConfigInput {
    pub(super) server: Option<ServerInput>,
    pub(super) page: Option<PageInput>,
    pub(super) limits: Option<LimitsInput>,
}

impl fmt::Debug for ConfigInput {
    ///
    /// 各テーブルの指定有無だけを表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 表示結果
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigInput")
            .field("server", &self.server.as_ref().map(|v| v.presence()))
            .field("page", &self.page.as_ref().map(|v| v.presence()))
            .field("limits", &self.limits.as_ref().map(|v| v.presence()))
            .finish()
    }
}

///
/// サーバー設定の未検証値。
///
#[allow(dead_code)]
pub(super) struct ServerInput {
    pub(super) url: Option<String>,
    pub(super) token: Option<Secret>,
    pub(super) user: Option<Secret>,
    pub(super) password: Option<Secret>,
}

impl ServerInput {
    ///
    /// 各キーの指定有無を返す。
    ///
    /// # 戻り値
    /// URL、token、user、passwordの指定状態
    ///
    fn presence(&self) -> [bool; 4] {
        [
            self.url.is_some(),
            self.token.is_some(),
            self.user.is_some(),
            self.password.is_some(),
        ]
    }
}

///
/// ページ設定の未検証値。
///
#[allow(dead_code)]
pub(super) struct PageInput {
    pub(super) prefix: Option<String>,
    pub(super) mode: Option<String>,
}

impl PageInput {
    ///
    /// 各キーの指定有無を返す。
    ///
    /// # 戻り値
    /// prefixとmodeの指定状態
    ///
    fn presence(&self) -> [bool; 2] {
        [self.prefix.is_some(), self.mode.is_some()]
    }
}

///
/// 八つの入力上限の未検証値。
///
#[allow(dead_code)]
pub(super) struct LimitsInput {
    pub(super) max_pages: Option<i64>,
    pub(super) max_page_bytes: Option<i64>,
    pub(super) max_zip_entries: Option<i64>,
    pub(super) max_zip_compressed_bytes: Option<i64>,
    pub(super) max_zip_uncompressed_bytes: Option<i64>,
    pub(super) max_git_bytes: Option<i64>,
    pub(super) max_processing_seconds: Option<i64>,
    pub(super) max_http_concurrency: Option<i64>,
}

impl LimitsInput {
    ///
    /// 各キーの指定有無を返す。
    ///
    /// # 戻り値
    /// 仕様表の順に並ぶ八つの指定状態
    ///
    fn presence(&self) -> [bool; 8] {
        [
            self.max_pages.is_some(),
            self.max_page_bytes.is_some(),
            self.max_zip_entries.is_some(),
            self.max_zip_compressed_bytes.is_some(),
            self.max_zip_uncompressed_bytes.is_some(),
            self.max_git_bytes.is_some(),
            self.max_processing_seconds.is_some(),
            self.max_http_concurrency.is_some(),
        ]
    }
}

///
/// TOMLからデシリアライズする一時的な構造。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: Option<RawServer>,
    page: Option<RawPage>,
    limits: Option<RawLimits>,
}

///
/// サーバー設定の一時的な構造。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    url: Option<String>,
    token: Option<String>,
    user: Option<String>,
    password: Option<String>,
}

///
/// ページ設定の一時的な構造。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPage {
    prefix: Option<String>,
    mode: Option<String>,
}

///
/// 入力上限の一時的な構造。
///
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_pages: Option<i64>,
    max_page_bytes: Option<i64>,
    max_zip_entries: Option<i64>,
    max_zip_compressed_bytes: Option<i64>,
    max_zip_uncompressed_bytes: Option<i64>,
    max_git_bytes: Option<i64>,
    max_processing_seconds: Option<i64>,
    max_http_concurrency: Option<i64>,
}

impl From<RawConfig> for ConfigInput {
    ///
    /// 認証値を秘匿型へ移し、キーの指定有無を保存する。
    ///
    /// # 概要
    /// server・page・limitsの指定有無を保ち、認証値だけを秘匿型へ移す。
    ///
    /// # 引数
    /// * `raw` - 型変換済みのTOML設定
    ///
    /// # 戻り値
    /// 未検証の設定値
    ///
    fn from(raw: RawConfig) -> Self {
        /*
         * 各テーブルを独立した候補値として移す
         */
        Self {
            server: raw.server.map(|v| ServerInput {
                url: v.url,
                token: v.token.map(Secret::new),
                user: v.user.map(Secret::new),
                password: v.password.map(Secret::new),
            }),
            page: raw.page.map(|v| PageInput {
                prefix: v.prefix,
                mode: v.mode,
            }),
            limits: raw.limits.map(|v| LimitsInput {
                max_pages: v.max_pages,
                max_page_bytes: v.max_page_bytes,
                max_zip_entries: v.max_zip_entries,
                max_zip_compressed_bytes: v.max_zip_compressed_bytes,
                max_zip_uncompressed_bytes: v.max_zip_uncompressed_bytes,
                max_git_bytes: v.max_git_bytes,
                max_processing_seconds: v.max_processing_seconds,
                max_http_concurrency: v.max_http_concurrency,
            }),
        }
    }
}

///
/// CLI指定またはXDG既定位置の設定ファイルを読み取る。
///
/// # 引数
/// * `explicit_path` - CLIで指定された設定ファイルのパス
///
/// # 戻り値
/// ファイルの所在と未検証値。既定ファイル不在時は空の値を返す。
///
pub(super) fn load(explicit_path: Option<&Path>)
    -> Result<LoadedConfig, ConfigError>
{
    /*
     * 明示指定がない場合だけXDG既定パスを調べる
     */
    let default_path;
    let path = if let Some(path) = explicit_path {
        path
    } else {
        let base = BaseDirs::new()
            .ok_or(ConfigError::DefaultPathUnavailable)?;
        default_path = default_path_from(base.config_dir());
        &default_path
    };

    load_from_path(path, explicit_path.is_some())
}

///
/// XDG設定ディレクトリから既定ファイル位置を構築する。
///
/// # 引数
/// * `config_dir` - XDGの設定ディレクトリ
///
/// # 戻り値
/// LuWiki設定ファイルの既定パス
///
fn default_path_from(config_dir: &Path) -> PathBuf {
    config_dir.join("luwiki/config.toml")
}

///
/// テスト等で指定された既定パスから設定ファイルを読む。
///
/// # 引数
/// * `explicit_path` - CLIで明示されたパス
/// * `default_path` - 既定の探索先
///
/// # 戻り値
/// ファイルの所在と未検証値
///
#[cfg(test)]
pub(super) fn load_with_default_path(
    explicit_path: Option<&Path>,
    default_path: &Path,
) -> Result<LoadedConfig, ConfigError> {
    let path = explicit_path.unwrap_or(default_path);
    load_from_path(path, explicit_path.is_some())
}

///
/// 指定パスの設定ファイルを読み取る。
///
/// # 概要
/// 同じファイルの内容と権限を読み、既定ファイルの不在だけを許す。
///
/// # 引数
/// * `path` - 読込先のパス
/// * `explicit` - CLIによる明示指定の有無
///
/// # 戻り値
/// 読込結果。既定ファイルだけは不在を許容する。
///
fn load_from_path(path: &Path, explicit: bool)
    -> Result<LoadedConfig, ConfigError>
{
    /*
     * 既定ファイルの不在だけを許容する
     */
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error)
            if !explicit && error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(LoadedConfig {
                path: path.to_path_buf(),
                explicit,
                present: false,
                permissions: None,
                values: ConfigInput::default(),
            });
        }
        Err(_) => return Err(ConfigError::Read),
    };

    /*
     * 内容と同じファイルから権限を取得する
     */
    let permissions = file.metadata().map_err(|_| ConfigError::Read)?
        .permissions();
    let mut source = String::new();
    file.read_to_string(&mut source).map_err(|_| ConfigError::Read)?;

    /*
     * 読み取った値を型付きの未検証設定へ変換する
     */
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        explicit,
        present: true,
        permissions: Some(permissions),
        values: parse_document(&source)?,
    })
}

///
/// TOMLの構文、キー集合、型を順に確認する。
///
/// # 引数
/// * `source` - TOMLの文字列
///
/// # 戻り値
/// 未検証の設定値、または安全なエラー分類
///
fn parse_document(source: &str) -> Result<ConfigInput, ConfigError> {
    /*
     * 構文とキー集合を別々に検査する
     */
    let table: toml::Table =
        toml::from_str(source).map_err(|_| ConfigError::Syntax)?;
    check_keys(&table)?;

    /*
     * 型だけを確定し、値域の検証はMS2.2へ渡す
     */
    let raw: RawConfig = toml::Value::Table(table)
        .try_into()
        .map_err(|_| ConfigError::TypeMismatch)?;
    Ok(raw.into())
}

///
/// 許可されたテーブルとキーだけがあることを確認する。
///
/// # 概要
/// トップレベルの許容テーブル名と、子テーブルの許容キーを順に照合する。
///
/// # 引数
/// * `table` - 構文解析済みのTOMLテーブル
///
/// # 戻り値
/// 未知キーがない場合は成功を返す。
///
fn check_keys(table: &toml::Table) -> Result<(), ConfigError> {
    /*
     * トップレベルのテーブル名を照合する
     */
    for (name, value) in table {
        let allowed = match name.as_str() {
            "server" => &["url", "token", "user", "password"][..],
            "page" => &["prefix", "mode"][..],
            "limits" => &[
                "max_pages", "max_page_bytes", "max_zip_entries",
                "max_zip_compressed_bytes", "max_zip_uncompressed_bytes",
                "max_git_bytes", "max_processing_seconds",
                "max_http_concurrency",
            ][..],
            _ => return Err(ConfigError::UnknownKey),
        };

        /*
         * テーブル型なら内部キーも照合する
         */
        if let Some(nested) = value.as_table() {
            if nested.keys().any(|key| !allowed.contains(&key.as_str())) {
                return Err(ConfigError::UnknownKey);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        default_path_from, load_from_path, parse_document, ConfigError,
    };

    ///
    /// XDG設定ディレクトリ配下の既定パスを確認する。
    ///
    /// # 注記
    /// 入力ディレクトリと仕様上の相対パスを結合する。
    ///
    /// # 戻り値
    /// なし。既定パスが仕様と異なる場合はassertで失敗する。
    ///
    #[test]
    fn uses_xdg_config_location() {
        let path = default_path_from(Path::new("/tmp/xdg-config"));
        assert_eq!(
            path,
            Path::new("/tmp/xdg-config/luwiki/config.toml")
        );
    }

    ///
    /// TOMLの指定有無と八つの上限値を保持できることを確認する。
    ///
    /// # 概要
    /// 三つのテーブルと八つの上限を読み、値の保持と秘密値の表示抑制を確認する。
    ///
    /// # 注記
    /// 三つのテーブルを読み、値域検証前の値を照合する。
    ///
    /// # 戻り値
    /// なし。未検証値の保持や表示境界が異なる場合はassertで失敗する。
    ///
    #[test]
    fn collects_known_tables_and_keys() {
        /*
         * 三つのテーブルを型付きの未検証値として読む
         */
        let input = parse_document(
            "[server]\nurl='https://wiki.example'\ntoken='private'\n\
             [page]\nprefix='/docs'\nmode='update'\n\
             [limits]\nmax_pages=1\nmax_page_bytes=2\n\
             max_zip_entries=3\nmax_zip_compressed_bytes=4\n\
             max_zip_uncompressed_bytes=5\nmax_git_bytes=6\n\
             max_processing_seconds=7\nmax_http_concurrency=8\n",
        )
        .expect("TOMLの読込");
        let server = input.server.as_ref().expect("serverテーブル");
        let limits = input.limits.as_ref().expect("limitsテーブル");

        /*
         * 上限と秘密値の保持・表示境界を確認する
         */
        assert_eq!(server.url.as_deref(), Some("https://wiki.example"));
        assert_eq!(server.token.as_ref().map(|v| v.expose()), Some("private"));
        assert_eq!(limits.presence(), [true; 8]);
        assert_eq!(limits.max_http_concurrency, Some(8));
        assert!(!format!("{input:?}").contains("private"));
    }

    ///
    /// 構文、未知キー、型不一致を別々に分類する。
    ///
    /// # 概要
    /// 不正な構文・未知キー・型不一致を入力し、それぞれの失敗分類を確認する。
    ///
    /// # 注記
    /// 各不正入力で返る安全な分類を比較する。
    ///
    /// # 戻り値
    /// なし。失敗分類が仕様と異なる場合はassertで失敗する。
    ///
    #[test]
    fn classifies_invalid_documents() {
        /*
         * 構文とキー集合の失敗を区別する
         */
        assert!(matches!(
            parse_document("[server\n"),
            Err(ConfigError::Syntax)
        ));
        assert!(matches!(
            parse_document("[unexpected]\nvalue=1\n"),
            Err(ConfigError::UnknownKey)
        ));
        assert!(matches!(
            parse_document("[server]\nunknown='value'\n"),
            Err(ConfigError::UnknownKey)
        ));
        assert!(matches!(
            parse_document("[server]\ngithub_token='value'\n"),
            Err(ConfigError::UnknownKey)
        ));

        /*
         * 型の誤りは別の分類で返す
         */
        assert!(matches!(
            parse_document("[limits]\nmax_pages='many'\n"),
            Err(ConfigError::TypeMismatch)
        ));
    }

    ///
    /// 明示パスの失敗と既定ファイル不在を区別する。
    ///
    /// # 注記
    /// 同じ存在しないパスを両条件で読み取る。
    ///
    /// # 戻り値
    /// なし。既定ファイル不在と明示読込失敗を区別できなければassertで失敗する。
    ///
    #[test]
    fn distinguishes_missing_default_and_explicit_file() {
        let temporary = tempdir().expect("一時ディレクトリ");
        let path = temporary.path().join("absent.toml");

        let default = load_from_path(&path, false).expect("既定ファイル不在");
        assert!(!default.present);
        assert!(default.values.server.is_none());
        assert!(matches!(
            load_from_path(&path, true),
            Err(ConfigError::Read)
        ));
    }

    ///
    /// 明示ファイルを読み、本文を公開エラーへ出さない。
    ///
    /// # 概要
    /// 明示ファイルの正常読込と構文不正を確認し、Debugとエラーの秘密非露出を検査する。
    ///
    /// # 注記
    /// 一時ファイルを作り、成功と不正TOMLを確認する。
    ///
    /// # 戻り値
    /// なし。読込結果や秘密非露出の契約が異なる場合はassertで失敗する。
    ///
    #[test]
    fn loads_file_without_exposing_secret_on_error() {
        /*
         * 正常な設定から秘密値を保持する
         */
        let temporary = tempdir().expect("一時ディレクトリ");
        let path = temporary.path().join("config.toml");
        fs::write(&path, "[server]\ntoken='hidden-value'\n")
            .expect("設定ファイル作成");
        let loaded = load_from_path(&path, true).expect("設定ファイル読込");
        assert!(loaded.present);
        assert!(loaded.explicit);
        assert!(!format!("{loaded:?}").contains("hidden-value"));

        /*
         * 不正な設定でも本文をエラーに含めない
         */
        fs::write(&path, "[server]\ntoken=[hidden-value\n")
            .expect("不正設定ファイル作成");
        let error = load_from_path(&path, true).err().expect("構文エラー");
        assert_eq!(error, ConfigError::Syntax);
        assert!(!error.to_string().contains("hidden-value"));
    }
}
