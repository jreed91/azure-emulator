//! T-SQL data types and values, and the conversions between them.

use std::cmp::Ordering;
use std::fmt;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::exec::SqlError;

/// A declared type. String and binary lengths are `None` for `MAX`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SqlType {
    Bit,
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    Decimal(u8, u8),
    Money,
    SmallMoney,
    Float,
    Real,
    Char(u16),
    VarChar(Option<u16>),
    NChar(u16),
    NVarChar(Option<u16>),
    Text,
    NText,
    Xml,
    Binary(u16),
    VarBinary(Option<u16>),
    UniqueIdentifier,
    Date,
    Time(u8),
    DateTime,
    SmallDateTime,
    DateTime2(u8),
    DateTimeOffset(u8),
}

impl SqlType {
    /// The type's name as `sys.types` and error messages give it.
    pub fn name(&self) -> &'static str {
        match self {
            SqlType::Bit => "bit",
            SqlType::TinyInt => "tinyint",
            SqlType::SmallInt => "smallint",
            SqlType::Int => "int",
            SqlType::BigInt => "bigint",
            SqlType::Decimal(..) => "decimal",
            SqlType::Money => "money",
            SqlType::SmallMoney => "smallmoney",
            SqlType::Float => "float",
            SqlType::Real => "real",
            SqlType::Char(_) => "char",
            SqlType::VarChar(_) => "varchar",
            SqlType::NChar(_) => "nchar",
            SqlType::NVarChar(_) => "nvarchar",
            SqlType::Text => "text",
            SqlType::NText => "ntext",
            SqlType::Xml => "xml",
            SqlType::Binary(_) => "binary",
            SqlType::VarBinary(_) => "varbinary",
            SqlType::UniqueIdentifier => "uniqueidentifier",
            SqlType::Date => "date",
            SqlType::Time(_) => "time",
            SqlType::DateTime => "datetime",
            SqlType::SmallDateTime => "smalldatetime",
            SqlType::DateTime2(_) => "datetime2",
            SqlType::DateTimeOffset(_) => "datetimeoffset",
        }
    }

    /// The type as it is written in T-SQL, with its length or precision.
    pub fn display(&self) -> String {
        let length = |n: &Option<u16>| n.map_or("max".to_string(), |n| n.to_string());
        match self {
            SqlType::Decimal(p, s) => format!("decimal({p},{s})"),
            SqlType::Char(n) | SqlType::NChar(n) | SqlType::Binary(n) => format!("{}({n})", self.name()),
            SqlType::VarChar(n) | SqlType::NVarChar(n) | SqlType::VarBinary(n) => format!("{}({})", self.name(), length(n)),
            SqlType::Time(s) | SqlType::DateTime2(s) | SqlType::DateTimeOffset(s) => format!("{}({s})", self.name()),
            _ => self.name().to_string(),
        }
    }

    pub fn is_string(&self) -> bool {
        matches!(
            self,
            SqlType::Char(_) | SqlType::VarChar(_) | SqlType::NChar(_) | SqlType::NVarChar(_) | SqlType::Text | SqlType::NText | SqlType::Xml
        )
    }

    pub fn is_unicode(&self) -> bool {
        matches!(self, SqlType::NChar(_) | SqlType::NVarChar(_) | SqlType::NText | SqlType::Xml)
    }

    /// The most characters (or bytes) a value of this type holds, if limited.
    pub fn max_length(&self) -> Option<usize> {
        match self {
            SqlType::Char(n) | SqlType::NChar(n) | SqlType::Binary(n) => Some(*n as usize),
            SqlType::VarChar(n) | SqlType::NVarChar(n) | SqlType::VarBinary(n) => n.map(|n| n as usize),
            _ => None,
        }
    }

    /// Bytes a column of this type takes, as `COL_LENGTH` and `max_length` report.
    pub fn storage_length(&self) -> i64 {
        match self {
            SqlType::Bit | SqlType::TinyInt => 1,
            SqlType::SmallInt => 2,
            SqlType::Int | SqlType::Real | SqlType::SmallMoney | SqlType::SmallDateTime => 4,
            SqlType::BigInt | SqlType::Float | SqlType::Money | SqlType::DateTime => 8,
            SqlType::Decimal(p, _) => match p {
                1..=9 => 5,
                10..=19 => 9,
                20..=28 => 13,
                _ => 17,
            },
            SqlType::Char(n) | SqlType::Binary(n) => *n as i64,
            SqlType::NChar(n) => *n as i64 * 2,
            SqlType::VarChar(n) | SqlType::VarBinary(n) => n.map_or(-1, |n| n as i64),
            SqlType::NVarChar(n) => n.map_or(-1, |n| n as i64 * 2),
            SqlType::Text | SqlType::NText | SqlType::Xml => -1,
            SqlType::UniqueIdentifier => 16,
            SqlType::Date => 3,
            SqlType::Time(_) => 5,
            SqlType::DateTime2(_) => 8,
            SqlType::DateTimeOffset(_) => 10,
        }
    }

    /// Data type precedence: in an operation on two types, the lower one is
    /// converted to the higher.
    pub fn precedence(&self) -> u8 {
        match self {
            SqlType::DateTimeOffset(_) => 30,
            SqlType::DateTime2(_) => 29,
            SqlType::DateTime => 28,
            SqlType::SmallDateTime => 27,
            SqlType::Date => 26,
            SqlType::Time(_) => 25,
            SqlType::Float => 24,
            SqlType::Real => 23,
            SqlType::Decimal(..) => 22,
            SqlType::Money => 21,
            SqlType::SmallMoney => 20,
            SqlType::BigInt => 19,
            SqlType::Int => 18,
            SqlType::SmallInt => 17,
            SqlType::TinyInt => 16,
            SqlType::Bit => 15,
            SqlType::NText => 14,
            SqlType::Text => 13,
            SqlType::Xml => 12,
            SqlType::UniqueIdentifier => 11,
            SqlType::NVarChar(_) => 10,
            SqlType::NChar(_) => 9,
            SqlType::VarChar(_) => 8,
            SqlType::Char(_) => 7,
            SqlType::VarBinary(_) => 6,
            SqlType::Binary(_) => 5,
        }
    }

    /// Parses a type name and its arguments (`None` for `MAX`).
    pub fn from_name(name: &str, args: &[Option<u32>]) -> Option<SqlType> {
        let arg = |i: usize| args.get(i).copied();
        let length = |default: u16| -> Option<u16> {
            match arg(0) {
                None => Some(default),
                Some(Some(n)) => u16::try_from(n).ok(),
                Some(None) => None,
            }
        };
        let fixed = |default: u16| -> Option<u16> {
            match arg(0) {
                None => Some(default),
                Some(Some(n)) => u16::try_from(n).ok(),
                Some(None) => None,
            }
        };
        let scale = |default: u8| -> u8 { arg(0).flatten().map_or(default, |s| s.min(7) as u8) };
        Some(match name.to_ascii_lowercase().as_str() {
            "bit" => SqlType::Bit,
            "tinyint" => SqlType::TinyInt,
            "smallint" => SqlType::SmallInt,
            "int" | "integer" => SqlType::Int,
            "bigint" => SqlType::BigInt,
            "decimal" | "numeric" | "dec" => {
                let p = arg(0).flatten().unwrap_or(18).clamp(1, 38) as u8;
                let s = arg(1).flatten().unwrap_or(0).min(p as u32) as u8;
                SqlType::Decimal(p, s)
            }
            "money" => SqlType::Money,
            "smallmoney" => SqlType::SmallMoney,
            "float" => match arg(0).flatten() {
                Some(n) if n <= 24 => SqlType::Real,
                _ => SqlType::Float,
            },
            "real" => SqlType::Real,
            "char" | "character" => SqlType::Char(fixed(1)?),
            "varchar" => SqlType::VarChar(length(1)),
            "nchar" => SqlType::NChar(fixed(1)?),
            "nvarchar" => SqlType::NVarChar(length(1)),
            "sysname" => SqlType::NVarChar(Some(128)),
            "text" => SqlType::Text,
            "ntext" => SqlType::NText,
            "xml" => SqlType::Xml,
            "binary" => SqlType::Binary(fixed(1)?),
            "varbinary" => SqlType::VarBinary(length(1)),
            "image" => SqlType::VarBinary(None),
            "uniqueidentifier" => SqlType::UniqueIdentifier,
            "date" => SqlType::Date,
            "time" => SqlType::Time(scale(7)),
            "datetime" => SqlType::DateTime,
            "smalldatetime" => SqlType::SmallDateTime,
            "datetime2" => SqlType::DateTime2(scale(7)),
            "datetimeoffset" => SqlType::DateTimeOffset(scale(7)),
            _ => return None,
        })
    }
}

/// One value. The declared type travels separately where it matters.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Value {
    Null,
    Bit(bool),
    Int(i64),
    Decimal(Decimal),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Guid(Uuid),
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(NaiveDateTime),
    DateTimeOffset(DateTime<FixedOffset>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

fn conversion_failed(value: &Value, target: &SqlType) -> SqlError {
    let (number, message) = match (value, target) {
        (Value::Str(_), SqlType::Date | SqlType::DateTime | SqlType::DateTime2(_) | SqlType::SmallDateTime | SqlType::DateTimeOffset(_) | SqlType::Time(_)) => {
            (241, "Conversion failed when converting date and/or time from character string.".to_string())
        }
        (Value::Str(s), SqlType::UniqueIdentifier) => {
            let _ = s;
            (8169, "Conversion failed when converting from a character string to uniqueidentifier.".to_string())
        }
        (Value::Str(_), SqlType::Decimal(..) | SqlType::Float | SqlType::Real | SqlType::Money | SqlType::SmallMoney) => {
            (8114, format!("Error converting data type varchar to {}.", if matches!(target, SqlType::Decimal(..)) { "numeric" } else { target.name() }))
        }
        (Value::Str(s), _) => (245, format!("Conversion failed when converting the varchar value '{s}' to data type {}.", target.name())),
        (v, t) => (529, format!("Explicit conversion from data type {} to {} is not allowed.", v.type_name(), t.name())),
    };
    SqlError::new(number, 16, message).batch_abort()
}

fn overflow(target: &SqlType) -> SqlError {
    SqlError::new(8115, 16, format!("Arithmetic overflow error converting expression to data type {}.", target.name())).batch_abort()
}

const GUID_STYLE: uuid::fmt::Hyphenated = uuid::fmt::Hyphenated::from_uuid(Uuid::nil());

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The natural type of the value, for results whose type is not declared.
    pub fn natural_type(&self) -> SqlType {
        match self {
            Value::Null => SqlType::Int,
            Value::Bit(_) => SqlType::Bit,
            Value::Int(i) => {
                if i32::try_from(*i).is_ok() {
                    SqlType::Int
                } else {
                    SqlType::BigInt
                }
            }
            Value::Decimal(d) => SqlType::Decimal(38, d.scale().min(38) as u8),
            Value::Float(_) => SqlType::Float,
            Value::Str(s) => SqlType::NVarChar(if s.chars().count() <= 4000 { Some(4000) } else { None }),
            Value::Bytes(b) => SqlType::VarBinary(if b.len() <= 8000 { Some(8000) } else { None }),
            Value::Guid(_) => SqlType::UniqueIdentifier,
            Value::Date(_) => SqlType::Date,
            Value::Time(_) => SqlType::Time(7),
            Value::DateTime(_) => SqlType::DateTime2(7),
            Value::DateTimeOffset(_) => SqlType::DateTimeOffset(7),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "varchar",
            other => other.natural_type().name(),
        }
    }

    /// The value as text, as `CONVERT(nvarchar, ...)` gives it.
    pub fn to_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bit(b) => if *b { "1" } else { "0" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Decimal(d) => d.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => s.clone(),
            Value::Bytes(b) => {
                let mut text = String::from("0x");
                for byte in b {
                    text.push_str(&format!("{byte:02X}"));
                }
                text
            }
            Value::Guid(g) => {
                let mut buffer = [0u8; 36];
                let _ = GUID_STYLE;
                g.hyphenated().encode_upper(&mut buffer).to_string()
            }
            Value::Date(d) => d.format("%Y-%m-%d").to_string(),
            Value::Time(t) => t.format("%H:%M:%S%.7f").to_string(),
            Value::DateTime(dt) => dt.format("%Y-%m-%d %H:%M:%S%.7f").to_string(),
            Value::DateTimeOffset(dt) => dt.format("%Y-%m-%d %H:%M:%S%.7f %:z").to_string(),
        }
    }

    /// The value as a truth value, for `IF` and `WHERE`; `None` is unknown.
    pub fn truth(&self) -> Option<bool> {
        match self {
            Value::Null => None,
            Value::Bit(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            Value::Decimal(d) => Some(!d.is_zero()),
            Value::Float(f) => Some(*f != 0.0),
            _ => Some(true),
        }
    }

    pub fn as_i64(&self) -> Result<Option<i64>, SqlError> {
        match self.convert(&SqlType::BigInt)? {
            Value::Int(i) => Ok(Some(i)),
            _ => Ok(None),
        }
    }

    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::Null => None,
            other => Some(other.to_text()),
        }
    }

    fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Value::Bit(b) => Some(Decimal::from(*b as i64)),
            Value::Int(i) => Some(Decimal::from(*i)),
            Value::Decimal(d) => Some(*d),
            Value::Float(f) => Decimal::from_f64(*f),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Bit(b) => Some(*b as i64 as f64),
            Value::Int(i) => Some(*i as f64),
            Value::Decimal(d) => d.to_f64(),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Converts the value to `target`, as `CAST` does. Strings longer than the
    /// type are cut, as `CAST` and assignment to a variable do.
    pub fn convert(&self, target: &SqlType) -> Result<Value, SqlError> {
        if self.is_null() {
            return Ok(Value::Null);
        }
        let integer = |min: i64, max: i64| -> Result<Value, SqlError> {
            let i = match self {
                Value::Bit(b) => *b as i64,
                Value::Int(i) => *i,
                Value::Decimal(d) => d.trunc().to_i64().ok_or_else(|| overflow(target))?,
                Value::Float(f) => {
                    if !f.is_finite() || f.trunc() < i64::MIN as f64 || f.trunc() > i64::MAX as f64 {
                        return Err(overflow(target));
                    }
                    f.trunc() as i64
                }
                Value::Str(s) => {
                    let t = s.trim();
                    t.parse::<i64>().map_err(|_| conversion_failed(self, target))?
                }
                Value::DateTime(dt) => (dt.date() - base_1900()).num_days(),
                _ => return Err(conversion_failed(self, target)),
            };
            if i < min || i > max {
                return Err(overflow(target));
            }
            Ok(Value::Int(i))
        };
        match target {
            SqlType::Bit => match self {
                Value::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bit(true)),
                    "false" => Ok(Value::Bit(false)),
                    t => t.parse::<f64>().map(|n| Value::Bit(n != 0.0)).map_err(|_| conversion_failed(self, target)),
                },
                Value::Bit(_) | Value::Int(_) | Value::Decimal(_) | Value::Float(_) => Ok(Value::Bit(self.truth().unwrap_or(false))),
                _ => Err(conversion_failed(self, target)),
            },
            SqlType::TinyInt => integer(0, 255),
            SqlType::SmallInt => integer(i16::MIN as i64, i16::MAX as i64),
            SqlType::Int => integer(i32::MIN as i64, i32::MAX as i64),
            SqlType::BigInt => integer(i64::MIN, i64::MAX),
            SqlType::Decimal(..) | SqlType::Money => {
                let (precision, scale) = match target {
                    SqlType::Decimal(p, s) => (*p, *s),
                    _ => (19, 4),
                };
                let d = match self {
                    Value::Str(text) => {
                        let t = text.trim();
                        t.parse::<Decimal>()
                            .or_else(|_| Decimal::from_scientific(t))
                            .map_err(|_| conversion_failed(self, target))?
                    }
                    other => other.as_decimal().ok_or_else(|| conversion_failed(self, target))?,
                };
                let d = d.round_dp_with_strategy(scale as u32, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
                let integer_digits = d.trunc().abs().to_string().trim_start_matches('0').len();
                if integer_digits > (precision - scale) as usize {
                    return Err(overflow(target));
                }
                let mut d = d;
                d.rescale(scale as u32);
                Ok(Value::Decimal(d))
            }
            SqlType::SmallMoney => self.convert(&SqlType::Decimal(10, 4)),
            SqlType::Float | SqlType::Real => {
                let f = match self {
                    Value::Str(s) => s.trim().parse::<f64>().map_err(|_| conversion_failed(self, target))?,
                    other => other.as_f64().ok_or_else(|| conversion_failed(self, target))?,
                };
                Ok(Value::Float(if *target == SqlType::Real { f as f32 as f64 } else { f }))
            }
            t if t.is_string() => {
                let text = match self {
                    Value::Bytes(b) if !matches!(t, SqlType::Xml) => {
                        if t.is_unicode() {
                            let units: Vec<u16> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
                            String::from_utf16_lossy(&units)
                        } else {
                            b.iter().map(|&c| c as char).collect()
                        }
                    }
                    Value::DateTime(dt) => format_datetime_default(dt, target),
                    other => other.to_text(),
                };
                Ok(Value::Str(fit_string(text, t)))
            }
            SqlType::Binary(_) | SqlType::VarBinary(_) => {
                let mut bytes = match self {
                    Value::Bytes(b) => b.clone(),
                    Value::Str(s) => s.chars().map(|c| c as u32 as u8).collect(),
                    Value::Int(i) => {
                        if i32::try_from(*i).is_ok() {
                            (*i as i32).to_be_bytes().to_vec()
                        } else {
                            i.to_be_bytes().to_vec()
                        }
                    }
                    Value::Bit(b) => vec![*b as u8],
                    Value::Guid(g) => guid_to_wire(g).to_vec(),
                    _ => return Err(conversion_failed(self, target)),
                };
                if let Some(max) = target.max_length() {
                    bytes.truncate(max);
                    if let SqlType::Binary(n) = target {
                        bytes.resize(*n as usize, 0);
                    }
                }
                Ok(Value::Bytes(bytes))
            }
            SqlType::UniqueIdentifier => match self {
                Value::Guid(g) => Ok(Value::Guid(*g)),
                Value::Str(s) => {
                    let t = s.trim().trim_start_matches('{').trim_end_matches('}');
                    let t = t.get(..36).unwrap_or(t);
                    Uuid::parse_str(t).map(Value::Guid).map_err(|_| conversion_failed(self, target))
                }
                Value::Bytes(b) if b.len() == 16 => {
                    let mut wire = [0u8; 16];
                    wire.copy_from_slice(b);
                    Ok(Value::Guid(guid_from_wire(&wire)))
                }
                _ => Err(conversion_failed(self, target)),
            },
            SqlType::Date => match self {
                Value::Date(d) => Ok(Value::Date(*d)),
                Value::DateTime(dt) => Ok(Value::Date(dt.date())),
                Value::DateTimeOffset(dt) => Ok(Value::Date(dt.naive_local().date())),
                Value::Str(s) => parse_datetime(s).map(|dt| Value::Date(dt.date())).ok_or_else(|| conversion_failed(self, target)),
                _ => Err(conversion_failed(self, target)),
            },
            SqlType::Time(scale) => match self {
                Value::Time(t) => Ok(Value::Time(round_time(*t, *scale))),
                Value::DateTime(dt) => Ok(Value::Time(round_time(dt.time(), *scale))),
                Value::DateTimeOffset(dt) => Ok(Value::Time(round_time(dt.naive_local().time(), *scale))),
                Value::Str(s) => parse_time(s)
                    .or_else(|| parse_datetime(s).map(|dt| dt.time()))
                    .map(|t| Value::Time(round_time(t, *scale)))
                    .ok_or_else(|| conversion_failed(self, target)),
                _ => Err(conversion_failed(self, target)),
            },
            SqlType::DateTime | SqlType::SmallDateTime | SqlType::DateTime2(_) => {
                let dt = match self {
                    Value::DateTime(dt) => *dt,
                    Value::Date(d) => d.and_time(NaiveTime::MIN),
                    Value::DateTimeOffset(dt) => dt.naive_local(),
                    Value::Time(t) => base_1900().and_time(*t),
                    Value::Str(s) => parse_datetime(s).ok_or_else(|| conversion_failed(self, target))?,
                    Value::Int(_) | Value::Decimal(_) | Value::Float(_) if !matches!(target, SqlType::DateTime2(_)) => {
                        let days = self.as_f64().unwrap_or(0.0);
                        let millis = (days * 86_400_000.0).round() as i64;
                        base_1900().and_time(NaiveTime::MIN) + chrono::Duration::milliseconds(millis)
                    }
                    _ => return Err(conversion_failed(self, target)),
                };
                Ok(Value::DateTime(match target {
                    SqlType::DateTime => round_datetime(dt),
                    SqlType::SmallDateTime => {
                        let rounded = dt + chrono::Duration::seconds(30);
                        rounded.date().and_hms_opt(rounded.hour(), rounded.minute(), 0).unwrap_or(rounded)
                    }
                    SqlType::DateTime2(scale) => dt.date().and_time(round_time(dt.time(), *scale)),
                    _ => dt,
                }))
            }
            SqlType::DateTimeOffset(_) => match self {
                Value::DateTimeOffset(dt) => Ok(Value::DateTimeOffset(*dt)),
                Value::DateTime(dt) => Ok(Value::DateTimeOffset(dt.and_utc().fixed_offset())),
                Value::Date(d) => Ok(Value::DateTimeOffset(d.and_time(NaiveTime::MIN).and_utc().fixed_offset())),
                Value::Str(s) => {
                    let t = s.trim();
                    DateTime::parse_from_rfc3339(t)
                        .ok()
                        .or_else(|| DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f %:z").ok())
                        .or_else(|| parse_datetime(t).map(|dt| dt.and_utc().fixed_offset()))
                        .map(Value::DateTimeOffset)
                        .ok_or_else(|| conversion_failed(self, target))
                }
                _ => Err(conversion_failed(self, target)),
            },
            _ => Err(conversion_failed(self, target)),
        }
    }

    /// Converts for storing in a column or parameter of type `target`: like
    /// [`Value::convert`], but a string too long for the type is an error.
    pub fn store(&self, target: &SqlType) -> Result<Value, StoreError> {
        if let (Value::Str(s), Some(max)) = (self, target.max_length()) {
            if target.is_string() {
                let trimmed = s.trim_end_matches(' ');
                if trimmed.chars().count() > max {
                    return Err(StoreError::Truncated(s.chars().take(max).collect()));
                }
            }
        }
        if let (Value::Bytes(b), Some(max)) = (self, target.max_length()) {
            if b.len() > max {
                return Err(StoreError::Truncated(String::new()));
            }
        }
        self.convert(target).map_err(StoreError::Conversion)
    }
}

/// Why a value could not be stored.
pub enum StoreError {
    /// Too long; the value cut to fit, for the message.
    Truncated(String),
    Conversion(SqlError),
}

fn fit_string(text: String, target: &SqlType) -> String {
    match target {
        SqlType::Char(n) | SqlType::NChar(n) => {
            let mut s: String = text.chars().take(*n as usize).collect();
            let len = s.chars().count();
            s.extend(std::iter::repeat_n(' ', *n as usize - len));
            s
        }
        other => match other.max_length() {
            Some(max) if text.chars().count() > max => text.chars().take(max).collect(),
            _ => text,
        },
    }
}

fn format_datetime_default(dt: &NaiveDateTime, _target: &SqlType) -> String {
    dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        let text = format!("{f}");
        if text.len() > 17 {
            format!("{f:e}")
        } else {
            text
        }
    }
}

pub fn base_1900() -> NaiveDate {
    NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default()
}

fn round_time(t: NaiveTime, scale: u8) -> NaiveTime {
    let unit = 10u32.pow(9 - scale.min(7) as u32);
    let nanos = t.nanosecond() % 1_000_000_000;
    let rounded = (nanos + unit / 2) / unit * unit;
    if rounded >= 1_000_000_000 {
        let base = t.with_nanosecond(0).unwrap_or(t);
        let (next, _) = base.overflowing_add_signed(chrono::Duration::seconds(1));
        return next;
    }
    t.with_nanosecond(rounded).unwrap_or(t)
}

/// `datetime` keeps time in 1/300 second ticks.
pub fn round_datetime(dt: NaiveDateTime) -> NaiveDateTime {
    let nanos = dt.time().nanosecond() as u64 % 1_000_000_000;
    let ticks = (nanos * 300 + 500_000_000) / 1_000_000_000;
    let base = dt.with_nanosecond(0).unwrap_or(dt);
    base + chrono::Duration::nanoseconds((ticks * 1_000_000_000 / 300) as i64)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let t = s.trim();
    for format in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(t, format) {
            return Some(time);
        }
    }
    None
}

/// Reads the date and time formats SQL Server accepts most often.
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let t = s.trim();
    let t = t.strip_suffix('Z').unwrap_or(t);
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
        "%Y%m%d %H:%M:%S%.f",
        "%Y%m%d %H:%M:%S",
        "%m/%d/%Y %H:%M:%S%.f",
        "%m/%d/%Y %H:%M:%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(t, format) {
            return Some(dt);
        }
    }
    for format in ["%Y-%m-%d", "%Y%m%d", "%m/%d/%Y", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, format) {
            return Some(d.and_time(NaiveTime::MIN));
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s.trim()) {
        return Some(dt.naive_utc());
    }
    parse_time(t).map(|time| base_1900().and_time(time))
}

/// A GUID in SQL Server's byte order (the first three groups little-endian).
pub fn guid_to_wire(g: &Uuid) -> [u8; 16] {
    let b = g.as_bytes();
    [b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]
}

pub fn guid_from_wire(w: &[u8; 16]) -> Uuid {
    Uuid::from_bytes([w[3], w[2], w[1], w[0], w[5], w[4], w[7], w[6], w[8], w[9], w[10], w[11], w[12], w[13], w[14], w[15]])
}

/// Compares two strings as the default collation (`SQL_Latin1_General_CP1_CI_AS`)
/// does: without case, and ignoring trailing spaces.
pub fn compare_text(a: &str, b: &str) -> Ordering {
    let a = a.trim_end_matches(' ');
    let b = b.trim_end_matches(' ');
    let mut left = a.chars().flat_map(char::to_lowercase);
    let mut right = b.chars().flat_map(char::to_lowercase);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => continue,
                other => return other,
            },
        }
    }
}

/// The type both sides of a comparison or arithmetic are converted to.
fn common_type(a: &Value, b: &Value) -> SqlType {
    let ta = a.natural_type();
    let tb = b.natural_type();
    let ta = if matches!(a, Value::Str(_)) { SqlType::VarChar(None) } else { ta };
    let tb = if matches!(b, Value::Str(_)) { SqlType::VarChar(None) } else { tb };
    if ta.precedence() >= tb.precedence() {
        ta
    } else {
        tb
    }
}

/// Compares two non-null values after converting them to a common type.
pub fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>, SqlError> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    let ordering = match (a, b) {
        (Value::Str(x), Value::Str(y)) => compare_text(x, y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Bit(x), Value::Bit(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Guid(x), Value::Guid(y)) => guid_order(x).cmp(&guid_order(y)),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        (Value::DateTimeOffset(x), Value::DateTimeOffset(y)) => x.cmp(y),
        (Value::Float(_), _) | (_, Value::Float(_)) if numeric(a) && numeric(b) => {
            a.as_f64().unwrap_or(0.0).partial_cmp(&b.as_f64().unwrap_or(0.0)).unwrap_or(Ordering::Equal)
        }
        _ if numeric(a) && numeric(b) => a.as_decimal().unwrap_or_default().cmp(&b.as_decimal().unwrap_or_default()),
        _ => {
            let target = common_type(a, b);
            let x = a.convert(&target)?;
            let y = b.convert(&target)?;
            if std::mem::discriminant(&x) == std::mem::discriminant(a) && std::mem::discriminant(&y) == std::mem::discriminant(b) {
                return Ok(Some(Ordering::Equal));
            }
            return compare(&x, &y);
        }
    };
    Ok(Some(ordering))
}

/// SQL Server orders uniqueidentifiers by their last group first.
fn guid_order(g: &Uuid) -> [u8; 16] {
    let b = g.as_bytes();
    [b[10], b[11], b[12], b[13], b[14], b[15], b[8], b[9], b[6], b[7], b[4], b[5], b[0], b[1], b[2], b[3]]
}

fn numeric(v: &Value) -> bool {
    matches!(v, Value::Bit(_) | Value::Int(_) | Value::Decimal(_) | Value::Float(_))
}

/// An order for sorting, with nulls first as SQL Server sorts them.
pub fn sort_order(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => compare(a, b).ok().flatten().unwrap_or(Ordering::Equal),
    }
}

/// Whether two values are the same for grouping, `DISTINCT` and keys (nulls equal).
pub fn same(a: &Value, b: &Value) -> bool {
    sort_order(a, b) == Ordering::Equal
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
}

fn divide_by_zero() -> SqlError {
    SqlError::new(8134, 16, "Divide by zero error encountered.")
}

/// Applies an arithmetic operator, converting by data type precedence.
pub fn arithmetic(op: ArithOp, a: &Value, b: &Value) -> Result<Value, SqlError> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    // String concatenation.
    if let (ArithOp::Add, Value::Str(x), Value::Str(y)) = (op, a, b) {
        return Ok(Value::Str(format!("{x}{y}")));
    }
    if let (ArithOp::Add, Value::Bytes(x), Value::Bytes(y)) = (op, a, b) {
        return Ok(Value::Bytes([x.as_slice(), y.as_slice()].concat()));
    }
    // Date arithmetic: datetime + number of days.
    match (op, a, b) {
        (ArithOp::Add | ArithOp::Sub, Value::DateTime(dt), n) if numeric(n) => {
            let days = n.as_f64().unwrap_or(0.0);
            let millis = (days * 86_400_000.0).round() as i64;
            let delta = chrono::Duration::milliseconds(if op == ArithOp::Sub { -millis } else { millis });
            return Ok(Value::DateTime(*dt + delta));
        }
        (ArithOp::Add | ArithOp::Sub, Value::DateTime(x), Value::DateTime(y)) => {
            let base = base_1900().and_time(NaiveTime::MIN);
            let delta = if op == ArithOp::Sub { *x - *y } else { (*x - base) + (*y - base) };
            return Ok(Value::DateTime(base + delta));
        }
        _ => {}
    }
    let target = common_type(a, b);
    let x = a.convert(&target)?;
    let y = b.convert(&target)?;
    match (&x, &y) {
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x, *y);
            let result = match op {
                ArithOp::Add => x.checked_add(y),
                ArithOp::Sub => x.checked_sub(y),
                ArithOp::Mul => x.checked_mul(y),
                ArithOp::Div => {
                    if y == 0 {
                        return Err(divide_by_zero());
                    }
                    x.checked_div(y)
                }
                ArithOp::Mod => {
                    if y == 0 {
                        return Err(divide_by_zero());
                    }
                    x.checked_rem(y)
                }
                ArithOp::BitAnd => Some(x & y),
                ArithOp::BitOr => Some(x | y),
                ArithOp::BitXor => Some(x ^ y),
            };
            let result = result.ok_or_else(|| overflow(&target))?;
            let fits = match target {
                SqlType::BigInt => true,
                _ => i32::try_from(result).is_ok(),
            };
            if !fits && matches!(a, Value::Int(i) if i32::try_from(*i).is_ok()) && matches!(b, Value::Int(i) if i32::try_from(*i).is_ok()) {
                return Err(SqlError::new(8115, 16, "Arithmetic overflow error converting expression to data type int.").batch_abort());
            }
            Ok(Value::Int(result))
        }
        (Value::Bit(x), Value::Bit(y)) => {
            let (x, y) = (*x as i64, *y as i64);
            arithmetic(op, &Value::Int(x), &Value::Int(y))
        }
        (Value::Decimal(x), Value::Decimal(y)) => {
            let result = match op {
                ArithOp::Add => x.checked_add(*y),
                ArithOp::Sub => x.checked_sub(*y),
                ArithOp::Mul => x.checked_mul(*y),
                ArithOp::Div => {
                    if y.is_zero() {
                        return Err(divide_by_zero());
                    }
                    x.checked_div(*y).map(|d| {
                        let scale = x.scale().clamp(6, 28);
                        d.round_dp(scale)
                    })
                }
                ArithOp::Mod => {
                    if y.is_zero() {
                        return Err(divide_by_zero());
                    }
                    x.checked_rem(*y)
                }
                _ => return Err(SqlError::new(402, 16, "The data types decimal and decimal are incompatible in the bitwise operator.")),
            };
            result.map(Value::Decimal).ok_or_else(|| overflow(&target))
        }
        (Value::Float(x), Value::Float(y)) => {
            let result = match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => {
                    if *y == 0.0 {
                        return Err(divide_by_zero());
                    }
                    x / y
                }
                ArithOp::Mod => x % y,
                _ => return Err(SqlError::new(402, 16, "The data types float and float are incompatible in the bitwise operator.")),
            };
            Ok(Value::Float(result))
        }
        _ => Err(SqlError::new(
            402,
            16,
            format!("The data types {} and {} are incompatible in the {} operator.", a.type_name(), b.type_name(), op_name(op)),
        )),
    }
}

fn op_name(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "add",
        ArithOp::Sub => "subtract",
        ArithOp::Mul => "multiply",
        ArithOp::Div => "divide",
        ArithOp::Mod => "modulo",
        ArithOp::BitAnd => "&",
        ArithOp::BitOr => "|",
        ArithOp::BitXor => "^",
    }
}

/// Days from 0001-01-01, as `date` is sent.
pub fn days_from_ce(d: &NaiveDate) -> i32 {
    d.num_days_from_ce() - 1
}
