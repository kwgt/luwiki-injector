/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 承認済み実Wikiでの製品CLI代表確認。実接続入口は明示ignoredのみ
//!

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use reqwest::header::HeaderValue;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[path = "support/real_cli_http.rs"]
mod real_cli_http;

// 共通解析のMS51専用入口はこの試験crateでは使用しない。
#[allow(dead_code)]
#[path = "support/real_cli_report.rs"]
mod real_cli_report;

use real_cli_http::{Auth, Environment, Lookup, Metadata, Observer, ROOT,
    approved_target, parse_lock, valid_id, within};
use real_cli_report::{Captured, observed_number, safe_category, safe_stage};

// 実ページから取得した本文ではなく、この試験だけの固定合成入力。
const ORIGINAL: &[u8] = "\u{feff}MS43合成初回\r\n末尾".as_bytes();
const CHANGED: &[u8] = "\u{feff}MS43合成変更\r\n末尾\n".as_bytes();
const INVALID: &[u8] = "---\nkey: value\n# title\n本文".as_bytes();
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

///
/// 新しい合成caseとローカルfixtureだけの寿命を所有する。
///
struct Case {
    /// 許可済みroot内の新subtree。
    prefix: String,
    /// 認証を含まないZIP/設定の一時領域。
    area: TempDir,
}

impl Case {
    ///
    /// 時刻/プロセス/連番で新run領域を準備する。
    ///
    /// # 引数
    /// * `name` - コード内の固定case名
    ///
    /// # 戻り値
    /// HTTP発行前の合成case
    ///
    fn new(name: &str) -> Result<Self> {
        let tick = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("test clock"))?.as_nanos();
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("{ROOT}/ms43-cli-{}-{tick}-{seq}/{name}",
            std::process::id());
        ensure!(within(&prefix, ROOT), "new case region rejected");
        eprintln!("MS43 case={name} path={prefix}");
        Ok(Self { prefix, area: tempfile::tempdir()
            .map_err(|_| anyhow!("test fixture area"))? })
    }

    ///
    /// 固定入力のページ名をcase配下へ結合する。
    ///
    /// # 引数
    /// * `name` - 拡張子なしの固定相対名
    ///
    /// # 戻り値
    /// 検証済みcase内のpath
    ///
    fn path(&self, name: &str) -> Result<String> {
        let path = format!("{}/{name}", self.prefix);
        ensure!(within(&path, &self.prefix), "case page rejected");
        Ok(path)
    }

    ///
    /// 実製品CLIへ合成ZIPと選択した認証envだけを渡す。
    ///
    /// # 概要
    /// raw stdout/stderrやCommand/errorを表示せず、検査済みの記録を返す。
    ///
    /// # 引数
    /// * `environment` - 明示専用環境
    /// * `auth` - 今回の認証方式
    /// * `mode` - 固定mode
    /// * `entries` - 合成entry名/本文bytes
    ///
    /// # 戻り値
    /// 秘匿検査済みの子出力。Debugへ渡さない。
    ///
    fn run(&self, environment: &Environment, auth: Auth, mode: &str,
        entries: &[(&str, &[u8])]) -> Result<Captured>
    {
        ensure!(matches!(mode, "error" | "update" | "overwrite"), "test mode");
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let input = self.area.path().join(format!("input-{seq}.zip"));
        let config = self.area.path().join("empty.toml");
        /*
         * 認証を含まない明示0600設定と入力だけを保存する
         */
        fs::write(&config, b"").map_err(|_| anyhow!("test config"))?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))
            .map_err(|_| anyhow!("test config permissions"))?;
        self.archive(&input, entries)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_luwiki-injector"));
        environment.configure_child(&mut command, auth);
        command.arg("--config-path").arg(config).args(["--prefix", &self.prefix,
            "--mode", mode]).arg(input).stdin(Stdio::null())
            .env("XDG_CONFIG_HOME", self.area.path()).env("RUST_LOG", "trace");
        /*
         * 子出力は回収してから秘匿と構造を検査し、rawを公開しない
         */
        let output = command.output().map_err(|_| anyhow!("real CLI spawn"))?;
        let code = output.status.code();
        eprintln!("MS43 subprocess_exit={code:?}");
        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| {
                eprintln!("MS43 stdout=invalid_utf8 recovery=unknown");
                anyhow!("real CLI stdout UTF8")
            })?;
        let stderr = String::from_utf8(output.stderr)
            .map_err(|_| {
                eprintln!("MS43 stderr=invalid_utf8 recovery=unknown");
                anyhow!("real CLI stderr UTF8")
            })?;
        if environment.check_output(&stdout).is_err()
            || environment.check_output(&stderr).is_err()
            || validate_output(&stdout, &stderr).is_err()
        {
            eprintln!("MS43 output=rejected recovery=unknown");
            return Err(anyhow!("real CLI output boundary rejected"));
        }
        let captured = Captured { code, stdout, stderr };
        /*
         * 期待assertより前に抽出できた有限復旧事実だけを残す
         */
        eprintln!("{}", captured.safe_summary());
        eprintln!("{}", captured.safe_global());
        for (name, _) in entries {
            let name = name.strip_suffix(".md")
                .ok_or_else(|| anyhow!("test entry extension"))?;
            eprintln!("{}", captured.safe_record(&self.path(name)?));
        }
        ensure!(captured.stdout.lines().filter(|v| v.starts_with("page="))
            .count()
            == entries.len(), "real CLI page count mismatch");
        Ok(captured)
    }

    ///
    /// 操作対象を固定caseへ限定したZIPを保存する。
    ///
    /// # 概要
    /// 外部ファイルや資格情報を入力へ含めず、元bytesを変換しない。
    ///
    /// # 引数
    /// * `path` - 一時領域内のZIP保存先
    /// * `entries` - 合成名と本文
    ///
    /// # 戻り値
    /// なし。fixture保存失敗は固定診断へ写す。
    ///
    fn archive(&self, path: &PathBuf, entries: &[(&str, &[u8])]) -> Result<()> {
        let file = File::create(path).map_err(|_| anyhow!("test ZIP file"))?;
        let mut zip = ZipWriter::new(file);
        /*
         * 全entryの投影先もHTTP前に同じcaseへ閉じる
         */
        for (name, body) in entries {
            let name = name.strip_suffix(".md")
                .ok_or_else(|| anyhow!("test entry extension"))?;
            self.path(name)?;
            zip.start_file(format!("root/{name}.md"),
                SimpleFileOptions::default())
                .map_err(|_| anyhow!("test ZIP entry"))?;
            zip.write_all(body).map_err(|_| anyhow!("test ZIP body"))?;
        }
        zip.finish().map_err(|_| anyhow!("test ZIP finish"))?;
        Ok(())
    }
}

///
/// 回収した子出力に合成本文や生認証表現が混ざることを拒否する。
///
/// # 概要
/// 合成固有断片と出力先も検査し、失敗時に元出力を表示しない。
///
/// # 引数
/// * `stdout` - 子の結果
/// * `stderr` - 子の診断
///
/// # 戻り値
/// 分離と非露出の検査。出力そのものをerrorへ載せない。
///
fn validate_output(stdout: &str, stderr: &str) -> Result<()> {
    /*
     * 全文と合成固有の断片を検査し、本文を診断へ転記しない
     */
    for body in [ORIGINAL, CHANGED, INVALID] {
        let body = std::str::from_utf8(body).map_err(|_| anyhow!("test body"))?;
        ensure!(!stdout.contains(body) && !stderr.contains(body),
            "synthetic body output rejected");
    }
    for marker in ["MS43合成初回", "MS43合成変更", "key: value"] {
        ensure!(!stdout.contains(marker) && !stderr.contains(marker),
            "synthetic body fragment rejected");
    }
    ensure!(!stdout.contains("warning:") && !stdout.contains("error:")
        && !stderr.contains("result: exit=")
        && !stdout.contains("Authorization")
        && !stderr.contains("Authorization")
        && !stdout.contains("X-Lock-") && !stderr.contains("X-Lock-"),
        "CLI output boundary rejected");
    Ok(())
}

///
/// 実際の有効IDを確認しCLIの確定IDと比較する。
///
/// # 引数
/// * `observer` - このcaseの独立検査
/// * `path` - 期待path
/// * `expected` - CLIの完全確定ID
///
/// # 戻り値
/// 確認済みactive ID
///
async fn active(observer: &mut Observer, path: &str, expected: &str)
    -> Result<String>
{
    match observer.lookup(path).await? {
        Lookup::Found(id) if id == expected => Ok(id),
        _ => Err(anyhow!("active identity mismatch")),
    }
}

///
/// 不存在のstatusを保ったまま確認し、通信失敗と区別する。
///
/// # 引数
/// * `observer` - 許可caseの検査
/// * `path` - 対象path
///
/// # 戻り値
/// 有効ページなし。404だけで旧削除を断定しない。
///
async fn absent(observer: &mut Observer, path: &str) -> Result<()> {
    match observer.lookup(path).await? {
        Lookup::Missing(code) => {
            eprintln!("MS43 observation=lookup absence_status={code}");
            Ok(())
        }
        Lookup::Found(_) => Err(anyhow!("case page already present")),
    }
}

///
/// 通常ページの非draft/非deleted/currentとrevisionを確認する。
///
/// # 引数
/// * `meta` - 期待path照合済みの実観測
///
/// # 戻り値
/// 確定revision。欠落はdraftや0へ補完せず失敗する。
///
fn normal_revision(meta: &Metadata) -> Result<u64> {
    ensure!(!meta.page_info.deleted && !meta.page_info.locked
        && meta.page_info.path.kind == "current",
        "normal metadata mismatch");
    meta.revision_info.as_ref().map(|v| v.revision)
        .ok_or_else(|| anyhow!("normal revision missing"))
}

///
/// 両認証の実製品CLIで通常3modeと再実行・bytes・旧新IDを確認する。
///
/// # 概要
/// 合成ZIPのsetupから実CLIへ通し、独立observerで各確定状態を照合する。
///
/// # 注記
/// 旧runを再利用せず、新caseの正常結果も自動一括削除しない。
///
/// # 戻り値
/// 確認結果。予期しない応答なら後続caseへ進まない。
///
#[tokio::test]
#[ignore = "requires explicitly approved MS43 real Wiki environment"]
async fn ms43_real_cli_normal_modes() -> Result<()> {
    let environment = Environment::load()?;
    for (auth, label) in [(Auth::Basic, "basic"), (Auth::Bearer, "bearer")] {
        for mode in ["error", "update", "overwrite"] {
            /*
             * 新caseを実CLIで作成し、完全bytesとactive IDを独立確認する
             */
            let case = Case::new(&format!("normal-{label}-{mode}"))?;
            let path = case.path("page")?;
            let mut observer = environment.observer(&case.prefix, auth)?;
            observer.hello().await?;
            absent(&mut observer, &path).await?;
            let first = case.run(&environment, auth, mode,
                &[("page.md", ORIGINAL)])?;
            first.expect(0)?;
            first.page(&path, "Created")?;
            let old = active(&mut observer, &path,
                &first.id(&path, "draft_id")?).await?;
            let before_meta = observer.metadata(&old, &path).await?;
            let before = normal_revision(&before_meta)?;
            observer.source_equals(&old, &path, ORIGINAL).await?;
            let same = case.run(&environment, auth, mode,
                &[("page.md", ORIGINAL)])?;
            if mode == "error" {
                same.expect(1)?;
                same.page(&path, "Failed")?;
                ensure!(same.stderr.contains("category=Conflict stage=Lookup"),
                    "existing error mismatch");
                active(&mut observer, &path, &old).await?;
                ensure!(normal_revision(&observer.metadata(&old, &path)
                    .await?)? == before, "existing error changed revision");
                observer.source_equals(&old, &path, ORIGINAL).await?;
                continue;
            }
            /*
             * 同一skip・相違更新/再作成・再実行の順で確定状態を確認する
             */
            same.expect(0)?;
            same.page(&path, "SkippedIdentical")?;
            active(&mut observer, &path, &old).await?;
            ensure!(normal_revision(&observer.metadata(&old, &path).await?)?
                == before, "identical skip changed revision");
            observer.source_equals(&old, &path, ORIGINAL).await?;
            let changed = case.run(&environment, auth, mode,
                &[("page.md", CHANGED)])?;
            changed.expect(0)?;
            changed.page(&path, if mode == "update" { "Updated" }
                else { "Recreated" })?;
            let new = if mode == "update" { changed.id(&path, "old_id")? }
                else { changed.id(&path, "draft_id")? };
            active(&mut observer, &path, &new).await?;
            observer.source_equals(&new, &path, CHANGED).await?;
            let after_meta = observer.metadata(&new, &path).await?;
            let after = normal_revision(&after_meta)?;
            if mode == "update" {
                ensure!(old == new && after == before + 1,
                    "update identity/revision mismatch");
            } else {
                ensure!(old != new, "recreated identity mismatch");
                let old_meta = observer.metadata(&old, &path).await?;
                ensure!(old_meta.page_info.deleted
                    && old_meta.page_info.path.kind == "last_deleted",
                    "old deletion not observed");
            }
            let again = case.run(&environment, auth, mode,
                &[("page.md", CHANGED)])?;
            again.expect(0)?;
            again.page(&path, "SkippedIdentical")?;
            active(&mut observer, &path, &new).await?;
            ensure!(normal_revision(&observer.metadata(&new, &path).await?)?
                == after, "rerun skip changed revision");
            observer.source_equals(&new, &path, CHANGED).await?;
        }
    }
    Ok(())
}

///
/// 実CLIの先行成功/後続未実行と明示再実行を確認する。
///
/// # 概要
/// bだけseedしたa/b/cのerror停止後、別のupdate実行で残作業へ進む。
///
/// # 注記
/// 最初の失敗で自動再実行せず、保存したaとb/cの実状態を先に検査する。
///
/// # 戻り値
/// 明示再実行までの確認結果
///
#[tokio::test]
#[ignore = "requires explicitly approved MS43 real Wiki environment"]
async fn ms43_real_cli_partial_success_and_rerun() -> Result<()> {
    let environment = Environment::load()?;
    let case = Case::new("partial")?;
    let mut observer = environment.observer(&case.prefix, Auth::Bearer)?;
    let (a, b, c) = (case.path("a")?, case.path("b")?, case.path("c")?);
    /*
     * 全対象の初期不存在を確認し、bだけ実CLIでseedする
     */
    for path in [&a, &b, &c] { absent(&mut observer, path).await?; }
    let seed = case.run(&environment, Auth::Basic, "error",
        &[("b.md", ORIGINAL)])?;
    seed.expect(0)?;
    let old_b = active(&mut observer, &b, &seed.id(&b, "draft_id")?).await?;
    let first = case.run(&environment, Auth::Bearer, "error",
        &[("c.md", CHANGED), ("b.md", CHANGED), ("a.md", CHANGED)])?;
    first.expect(1)?;
    first.page(&a, "Created")?;
    first.page(&b, "Failed")?;
    first.page(&c, "NotExecuted")?;
    ensure!(first.stdout.contains("created=1")
        && first.stdout.contains("failed=1 not_executed=1"), "partial counts");
    let id_a = active(&mut observer, &a, &first.id(&a, "draft_id")?).await?;
    observer.source_equals(&id_a, &a, CHANGED).await?;
    observer.source_equals(&old_b, &b, ORIGINAL).await?;
    absent(&mut observer, &c).await?;
    /*
     * 状態確認後にだけ別invocationの明示update再実行を行う
     */
    let again = case.run(&environment, Auth::Bearer, "update",
        &[("a.md", CHANGED), ("b.md", CHANGED), ("c.md", CHANGED)])?;
    again.expect(0)?;
    again.page(&a, "SkippedIdentical")?;
    again.page(&b, "Updated")?;
    again.page(&c, "Created")?;
    active(&mut observer, &a, &id_a).await?;
    active(&mut observer, &b, &old_b).await?;
    let id_c = active(&mut observer, &c, &again.id(&c, "draft_id")?).await?;
    for (id, path) in [(&id_a, &a), (&old_b, &b), (&id_c, &c)] {
        observer.source_equals(id, path, CHANGED).await?;
    }
    Ok(())
}

///
/// actorの事前保持lockに対する実CLIの409競合を確認する。
///
/// # 概要
/// update/overwriteの双方でAcquireLock停止と本文維持を確認後だけ正常解放する。
///
/// # 注記
/// 期限/不確定/期待外では追加解除を行わない。読取後raceの実証ではない。
///
/// # 戻り値
/// 競合とactor自身の単回正常解放の確認
///
#[tokio::test]
#[ignore = "requires explicitly approved MS43 real Wiki environment"]
async fn ms43_real_cli_lock_conflict() -> Result<()> {
    let environment = Environment::load()?;
    for mode in ["update", "overwrite"] {
        /*
         * 通常ページの根拠を確認して独立actorが既存lockを保持する
         */
        let case = Case::new(&format!("lock-{mode}"))?;
        let path = case.path("page")?;
        let mut actor = environment.observer(&case.prefix, Auth::Basic)?;
        absent(&mut actor, &path).await?;
        let seed = case.run(&environment, Auth::Basic, "error",
            &[("page.md", ORIGINAL)])?;
        seed.expect(0)?;
        let id = active(&mut actor, &path, &seed.id(&path, "draft_id")?).await?;
        let meta = actor.metadata(&id, &path).await?;
        let revision = normal_revision(&meta)?;
        ensure!(!meta.page_info.locked, "actor seed already locked");
        let mut lease = actor.acquire(&id, &path).await?;
        let result = case.run(&environment, Auth::Bearer, mode,
            &[("page.md", CHANGED)])?;
        result.expect(1)?;
        result.page(&path, "Failed")?;
        result.failure("Conflict", "AcquireLock", "AcquireLock", 409)?;
        ensure!(result.id(&path, "old_id")? == id, "conflict old ID");
        actor.source_equals(&id, &path, ORIGINAL).await?;
        /*
         * 確定409と本文維持の後にだけ所有する有効lockを一度解除する
         */
        actor.release(&mut lease).await?;
        let meta = actor.metadata(&id, &path).await?;
        ensure!(normal_revision(&meta)? == revision && !meta.page_info.locked,
            "actor release state mismatch");
    }
    Ok(())
}

///
/// 実overwriteの旧削除後PUT400で旧新状態と復旧情報を確認する。
///
/// # 概要
/// UTF8検証を通る不完全front matterを保存時拒否させ、新draftを自動回収しない。
///
/// # 注記
/// metadata観測はexpiry cleanupを伴い得る。期待外なら追加操作せず停止する。
///
/// # 戻り値
/// 合成領域での確定拒否/部分適用の確認。500/喪失の実証へ拡張しない。
///
#[tokio::test]
#[ignore = "requires explicitly approved MS43 real Wiki environment"]
async fn ms43_real_cli_recreation_rejected_after_delete() -> Result<()> {
    let environment = Environment::load()?;
    let case = Case::new("recreate-400")?;
    let (a, b) = (case.path("a")?, case.path("b")?);
    let mut observer = environment.observer(&case.prefix, Auth::Basic)?;
    /*
     * seedを実CLIで作り、失敗前の旧ID/revisionを保存する
     */
    absent(&mut observer, &a).await?;
    absent(&mut observer, &b).await?;
    let seed = case.run(&environment, Auth::Basic, "error",
        &[("a.md", ORIGINAL)])?;
    seed.expect(0)?;
    let old = active(&mut observer, &a, &seed.id(&a, "draft_id")?).await?;
    let revision = normal_revision(&observer.metadata(&old, &a).await?)?;
    let result = case.run(&environment, Auth::Bearer, "overwrite",
        &[("b.md", CHANGED), ("a.md", INVALID)])?;
    result.expect(1)?;
    result.page(&a, "Failed")?;
    result.page(&b, "NotExecuted")?;
    result.failure("Rejected", "PutSource", "PutSource", 400)?;
    ensure!(result.stdout.contains("old_delete_confirmed=true")
        && result.stderr.contains("kind: Syntax"), "400 recovery facts");
    ensure!(result.id(&a, "old_id")? == old, "400 old ID mismatch");
    let new = result.id(&a, "draft_id")?;
    ensure!(new != old, "400 new ID mismatch");
    observer.remember(&new, &a)?;
    /*
     * 旧soft-deleteと後続不存在を確認し、draftは最後に一回だけ観測する
     */
    let old_meta = observer.metadata(&old, &a).await?;
    ensure!(old_meta.page_info.deleted
        && old_meta.page_info.path.kind == "last_deleted"
        && old_meta.revision_info.as_ref()
            .is_some_and(|v| v.revision == revision),
        "400 old deletion not observed");
    observer.source_equals(&old, &a, ORIGINAL).await?;
    absent(&mut observer, &a).await?;
    absent(&mut observer, &b).await?;
    let new_meta = observer.metadata(&new, &a).await?;
    ensure!(!new_meta.page_info.deleted && new_meta.page_info.locked
        && new_meta.revision_info.is_none(),
        "draft state differs; preserve facts without cleanup/retry");
    eprintln!("MS43 old_id={old} draft_id={new} revision={revision} \
        old_delete_confirmed=true automatic_recovery=false");
    Ok(())
}

///
/// 実環境を読まず対象/地域/IDのguardを検証する。
///
/// # 概要
/// 許可root自身・隣接prefix・危険segmentとTLS検証迂回に相当するURLを拒否する。
///
/// # 注記
/// HTTP/実資格情報/env値を使用しない通常試験。
///
/// # 戻り値
/// なし。許可範囲を拡張する誤りがあれば失敗する。
///
#[test]
fn real_cli_guards_reject_foreign_regions_and_targets() {
    /*
     * 公開対象とsegment境界だけを許可する
     */
    assert!(approved_target("https://localhost:8040"));
    for target in ["http://localhost:8040", "https://127.0.0.1:8040",
        "https://localhost:8040/other", "https://localhost:8040?x=1"]
    { assert!(!approved_target(target)); }
    assert!(within(&format!("{ROOT}/ms43-cli-run/case/page"), ROOT));
    for tail in ["", "x/page", "/", "/../x", "/a//b", "/bad name"] {
        assert!(!within(&format!("{ROOT}{tail}"), ROOT));
    }
    assert!(!valid_id("untrusted-page-or-lock"));
}

///
/// 合成lock headerの重複/期限/形式を値を出さず拒否する。
///
/// # 概要
/// 検査済み期限とsensitive認証だけが返ることを通常試験で確認する。
///
/// # 注記
/// 実HTTP/実token/現在時刻に依存せず、固定観測時刻を渡す。
///
/// # 戻り値
/// 通常の検証結果
///
#[test]
fn actor_lock_header_rejects_ambiguity_and_expiry() -> Result<()> {
    let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .map_err(|_| anyhow!("test date"))?.with_timezone(&Utc);
    let token = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
    /*
     * 正常な独立tokenだけをsensitiveとして返す
     */
    let header = HeaderValue::from_str(&format!(
        "expire=2026-01-02T00:00:00Z token={token}"))?;
    let (secret, _) = parse_lock(&header, now)?;
    assert!(secret.is_sensitive());
    /*
     * 重複/期限切れ/未知項目を元文字列なしのerrorへ変換する
     */
    for raw in [format!("expire=2026-01-01T00:00:00Z token={token}"),
        format!("expire=2026-01-02T00:00:00Z token={token} token={token}"),
        format!("expire=2026-01-02T00:00:00Z token={token} x=1")]
    {
        assert!(parse_lock(&HeaderValue::from_str(&raw)?, now).is_err());
    }
    Ok(())
}

///
/// 合成出力の本文/認証拒否と復旧ID関連の厳格な取得を確認する。
///
/// # 概要
/// raw子出力を表示せず、不正/重複の元結果を成功へ補完しない。
///
/// # 注記
/// 合成文字列だけを使う通常試験で、実envやHTTPを参照しない。
///
/// # 戻り値
/// なし。不正出力を許可すれば失敗する。
///
#[test]
fn captured_output_rejects_body_headers_and_ambiguous_recovery() {
    /*
     * 本文/認証表現の露出を値を含めない失敗へ写す
     */
    assert!(validate_output(std::str::from_utf8(ORIGINAL).unwrap(), "")
        .is_err());
    assert!(validate_output("", "Authorization: private").is_err());
    assert!(validate_output("", "MS43合成初回").is_err());
    let line = "recovery: path=/fixture/a old_id=None revision=None \
        draft_id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAV\")";
    let captured = Captured { code: Some(0), stdout: line.to_owned(),
        stderr: String::new() };
    /*
     * 一意で完全なIDだけを採用し候補や重複を拒否する
     */
    assert!(captured.id("/fixture/a", "draft_id").is_ok());
    assert!(captured.id("/fixture/a", "old_id").is_err());
    let duplicate = Captured { code: Some(0),
        stdout: format!("{line}\n{line}\n"),
        stderr: String::new() };
    assert!(duplicate.id("/fixture/a", "draft_id").is_err());
    /*
     * 期待外の出力でも型検証した有限事実だけをassert前記録へ残す
     */
    let path = format!("{ROOT}/ms43-cli-pure/case/a");
    let observed = Captured { code: Some(1), stdout: format!(
        "created=0 skipped=0 updated=0 recreated=0 warnings=1 failed=1 \
         not_executed=1\npage={path} source=Zip:root/a.md \
         outcome=Failed {{ category: Uncertain, \
         stage: DeleteOld }} stage=Reconcile\nrecovery: path={path} \
         old_id=Some(\"01ARZ3NDEKTSV4RRFFQ69G5FAV\") revision=Some(7) \
         draft_id=None old_delete_confirmed=true old_lock_observed=None\n"),
        stderr: format!("error: page={path} category=Uncertain \
            stage=DeleteOld\nerror: category=Cancelled stage=Preflight\n\
            raw-private\n") };
    let record = observed.safe_record(&path);
    assert!(observed.safe_summary().contains("warnings=1 failed=1"));
    assert_eq!(observed.safe_global(),
        "MS43 role=global category=Cancelled stage=Preflight");
    assert!(duplicate.safe_summary().contains("created=unknown"));
    for expected in ["exit=Some(1)", "outcome=Failed", "last_stage=Reconcile",
        "category=Uncertain", "cause_stage=DeleteOld", "revision=7",
        "old_delete_confirmed=true"]
    { assert!(record.contains(expected)); }
    assert!(!record.contains("raw-private"));
    assert_eq!(safe_stage(Some("raw-private")), "unknown");
    assert_eq!(safe_category(Some("raw-private")), "unknown");
    assert_eq!(observed_number(Some("revision=None"), "revision="), None);
}
