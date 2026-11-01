/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! chunk readerで使う有限JSON lexer。文字列や数値を全量収集しない
//!

use std::io::{BufReader, Read};

use crate::source::StopContext;

use super::super::types::FailureKind;

///
/// 1byte lookaheadと有限作業bufferだけを持つreader。
///
pub(super) struct Lexer<R> {
    input: BufReader<R>,
    look: Option<u8>,
    checked: usize,
    stop: StopContext,
}

impl<R: Read> Lexer<R> {
    ///
    /// readerと共有contextを受け取る。
    ///
    /// # 引数
    /// * `input` - 逐次reader
    /// * `stop` - 共有停止境界
    ///
    /// # 戻り値
    /// 全量を収集しないlexer
    ///
    pub(super) fn new(input: R, stop: StopContext) -> Self {
        Self { input: BufReader::new(input), look: None, checked: 0, stop }
    }

    ///
    /// readerから1byteを取得し、作業境界で停止確認する。
    ///
    /// # 引数
    /// * `self` - lookahead以外のreader状態
    ///
    /// # 戻り値
    /// 次byteまたはEOF
    ///
    fn raw(&mut self) -> Result<Option<u8>, FailureKind> {
        if self.checked == 0 { super::super::check(&self.stop)?; }
        self.checked = (self.checked + 1) % 4096;
        let mut byte = [0];
        match self.input.read(&mut byte).map_err(|_| FailureKind::Io)? {
            0 => Ok(None), _ => Ok(Some(byte[0])),
        }
    }

    ///
    /// 次byteを消費せず確認する。
    ///
    /// # 戻り値
    /// 次byteまたはEOF
    ///
    pub(super) fn peek(&mut self) -> Result<Option<u8>, FailureKind> {
        if self.look.is_none() { self.look = self.raw()?; }
        Ok(self.look)
    }

    ///
    /// 次byteを消費する。
    ///
    /// # 戻り値
    /// 次byteまたはEOF
    ///
    pub(super) fn take(&mut self) -> Result<Option<u8>, FailureKind> {
        if let Some(byte) = self.look.take() { Ok(Some(byte)) }
        else { self.raw() }
    }

    ///
    /// JSONの4種類の空白だけを読み飛ばす。
    ///
    /// # 戻り値
    /// 次の非空白byteまたはEOF
    ///
    pub(super) fn whitespace(&mut self) -> Result<Option<u8>, FailureKind> {
        while matches!(self.peek()?, Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.take()?;
        }
        self.peek()
    }

    ///
    /// 固定構文byteを消費する。
    ///
    /// # 引数
    /// * `expected` - 必要なbyte
    ///
    /// # 戻り値
    /// 一致なら成功
    ///
    pub(super) fn expect(&mut self, expected: u8) -> Result<(), FailureKind> {
        if self.take()? == Some(expected) { Ok(()) }
        else { Err(FailureKind::Protocol) }
    }

    ///
    /// true/false/nullを収集せず検査する。
    ///
    /// # 引数
    /// * `word` - 固定literal
    ///
    /// # 戻り値
    /// 一致なら成功
    ///
    pub(super) fn literal(&mut self, word: &[u8]) -> Result<(), FailureKind> {
        for byte in word { self.expect(*byte)?; }
        Ok(())
    }

    ///
    /// 4桁のhex escapeを有限整数へ解析する。
    ///
    /// # 戻り値
    /// UTF-16 code unit
    ///
    fn hex(&mut self) -> Result<u16, FailureKind> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.take()? {
                Some(b @ b'0'..=b'9') => b - b'0',
                Some(b @ b'a'..=b'f') => b - b'a' + 10,
                Some(b @ b'A'..=b'F') => b - b'A' + 10,
                _ => return Err(FailureKind::Protocol),
            };
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    ///
    /// escapeを一つのUnicode scalarへ写す。
    ///
    /// # 概要
    /// surrogate pairは隣接する二つのescapeとして検査する。
    ///
    /// # 戻り値
    /// 復号した文字
    ///
    fn escape(&mut self) -> Result<char, FailureKind> {
        let simple = match self.take()? {
            Some(b'"') => '"', Some(b'\\') => '\\', Some(b'/') => '/',
            Some(b'b') => '\u{8}', Some(b'f') => '\u{c}',
            Some(b'n') => '\n', Some(b'r') => '\r', Some(b't') => '\t',
            Some(b'u') => {
                /*
                 * UTF-16の単独surrogateを拒否してscalarへ変換する
                 */
                let high = self.hex()?;
                let code = if (0xd800..=0xdbff).contains(&high) {
                    self.expect(b'\\')?;
                    self.expect(b'u')?;
                    let low = self.hex()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(FailureKind::Protocol);
                    }
                    0x10000 + ((high as u32 - 0xd800) << 10)
                        + low as u32 - 0xdc00
                } else { high as u32 };
                return char::from_u32(code).ok_or(FailureKind::Protocol);
            }
            _ => return Err(FailureKind::Protocol),
        };
        Ok(simple)
    }

    ///
    /// 生UTF-8文字を有限carryで検証する。
    ///
    /// # 引数
    /// * `first` - 非ASCIIの先頭byte
    ///
    /// # 戻り値
    /// 厳格UTF-8のscalar
    ///
    fn scalar(&mut self, first: u8) -> Result<char, FailureKind> {
        let length = match first {
            0xc2..=0xdf => 2, 0xe0..=0xef => 3, 0xf0..=0xf4 => 4,
            _ => return Err(FailureKind::Protocol),
        };
        let mut bytes = [0; 4];
        bytes[0] = first;
        /*
         * 一つの多byte列だけを収集して標準検査する
         */
        for byte in bytes.iter_mut().take(length).skip(1) {
            *byte = self.take()?.ok_or(FailureKind::Protocol)?;
        }
        std::str::from_utf8(&bytes[..length])
            .map_err(|_| FailureKind::Protocol)?.chars().next()
            .ok_or(FailureKind::Protocol)
    }

    ///
    /// stringを逐次復号し、必要な文字だけをsinkへ渡す。
    ///
    /// # 概要
    /// 未知stringも構文とUnicodeを検証し、全量Stringを作らない。
    ///
    /// # 引数
    /// * `sink` - 有限値抽出または読み飛ばし
    ///
    /// # 戻り値
    /// 終端quoteまで正常なら成功
    ///
    pub(super) fn string(&mut self,
        mut sink: impl FnMut(char) -> Result<(), FailureKind>)
        -> Result<(), FailureKind>
    {
        self.expect(b'"')?;
        /*
         * 復号scalarを一つずつ渡し、制御byteと未完列を拒否する
         */
        loop {
            let ch = match self.take()? {
                Some(b'"') => return Ok(()),
                Some(b'\\') => self.escape()?,
                Some(0..=0x1f) | None => return Err(FailureKind::Protocol),
                Some(byte @ 0x20..=0x7f) => byte as char,
                Some(byte) => self.scalar(byte)?,
            };
            sink(ch)?;
        }
    }

    ///
    /// 整数部の桁を逐次処理する。
    ///
    /// # 引数
    /// * `collect` - u64として保持する必要があるか
    /// * `value` - 現在の有限整数
    ///
    /// # 戻り値
    /// 消費した桁数が1以上ならtrue
    ///
    fn digits(&mut self, collect: bool, value: &mut u64)
        -> Result<bool, FailureKind>
    {
        let mut any = false;
        while let Some(byte @ b'0'..=b'9') = self.peek()? {
            self.take()?;
            any = true;
            if collect {
                *value = value.checked_mul(10)
                    .and_then(|v| v.checked_add((byte - b'0') as u64))
                    .ok_or(FailureKind::Protocol)?;
            }
        }
        Ok(any)
    }

    ///
    /// number構文を逐次検査し、必要時だけu64を取り出す。
    ///
    /// # 概要
    /// 未知の巨大桁列は収集しない。必要な整数fieldだけ型範囲を検査する。
    ///
    /// # 引数
    /// * `integer` - 非負整数fieldを抽出するか
    ///
    /// # 戻り値
    /// 抽出時は検査済み整数、未知fieldならNone
    ///
    pub(super) fn number(&mut self, integer: bool)
        -> Result<Option<u64>, FailureKind>
    {
        let mut value = 0;
        /*
         * 符号と整数部を検査し、先頭zeroの複数桁を拒否する
         */
        if self.peek()? == Some(b'-') {
            self.take()?;
            if integer { return Err(FailureKind::Protocol); }
        }
        if self.peek()? == Some(b'0') {
            self.take()?;
            if matches!(self.peek()?, Some(b'0'..=b'9')) {
                return Err(FailureKind::Protocol);
            }
        } else if !self.digits(integer, &mut value)? {
            return Err(FailureKind::Protocol);
        }
        /*
         * 未知の小数・指数も桁列を保持せず完全に検査する
         */
        if self.peek()? == Some(b'.') {
            self.take()?;
            if integer || !self.digits(false, &mut value)? {
                return Err(FailureKind::Protocol);
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            self.take()?;
            if matches!(self.peek()?, Some(b'+' | b'-')) { self.take()?; }
            if integer || !self.digits(false, &mut value)? {
                return Err(FailureKind::Protocol);
            }
        }
        Ok(integer.then_some(value))
    }
}

