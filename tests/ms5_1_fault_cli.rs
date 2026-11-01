/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! MS51の通常故障8群と、承認時だけ実行する実Wiki故障14入口
//!

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
#[path = "support/ms5_1_fault_http.rs"]
mod ms5_1_fault_http;
#[path = "support/ms5_1_fault_report.rs"]
mod ms5_1_fault_report;
#[path = "support/ms5_1_fault_cases.rs"]
mod ms5_1_fault_cases;

use anyhow::Result;

use ms5_1_fault_cases::{normal, real};

///
/// 破損・重複・上限で計画前に停止する。
///
/// # 注記
/// 合成ZIPと専用loopbackだけを使用する。
///
/// # 戻り値
/// 終了2、計画未成立、要求0の一致。
///
#[tokio::test]
async fn ms51_fault_n01_invalid_input() -> Result<()> { normal(1).await }

///
/// 新規/updateのPUT異常を単回照合する。
///
/// # 注記
/// 500/喪失と通常後続継続を完全要求列で照合する。
///
/// # 戻り値
/// 警告付き適用、元原因、単回送信の一致。
///
#[tokio::test]
async fn ms51_fault_n02_put_reconciliation() -> Result<()> { normal(2).await }

///
/// DELETEの完全500と不確定応答を区別する。
///
/// # 注記
/// 空・不完500と不正成功には再作成を許可しない。
///
/// # 戻り値
/// 削除両観測と条件付き再作成の一致。
///
#[tokio::test]
async fn ms51_fault_n03_delete_classification() -> Result<()> {
    normal(3).await
}

///
/// POST異常と削除後の七つの失敗を区別する。
///
/// # 注記
/// 再探索・再送・draft回収なしと先行警告保持を照合する。
///
/// # 戻り値
/// 元原因、旧新IDの確定範囲、後続未実行の一致。
///
#[tokio::test]
async fn ms51_fault_n04_post_and_recreation() -> Result<()> { normal(4).await }

///
/// 主失敗を補助照合失敗と解除失敗から分離する。
///
/// # 注記
/// 先行成功、停止ページ、後続未実行も同時に比較する。
///
/// # 戻り値
/// ロールと警告・復旧情報の独立保持。
///
#[tokio::test]
async fn ms51_fault_n05_independent_failures() -> Result<()> {
    normal(5).await
}

///
/// 共有CLI期限後に追加HTTPを送らない。
///
/// # 注記
/// OS signal取消と混同せず、実処理期限だけを用いる。
///
/// # 戻り値
/// 期限停止と追加照合・解除0。
///
#[tokio::test]
async fn ms51_fault_n06_shared_deadline() -> Result<()> { normal(6).await }

///
/// relayの有限HTTP解釈と転送gateを検査する。
///
/// # 注記
/// 実環境・実資格情報・外部接続を使用しない。
///
/// # 戻り値
/// 危険要求と曖昧framingの拒否。
///
#[test]
fn ms51_fault_n07_relay_boundary() -> Result<()> {
    ms5_1_fault_http::verify_boundary()
}

///
/// Report解析が有限語彙と一意なfieldに閉じることを検査する。
///
/// # 注記
/// 秘密を含まない合成出力だけを解析する。
///
/// # 戻り値
/// 重複・欠落・未知値と不整合の拒否。
///
#[test]
fn ms51_fault_n08_finite_report() -> Result<()> {
    ms5_1_fault_report::verify_finite_parser()
}

///
/// 新規PUTの実front matter拒否を確認する。
///
/// # 注記
/// 専用承認gateと既存実Wiki環境を必須とする。
///
/// # 戻り値
/// Syntax/400、draft残存、後続未実行の実観測。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r01_frontmatter_400() -> Result<()> { real(1).await }

///
/// 事前actor lockによる実409を確認する。
///
/// # 注記
/// 期待した拒否と本文維持の確認後だけactor資源を解除する。
///
/// # 戻り値
/// 旧ID/revision保持と所有資源の限定解除。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r02_existing_lock_409() -> Result<()> { real(2).await }

///
/// 初回本文取得後の実actor更新による競合を確認する。
///
/// # 注記
/// 保存済み初回応答を保持し、直接HTTPS actor終了後に解放する。
///
/// # 戻り値
/// 事前lockと異なるrace、旧情報、所有lock解除の一致。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r03_read_then_actor_race() -> Result<()> { real(3).await }

///
/// 新規PUT適用後の下流500を確認する。
///
/// # 注記
/// 上流実適用と製品の再照合観測を別に比較する。
///
/// # 戻り値
/// 警告付き作成と通常後続継続。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r04_create_put_500() -> Result<()> { real(4).await }

///
/// update適用後の下流応答喪失を確認する。
///
/// # 注記
/// 正常上流応答後にだけ下流を閉じ、本文を再送しない。
///
/// # 戻り値
/// statusなし警告と警告付き更新。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r05_update_put_lost() -> Result<()> { real(5).await }

///
/// 旧削除後の完全500から条件付き再作成する。
///
/// # 注記
/// metaとpathの両観測を要求列で照合する。
///
/// # 戻り値
/// 累積警告、旧新ID、通常後続継続。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r06_delete_500_recreates() -> Result<()> { real(6).await }

///
/// 旧削除後の空500で再作成しない。
///
/// # 注記
/// 完全なHttp500とProtocolを混同しない。
///
/// # 戻り値
/// 削除確認後もPOST0と終了1。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r07_delete_empty_500() -> Result<()> { real(7).await }

///
/// 旧削除後の応答喪失で再作成しない。
///
/// # 注記
/// 削除の実適用と下流status未観測を分離する。
///
/// # 戻り値
/// 削除確認後もPOST0と元原因保持。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r08_delete_lost() -> Result<()> { real(8).await }

///
/// 新規POST適用後の応答喪失で探索しない。
///
/// # 注記
/// relayだけが知る上流IDを製品の確定IDへ補完しない。
///
/// # 戻り値
/// 再POST・GET探索・draft解除0。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r09_initial_post_lost() -> Result<()> { real(9).await }

///
/// 削除500後の再作成POST喪失で停止する。
///
/// # 注記
/// 先行DELETE警告と旧情報を保持する。
///
/// # 戻り値
/// 新ID未確定、探索・再送・回収0。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r10_recreation_post_lost() -> Result<()> { real(10).await }

///
/// 削除500後の再作成renewを上流転送前に拒否する。
///
/// # 注記
/// 実上流renewを送らず、合成403と明示記録する。
///
/// # 戻り値
/// 先行警告、新draft確定、PUT・解除0。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r11_recreation_renew_403() -> Result<()> { real(11).await }

///
/// 削除500後の実再作成PUT400を確認する。
///
/// # 注記
/// 先行DELETE警告とSyntax拒否を別に保持する。
///
/// # 戻り値
/// 旧soft-delete、新draft、後続未実行の実観測。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r12_recreation_put_400() -> Result<()> { real(12).await }

///
/// 再作成PUT500を確認しても独立安全停止を保持する。
///
/// # 注記
/// 最終ページと後続ありを新caseで分けて実行する。
///
/// # 戻り値
/// 発生順警告、適用一件、終了1。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r13_recreation_put_500_stop() -> Result<()> {
    real(13).await
}

///
/// 実旧削除応答保持中にCLI共通期限へ到達させる。
///
/// # 注記
/// fault到達が必須で、監視の強制終了を成功へ変換しない。
///
/// # 戻り値
/// 期限後の再照合・再作成・解除HTTP0。
///
#[tokio::test]
#[ignore = "requires REAL-FAULT-01 and approved real Wiki"]
async fn ms51_fault_r14_delete_deadline() -> Result<()> { real(14).await }
