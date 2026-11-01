/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 設定解決の優先順位・排他・秘匿を検証するテスト
//!

use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{tempdir, TempDir};

use crate::cmd_args::{collect_with, parse_from, ParseOutcome};
use crate::cmd_args::UnresolvedRunConfig;
use crate::secret::Secret;

use super::auth::PasswordPrompt;
use super::source::{GitSelector, LocalSelector, SourceSpec};
use super::{resolve_with, Mode, ValidationError, Warning, WikiAuth};

///
/// 副作用を数えるテスト用パスワード取得境界。
///
struct Prompt {
    tty: bool,
    value: Option<Result<String, ValidationError>>,
    calls: usize,
}

impl Prompt {
    ///
    /// 非TTYで読込を禁止する境界を作る。
    ///
    /// # 戻り値
    /// 呼出し回数0の非TTY境界
    ///
    fn non_tty() -> Self { Self { tty: false, value: None, calls: 0 } }
}

impl PasswordPrompt for Prompt {
    ///
    /// テストで指定したTTY状態を返す。
    ///
    /// # 戻り値
    /// TTY設定
    ///
    fn is_tty(&self) -> bool { self.tty }

    ///
    /// 呼出し回数を増やし、用意した入力を返す。
    ///
    /// # 戻り値
    /// テスト用秘密値またはエラー
    ///
    fn read(&mut self) -> Result<Secret, ValidationError> {
        self.calls += 1;
        self.value.take().expect("想定外の対話呼出し").map(Secret::new)
    }
}

///
/// 全入力源を実プロセスの環境から隔離して用意する。
///
/// # 概要
/// 一時設定ファイルとCLI解析、注入環境変数を使い収集経路を通す。
///
/// # 引数
/// * `args` - ソースを含むCLI引数
/// * `document` - TOML本文
/// * `vars` - テスト用環境変数
/// * `mode` - Unix読取権限
///
/// # 戻り値
/// 一時領域の所有者と未検証設定
///
fn fixture(
    args: &[&str],
    document: &str,
    vars: &[(&'static str, &str)],
    mode: u32,
) -> (TempDir, UnresolvedRunConfig) {
    /*
     * 実際に読む設定ファイルと権限を用意する
     */
    let temporary = tempdir().expect("一時ディレクトリ");
    let path = temporary.path().join("settings.toml");
    fs::write(&path, document).expect("設定ファイル作成");
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(mode))
        .expect("テスト用権限設定");
    #[cfg(not(unix))]
    let _ = mode;

    /*
     * CLIと環境を実際の収集処理へ渡す
     */
    let mut command = vec![
        OsString::from("luwiki-injector"),
        OsString::from("--config-path"),
        path.clone().into_os_string(),
    ];
    command.extend(args.iter().map(|value| OsString::from(*value)));
    let ParseOutcome::Input(cli) = parse_from(command) else {
        panic!("テスト用CLIの解析失敗");
    };
    let input = collect_with(cli, |name| {
        Ok(vars.iter().find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned()))
    }, &path).expect("入力の収集");
    (temporary, input)
}

///
/// 採用値だけを検証し、不採用の不正値を無視する。
///
/// # 概要
/// CLIの正常値と不採用の不正値を与え、採用値だけの意味検証を確認する。
///
/// # 注記
/// CLI正常値を環境とTOMLの不正値より優先する。
///
/// # 戻り値
/// なし。優先順位や採用値の検証結果が異なる場合はassertで失敗する。
///
#[test]
fn selected_values_override_invalid_lower_candidates() {
    /*
     * 正常CLIと不正な下位候補を同時に用意する
     */
    let (_temporary, input) = fixture(
        &["-t", "cli-value", "-p", "/cli", "-m", "update",
          "owner/repo", "http://localhost:8080"],
        "[server]\nurl='invalid'\ntoken=''\n\
         [page]\nprefix='invalid'\nmode='invalid'\n",
        &[("LUWIKI_URL", "invalid"), ("LUWIKI_TOKEN", "")], 0o600,
    );
    /*
     * 採用済み接続先・ページ設定・認証値を照合する
     */
    let result = resolve_with(input, &mut Prompt::non_tty())
        .expect("優先値の検証");
    assert_eq!(result.settings.target.as_str(), "http://localhost:8080/");
    assert_eq!(result.settings.prefix, "/cli");
    assert_eq!(result.settings.mode, Mode::Update);
    let WikiAuth::Bearer(token) = &result.auth else { panic!("認証方式"); };
    assert_eq!(token.expose(), "cli-value");
}

///
/// 検証済み設定のsource・上限・認証を共通取得とReportへ渡す。
///
/// # 概要
/// 実ZIPを作り、設定上限の拒否と正常取得を同じ設定解決経路で確認する。
///
/// # 注記
/// ダミー資格情報を注入し、入力失敗の終了2と報告の非露出を確認する。
///
/// # 戻り値
/// なし。取得上限・Report・秘密非露出・正常取得の契約が異なればassertで失敗する。
///
#[tokio::test]
async fn resolved_config_connects_source_limits_and_safe_report() {
    use std::io::{Cursor, Write};

    use ::zip::ZipWriter;
    use ::zip::write::SimpleFileOptions;

    /*
     * 元入力と設定・環境をテスト所有の領域へ用意する
     */
    let source = tempdir().expect("source input");
    let path = source.path().join("fixture.zip");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("root/page.md", SimpleFileOptions::default())
        .expect("fixture entry");
    writer.write_all(b"dummy-body").expect("fixture body");
    fs::write(&path, writer.finish().expect("archive").into_inner())
        .expect("fixture ZIP");
    let vars = [("LUWIKI_TOKEN", "dummy-wiki-secret"),
        ("GITHUB_TOKEN", "dummy-github-secret")];
    let args = [path.to_str().expect("fixture path"), "-t",
        "dummy-wiki-secret"];
    let (_config_area, input) = fixture(&args,
        "[limits]\nmax_zip_compressed_bytes=1\n", &vars, 0o600);
    let config = resolve_with(input, &mut Prompt::non_tty())
        .expect("verified config");
    /*
     * 上限を取得境界へ渡し、失敗と警告を既存Reportへ追加する
     */
    let failure = crate::source::acquire_for_report(&config).await
        .expect_err("configured size limit");
    let mut report = crate::report::Report::preflight();
    report.add_warnings(config.warnings());
    report.fail(failure);
    assert!(report.exit_code() == 2);
    assert!(report.summary().warnings == 1);
    let rendered = report.render();
    for value in ["dummy-wiki-secret", "dummy-github-secret", "dummy-body"] {
        assert!(!rendered.results.contains(value));
        assert!(!rendered.diagnostics.contains(value));
    }
    /*
     * 同じsourceを正常上限で取得し、本文の取得元を確認する
     */
    let (_config_area, input) = fixture(&args, "", &vars, 0o600);
    let config = resolve_with(input, &mut Prompt::non_tty())
        .expect("verified success config");
    let snapshot = crate::source::acquire_for_report(&config).await
        .expect("configured acquisition");
    assert!(snapshot.entries().len() == 1);
    assert!(snapshot.entries()[0].byte_len() == b"dummy-body".len() as u64);
    assert!(snapshot.open_body(0).is_ok());
}

///
/// URL・ページ設定と認証が経路ごとにフォールバックする。
///
/// # 概要
/// 環境とTOMLの優先採用を照合し、最後に接続先・ページ設定の既定値を確認する。
///
/// # 注記
/// CLI省略時の環境、TOML、既定値を順に照合する。
///
/// # 戻り値
/// なし。入力経路の優先順位や既定値が異なる場合はassertで失敗する。
///
#[test]
fn falls_back_through_environment_config_and_defaults() {
    /*
     * TOML候補と環境の指定有無ごとの期待値を用意する
     */
    let document = "[server]\nurl='https://config.example'\n\
                    token='config-value'\n[page]\nprefix='/config'\n";
    for (vars, expected_url, expected_token) in [
        (vec![("LUWIKI_URL", "https://env.example"),
              ("LUWIKI_TOKEN", "env-value")],
         "https://env.example/", "env-value"),
        (vec![], "https://config.example/", "config-value"),
    ] {
        /*
         * 各入力経路から設定を解決する
         */
        let (_temporary, input) = fixture(
            &["owner/repo"], document, &vars, 0o600,
        );
        let result = resolve_with(input, &mut Prompt::non_tty())
            .expect("フォールバック");
        /*
         * 採用済みURL・prefix・認証値を期待値と照合する
         */
        assert_eq!(result.settings.target.as_str(), expected_url);
        assert_eq!(result.settings.prefix, "/config");
        let WikiAuth::Bearer(token) = &result.auth else { panic!("方式"); };
        assert_eq!(token.expose(), expected_token);
    }
    /*
     * 対象設定の候補がない場合に既定値を採用する
     */
    let (_temporary, input) = fixture(
        &["-t", "value", "owner/repo"], "", &[], 0o600,
    );
    let result = resolve_with(input, &mut Prompt::non_tty()).expect("既定値");
    assert_eq!(result.settings.target.as_str(), "https://localhost:8080/");
    assert_eq!(result.settings.prefix, "/");
    assert_eq!(result.settings.mode, Mode::Error);
}

///
/// Basic認証を項目ごとに異なる入力元から解決する。
///
/// # 注記
/// CLIユーザー名と環境パスワードを採用し、TTYを呼ばない。
///
/// # 戻り値
/// なし。Basicの項目別採用や非対話の契約が異なる場合はassertで失敗する。
///
#[test]
fn resolves_basic_fields_independently() {
    let (_temporary, input) = fixture(
        &["--user", "cli-user", "owner/repo"],
        "[server]\nuser=''\npassword=''\n",
        &[("LUWIKI_PASSWORD", "env-password")], 0o600,
    );
    let mut prompt = Prompt::non_tty();
    let result = resolve_with(input, &mut prompt).expect("Basic解決");
    let WikiAuth::Basic { user, password } = &result.auth else {
        panic!("Basic認証ではない");
    };
    assert_eq!(user.expose(), "cli-user");
    assert_eq!(password.expose(), "env-password");
    assert_eq!(prompt.calls, 0);
}

///
/// 認証の不足・排他・空値を対話せず拒否する。
///
/// # 概要
/// 認証不足・同時指定・空値・非TTYを個別に与え、失敗分類と対話回数を確認する。
///
/// # 注記
/// 未指定、パスワードだけ、同時指定、空値、非TTYを照合する。
///
/// # 戻り値
/// なし。認証失敗分類や対話非発生の契約が異なる場合はassertで失敗する。
///
#[test]
fn rejects_auth_errors_without_prompting() {
    /*
     * 認証条件ごとの入力と期待する失敗分類を用意する
     */
    for (args, expected) in [
        (vec!["owner/repo"], ValidationError::AuthMissing),
        (vec!["--password", "p", "owner/repo"],
         ValidationError::AuthMissing),
        (vec!["-t", "t", "-u", "u", "owner/repo"],
         ValidationError::AuthConflict),
        (vec!["-t", "", "owner/repo"],
         ValidationError::EmptyCredential("Wiki token")),
        (vec!["-u", "u", "owner/repo"],
         ValidationError::PasswordUnavailable),
    ] {
        /*
         * 各条件を解決して安全な失敗分類と対話ゼロを照合する
         */
        let (_temporary, input) = fixture(&args, "", &[], 0o600);
        let mut prompt = Prompt::non_tty();
        assert_eq!(resolve_with(input, &mut prompt).err(), Some(expected));
        assert_eq!(prompt.calls, 0);
    }
}

///
/// 条件を満たすTTYだけでパスワードを取得する。
///
/// # 概要
/// TTYの不足パスワードを対話で補い、正常値・空値・読込失敗と呼出回数を確認する。
///
/// # 注記
/// 対話成功、空入力、読込失敗を注入して回数と結果を確認する。
///
/// # 戻り値
/// なし。対話結果の分類や呼出回数が異なる場合はassertで失敗する。
///
#[test]
fn prompts_only_for_missing_basic_password_on_tty() {
    for value in [
        Ok("prompt-password".to_owned()), Ok(String::new()),
        Err(ValidationError::PromptFailed),
    ] {
        /*
         * 対話の正常値・空値・読込失敗ごとに期待結果を決める
         */
        let expected = match &value {
            Ok(v) if v.is_empty() =>
                Some(ValidationError::EmptyCredential("Basic password")),
            Err(_) => Some(ValidationError::PromptFailed),
            _ => None,
        };
        /*
         * パスワード不足のTTY入力を解決し結果と呼出一回を照合する
         */
        let (_temporary, input) = fixture(
            &["-u", "user", "owner/repo"], "", &[], 0o600,
        );
        let mut prompt = Prompt { tty: true, value: Some(value), calls: 0 };
        assert_eq!(resolve_with(input, &mut prompt).err(), expected);
        assert_eq!(prompt.calls, 1);
    }
}

///
/// 設定の異常は対話入力より先に拒否される。
///
/// # 注記
/// 不正URLと空GitHubトークンがTTYを呼ばないことを確認する。
///
/// # 戻り値
/// なし。事前検証の拒否や対話非発生の契約が異なる場合はassertで失敗する。
///
#[test]
fn preflight_errors_do_not_trigger_tty() {
    for (target, vars) in [
        ("https://wiki.example/nonroot", vec![]),
        ("https://wiki.example", vec![("GITHUB_TOKEN", "")]),
    ] {
        let (_temporary, input) = fixture(
            &["-u", "user", "owner/repo", target], "", &vars, 0o600,
        );
        let mut prompt = Prompt { tty: true, value: None, calls: 0 };
        assert!(resolve_with(input, &mut prompt).is_err());
        assert_eq!(prompt.calls, 0);
    }
}

///
/// 入力種別省略とautoが同じGitHub指定を返す。
///
/// # 概要
/// 省略・auto・githubの指定結果を照合し、GitHub構文不正とHTTPS条件違反を拒否する。
///
/// # 注記
/// 省略・auto・githubを比較し、構文不正とHTTPS制限も確認する。
///
/// # 戻り値
/// なし。入力種別の解決結果や不正入力の拒否が異なる場合はassertで失敗する。
///
#[test]
fn validates_remote_and_github_source_shapes() {
    /*
     * 入力種別省略・auto・githubが同じGitHub指定になることを照合する
     */
    for extra in [vec![], vec!["-s", "auto"], vec!["-s", "github"]] {
        let mut args = vec!["-t", "value"];
        args.extend(extra);
        args.push("owner/repo");
        let (_temporary, input) = fixture(&args, "", &[], 0o600);
        let result = resolve_with(input, &mut Prompt::non_tty())
            .expect("GitHub指定");
        assert!(matches!(result.source, SourceSpec::GitHub { .. }));
    }
    /*
     * GitHubの構文不正とリモートGitの接続条件違反を拒否する
     */
    for value in [
        "owner/repo/extra", "owner/", "/repo", "owner/re po",
        "ssh://host/repo", "http://host/repo",
        "https://user:secret@host/repo",
    ] {
        let (_temporary, input) = fixture(
            &["-t", "value", value], "", &[], 0o600,
        );
        assert!(resolve_with(input, &mut Prompt::non_tty()).is_err());
    }
}

///
/// ローカルGit、bare、非Gitの判別を確認する。
///
/// # 概要
/// 実体のあるworktreeとbareを用意し、worktree採用とbare・非Gitの拒否を確認する。
///
/// # 注記
/// gixで一時リポジトリを作り、worktreeとbareを比較する。
///
/// # 戻り値
/// なし。ローカルGitの判別結果が仕様と異なる場合はassertで失敗する。
///
#[test]
fn detects_local_worktree_and_rejects_bare() {
    /*
     * worktreeとbareを実体のあるfixtureにする
     */
    let temporary = tempdir().expect("一時領域");
    let worktree = temporary.path().join("worktree");
    let bare = temporary.path().join("bare");
    gix::init(&worktree).expect("worktree初期化");
    gix::init_bare(&bare).expect("bare初期化");
    let value = worktree.to_str().expect("fixtureパス");
    let (_config, input) = fixture(
        &["-t", "value", "--worktree", value], "", &[], 0o600,
    );
    let result = resolve_with(input, &mut Prompt::non_tty())
        .expect("worktree判別");
    assert!(matches!(result.source, SourceSpec::LocalGit {
        selector: LocalSelector::Worktree, ..
    }));

    /*
     * bareとGitでないディレクトリは拒否する
     */
    let non_git = temporary.path().to_path_buf();
    for path in [&bare, &non_git] {
        let (_config, input) = fixture(
            &["-t", "value", path.to_str().expect("fixtureパス")],
            "", &[], 0o600,
        );
        assert!(resolve_with(input, &mut Prompt::non_tty()).is_err());
    }
}

///
/// ZIP自動判別と参照の適合を確認する。
///
/// # 概要
/// 小文字zipの自動判別と参照指定の拒否を照合し、大文字拡張子の入力も拒否する。
///
/// # 注記
/// ファイル名の大小文字と明示zip、ZIPへのGit ref指定を比較する。
///
/// # 戻り値
/// なし。ZIPの判別や参照指定の許容・拒否が異なる場合はassertで失敗する。
///
#[test]
fn validates_local_zip_and_selector_combinations() {
    /*
     * 小文字zipの実在ファイルと入力指定を準備する
     */
    let temporary = tempdir().expect("一時領域");
    let path = temporary.path().join("input.zip");
    fs::write(&path, "fixture").expect("入力ファイル");
    let value = path.to_str().expect("fixtureパス");
    /*
     * ZIPへの参照指定・worktree・入力種別の組合せを照合する
     */
    for (extra, valid) in [
        (vec![], true), (vec!["--branch", "main"], false),
        (vec!["--worktree"], false), (vec!["-s", "github"], false),
    ] {
        let mut args = vec!["-t", "value"];
        args.extend(extra);
        args.push(value);
        let (_config, input) = fixture(&args, "", &[], 0o600);
        assert_eq!(resolve_with(input, &mut Prompt::non_tty()).is_ok(), valid);
    }
    /*
     * 大文字拡張子は自動判別と明示zipの両方で拒否する
     */
    let upper = temporary.path().join("input.ZIP");
    fs::write(&upper, "fixture").expect("大文字拡張子");
    for (extra, valid) in [(vec![], false), (vec!["-s", "zip"], false)] {
        let mut args = vec!["-t", "value"];
        args.extend(extra);
        args.push(upper.to_str().expect("fixtureパス"));
        let (_config, input) = fixture(&args, "", &[], 0o600);
        assert_eq!(resolve_with(input, &mut Prompt::non_tty()).is_ok(), valid);
    }
}

///
/// ref排他とremoteのworktree拒否を確認する。
///
/// # 概要
/// branch・tag・commitの単独指定を保持し、参照混在・空値・remote worktreeを拒否する。
///
/// # 注記
/// branch/tag/commitを個別に保持し、混在を拒否する。
///
/// # 戻り値
/// なし。Git参照指定の保持や拒否が仕様と異なる場合はassertで失敗する。
///
#[test]
fn validates_git_selectors() {
    /*
     * branch・tag・commitの単独指定を解決して保持を照合する
     */
    for option in ["--branch", "--tag", "--commit"] {
        let (_config, input) = fixture(
            &["-t", "value", option, "ref-value", "https://host/repo"],
            "", &[], 0o600,
        );
        let result = resolve_with(input, &mut Prompt::non_tty())
            .expect("参照指定");
        assert!(matches!(result.source, SourceSpec::HttpsGit {
            selector: GitSelector::Branch(_)
                | GitSelector::Tag(_) | GitSelector::Commit(_), ..
        }));
    }
    /*
     * 参照混在・空値とリモートGitへのworktree指定を拒否する
     */
    for extra in [
        vec!["--branch", "a", "--tag", "b"],
        vec!["--commit", "a", "--worktree"],
        vec!["--worktree"], vec!["--branch", ""],
    ] {
        let mut args = vec!["-t", "value"];
        args.extend(extra);
        args.push("https://host/repo");
        let (_config, input) = fixture(&args, "", &[], 0o600);
        assert!(resolve_with(input, &mut Prompt::non_tty()).is_err());
    }
}

///
/// 権限警告は認証キーの存在と読取権限の両方を必要とする。
///
/// # 概要
/// 認証キーの有無と読取権限を組み合わせ、設定読込を拒否せず権限警告を判定する。
///
/// # 注記
/// 0600/0640/0604と認証キーの有無を組み合わせる。
///
/// # 戻り値
/// なし。認証キーと権限に基づく警告条件が異なる場合はassertで失敗する。
///
#[cfg(unix)]
#[test]
fn warns_only_when_auth_file_is_readable_by_others() {
    for mode in [0o600, 0o640, 0o604] {
        for has_auth in [false, true] {
            /*
             * 権限ごとに認証キーの有無を変えて設定を解決する
             */
            let document = if has_auth {
                "[server]\ntoken='config-value'\n"
            } else { "" };
            let (_temporary, input) = fixture(
                &["-t", "cli-value", "owner/repo"], document, &[], mode,
            );
            let result = resolve_with(input, &mut Prompt::non_tty())
                .expect("警告のみで実行を拒否しない");
            /*
             * CLI秘密指定警告と設定権限警告の成立条件を照合する
             */
            assert!(result.warnings.contains(&Warning::CliSecret));
            assert_eq!(
                result.warnings.contains(&Warning::ConfigPermissions),
                has_auth && mode != 0o600,
            );
        }
    }
}

///
/// 解決結果と警告は秘密値を表示しない。
///
/// # 概要
/// CLI・TOML・GitHubの固有ダミー値を与え、解決結果と警告の表示で非露出を確認する。
///
/// # 注記
/// 各資格情報に固有値を与え、Debug・警告から非露出を確認する。
///
/// # 戻り値
/// なし。解決結果や警告が秘密値を含む場合はassertで失敗する。
///
#[test]
fn keeps_resolved_debug_and_warnings_free_of_secret_values() {
    /*
     * 各入力経路に固有ダミー資格情報を用意して解決する
     */
    let (_temporary, input) = fixture(
        &["-t", "unique-cli-secret", "owner/repo"],
        "[server]\ntoken='unique-config-secret'\n",
        &[("GITHUB_TOKEN", "unique-github-secret")], 0o644,
    );
    let result = resolve_with(input, &mut Prompt::non_tty())
        .expect("秘匿設定");
    /*
     * Debugと各警告の表示からダミー秘密値が除かれることを照合する
     */
    let output = format!("{result:?} {:?}", result.warnings);
    for value in [
        "unique-cli-secret", "unique-config-secret", "unique-github-secret",
    ] {
        assert!(!output.contains(value));
        for warning in result.warnings() {
            assert!(!warning.to_string().contains(value));
        }
    }
}

///
/// 八つのTOML上限を実際の解決経路で検証する。
///
/// # 概要
/// 八つのTOML上限を設定解決へ渡し、許容端点の採用と範囲外の安全な分類を確認する。
///
/// # 注記
/// 各キーの最大値は採用し、0と最大超過を起動エラーにする。
///
/// # 戻り値
/// なし。設定上限の採用や失敗分類が異なる場合はassertで失敗する。
///
#[test]
fn validates_each_config_limit_at_the_resolution_boundary() {
    /*
     * 上限キーごとの許容最大値を準備する
     */
    let limits: [(&str, i64); 8] = [
        ("max_pages", 4_000), ("max_page_bytes", 4_194_304),
        ("max_zip_entries", 20_000),
        ("max_zip_compressed_bytes", 536_870_912),
        ("max_zip_uncompressed_bytes", 1_073_741_824),
        ("max_git_bytes", 2_147_483_648),
        ("max_processing_seconds", 1_200), ("max_http_concurrency", 4),
    ];
    for (name, maximum) in limits {
        for (value, valid) in [(1, true), (maximum, true),
                               (0, false), (maximum + 1, false)] {
            /*
             * 許容端点と範囲外を実際の設定解決へ渡して結果を照合する
             */
            let document = format!("[limits]\n{name}={value}\n");
            let (_temporary, input) = fixture(
                &["-t", "value", "owner/repo"], &document, &[], 0o600,
            );
            let result = resolve_with(input, &mut Prompt::non_tty());
            assert_eq!(result.is_ok(), valid);
            if !valid {
                assert_eq!(result.err(), Some(ValidationError::Limit(name)));
            }
        }
    }
}
