/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! JSONの入れ子stateを一時fileへ退避するtop-only stack
//!

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::measurement::Area as TempDir;

use super::super::types::FailureKind;

///
/// 固定形式のparser frame。応答のkeyや値を保持しない。
///
#[derive(Clone, Copy)]
pub(super) struct Frame {
    pub(super) context: u8,
    pub(super) phase: u8,
    pub(super) key: u8,
    pub(super) seen: u16,
}

impl Frame {
    ///
    /// objectまたはarrayの初期frameを作る。
    ///
    /// # 引数
    /// * `context` - 有限schema状態
    /// * `array` - arrayの開始か
    ///
    /// # 戻り値
    /// 空のframe
    ///
    pub(super) fn new(context: u8, array: bool) -> Self {
        Self { context, phase: if array { 5 } else { 0 }, key: 0, seen: 0 }
    }

    ///
    /// frameを固定8bytesへ符号化する。
    ///
    /// # 戻り値
    /// 固定state record
    ///
    fn bytes(self) -> [u8; 8] {
        let seen = self.seen.to_le_bytes();
        [self.context, self.phase, self.key, seen[0], seen[1], 0, 0, 0]
    }

    ///
    /// 所有fileの固定recordを復号する。
    ///
    /// # 引数
    /// * `bytes` - このstackが書いたrecord
    ///
    /// # 戻り値
    /// parser frame
    ///
    fn from_bytes(bytes: [u8; 8]) -> Self {
        Self { context: bytes[0], phase: bytes[1], key: bytes[2],
            seen: u16::from_le_bytes([bytes[3], bytes[4]]) }
    }
}

///
/// 任意depthのうちtopだけをメモリへ保持するstack。
///
pub(super) struct Stack {
    _area: TempDir,
    file: crate::measurement::File,
    depth: u64,
    pub(super) top: Option<Frame>,
}

impl Stack {
    ///
    /// parser専用領域と空stackを準備する。
    ///
    /// # 戻り値
    /// 空の所有stack
    ///
    pub(super) fn new() -> Result<Self, FailureKind> {
        let area = crate::measurement::area(crate::measurement::Role::Json)
            .map_err(|_| FailureKind::Io)?;
        let file = File::options().create_new(true).read(true).write(true)
            .open(area.path().join("stack")).map_err(|_| FailureKind::Io)?;
        let file = crate::measurement::file(file,
            crate::measurement::Role::Json);
        Ok(Self { _area: area, file, depth: 0, top: None })
    }

    ///
    /// 固定recordの位置へ移動する。
    ///
    /// # 引数
    /// * `index` - record位置
    ///
    /// # 戻り値
    /// 移動成功
    ///
    fn seek(&mut self, index: u64) -> Result<(), FailureKind> {
        let offset = index.checked_mul(8).ok_or(FailureKind::Io)?;
        self.file.seek(SeekFrom::Start(offset)).map_err(|_| FailureKind::Io)?;
        Ok(())
    }

    ///
    /// 現topをfileへ退避して次frameを保持する。
    ///
    /// # 引数
    /// * `frame` - 新しい有限state
    ///
    /// # 戻り値
    /// 退避成功
    ///
    pub(super) fn push(&mut self, frame: Frame) -> Result<(), FailureKind> {
        if let Some(top) = self.top {
            self.seek(self.depth - 1)?;
            self.file.write_all(&top.bytes()).map_err(|_| FailureKind::Io)?;
        }
        self.depth = self.depth.checked_add(1).ok_or(FailureKind::Io)?;
        self.top = Some(frame);
        Ok(())
    }

    ///
    /// 現topを破棄し、fileから一つだけ復帰する。
    ///
    /// # 戻り値
    /// 復帰成功
    ///
    pub(super) fn pop(&mut self) -> Result<(), FailureKind> {
        self.depth = self.depth.checked_sub(1).ok_or(FailureKind::Protocol)?;
        self.top = if self.depth == 0 { None } else {
            self.seek(self.depth - 1)?;
            let mut bytes = [0; 8];
            self.file.read_exact(&mut bytes).map_err(|_| FailureKind::Io)?;
            Some(Frame::from_bytes(bytes))
        };
        Ok(())
    }
}

