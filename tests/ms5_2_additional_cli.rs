/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS52の追加参照代表と実環境不要の指定境界検査
//!

use anyhow::Result;

// 共有補助には他のMS51入口専用APIも含まれるため未使用を局所許容する。
#[allow(dead_code)]
#[path = "support/cli_http.rs"]
mod cli_http;
#[allow(dead_code)]
#[path = "support/real_cli_http.rs"]
mod real_cli_http;
#[allow(dead_code)]
#[path = "support/real_cli_report.rs"]
mod real_cli_report;
#[allow(dead_code)]
#[path = "support/source_fixture.rs"]
mod source_fixture;
#[allow(dead_code)]
#[path = "support/ms5_1_harness.rs"]
mod ms5_1_harness;

use ms5_1_harness::remote_fixture::additional::{Role, run, verify_boundary};

///
/// 閉じたspecと広告の境界を実envなしで検査する。
///
/// # 注記
/// 純粋parser/validatorに合成値だけを渡しGit/HTTPを呼ばない。
///
/// # 戻り値
/// 未知キー/誤source/広告祖先/不正tagの拒否
///
#[test]
fn ms52_reference_spec_and_advertisement_guards() -> Result<()> {
    verify_boundary()
}

///
/// HTTPSでHEADよりmainを優先する実代表を確認する。
///
/// # 注記
/// PUB52-Aの全事前条件後に一CLIと独立状態を比較する。
///
/// # 戻り値
/// A2 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_https_main_over_head() -> Result<()> {
    run(false, Role::Main).await
}

///
/// GitHub defaultと異なるmainを選ぶ実代表を確認する。
///
/// # 注記
/// PUB52-AのAPI観測と製品取得を別々に照合する。
///
/// # 戻り値
/// A2 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_github_main_over_default() -> Result<()> {
    run(true, Role::Main).await
}

///
/// HTTPSでmain不在時のmaster選択を確認する。
///
/// # 注記
/// PUB52-Bの広告がmasterだけであることを事前/事後に照合する。
///
/// # 戻り値
/// A0 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_https_master_fallback() -> Result<()> {
    run(false, Role::Master).await
}

///
/// GitHubでmain不在時のmaster選択を確認する。
///
/// # 注記
/// 独立APIはmain404を確認し、それ以外の失敗を代用しない。
///
/// # 戻り値
/// A0 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_github_master_fallback() -> Result<()> {
    run(true, Role::Master).await
}

///
/// HTTPSの注釈tagをcommitへpeelして取得する。
///
/// # 注記
/// tag objectとpeeled A0を独立local/広告で確認する。
///
/// # 戻り値
/// A0 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_https_annotated_tag() -> Result<()> {
    run(false, Role::Tag).await
}

///
/// private資格とGitHub注釈tag取得を一代表へ結合する。
///
/// # 注記
/// 旧private gateと今回tag gateを両方確認し既存V0だけを保存する。
///
/// # 戻り値
/// private V0全bytes/Created/exit0。実名や本文を表示しない。
///
#[tokio::test]
#[ignore = "requires separate approved private tag import and real Wiki"]
async fn ms52_real_private_github_annotated_tag() -> Result<()> {
    run(true, Role::PrivateTag).await
}

///
/// HTTPSで広告tipでない到達可能commitを取得する。
///
/// # 注記
/// A1の祖先性と非広告性を別々に照合しA2本文と区別する。
///
/// # 戻り値
/// A1 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_https_reachable_unadvertised_commit() -> Result<()> {
    run(false, Role::Ancestor).await
}

///
/// GitHubの完全commit解決と非広告祖先取得を確認する。
///
/// # 注記
/// public A1のAPI観測とGit取得を独立期待へ照合する。
///
/// # 戻り値
/// A1 bytes/Created/exit0
///
#[tokio::test]
#[ignore = "requires approved MS52 sources and real Wiki"]
async fn ms52_real_github_reachable_unadvertised_commit() -> Result<()> {
    run(true, Role::Ancestor).await
}
