/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 公開可能な件数だけを補助ログへ渡す境界
//!

use std::io::Write;

use env_logger::{Builder, Target};
use log::LevelFilter;

use crate::report::Report;

///
/// 外部クレートの詳細ログを有効にしないロガーを作る。
///
/// # 戻り値
/// 自クレートの補助情報だけをstderrへ出すBuilder
///
fn builder() -> Builder {
    let mut builder = Builder::new();
    builder.filter_level(LevelFilter::Off)
        .filter_module("luwiki_injector", LevelFilter::Info)
        .target(Target::Stderr)
        .format(|buffer, record| {
            writeln!(buffer, "[{}] {}", record.level(), record.args())
        });
    builder
}

///
/// env_loggerを初期化する。
///
/// # 戻り値
/// 初期化できた場合true。生の初期化エラーは出力しない。
///
pub(crate) fn init() -> bool { builder().try_init().is_ok() }

///
/// 結果報告の補助となる件数だけを記録する。
///
/// # 引数
/// * `report` - 安全な結果報告
///
/// # 戻り値
/// なし。結果報告はこのログとは別に出力する。
///
pub(crate) fn record(report: &Report) {
    log::info!(target: "luwiki_injector", "{}", event(report));
}

///
/// ページ識別子や資格情報を含めない補助イベントを作る。
///
/// # 引数
/// * `report` - 結果報告
///
/// # 戻り値
/// 終了コードと件数だけのログ文面
///
fn event(report: &Report) -> String {
    let summary = report.summary();
    format!("report exit={} warnings={} failed={} not_executed={}",
        report.exit_code(), summary.warnings, summary.failed,
        summary.not_executed,
    )
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use env_logger::Target;
    use log::{Level, Log, Record};

    use crate::report::Report;

    use super::{builder, event};

    ///
    /// ロガーの出力をテスト内で回収する。
    ///
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        ///
        /// 出力バイトを保存する。
        ///
        /// # 引数
        /// * `bytes` - ログの出力
        ///
        /// # 戻り値
        /// 保存したバイト数
        ///
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("ログバッファ").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        ///
        /// flushを受け付ける。
        ///
        /// # 戻り値
        /// 常に成功
        ///
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    ///
    /// 補助ログは件数だけを出し、依存クレートの秘密値を出さない。
    ///
    /// # 概要
    /// 自クレートの件数ログと依存クレートのダミー秘密ログを送り、前者だけの出力を確認する。
    ///
    /// # 注記
    /// グローバル登録せずLoggerを作り、二つのtargetを照合する。
    ///
    /// # 戻り値
    /// なし。安全な件数ログや依存ログの抑制が異なる場合はassertで失敗する。
    ///
    #[test]
    fn logger_emits_safe_summary_and_blocks_dependency_logs() {
        /*
         * テスト内のログ出力先と安全な件数報告を準備する
         */
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let logger = builder().target(Target::Pipe(Box::new(
            Buffer(Arc::clone(&bytes)),
        ))).build();
        let report = Report::preflight();
        /*
         * 自クレートの件数だけを出力する
         */
        logger.log(&Record::builder().level(Level::Info)
            .target("luwiki_injector")
            .args(format_args!("{}", event(&report))).build());
        /*
         * 依存クレートの秘密ログを抑制して出力を照合する
         */
        logger.log(&Record::builder().level(Level::Info)
            .target("reqwest")
            .args(format_args!("Authorization: unique-secret")).build());
        logger.flush();
        let output = String::from_utf8(bytes.lock().expect("ログ").clone())
            .expect("UTF-8ログ");
        assert!(output.contains("report exit=0"));
        assert!(!output.contains("unique-secret"));
    }
}
