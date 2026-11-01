/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51の固定合成bytesとネットワーク不要の一時入力fixture
//!

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, ensure};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::real_cli_http::isolate_child;

/// 合成初回本文。BOM/CRLFと末尾改行なしをそのまま使用する。
pub const F0: &[u8] = "\u{feff}MS51合成初回\r\n末尾".as_bytes();
/// 合成変更本文。初回と異なり末尾LFを含む。
pub const F1: &[u8] = "\u{feff}MS51合成変更\r\n末尾\n".as_bytes();

///
/// 外部/private本文を受け取らない固定fixtureの1ファイル。
///
#[derive(Clone, Copy)]
pub struct Page {
    /// root基準の固定相対名。使用前に危険segmentを拒否する。
    pub name: &'static str,
    /// 変換しない固定本文bytes。
    pub body: &'static [u8],
}

///
/// 明示的に区別する小規模ZIP形式。
///
#[derive(Clone, Copy)]
pub enum ZipKind {
    /// 通常ZIP。
    Normal,
    /// ZIP64 entryを含み、footerを強制しない。
    Entry64,
    /// ZIP64 entryとZIP64 footerの双方を含む。
    Footer64,
}

///
/// ZIPとその保存領域を子/観測終了まで所有する。
///
pub struct ZipFixture {
    /// 自動破棄はローカルfixtureだけへ適用する。
    area: TempDir,
    /// 入力ZIPの保存先。
    path: PathBuf,
}

impl ZipFixture {
    ///
    /// 固定bytesからZIPを生成する。
    ///
    /// # 概要
    /// 通常/ZIP64を同じzip crateで作り、外部ファイルは取り込まない。
    ///
    /// # 引数
    /// * `pages` - コード内で定めたファイル集合
    /// * `kind` - 明示形式
    ///
    /// # 戻り値
    /// 一時領域を所有するZIP入力
    ///
    pub fn new(pages: &[Page], kind: ZipKind) -> Result<Self> {
        let area = tempfile::tempdir().map_err(|_| anyhow!("ZIP area"))?;
        let path = area.path().join("input.zip");
        let file = File::create(&path).map_err(|_| anyhow!("ZIP file"))?;
        let mut writer = ZipWriter::new(file);
        /*
         * 小さい本文でも要求したZIP64構造を生成する
         */
        if matches!(kind, ZipKind::Footer64) {
            writer.set_raw_zip64_extensible_data_sector(
                Vec::new().into_boxed_slice());
        }
        for page in pages {
            ensure!(safe_entry(page.name), "fixture entry rejected");
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .large_file(!matches!(kind, ZipKind::Normal));
            writer.start_file(format!("root/{}", page.name), options)
                .map_err(|_| anyhow!("ZIP entry"))?;
            writer.write_all(page.body).map_err(|_| anyhow!("ZIP body"))?;
        }
        if pages.is_empty() {
            writer.add_directory("root/", SimpleFileOptions::default())
                .map_err(|_| anyhow!("ZIP root"))?;
        }
        writer.finish().map_err(|_| anyhow!("ZIP finish"))?;
        Ok(Self { area, path })
    }

    ///
    /// 保存領域の生存を保ったまま入力pathを借用する。
    ///
    /// # 戻り値
    /// ローカルZIP path
    ///
    pub fn path(&self) -> &Path {
        let _owner = self.area.path();
        &self.path
    }
}

///
/// 外部設定や秘密を利用しない一時Git repository。
///
pub struct GitFixture {
    /// 履歴/worktreeの所有者。
    area: TempDir,
}

impl GitFixture {
    ///
    /// 指定branchに固定ファイルをコミットする。
    ///
    /// # 概要
    /// Git CLIはfixture生成だけに使い、製品の取得はgixへ委ねる。
    ///
    /// # 引数
    /// * `pages` - コミットする固定集合
    /// * `branch` - 内部固定のmainまたはmaster
    ///
    /// # 戻り値
    /// ローカル一時履歴。外部repoへ接続しない。
    ///
    pub fn new(pages: &[Page], branch: &str) -> Result<Self> {
        ensure!(matches!(branch, "main" | "master"), "fixture branch");
        let fixture = Self { area: tempfile::tempdir()
            .map_err(|_| anyhow!("Git area"))? };
        /*
         * 空templateから履歴を作り、ファイルbytesを変換しない
         */
        fixture.git(&["init", "-q", "--template=", "-b", branch])?;
        fixture.write(pages)?;
        fixture.git(&["add", "."])?;
        fixture.git(&["commit", "-qm", "synthetic fixture"])?;
        Ok(fixture)
    }

    ///
    /// fixtureの所有するworktreeを借用する。
    ///
    /// # 戻り値
    /// ローカル入力root
    ///
    pub fn path(&self) -> &Path { self.area.path() }

    ///
    /// 既存ファイルの固定bytesを書き替えて次の版をコミットする。
    ///
    /// # 引数
    /// * `pages` - 同じページ集合の変更版
    ///
    /// # 戻り値
    /// 新しい完全commit ID
    ///
    pub fn commit(&self, pages: &[Page]) -> Result<String> {
        self.write(pages)?;
        self.git(&["add", "."])?;
        self.git(&["commit", "-qm", "synthetic change"])?;
        self.git(&["rev-parse", "HEAD"])
    }

    ///
    /// 固定ファイル集合を変換せずworktreeへ保存する。
    ///
    /// # 引数
    /// * `pages` - コード内の相対名/bytes
    ///
    /// # 戻り値
    /// 保存成功。OSエラーのpathや本文は保持しない。
    ///
    pub fn write(&self, pages: &[Page]) -> Result<()> {
        /*
         * entryの検査後にだけ一時repo内の保存先を作る
         */
        for page in pages {
            ensure!(safe_entry(page.name), "fixture entry rejected");
            let path = self.path().join("root").join(page.name);
            let parent = path.parent().ok_or_else(|| anyhow!("Git parent"))?;
            fs::create_dir_all(parent).map_err(|_| anyhow!("Git directory"))?;
            fs::write(path, page.body).map_err(|_| anyhow!("Git body"))?;
        }
        Ok(())
    }

    ///
    /// 一時repo内だけで固定Git操作を実行する。
    ///
    /// # 概要
    /// hooks/template・設定注入・本文変換・signingを無効にして実行する。
    ///
    /// # 引数
    /// * `args` - ハーネス内部で定めるGit操作
    ///
    /// # 戻り値
    /// 正常終了stdout。raw診断は表示せず固定失敗へ写す。
    ///
    pub fn git(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new("git");
        isolate_child(&mut command);
        /*
         * 環境とrepo設定の外部干渉を抑え、ネットワーク操作を使わない
         */
        command.current_dir(self.path()).args(["-c", "core.hooksPath=/dev/null",
            "-c", "core.autocrlf=false", "-c", "core.safecrlf=false",
            "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"])
            .args(args).env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .stdin(Stdio::null()).stderr(Stdio::null());
        let output = command.output().map_err(|_| anyhow!("fixture Git"))?;
        ensure!(output.status.success(), "fixture Git failed");
        String::from_utf8(output.stdout)
            .map(|text| text.trim().to_owned())
            .map_err(|_| anyhow!("fixture Git UTF8"))
    }
}

///
/// fixture生成前に絶対名/危険segmentを拒否する。
///
/// # 引数
/// * `name` - 固定相対entry名
///
/// # 戻り値
/// 安全な非空相対名ならtrue
///
pub fn safe_entry(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('/') && !name.contains('\\')
        && !name.contains('\u{fffd}')
        && !name.chars().any(|v| v.is_whitespace() || v.is_control())
        && name.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && name.split('/').all(|part| part != ".git")
}
