/*
 * Page injector for kwgt/luwiki
 *
 *  Copyright (C) 2026 Hiroshi KUWAGATA <kgt9221@gmail.com>
 */

//!
//! 有限schemaの選択抽出とfile-backed構造parser
//!

mod lexer;
mod stack;

use std::io::Read;

use crate::report::types::{PageId, PagePath};
use crate::source::StopContext;

use self::lexer::Lexer;
use self::stack::{Frame, Stack};
use super::types::{Diagnostics, FailureKind, FrontMatterKind, PageMetadata};
use super::types::{PathKind, PathRelation};

///
/// 単発応答の抽出目的。metadataのidはcaller由来。
///
pub(super) enum Purpose {
    Id,
    CreatedId,
    Metadata { id: PageId, expected: PagePath },
    Error,
}

///
/// JSON全体の検証後にだけ返す抽出値。
///
pub(super) enum JsonValue {
    Id(PageId), Metadata(PageMetadata), Error(Option<Diagnostics>),
}

impl std::fmt::Debug for JsonValue {
    ///
    /// 識別子・本文・応答値を含めない有限状態を表示する。
    ///
    /// # 引数
    /// * `formatter` - 出力先
    ///
    /// # 戻り値
    /// 有限状態
    ///
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Id(_) => "Id", Self::Metadata(_) => "Metadata",
            Self::Error(_) => "Error",
        })
    }
}

///
/// scalarまたは子objectへ渡す有限schema field。
///
#[derive(Clone, Copy)]
enum Field {
    Unknown, Object(u8), Id, Path, PathKind, Deleted, Locked, Revision,
    ErrorKind, DetailType, Line, Column,
}

///
/// 巨大key・未知stringも蓄積しない有限文字buffer。
///
struct SmallText { bytes: [u8; 26], length: usize, overflow: bool }

impl SmallText {
    ///
    /// 空の有限bufferを作る。
    ///
    /// # 戻り値
    /// 最大ULID長の作業buffer
    ///
    fn new() -> Self { Self { bytes: [0; 26], length: 0, overflow: false } }

    ///
    /// 復号したscalarを有限容量内だけに保持する。
    ///
    /// # 引数
    /// * `ch` - 検査済みscalar
    ///
    /// # 戻り値
    /// 成功。容量超過はunknown判定用に記録する。
    ///
    fn push(&mut self, ch: char) -> Result<(), FailureKind> {
        let mut bytes = [0; 4];
        let text = ch.encode_utf8(&mut bytes);
        if !self.overflow && self.length + text.len() <= self.bytes.len() {
            self.bytes[self.length..self.length + text.len()]
                .copy_from_slice(text.as_bytes());
            self.length += text.len();
        } else { self.overflow = true; }
        Ok(())
    }

    ///
    /// 小さい既知名だけを比較可能な形で返す。
    ///
    /// # 戻り値
    /// 検査済みtext。巨大値ならNone
    ///
    fn text(&self) -> Option<&str> {
        if self.overflow { None }
        else { std::str::from_utf8(&self.bytes[..self.length]).ok() }
    }
}

///
/// 期待pathとの照合と禁止要素検査を有限stateで行う。
///
struct PathCheck<'a> {
    expected: &'a [u8],
    cursor: usize,
    matches: bool,
    started: bool,
    root: bool,
    segment: u8,
}

impl<'a> PathCheck<'a> {
    ///
    /// 期待pathから照合状態を作る。
    ///
    /// # 引数
    /// * `expected` - callerが検証済みの絶対path
    ///
    /// # 戻り値
    /// 応答textを保持しない状態
    ///
    fn new(expected: &'a str) -> Self {
        Self { expected: expected.as_bytes(), cursor: 0, matches: true,
            started: false, root: false, segment: 0 }
    }

    ///
    /// 復号scalarの安全性と一致を逐次検査する。
    ///
    /// # 概要
    /// segmentのdot状態だけを持ち、応答pathの長さに比例して確保しない。
    ///
    /// # 引数
    /// * `ch` - 復号scalar
    ///
    /// # 戻り値
    /// 禁止要素がなければ成功
    ///
    fn push(&mut self, ch: char) -> Result<(), FailureKind> {
        if ch.is_whitespace() || ch.is_control()
            || matches!(ch, '\\' | '\u{fffd}')
        { return Err(FailureKind::Protocol); }
        /*
         * 完全bytes一致を比較するが、相違後は位置を増やさない
         */
        let mut bytes = [0; 4];
        for byte in ch.encode_utf8(&mut bytes).as_bytes() {
            if self.matches && self.expected.get(self.cursor) == Some(byte) {
                self.cursor += 1;
            } else { self.matches = false; }
        }
        /*
         * slashとdot segmentの状態を有限値で検査する
         */
        if !self.started {
            if ch != '/' { return Err(FailureKind::Protocol); }
            self.started = true;
            self.root = true;
        } else if ch == '/' {
            if self.segment != 3 { return Err(FailureKind::Protocol); }
            self.segment = 0;
            self.root = false;
        } else {
            self.root = false;
            self.segment = match (self.segment, ch) {
                (0, '.') => 1, (1, '.') => 2, _ => 3,
            };
        }
        Ok(())
    }

    ///
    /// 終端segmentと期待bytes長を確認する。
    ///
    /// # 戻り値
    /// 一致または相違。相違path自体は返さない。
    ///
    fn finish(self) -> Result<PathRelation, FailureKind> {
        if !self.started || (!self.root && self.segment != 3) {
            return Err(FailureKind::Protocol);
        }
        Ok(if self.matches && self.cursor == self.expected.len() {
            PathRelation::MatchesExpected
        } else { PathRelation::Different })
    }
}

///
/// 有限個の必要fieldだけを保持する抽出state。
///
struct Values {
    id: Option<PageId>,
    path_kind: Option<PathKind>,
    relation: Option<PathRelation>,
    deleted: Option<bool>,
    locked: Option<bool>,
    revision: Option<u64>,
    front_matter: bool,
    detail: FrontMatterKind,
    line: Option<u64>,
    column: Option<u64>,
}

impl Values {
    ///
    /// 未抽出のstateを作る。
    ///
    /// # 戻り値
    /// 応答本文を持たない状態
    ///
    fn new() -> Self {
        Self { id: None, path_kind: None, relation: None, deleted: None,
            locked: None, revision: None, front_matter: false,
            detail: FrontMatterKind::Unknown, line: None, column: None }
    }
}

///
/// lexer・file stack・有限抽出値の所有者。
///
struct Parser<R> {
    lexer: Lexer<R>,
    stack: Stack,
    purpose: Purpose,
    values: Values,
    stop: StopContext,
}

impl<R: Read> Parser<R> {
    ///
    /// 応答の目的と逐次readerを接続する。
    ///
    /// # 引数
    /// * `input` - 逐次reader
    /// * `purpose` - schema抽出目的
    /// * `stop` - 共有停止境界
    ///
    /// # 戻り値
    /// parserの所有者
    ///
    fn new(input: R, purpose: Purpose, stop: StopContext)
        -> Result<Self, FailureKind>
    {
        Ok(Self { lexer: Lexer::new(input, stop.clone()), stack: Stack::new()?,
            purpose, values: Values::new(), stop })
    }

    ///
    /// keyを有限名へ照合し、必須field重複を拒否する。
    ///
    /// # 引数
    /// * `frame` - 現object state
    ///
    /// # 戻り値
    /// 更新されたobject state
    ///
    fn key(&mut self, mut frame: Frame) -> Result<Frame, FailureKind> {
        let mut name = SmallText::new();
        self.lexer.string(|ch| name.push(ch))?;
        let key = key_code(frame.context, name.text());
        if key != 0 {
            let bit = 1 << (key - 1);
            if frame.seen & bit != 0 { return Err(FailureKind::Protocol); }
            frame.seen |= bit;
        }
        frame.key = key;
        frame.phase = 2;
        Ok(frame)
    }

    ///
    /// 必要stringを選択し、未知stringは収集せず消費する。
    ///
    /// # 概要
    /// pathだけは期待pathへ逐次比較し、IDと有限enum以外は保持しない。
    ///
    /// # 引数
    /// * `field` - schema field
    ///
    /// # 戻り値
    /// 正常なstringなら成功
    ///
    fn string(&mut self, field: Field) -> Result<(), FailureKind> {
        /*
         * 未知値とpathは全量textを作らない
         */
        if matches!(field, Field::Unknown) {
            return self.lexer.string(|_| Ok(()));
        }
        if matches!(field, Field::Path) {
            let Purpose::Metadata { expected, .. } = &self.purpose else {
                return Err(FailureKind::Protocol);
            };
            let mut path = PathCheck::new(expected.as_str());
            self.lexer.string(|ch| path.push(ch))?;
            self.values.relation = Some(path.finish()?);
            return Ok(());
        }
        /*
         * 必要な有限値だけを取り出す
         */
        let mut text = SmallText::new();
        self.lexer.string(|ch| text.push(ch))?;
        match field {
            Field::Id => self.values.id = Some(PageId::new(
                text.text().ok_or(FailureKind::Protocol)?,
            ).map_err(|_| FailureKind::Protocol)?),
            Field::PathKind => self.values.path_kind = Some(match text.text() {
                Some("current") => PathKind::Current,
                Some("last_deleted") => PathKind::LastDeleted,
                _ => return Err(FailureKind::Protocol),
            }),
            Field::ErrorKind =>
                self.values.front_matter = text.text() == Some("front_matter"),
            Field::DetailType => self.values.detail = match text.text() {
                Some("syntax") => FrontMatterKind::Syntax,
                Some("validation") => FrontMatterKind::Validation,
                _ => FrontMatterKind::Unknown,
            },
            _ => return Err(FailureKind::Protocol),
        }
        Ok(())
    }

    ///
    /// 非負整数fieldまたは未知numberを処理する。
    ///
    /// # 引数
    /// * `field` - schema field
    ///
    /// # 戻り値
    /// 正常な数値なら成功
    ///
    fn number(&mut self, field: Field) -> Result<(), FailureKind> {
        let needed = matches!(field, Field::Revision | Field::Line
            | Field::Column);
        if !needed && !matches!(field, Field::Unknown) {
            return Err(FailureKind::Protocol);
        }
        let value = self.lexer.number(needed)?;
        match field {
            Field::Revision => self.values.revision = value,
            Field::Line => self.values.line = value,
            Field::Column => self.values.column = value,
            _ => {}
        }
        Ok(())
    }

    ///
    /// 一つの値を開始し、containerはfile stackへ積む。
    ///
    /// # 概要
    /// 再帰せず有限stateを更新し、既知fieldの型不正を拒否する。
    ///
    /// # 引数
    /// * `field` - schema field
    ///
    /// # 戻り値
    /// 値の開始またはscalar処理成功
    ///
    fn value(&mut self, field: Field) -> Result<(), FailureKind> {
        /*
         * object/arrayは再帰呼出しではなくstackで処理する
         */
        match self.lexer.whitespace()? {
            Some(b'{') => {
                self.lexer.take()?;
                let context = match field {
                    Field::Object(context) => context, Field::Unknown => 0,
                    _ => return Err(FailureKind::Protocol),
                };
                self.stack.push(Frame::new(context, false))
            }
            Some(b'[') if matches!(field, Field::Unknown) => {
                self.lexer.take()?;
                self.stack.push(Frame::new(0, true))
            }
            Some(b'"') => self.string(field),
            Some(b'-' | b'0'..=b'9') => self.number(field),
            /*
             * boolean/nullは必要型とliteralを照合する
             */
            Some(byte @ (b't' | b'f')) => {
                let value = byte == b't';
                self.lexer.literal(if value { b"true" } else { b"false" })?;
                match field {
                    Field::Deleted => self.values.deleted = Some(value),
                    Field::Locked => self.values.locked = Some(value),
                    Field::Unknown => {}
                    _ => return Err(FailureKind::Protocol),
                }
                Ok(())
            }
            Some(b'n') if matches!(field, Field::Unknown) =>
                self.lexer.literal(b"null"),
            _ => Err(FailureKind::Protocol),
        }
    }

    ///
    /// container終端で必要fieldを検査する。
    ///
    /// # 引数
    /// * `frame` - 完成したframe
    /// * `end` - 必要な終端byte
    ///
    /// # 戻り値
    /// 必須項目検査とstack復帰の成功
    ///
    fn close(&mut self, frame: Frame, end: u8) -> Result<(), FailureKind> {
        self.lexer.expect(end)?;
        let required = match frame.context {
            1 | 5 => 1, 2 | 4 => 3, 3 => 7, _ => 0,
        };
        if frame.seen & required != required {
            return Err(FailureKind::Protocol);
        }
        self.stack.pop()
    }

    ///
    /// top frameの文法を一段進める。
    ///
    /// # 概要
    /// comma/colon/終端とvalue型を照合し、depth依存のメモリを作らない。
    ///
    /// # 引数
    /// * `frame` - 現top
    ///
    /// # 戻り値
    /// 一段の処理成功
    ///
    fn step(&mut self, mut frame: Frame) -> Result<(), FailureKind> {
        let next = self.lexer.whitespace()?;
        /*
         * objectのkey・colon・value・commaを区別する
         */
        match frame.phase {
            0 if next == Some(b'}') => self.close(frame, b'}'),
            0 | 1 => {
                self.stack.top = Some(self.key(frame)?);
                Ok(())
            }
            2 => {
                self.lexer.expect(b':')?;
                frame.phase = 3;
                self.stack.top = Some(frame);
                Ok(())
            }
            3 => {
                frame.phase = 4;
                self.stack.top = Some(frame);
                self.value(field(frame.context, frame.key))
            }
            4 if next == Some(b'}') => self.close(frame, b'}'),
            4 => {
                self.lexer.expect(b',')?;
                frame.phase = 1;
                self.stack.top = Some(frame);
                Ok(())
            }
            /*
             * arrayは未知値の構造だけを検査する
             */
            5 if next == Some(b']') => self.close(frame, b']'),
            5 | 6 => {
                frame.phase = 7;
                self.stack.top = Some(frame);
                self.value(Field::Unknown)
            }
            7 if next == Some(b']') => self.close(frame, b']'),
            7 => {
                self.lexer.expect(b',')?;
                frame.phase = 6;
                self.stack.top = Some(frame);
                Ok(())
            }
            _ => Err(FailureKind::Protocol),
        }
    }

    ///
    /// JSON全体を検査してから必要値を公開する。
    ///
    /// # 概要
    /// 末尾と停止を確認し、未完成や部分抽出だけを成功として返さない。
    ///
    /// # 戻り値
    /// 完全検証後の必要値
    ///
    fn run(mut self) -> Result<JsonValue, FailureKind> {
        let root = match self.purpose {
            Purpose::Id | Purpose::CreatedId => 1,
            Purpose::Metadata { .. } => 2,
            Purpose::Error => 6,
        };
        self.value(Field::Object(root))?;
        /*
         * topを一つずつ処理し、任意depthをfileへ退避する
         */
        while let Some(frame) = self.stack.top {
            super::check(&self.stop)?;
            self.step(frame)?;
        }
        if self.lexer.whitespace()?.is_some() {
            return Err(FailureKind::Protocol);
        }
        #[cfg(test)]
        if matches!(self.purpose, Purpose::CreatedId) {
            super::tests::writes::after_json_complete(&self.stop);
        }
        // POSTは全JSON/正常EOF検証済みIDを後続停止でも失わない。
        if !matches!(self.purpose, Purpose::CreatedId) {
            super::check(&self.stop)?;
        }
        /*
         * 必要項目を有限の公開型へ写す
         */
        let missing = FailureKind::Protocol;
        let values = self.values;
        match self.purpose {
            Purpose::Id | Purpose::CreatedId =>
                Ok(JsonValue::Id(values.id.ok_or(missing)?)),
            Purpose::Metadata { id, .. } => Ok(JsonValue::Metadata(
                PageMetadata {
                    id, path_kind: values.path_kind.ok_or(missing)?,
                    relation: values.relation.ok_or(missing)?,
                    deleted: values.deleted.ok_or(missing)?,
                    locked: values.locked.ok_or(missing)?,
                    revision: values.revision.ok_or(missing)?,
                },
            )),
            Purpose::Error => Ok(JsonValue::Error(
                values.front_matter.then_some(Diagnostics {
                    kind: values.detail, line: values.line,
                    column: values.column,
                }),
            )),
        }
    }
}

///
/// contextに対応する有限key codeを求める。
///
/// # 引数
/// * `context` - schema状態
/// * `name` - 小さい復号key。巨大keyならNone
///
/// # 戻り値
/// 未知は0、必要fieldは固有code
///
fn key_code(context: u8, name: Option<&str>) -> u8 {
    match (context, name) {
        (1, Some("id")) | (2, Some("page_info")) | (3, Some("path"))
            | (4, Some("kind")) | (5, Some("revision"))
            | (6, Some("kind")) | (7, Some("type")) => 1,
        (2, Some("revision_info")) | (3, Some("deleted"))
            | (4, Some("value")) | (6, Some("detail"))
            | (7, Some("line")) => 2,
        (3, Some("locked")) | (7, Some("column")) => 3,
        _ => 0,
    }
}

///
/// 有限context/keyから値の検査責務を選ぶ。
///
/// # 引数
/// * `context` - 現object
/// * `key` - 検査済みkey code
///
/// # 戻り値
/// 必要fieldまたは未知値
///
fn field(context: u8, key: u8) -> Field {
    match (context, key) {
        (1, 1) => Field::Id, (2, 1) => Field::Object(3),
        (2, 2) => Field::Object(5), (3, 1) => Field::Object(4),
        (3, 2) => Field::Deleted, (3, 3) => Field::Locked,
        (4, 1) => Field::PathKind, (4, 2) => Field::Path,
        (5, 1) => Field::Revision, (6, 1) => Field::ErrorKind,
        (6, 2) => Field::Object(7), (7, 1) => Field::DetailType,
        (7, 2) => Field::Line, (7, 3) => Field::Column,
        _ => Field::Unknown,
    }
}

///
/// readerから全JSONを逐次検査して必要値だけを返す。
///
/// # 引数
/// * `input` - chunk readerまたは単体fixture
/// * `purpose` - 抽出目的
/// * `stop` - 共有停止境界
///
/// # 戻り値
/// 安全な抽出値
///
pub(super) fn parse(input: impl Read, purpose: Purpose, stop: StopContext)
    -> Result<JsonValue, FailureKind>
{
    Parser::new(input, purpose, stop)?.run()
}

#[cfg(test)]
mod tests;
