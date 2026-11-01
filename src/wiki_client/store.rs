/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! Wiki本文の独立したreadonly所有者と逐次UTF-8保管
//!

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};

use crate::measurement::Area as TempDir;
use crate::report::types::PageId;
use crate::source::StopContext;

use super::types::{FailureKind, SendState, WikiFailure, WikiOperation};

///
/// 全受信検証後にだけ返す独立本文lease。
///
pub(crate) struct ReceivedBody {
    area: TempDir,
    bytes: u64,
    id: PageId,
}

impl ReceivedBody {
    ///
    /// 元bytesの実量を取得する。
    ///
    /// # 戻り値
    /// 実読込量（bytes）
    ///
    pub(crate) fn byte_len(&self) -> u64 { self.bytes }

    ///
    /// 保管済み本文をreadonlyで開く。
    ///
    /// # 戻り値
    /// 読込handle、または保管pathを含まない失敗
    ///
    pub(crate) fn open(&self) -> Result<File, WikiFailure> {
        File::open(self.area.path().join("body")).map_err(|_| WikiFailure::new(
            WikiOperation::ReadSource, FailureKind::Io, SendState::Started,
            Some(200), Some(&self.id),
        ))
    }
}

impl fmt::Debug for ReceivedBody {
    ///
    /// 本文・ID・保管pathを表示しない。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// サイズだけの表示
    ///
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ReceivedBody").field("bytes", &self.bytes)
            .finish()
    }
}

///
/// 未完UTF-8列だけをcarryして厳格検査する。
///
struct Utf8State {
    carry: [u8; 4],
    used: usize,
    needed: usize,
}

impl Utf8State {
    ///
    /// 空の検証状態を作る。
    ///
    /// # 戻り値
    /// 未完列なしの状態
    ///
    fn new() -> Self { Self { carry: [0; 4], used: 0, needed: 0 } }

    ///
    /// 一つの作業chunkを変換せず検査する。
    ///
    /// # 概要
    /// 多byte列の境界を持ち越し、過長列・surrogate・置換文字を拒否する。
    ///
    /// # 引数
    /// * `bytes` - 現chunk
    ///
    /// # 戻り値
    /// 厳格UTF-8なら成功
    ///
    fn check(&mut self, bytes: &[u8]) -> Result<(), FailureKind> {
        /*
         * 多byte先頭と継続byteを有限carryで検査する
         */
        for byte in bytes {
            if self.used == 0 {
                self.needed = match byte {
                    0x00..=0x7f => continue,
                    0xc2..=0xdf => 2, 0xe0..=0xef => 3, 0xf0..=0xf4 => 4,
                    _ => return Err(FailureKind::BodyUtf8),
                };
            } else if !(0x80..=0xbf).contains(byte) {
                return Err(FailureKind::BodyUtf8);
            }
            self.carry[self.used] = *byte;
            self.used += 1;
            /*
             * 完成した列だけを標準UTF-8検査し、置換文字を拒否する
             */
            if self.used == self.needed {
                let text = std::str::from_utf8(&self.carry[..self.used])
                    .map_err(|_| FailureKind::BodyUtf8)?;
                if text.contains('\u{fffd}') {
                    return Err(FailureKind::BodyReplacement);
                }
                self.used = 0;
            }
        }
        Ok(())
    }
}

///
/// 入力を逐次検査し、専用本文領域をworker内で所有する。
///
/// # 概要
/// 入力上限を流用せず、失敗時は未完成領域を回収する。
///
/// # 引数
/// * `input` - channelからの逐次reader
/// * `id` - 照会時点から既知のID
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 全受信成功時だけ返す本文所有者
///
pub(super) fn receive(input: &mut impl Read, id: PageId, stop: &StopContext)
    -> Result<ReceivedBody, FailureKind>
{
    let area = crate::measurement::area(crate::measurement::Role::Wiki)
        .map_err(|_| FailureKind::Io)?;
    let output = File::create(area.path().join("body"))
        .map_err(|_| FailureKind::Io)?;
    let mut output = crate::measurement::file(output,
        crate::measurement::Role::Wiki);
    let mut utf8 = Utf8State::new();
    let mut chunk = [0; super::receive::WORK_CHUNK];
    let mut bytes = 0_u64;
    /*
     * 停止・実量・UTF-8を確認して元bytesを保管する
     */
    loop {
        super::check(stop)?;
        let count = input.read(&mut chunk).map_err(|_| FailureKind::Io)?;
        if count == 0 { break; }
        utf8.check(&chunk[..count])?;
        bytes = bytes.checked_add(count as u64).ok_or(FailureKind::Io)?;
        output.write_all(&chunk[..count]).map_err(|_| FailureKind::Io)?;
    }
    /*
     * 未完列と保管完了を確認してreadonly所有者へ移す
     */
    if utf8.used != 0 { return Err(FailureKind::BodyUtf8); }
    output.flush().map_err(|_| FailureKind::Io)?;
    super::check(stop)?;
    drop(output);
    Ok(ReceivedBody { area, bytes, id })
}

