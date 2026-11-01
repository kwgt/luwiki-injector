/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 製品用入力取得境界のfixture試験
//!

mod git;
mod github;
pub(super) mod integration;
mod remote;
mod runtime;
mod zip;
// probeの生成・変造ヘルパーだけを使用し、inspect_zipは製品試験に使わない。
#[allow(dead_code)]
#[path = "../../../tests/ms1_2/zip_support.rs"]
mod zip_fixture;

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::{TempDir, tempdir};

use super::{AcquisitionLimits, SourceSnapshot, StopContext};

///
/// 小規模fixture用の独立した上限を用意する。
///
/// # 戻り値
/// 正常fixtureを含み、超過ケースで変更可能な上限
///
fn limits() -> AcquisitionLimits {
    AcquisitionLimits {
        zip_bytes: 1_000_000, zip_entries: 100, zip_expanded: 2_000_000,
        git_bytes: 3_000_000, page_bytes: 1_000_000,
    }
}

///
/// テスト用の停止境界を用意する。
///
/// # 戻り値
/// 五秒後に期限を迎える境界
///
fn stop() -> StopContext {
    StopContext::new(Instant::now() + Duration::from_secs(5))
}

///
/// 相対名の本文をsnapshotだけから読む。
///
/// # 引数
/// * `snapshot` - 所有された取得結果
/// * `name` - 試験対象のentry名
///
/// # 戻り値
/// 取得時点の生本文
///
fn body(snapshot: &SourceSnapshot, name: &str) -> Vec<u8> {
    let index = snapshot.entries().iter()
        .position(|v| v.name_raw() == name.as_bytes()).expect("対象entry");
    let mut bytes = Vec::new();
    snapshot.open_body(index).expect("本文取得元")
        .read_to_end(&mut bytes).expect("本文読込");
    bytes
}

///
/// 一時repo内だけでGit CLIを使いfixtureを作る。
///
/// # 概要
/// テストrepoで設定・環境の干渉を避けてGitを実行し、成功したstdoutをUTF-8で返す。
///
/// # 引数
/// * `path` - テスト専用リポジトリ
/// * `args` - Git引数
///
/// # 戻り値
/// 標準出力。外部リポジトリへ接続しない。
///
fn git_command(path: &Path, args: &[&str]) -> String {
    /*
     * fixture用Gitの設定・環境を限定してコマンドを実行する
     */
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"]).args(args).current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR").env_remove("GIT_INDEX_FILE")
        .env_remove("GITHUB_TOKEN")
        .output().expect("fixture用Git CLI");
    /*
     * 終了成功を確認してUTF-8のstdoutを取得する
     */
    assert!(output.status.success(), "fixture用Git CLI失敗");
    String::from_utf8(output.stdout).expect("Git stdout").trim().to_owned()
}

///
/// mainとその他の参照で内容が異なる小規模repo。
///
struct GitFixture {
    directory: TempDir,
    initial: String,
}

impl GitFixture {
    ///
    /// root内の本文と外側metadataを持つ独立したcommitを用意する。
    ///
    /// # 概要
    /// 低層の既存ref試験とは別に、共通投影へ渡す有効入力を生成する。
    ///
    /// # 戻り値
    /// 最上位rootを持つ一時repoと完全commit。
    ///
    fn rooted() -> Self {
        let directory = tempdir().expect("rooted Git area");
        let path = directory.path();
        /*
         * 本文はroot内へ、プロジェクトmetadataはroot外へ置く
         */
        git_command(path, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(path.join("root/pages")).unwrap();
        fs::write(path.join("root/pages/page.md"), b"old\r\n").unwrap();
        fs::write(path.join("root/pages/deleted.md"), b"delete\n").unwrap();
        fs::write(path.join("plain.txt"), b"ordinary").unwrap();
        fs::write(path.join(".gitignore"), b"*.skip.md\n").unwrap();
        /*
         * 元fixtureの履歴を変えず、この入力だけのcommitを確定する
         */
        git_command(path, &["add", "."]);
        git_command(path, &["commit", "-qm", "rooted fixture"]);
        let initial = git_command(path, &["rev-parse", "HEAD"]);
        Self { directory, initial }
    }

    ///
    /// 初期ref群と後続main commitを用意する。
    ///
    /// # 概要
    /// master/fixture/tagは旧内容、mainは新内容を指す。
    ///
    /// # 戻り値
    /// 独立した履歴を持つ一時repo
    ///
    fn new() -> Self {
        let directory = tempdir().expect("Git一時領域");
        let path = directory.path();
        /*
         * 元commitと複数refを作る
         */
        git_command(path, &["init", "-q", "-b", "main"]);
        fs::create_dir(path.join("pages")).expect("fixture directory");
        fs::write(path.join("pages/page.md"), b"old\r\n").expect("元本文");
        fs::write(path.join("pages/deleted.md"), b"delete\n").expect("削除対象");
        fs::write(path.join("plain.txt"), b"ordinary").expect("非Markdown");
        fs::write(path.join(".gitignore"), b"*.skip.md\n").expect("ignore");
        git_command(path, &["add", "."]);
        git_command(path, &["commit", "-qm", "initial"]);
        let initial = git_command(path, &["rev-parse", "HEAD"]);
        git_command(path, &["branch", "master"]);
        git_command(path, &["branch", "fixture-branch"]);
        git_command(path, &["tag", "-a", "fixture-tag", "-m", "fixture"]);

        /*
         * mainだけを変更して優先順位を識別する
         */
        fs::write(path.join("pages/page.md"), b"main\n").expect("main本文");
        git_command(path, &["add", "."]);
        git_command(path, &["commit", "-qm", "main change"]);
        Self { directory, initial }
    }

    ///
    /// fixtureのrootを取得する。
    ///
    /// # 戻り値
    /// 一時worktree root
    ///
    fn path(&self) -> &Path { self.directory.path() }
}
