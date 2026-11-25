//! Encoding result tokens and decoding RPC parameters.

use std::io;

use chrono::{Datelike, NaiveDate, NaiveTime, Timelike};
use rust_decimal::Decimal;

use crate::exec::{ColumnMeta, SqlError};
use crate::value::{days_from_ce, guid_from_wire, guid_to_wire, round_datetime, SqlType, Value};

/// `SQL_Latin1_General_CP1_CI_AS`.
pub const COLLATION: [u8; 5] = [0x09, 0x04, 0xD0, 0x00, 0x34];

pub const TOKEN_COLMETADATA: u8 = 0x81;
pub const TOKEN_ROW: u8 = 0xD1;
pub const TOKEN_DONE: u8 = 0xFD;
pub const TOKEN_DONEPROC: u8 = 0xFE;
pub const TOKEN_DONEINPROC: u8 = 0xFF;
pub const TOKEN_ENVCHANGE: u8 = 0xE3;
pub const TOKEN_INFO: u8 = 0xAB;
pub const TOKEN_ERROR: u8 = 0xAA;
pub const TOKEN_LOGINACK: u8 = 0xAD;
pub const TOKEN_RETURNSTATUS: u8 = 0x79;
pub const TOKEN_RETURNVALUE: u8 = 0xAC;

pub const DONE_MORE: u16 = 0x01;
pub const DONE_ERROR: u16 = 0x02;
pub const DONE_INXACT: u16 = 0x04;
pub const DONE_COUNT: u16 = 0x10;
pub const DONE_ATTN: u16 = 0x20;

pub const SERVER_NAME: &str = "azure-emulator";

/// Builds a token stream.
#[derive(Default)]
pub struct Tokens {
    pub buf: Vec<u8>,
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

impl Tokens {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// A string with a one-byte length in characters.
    fn b_varchar(&mut self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().take(255).collect();
        self.u8(units.len() as u8);
        for u in units {
            self.u16(u);
        }
    }
    /// A string with a two-byte length in characters.
    fn us_varchar(&mut self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().take(65535).collect();
        self.u16(units.len() as u16);
        for u in units {
            self.u16(u);
        }
    }

    /// A token with a two-byte length prefix, written by `body`.
    fn with_length(&mut self, token: u8, body: impl FnOnce(&mut Tokens)) {
        let mut inner = Tokens::default();
        body(&mut inner);
        self.u8(token);
        self.u16(inner.buf.len() as u16);
        self.buf.extend_from_slice(&inner.buf);
    }

    pub fn env_change_text(&mut self, kind: u8, new: &str, old: &str) {
        self.with_length(TOKEN_ENVCHANGE, |t| {
            t.u8(kind);
            t.b_varchar(new);
            t.b_varchar(old);
        });
    }

    pub fn env_change_bytes(&mut self, kind: u8, new: &[u8], old: &[u8]) {
        self.with_length(TOKEN_ENVCHANGE, |t| {
            t.u8(kind);
            t.u8(new.len() as u8);
            t.buf.extend_from_slice(new);
            t.u8(old.len() as u8);
            t.buf.extend_from_slice(old);
        });
    }

    /// An ERROR token, or an INFO token when `error` is false.
    pub fn message(&mut self, error: bool, e: &SqlError) {
        self.with_length(if error { TOKEN_ERROR } else { TOKEN_INFO }, |t| {
            t.u32(e.number as u32);
            t.u8(e.state);
            t.u8(e.class);
            t.us_varchar(&e.message);
            t.b_varchar(SERVER_NAME);
            t.b_varchar(&e.procedure);
            t.u32(e.line);
        });
    }

    pub fn error(&mut self, e: &SqlError) {
        self.message(true, e);
    }

    pub fn login_ack(&mut self, tds_version: u32) {
        self.with_length(TOKEN_LOGINACK, |t| {
            t.u8(1);
            t.buf.extend_from_slice(&tds_version.to_be_bytes());
            t.b_varchar("Microsoft SQL Server");
            t.buf.extend_from_slice(&[16, 0, 0x03, 0xE8]);
        });
    }

    pub fn done(&mut self, token: u8, status: u16, count: u64) {
        self.u8(token);
        self.u16(status);
        self.u16(0xC1);
        self.u64(count);
    }

    pub fn return_status(&mut self, status: i32) {
        self.u8(TOKEN_RETURNSTATUS);
        self.u32(status as u32);
    }

    pub fn return_value(&mut self, ordinal: usize, name: &str, ty: &SqlType, value: &Value) {
        let ty = widen(ty, std::slice::from_ref(value));
        self.u8(TOKEN_RETURNVALUE);
        self.u16(ordinal as u16);
        let name = if name.starts_with('@') { name.to_string() } else { format!("@{name}") };
        self.b_varchar(&name);
        self.u8(0x01);
        self.u32(0);
        self.u16(0x0001 | 0x0008);
        self.type_info(&ty);
        self.value(&ty, value);
    }

    pub fn result_set(&mut self, columns: &[ColumnMeta], rows: &[Vec<Value>]) {
        let types: Vec<SqlType> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let values: Vec<Value> = rows.iter().filter_map(|r| r.get(i).cloned()).collect();
                widen(&c.ty, &values)
            })
            .collect();
        self.u8(TOKEN_COLMETADATA);
        self.u16(columns.len() as u16);
        for (column, ty) in columns.iter().zip(&types) {
            self.u32(0);
            let mut flags = 0u16;
            if column.nullable {
                flags |= 0x0001;
            }
            if column.identity {
                flags |= 0x0010;
            } else {
                flags |= 0x0008;
            }
            self.u16(flags);
            self.type_info(ty);
            self.b_varchar(&column.name);
        }
        for row in rows {
            self.u8(TOKEN_ROW);
            for (i, ty) in types.iter().enumerate() {
                self.value(ty, row.get(i).unwrap_or(&Value::Null));
            }
        }
    }

    fn type_info(&mut self, ty: &SqlType) {
        let length = |n: Option<u16>, unit: u16| n.map_or(0xFFFF, |n| n.saturating_mul(unit));
        match ty {
            SqlType::Bit => {
                self.u8(0x68);
                self.u8(1);
            }
            SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt => {
                self.u8(0x26);
                self.u8(int_width(ty) as u8);
            }
            SqlType::Decimal(p, s) => {
                self.u8(0x6A);
                self.u8(decimal_width(*p) as u8 + 1);
                self.u8(*p);
                self.u8(*s);
            }
            SqlType::Money | SqlType::SmallMoney => {
                self.u8(0x6A);
                self.u8(17);
                self.u8(19);
                self.u8(4);
            }
            SqlType::Float | SqlType::Real => {
                self.u8(0x6D);
                self.u8(if *ty == SqlType::Real { 4 } else { 8 });
            }
            SqlType::Char(n) | SqlType::VarChar(Some(n)) => {
                self.u8(0xA7);
                self.u16(*n);
                self.buf.extend_from_slice(&COLLATION);
            }
            SqlType::VarChar(None) | SqlType::Text => {
                self.u8(0xA7);
                self.u16(0xFFFF);
                self.buf.extend_from_slice(&COLLATION);
            }
            SqlType::NChar(n) => {
                self.u8(0xE7);
                self.u16(length(Some(*n), 2));
                self.buf.extend_from_slice(&COLLATION);
            }
            SqlType::NVarChar(n) => {
                self.u8(0xE7);
                self.u16(length(*n, 2));
                self.buf.extend_from_slice(&COLLATION);
            }
            SqlType::NText | SqlType::Xml => {
                self.u8(0xE7);
                self.u16(0xFFFF);
                self.buf.extend_from_slice(&COLLATION);
            }
            SqlType::Binary(n) => {
                self.u8(0xA5);
                self.u16(*n);
            }
            SqlType::VarBinary(n) => {
                self.u8(0xA5);
                self.u16(length(*n, 1));
            }
            SqlType::UniqueIdentifier => {
                self.u8(0x24);
                self.u8(16);
            }
            SqlType::Date => self.u8(0x28),
            SqlType::Time(s) => {
                self.u8(0x29);
                self.u8(*s);
            }
            SqlType::DateTime | SqlType::SmallDateTime => {
                self.u8(0x6F);
                self.u8(8);
            }
            SqlType::DateTime2(s) => {
                self.u8(0x2A);
                self.u8(*s);
            }
            SqlType::DateTimeOffset(s) => {
                self.u8(0x2B);
                self.u8(*s);
            }
        }
    }

    fn plp(&mut self, data: Option<&[u8]>) {
        match data {
            None => self.u64(u64::MAX),
            Some(bytes) => {
                self.u64(bytes.len() as u64);
                for chunk in bytes.chunks(8000) {
                    self.u32(chunk.len() as u32);
                    self.buf.extend_from_slice(chunk);
                }
                self.u32(0);
            }
        }
    }

    fn value(&mut self, ty: &SqlType, value: &Value) {
        let value = match value {
            Value::Null => Value::Null,
            v => v.convert(ty).unwrap_or(Value::Null),
        };
        let is_plp = matches!(ty, SqlType::VarChar(None) | SqlType::NVarChar(None) | SqlType::VarBinary(None) | SqlType::Text | SqlType::NText | SqlType::Xml);
        match ty {
            t if t.is_string() => {
                let bytes = value.as_string().map(|s| if t.is_unicode() || matches!(t, SqlType::Xml | SqlType::NText) { utf16(&s) } else { latin1(&s) });
                if is_plp {
                    self.plp(bytes.as_deref());
                } else {
                    match bytes {
                        None => self.u16(0xFFFF),
                        Some(b) => {
                            self.u16(b.len() as u16);
                            self.buf.extend_from_slice(&b);
                        }
                    }
                }
            }
            SqlType::Binary(_) | SqlType::VarBinary(_) => {
                let bytes = match &value {
                    Value::Bytes(b) => Some(b.clone()),
                    _ => None,
                };
                if is_plp {
                    self.plp(bytes.as_deref());
                } else {
                    match bytes {
                        None => self.u16(0xFFFF),
                        Some(b) => {
                            self.u16(b.len() as u16);
                            self.buf.extend_from_slice(&b);
                        }
                    }
                }
            }
            _ if value.is_null() => self.u8(0),
            SqlType::Bit => {
                self.u8(1);
                self.u8(matches!(value, Value::Bit(true)) as u8);
            }
            SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::BigInt => {
                let width = int_width(ty);
                let n = match value {
                    Value::Int(i) => i,
                    _ => 0,
                };
                self.u8(width as u8);
                self.buf.extend_from_slice(&n.to_le_bytes()[..width]);
            }
            SqlType::Decimal(..) | SqlType::Money | SqlType::SmallMoney => {
                let (precision, scale) = match ty {
                    SqlType::Decimal(p, s) => (*p, *s),
                    _ => (19, 4),
                };
                let mut d = match value {
                    Value::Decimal(d) => d,
                    _ => Decimal::ZERO,
                };
                d.rescale(scale as u32);
                let width = decimal_width(precision);
                self.u8(width as u8 + 1);
                self.u8(if d.is_sign_negative() { 0 } else { 1 });
                let magnitude = d.mantissa().unsigned_abs();
                self.buf.extend_from_slice(&magnitude.to_le_bytes()[..width]);
            }
            SqlType::Float | SqlType::Real => {
                let f = match value {
                    Value::Float(f) => f,
                    _ => 0.0,
                };
                if *ty == SqlType::Real {
                    self.u8(4);
                    self.buf.extend_from_slice(&(f as f32).to_le_bytes());
                } else {
                    self.u8(8);
                    self.buf.extend_from_slice(&f.to_le_bytes());
                }
            }
            SqlType::UniqueIdentifier => {
                if let Value::Guid(g) = value {
                    self.u8(16);
                    self.buf.extend_from_slice(&guid_to_wire(&g));
                } else {
                    self.u8(0);
                }
            }
            SqlType::Date => {
                if let Value::Date(d) = value {
                    self.u8(3);
                    self.buf.extend_from_slice(&(days_from_ce(&d) as u32).to_le_bytes()[..3]);
                } else {
                    self.u8(0);
                }
            }
            SqlType::Time(scale) => {
                if let Value::Time(t) = value {
                    let width = time_width(*scale);
                    self.u8(width as u8);
                    self.buf.extend_from_slice(&time_units(&t, *scale).to_le_bytes()[..width]);
                } else {
                    self.u8(0);
                }
            }
            SqlType::DateTime | SqlType::SmallDateTime => {
                if let Value::DateTime(dt) = value {
                    let dt = round_datetime(dt);
                    let days = (dt.date() - crate::value::base_1900()).num_days() as i32;
                    let millis = dt.time().num_seconds_from_midnight() as u64 * 1000 + dt.time().nanosecond() as u64 / 1_000_000;
                    let ticks = ((millis * 300 + 500) / 1000) as u32;
                    self.u8(8);
                    self.buf.extend_from_slice(&days.to_le_bytes());
                    self.buf.extend_from_slice(&ticks.to_le_bytes());
                } else {
                    self.u8(0);
                }
            }
            SqlType::DateTime2(scale) => {
                if let Value::DateTime(dt) = value {
                    let width = time_width(*scale);
                    self.u8(width as u8 + 3);
                    self.buf.extend_from_slice(&time_units(&dt.time(), *scale).to_le_bytes()[..width]);
                    self.buf.extend_from_slice(&(days_from_ce(&dt.date()) as u32).to_le_bytes()[..3]);
                } else {
                    self.u8(0);
                }
            }
            SqlType::DateTimeOffset(scale) => {
                if let Value::DateTimeOffset(dt) = value {
                    let width = time_width(*scale);
                    let utc = dt.naive_utc();
                    self.u8(width as u8 + 5);
                    self.buf.extend_from_slice(&time_units(&utc.time(), *scale).to_le_bytes()[..width]);
                    self.buf.extend_from_slice(&(days_from_ce(&utc.date()) as u32).to_le_bytes()[..3]);
                    self.buf.extend_from_slice(&((dt.offset().local_minus_utc() / 60) as i16).to_le_bytes());
                } else {
                    self.u8(0);
                }
            }
            _ => self.u8(0),
        }
    }
}

fn latin1(s: &str) -> Vec<u8> {
    s.chars().map(|c| if (c as u32) < 256 { c as u32 as u8 } else { b'?' }).collect()
}

fn int_width(ty: &SqlType) -> usize {
    match ty {
        SqlType::TinyInt => 1,
        SqlType::SmallInt => 2,
        SqlType::Int => 4,
        _ => 8,
    }
}

fn decimal_width(precision: u8) -> usize {
    match precision {
        0..=9 => 4,
        10..=19 => 8,
        20..=28 => 12,
        _ => 16,
    }
}

fn time_width(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

fn time_units(t: &NaiveTime, scale: u8) -> u64 {
    let nanos = t.num_seconds_from_midnight() as u64 * 1_000_000_000 + (t.nanosecond() % 1_000_000_000) as u64;
    nanos / 10u64.pow(9 - scale.min(7) as u32)
}

/// A column's type, widened so every value fits its declared length.
fn widen(ty: &SqlType, values: &[Value]) -> SqlType {
    let longest = || values.iter().filter_map(|v| v.as_string()).map(|s| s.chars().count()).max().unwrap_or(0);
    match ty {
        SqlType::VarChar(Some(n)) | SqlType::Char(n) if longest() > *n as usize => {
            if longest() <= 8000 { SqlType::VarChar(Some(longest() as u16)) } else { SqlType::VarChar(None) }
        }
        SqlType::NVarChar(Some(n)) | SqlType::NChar(n) if longest() > *n as usize => {
            if longest() <= 4000 { SqlType::NVarChar(Some(longest() as u16)) } else { SqlType::NVarChar(None) }
        }
        SqlType::VarChar(Some(n)) | SqlType::Char(n) if values.iter().any(|v| v.as_string().is_some_and(|s| s.chars().any(|c| c as u32 > 255))) => {
            SqlType::NVarChar(if *n <= 4000 { Some(*n) } else { None })
        }
        SqlType::VarBinary(Some(n)) | SqlType::Binary(n) => {
            let longest = values.iter().map(|v| if let Value::Bytes(b) = v { b.len() } else { 0 }).max().unwrap_or(0);
            if longest > *n as usize {
                if longest <= 8000 { SqlType::VarBinary(Some(longest as u16)) } else { SqlType::VarBinary(None) }
            } else {
                ty.clone()
            }
        }
        other => other.clone(),
    }
}

// ----- reading -----

pub struct Reader<'a> {
    data: &'a [u8],
    pub pos: usize,
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "TDS message ended early")
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(short)?;
        let slice = self.data.get(self.pos..end).ok_or_else(short)?;
        self.pos = end;
        Ok(slice)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> io::Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    pub fn utf16(&mut self, chars: usize) -> io::Result<String> {
        let b = self.bytes(chars * 2)?;
        let units: Vec<u16> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(String::from_utf16_lossy(&units))
    }
    pub fn b_varchar(&mut self) -> io::Result<String> {
        let n = self.u8()? as usize;
        self.utf16(n)
    }
    /// Skips `ALL_HEADERS` at the start of a request.
    pub fn skip_all_headers(&mut self) -> io::Result<()> {
        if self.remaining() < 4 {
            return Ok(());
        }
        let total = u32::from_le_bytes([self.data[self.pos], self.data[self.pos + 1], self.data[self.pos + 2], self.data[self.pos + 3]]) as usize;
        // Only TDS 7.2+ clients send it; a total under 4 cannot be one.
        if total >= 4 && total <= self.remaining() {
            self.bytes(total)?;
        }
        Ok(())
    }
}

/// A parameter's wire type.
pub struct WireType {
    id: u8,
    length: usize,
    precision: u8,
    scale: u8,
    plp: bool,
    unicode: bool,
}

impl WireType {
    /// The SQL type a parameter of this wire type declares.
    pub fn sql_type(&self) -> Option<SqlType> {
        let n = |unit: usize| u16::try_from(self.length / unit).ok().filter(|n| *n > 0);
        Some(match self.id {
            0x30 => SqlType::TinyInt,
            0x34 => SqlType::SmallInt,
            0x38 => SqlType::Int,
            0x7F => SqlType::BigInt,
            0x26 => match self.length {
                1 => SqlType::TinyInt,
                2 => SqlType::SmallInt,
                4 => SqlType::Int,
                _ => SqlType::BigInt,
            },
            0x32 | 0x68 => SqlType::Bit,
            0x3B => SqlType::Real,
            0x3E => SqlType::Float,
            0x6D => {
                if self.length == 4 {
                    SqlType::Real
                } else {
                    SqlType::Float
                }
            }
            0x6A | 0x6C => SqlType::Decimal(self.precision, self.scale),
            0x3C | 0x7A | 0x6E => SqlType::Money,
            0x24 => SqlType::UniqueIdentifier,
            0x28 => SqlType::Date,
            0x29 => SqlType::Time(self.scale),
            0x2A => SqlType::DateTime2(self.scale),
            0x2B => SqlType::DateTimeOffset(self.scale),
            0x3D | 0x6F | 0x3A => SqlType::DateTime,
            0xA7 | 0xAF | 0x23 => {
                if self.plp || self.id == 0x23 {
                    SqlType::VarChar(None)
                } else {
                    SqlType::VarChar(n(1))
                }
            }
            0xE7 | 0xEF | 0x63 | 0xF1 => {
                if self.plp || self.id != 0xE7 && self.id != 0xEF {
                    SqlType::NVarChar(None)
                } else {
                    SqlType::NVarChar(n(2))
                }
            }
            0xA5 | 0xAD | 0x22 => {
                if self.plp || self.id == 0x22 {
                    SqlType::VarBinary(None)
                } else {
                    SqlType::VarBinary(n(1))
                }
            }
            _ => return None,
        })
    }
}

fn unsupported_type(id: u8) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("unsupported parameter type 0x{id:02X}"))
}

pub fn read_type_info(r: &mut Reader) -> io::Result<WireType> {
    let id = r.u8()?;
    let mut t = WireType { id, length: 0, precision: 0, scale: 0, plp: false, unicode: false };
    match id {
        0x1F => {}
        0x30 | 0x32 => t.length = 1,
        0x34 => t.length = 2,
        0x38 | 0x3B | 0x3A | 0x7A => t.length = 4,
        0x7F | 0x3E | 0x3D | 0x3C => t.length = 8,
        0x24 | 0x26 | 0x68 | 0x6D | 0x6E | 0x6F => t.length = r.u8()? as usize,
        0x6A | 0x6C => {
            t.length = r.u8()? as usize;
            t.precision = r.u8()?;
            t.scale = r.u8()?;
        }
        0x28 => {}
        0x29..=0x2B => t.scale = r.u8()?,
        0xA7 | 0xAF | 0xE7 | 0xEF => {
            t.length = r.u16()? as usize;
            t.plp = t.length == 0xFFFF;
            r.bytes(5)?;
            t.unicode = id == 0xE7 || id == 0xEF;
        }
        0xA5 | 0xAD => {
            t.length = r.u16()? as usize;
            t.plp = t.length == 0xFFFF;
        }
        0x23 | 0x63 => {
            t.length = r.u32()? as usize;
            r.bytes(5)?;
            t.unicode = id == 0x63;
        }
        0x22 => t.length = r.u32()? as usize,
        0xF1 => {
            let schema = r.u8()?;
            if schema != 0 {
                let n = r.u8()? as usize;
                r.utf16(n)?;
                let n = r.u8()? as usize;
                r.utf16(n)?;
                let n = r.u16()? as usize;
                r.utf16(n)?;
            }
            t.plp = true;
            t.unicode = true;
        }
        other => return Err(unsupported_type(other)),
    }
    Ok(t)
}

fn read_plp(r: &mut Reader) -> io::Result<Option<Vec<u8>>> {
    let total = r.u64()?;
    if total == u64::MAX {
        return Ok(None);
    }
    let mut data = Vec::new();
    loop {
        let n = r.u32()? as usize;
        if n == 0 {
            break;
        }
        data.extend_from_slice(r.bytes(n)?);
    }
    Ok(Some(data))
}

fn decode_text(bytes: &[u8], unicode: bool) -> Value {
    if unicode {
        let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
        Value::Str(String::from_utf16_lossy(&units))
    } else {
        Value::Str(bytes.iter().map(|&b| b as char).collect())
    }
}

fn le_int(bytes: &[u8]) -> i64 {
    let mut a = [0u8; 8];
    a[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
    let n = i64::from_le_bytes(a);
    // Sign-extend widths under eight bytes.
    let bits = bytes.len() * 8;
    if bits < 64 && bits > 0 {
        (n << (64 - bits)) >> (64 - bits)
    } else {
        n
    }
}

fn date_from(bytes: &[u8]) -> Option<NaiveDate> {
    let days = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as i32;
    NaiveDate::from_num_days_from_ce_opt(days + 1)
}

fn time_from(bytes: &[u8], scale: u8) -> Option<NaiveTime> {
    let mut a = [0u8; 8];
    a[..bytes.len()].copy_from_slice(bytes);
    let units = u64::from_le_bytes(a);
    let nanos = units * 10u64.pow(9 - scale.min(7) as u32);
    NaiveTime::from_num_seconds_from_midnight_opt((nanos / 1_000_000_000) as u32, (nanos % 1_000_000_000) as u32)
}

pub fn read_value(r: &mut Reader, t: &WireType) -> io::Result<Value> {
    if t.plp {
        return Ok(match read_plp(r)? {
            None => Value::Null,
            Some(bytes) => match t.id {
                0xA5 | 0xAD => Value::Bytes(bytes),
                _ => decode_text(&bytes, t.unicode),
            },
        });
    }
    match t.id {
        0x1F => Ok(Value::Null),
        0x30 => Ok(Value::Int(r.u8()? as i64)),
        0x32 => Ok(Value::Bit(r.u8()? != 0)),
        0x34 | 0x38 | 0x7F => Ok(Value::Int(le_int(r.bytes(t.length)?))),
        0x3B => Ok(Value::Float(f32::from_le_bytes(r.bytes(4)?.try_into().map_err(|_| short())?) as f64)),
        0x3E => Ok(Value::Float(f64::from_le_bytes(r.bytes(8)?.try_into().map_err(|_| short())?))),
        0x23 | 0x63 | 0x22 => {
            // Text pointer length, then (if not null) the pointer, a timestamp and the data.
            let pointer = r.u8()? as usize;
            if pointer == 0 {
                return Ok(Value::Null);
            }
            r.bytes(pointer + 8)?;
            let n = r.u32()? as usize;
            let bytes = r.bytes(n)?;
            Ok(if t.id == 0x22 { Value::Bytes(bytes.to_vec()) } else { decode_text(bytes, t.unicode) })
        }
        0xA7 | 0xAF | 0xE7 | 0xEF | 0xA5 | 0xAD => {
            let n = r.u16()?;
            if n == 0xFFFF {
                return Ok(Value::Null);
            }
            let bytes = r.bytes(n as usize)?;
            Ok(match t.id {
                0xA5 | 0xAD => Value::Bytes(bytes.to_vec()),
                _ => decode_text(bytes, t.unicode),
            })
        }
        0x3D | 0x3C | 0x3A | 0x7A => {
            let bytes = r.bytes(t.length)?.to_vec();
            fixed_or_n(t.id, &bytes)
        }
        _ => {
            let n = r.u8()? as usize;
            if n == 0 {
                return Ok(Value::Null);
            }
            let bytes = r.bytes(n)?.to_vec();
            match t.id {
                0x26 => Ok(Value::Int(le_int(&bytes))),
                0x68 => Ok(Value::Bit(bytes[0] != 0)),
                0x6D => Ok(Value::Float(if n == 4 {
                    f32::from_le_bytes(bytes[..4].try_into().map_err(|_| short())?) as f64
                } else {
                    f64::from_le_bytes(bytes[..8].try_into().map_err(|_| short())?)
                })),
                0x6A | 0x6C => {
                    let mut magnitude = [0u8; 16];
                    magnitude[..n - 1].copy_from_slice(&bytes[1..]);
                    let mut m = i128::from_le_bytes(magnitude);
                    if bytes[0] == 0 {
                        m = -m;
                    }
                    Decimal::try_from_i128_with_scale(m, t.scale as u32).map(Value::Decimal).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "decimal parameter out of range"))
                }
                0x24 => {
                    let mut wire = [0u8; 16];
                    wire.copy_from_slice(&bytes[..16]);
                    Ok(Value::Guid(guid_from_wire(&wire)))
                }
                0x6E | 0x6F => fixed_or_n(if t.id == 0x6E { if n == 4 { 0x7A } else { 0x3C } } else if n == 4 { 0x3A } else { 0x3D }, &bytes),
                0x28 => Ok(date_from(&bytes).map_or(Value::Null, Value::Date)),
                0x29 => Ok(time_from(&bytes, t.scale).map_or(Value::Null, Value::Time)),
                0x2A => {
                    let (time, date) = bytes.split_at(n - 3);
                    Ok(match (date_from(date), time_from(time, t.scale)) {
                        (Some(d), Some(t)) => Value::DateTime(d.and_time(t)),
                        _ => Value::Null,
                    })
                }
                0x2B => {
                    let (rest, offset) = bytes.split_at(n - 2);
                    let (time, date) = rest.split_at(rest.len() - 3);
                    let minutes = i16::from_le_bytes([offset[0], offset[1]]) as i32;
                    Ok(match (date_from(date), time_from(time, t.scale), chrono::FixedOffset::east_opt(minutes * 60)) {
                        (Some(d), Some(tm), Some(zone)) => Value::DateTimeOffset(d.and_time(tm).and_utc().with_timezone(&zone)),
                        _ => Value::Null,
                    })
                }
                other => Err(unsupported_type(other)),
            }
        }
    }
}

fn fixed_or_n(id: u8, bytes: &[u8]) -> io::Result<Value> {
    match id {
        0x3D => {
            let days = i32::from_le_bytes(bytes[..4].try_into().map_err(|_| short())?);
            let ticks = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| short())?);
            let base = crate::value::base_1900().and_time(NaiveTime::MIN);
            Ok(Value::DateTime(base + chrono::Duration::days(days as i64) + chrono::Duration::milliseconds((ticks as i64 * 1000 + 150) / 300)))
        }
        0x3A => {
            let days = u16::from_le_bytes([bytes[0], bytes[1]]);
            let minutes = u16::from_le_bytes([bytes[2], bytes[3]]);
            let base = crate::value::base_1900().and_time(NaiveTime::MIN);
            Ok(Value::DateTime(base + chrono::Duration::days(days as i64) + chrono::Duration::minutes(minutes as i64)))
        }
        0x3C => {
            let high = i32::from_le_bytes(bytes[..4].try_into().map_err(|_| short())?) as i64;
            let low = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| short())?) as i64;
            Ok(Value::Decimal(Decimal::new((high << 32) | low, 4)))
        }
        _ => {
            let n = i32::from_le_bytes(bytes[..4].try_into().map_err(|_| short())?) as i64;
            Ok(Value::Decimal(Decimal::new(n, 4)))
        }
    }
}

#[allow(dead_code)]
fn _year(d: &NaiveDate) -> i32 {
    d.year()
}
