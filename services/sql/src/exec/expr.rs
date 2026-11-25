//! Expressions and built-in functions.

use std::cmp::Ordering;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

use super::query::{is_aggregate, unknown_column, Scope};
use super::{Executor, SqlError, Variable};
use crate::ast::*;
use crate::value::{arithmetic, base_1900, compare, compare_text, same, ArithOp, SqlType, Value};

pub const VERSION_TEXT: &str = "Microsoft SQL Server 2022 (RTM) - 16.0.1000.6 (X64) \n\tAzure Emulator (in-memory T-SQL subset)\n";

impl Executor<'_> {
    /// Evaluates an expression that refers to no columns.
    pub(super) fn eval_plain(&mut self, expr: &Expr) -> Result<Value, SqlError> {
        self.eval(expr, &Scope::empty())
    }

    pub(super) fn eval(&mut self, expr: &Expr, scope: &Scope) -> Result<Value, SqlError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::String { value, .. } => Ok(Value::Str(value.clone())),
            Expr::Default => Ok(Value::Null),
            Expr::Keyword(k) => Ok(Value::Str(k.clone())),
            Expr::Var(name) => match self.frame().vars.get(&name.to_lowercase()) {
                Some(Variable::Scalar { value, .. }) => Ok(value.clone()),
                _ => Err(SqlError::new(137, 15, format!("Must declare the scalar variable \"{name}\".")).batch_abort()),
            },
            Expr::Global(name) => self.global(name),
            Expr::Column(parts) => match scope.lookup(parts)? {
                Some(v) => Ok(v),
                None => Err(unknown_column(parts, scope)),
            },
            Expr::Unary { op, expr } => {
                let v = self.eval(expr, scope)?;
                match op {
                    '-' => arithmetic(ArithOp::Sub, &Value::Int(0), &v).map(|r| match (&v, r) {
                        (Value::Float(f), _) => Value::Float(-f),
                        (_, r) => r,
                    }),
                    _ => match v {
                        Value::Null => Ok(Value::Null),
                        Value::Bit(b) => Ok(Value::Bit(!b)),
                        other => Ok(Value::Int(!other.as_i64()?.unwrap_or(0))),
                    },
                }
            }
            Expr::Arith { op, left, right } => {
                let l = self.eval(left, scope)?;
                let r = self.eval(right, scope)?;
                arithmetic(*op, &l, &r)
            }
            Expr::Compare { op, left, right } => {
                let l = self.eval(left, scope)?;
                let r = self.eval(right, scope)?;
                Ok(match compare(&l, &r)? {
                    None => Value::Null,
                    Some(o) => Value::Bit(match op {
                        CmpOp::Eq => o == Ordering::Equal,
                        CmpOp::Ne => o != Ordering::Equal,
                        CmpOp::Lt => o == Ordering::Less,
                        CmpOp::Le => o != Ordering::Greater,
                        CmpOp::Gt => o == Ordering::Greater,
                        CmpOp::Ge => o != Ordering::Less,
                    }),
                })
            }
            Expr::And(a, b) => {
                let l = self.eval(a, scope)?.truth();
                if l == Some(false) {
                    return Ok(Value::Bit(false));
                }
                let r = self.eval(b, scope)?.truth();
                Ok(match (l, r) {
                    (_, Some(false)) => Value::Bit(false),
                    (Some(true), Some(true)) => Value::Bit(true),
                    _ => Value::Null,
                })
            }
            Expr::Or(a, b) => {
                let l = self.eval(a, scope)?.truth();
                if l == Some(true) {
                    return Ok(Value::Bit(true));
                }
                let r = self.eval(b, scope)?.truth();
                Ok(match (l, r) {
                    (_, Some(true)) => Value::Bit(true),
                    (Some(false), Some(false)) => Value::Bit(false),
                    _ => Value::Null,
                })
            }
            Expr::Not(e) => Ok(match self.eval(e, scope)?.truth() {
                Some(b) => Value::Bit(!b),
                None => Value::Null,
            }),
            Expr::IsNull { expr, negated } => {
                let v = self.eval(expr, scope)?;
                Ok(Value::Bit(v.is_null() != *negated))
            }
            Expr::Like { expr, pattern, escape, negated } => {
                let v = self.eval(expr, scope)?;
                let p = self.eval(pattern, scope)?;
                let escape = match escape {
                    Some(e) => self.eval(e, scope)?.as_string().and_then(|s| s.chars().next()),
                    None => None,
                };
                match (v.as_string(), p.as_string()) {
                    (Some(v), Some(p)) => Ok(Value::Bit(like(&v, &p, escape) != *negated)),
                    _ => Ok(Value::Null),
                }
            }
            Expr::InList { expr, list, negated } => {
                let v = self.eval(expr, scope)?;
                let mut unknown = v.is_null();
                for item in list {
                    let candidate = self.eval(item, scope)?;
                    match compare(&v, &candidate)? {
                        Some(Ordering::Equal) => return Ok(Value::Bit(!negated)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown { Value::Null } else { Value::Bit(*negated) })
            }
            Expr::InQuery { expr, query, negated } => {
                let v = self.eval(expr, scope)?;
                let rowset = self.run_query(query, Some(scope))?;
                if rowset.layout.columns.iter().filter(|c| !c.hidden).count() != 1 {
                    return Err(SqlError::new(116, 16, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS.").batch_abort());
                }
                let mut unknown = v.is_null();
                for row in &rowset.rows {
                    match compare(&v, &row[0])? {
                        Some(Ordering::Equal) => return Ok(Value::Bit(!negated)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                Ok(if unknown && !rowset.rows.is_empty() { Value::Null } else { Value::Bit(*negated) })
            }
            Expr::Between { expr, low, high, negated } => {
                let v = self.eval(expr, scope)?;
                let lo = self.eval(low, scope)?;
                let hi = self.eval(high, scope)?;
                match (compare(&v, &lo)?, compare(&v, &hi)?) {
                    (Some(a), Some(b)) => Ok(Value::Bit((a != Ordering::Less && b != Ordering::Greater) != *negated)),
                    _ => Ok(Value::Null),
                }
            }
            Expr::Exists(query) => {
                let rowset = self.run_query(query, Some(scope))?;
                Ok(Value::Bit(!rowset.rows.is_empty()))
            }
            Expr::Subquery(query) => {
                let rowset = self.run_query(query, Some(scope))?;
                if let Some(options) = &query.for_json {
                    return Ok(Value::Str(super::json::for_json(&rowset, options)));
                }
                if rowset.layout.columns.iter().filter(|c| !c.hidden).count() != 1 {
                    return Err(SqlError::new(116, 16, "Only one expression can be specified in the select list when the subquery is not introduced with EXISTS.").batch_abort());
                }
                match rowset.rows.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(rowset.rows[0][0].clone()),
                    _ => Err(SqlError::new(512, 16, "Subquery returned more than 1 value. This is not permitted when the subquery follows =, !=, <, <= , >, >= or when the subquery is used as an expression.")),
                }
            }
            Expr::Case { operand, branches, otherwise } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, scope)?),
                    None => None,
                };
                for (when, then) in branches {
                    let hit = match &subject {
                        Some(s) => {
                            let w = self.eval(when, scope)?;
                            compare(s, &w)? == Some(Ordering::Equal)
                        }
                        None => self.eval(when, scope)?.truth() == Some(true),
                    };
                    if hit {
                        return self.eval(then, scope);
                    }
                }
                match otherwise {
                    Some(e) => self.eval(e, scope),
                    None => Ok(Value::Null),
                }
            }
            Expr::Cast { expr, ty, style, try_ } => {
                let v = self.eval(expr, scope)?;
                let style = match style {
                    Some(s) => self.eval(s, scope)?.as_i64()?,
                    None => None,
                };
                let result = convert_with_style(&v, ty, style);
                match result {
                    Err(_) if *try_ => Ok(Value::Null),
                    other => other,
                }
            }
            Expr::Function { name, args, distinct, star, over } => {
                if over.is_some() && !name.ends_with("#WITHIN") {
                    let key = expr as *const Expr as usize;
                    return Ok(self.windows.get(&key).and_then(|v| v.get(scope.row_index)).cloned().unwrap_or(Value::Null));
                }
                if is_aggregate(name) {
                    return self.aggregate(name, args, *distinct, *star, over.as_deref(), scope);
                }
                self.function(name, args, scope)
            }
        }
    }

    fn global(&mut self, name: &str) -> Result<Value, SqlError> {
        Ok(match name {
            "IDENTITY" => self.session.identity.clone(),
            "ROWCOUNT" => Value::Int(self.session.rowcount),
            "TRANCOUNT" => Value::Int(self.session.transaction.as_ref().map_or(0, |t| t.count as i64)),
            "VERSION" => Value::Str(VERSION_TEXT.to_string()),
            "SPID" => Value::Int(self.session.spid as i64),
            "SERVERNAME" | "SERVICENAME" => Value::Str("azure-emulator".into()),
            "ERROR" => Value::Int(self.session.last_error as i64),
            "LANGUAGE" => Value::Str("us_english".into()),
            "DATEFIRST" => Value::Int(7),
            "MAX_PRECISION" => Value::Int(38),
            "NESTLEVEL" => Value::Int(self.frames.len().saturating_sub(1) as i64),
            "LOCK_TIMEOUT" => Value::Int(-1),
            "TEXTSIZE" => Value::Int(2147483647),
            "OPTIONS" => Value::Int(5496),
            "PROCID" => Value::Int(0),
            "MICROSOFTVERSION" => Value::Int(268_436_456),
            "FETCH_STATUS" => Value::Int(-1),
            other => return Err(SqlError::new(40517, 16, format!("@@{other} is not supported by the Azure emulator's SQL data plane yet.")).batch_abort()),
        })
    }

    fn aggregate(&mut self, name: &str, args: &[Expr], distinct: bool, star: bool, within: Option<&Over>, scope: &Scope) -> Result<Value, SqlError> {
        let Some(group) = scope.group else {
            return Err(SqlError::new(147, 15, "An aggregate may not appear in the WHERE clause unless it is in a subquery contained in a HAVING clause or a select list, and the column being aggregated is an outer reference.").batch_abort());
        };
        if star {
            return Ok(Value::Int(group.len() as i64));
        }
        let Some(arg) = args.first() else {
            return Err(SqlError::new(174, 15, format!("The {} function requires 1 argument(s).", name.to_lowercase())).batch_abort());
        };
        let mut values: Vec<(Value, Vec<Value>)> = Vec::new();
        for row in group {
            let row_scope = Scope { layout: scope.layout, row, group: None, outer: scope.outer, row_index: 0 };
            let v = self.eval(arg, &row_scope)?;
            if v.is_null() {
                continue;
            }
            let mut keys = Vec::new();
            if let Some(within) = within {
                for item in &within.order_by {
                    keys.push(self.eval(&item.expr, &row_scope)?);
                }
            }
            values.push((v, keys));
        }
        if distinct {
            let mut unique: Vec<(Value, Vec<Value>)> = Vec::new();
            for v in values {
                if !unique.iter().any(|u| same(&u.0, &v.0)) {
                    unique.push(v);
                }
            }
            values = unique;
        }
        match name {
            "COUNT" | "COUNT_BIG" => Ok(Value::Int(values.len() as i64)),
            "SUM" | "AVG" => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let mut total = Value::Int(0);
                for (v, _) in &values {
                    if matches!(v, Value::Str(_)) {
                        return Err(SqlError::new(8117, 16, format!("Operand data type varchar is invalid for {} operator.", name.to_lowercase())).batch_abort());
                    }
                    let v = if let Value::Bit(b) = v { Value::Int(*b as i64) } else { v.clone() };
                    total = arithmetic(ArithOp::Add, &total, &v)?;
                }
                if name == "AVG" {
                    let count = Value::Int(values.len() as i64);
                    return arithmetic(ArithOp::Div, &total, &count);
                }
                Ok(total)
            }
            "MIN" | "MAX" => {
                let mut best: Option<Value> = None;
                for (v, _) in values {
                    best = Some(match best {
                        None => v,
                        Some(b) => {
                            let o = crate::value::sort_order(&v, &b);
                            if (name == "MIN" && o == Ordering::Less) || (name == "MAX" && o == Ordering::Greater) {
                                v
                            } else {
                                b
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(Value::Null))
            }
            "STRING_AGG" | "STRING_AGG#WITHIN" => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let separator = match args.get(1) {
                    Some(e) => self.eval(e, scope)?.as_string().unwrap_or_default(),
                    None => String::new(),
                };
                if let Some(within) = within {
                    values.sort_by(|a, b| {
                        for (i, item) in within.order_by.iter().enumerate() {
                            let mut o = crate::value::sort_order(&a.1[i], &b.1[i]);
                            if item.descending {
                                o = o.reverse();
                            }
                            if o != Ordering::Equal {
                                return o;
                            }
                        }
                        Ordering::Equal
                    });
                }
                Ok(Value::Str(values.iter().map(|(v, _)| v.to_text()).collect::<Vec<_>>().join(&separator)))
            }
            other => Err(SqlError::new(40517, 16, format!("The aggregate {other} is not supported by the Azure emulator's SQL data plane yet.")).batch_abort()),
        }
    }

    fn args(&mut self, args: &[Expr], scope: &Scope) -> Result<Vec<Value>, SqlError> {
        let mut values = Vec::with_capacity(args.len());
        for a in args {
            values.push(self.eval(a, scope)?);
        }
        Ok(values)
    }

    fn function(&mut self, name: &str, args: &[Expr], scope: &Scope) -> Result<Value, SqlError> {
        let count = |n: usize| -> Result<(), SqlError> {
            if args.len() == n {
                Ok(())
            } else {
                Err(SqlError::new(174, 15, format!("The {} function requires {n} argument(s).", name.to_lowercase())).batch_abort())
            }
        };
        let text = |v: &Value| v.as_string();
        // Functions whose first argument is a date part keyword.
        if matches!(name, "DATEADD" | "DATEDIFF" | "DATEDIFF_BIG" | "DATEPART" | "DATENAME" | "DATETRUNC") {
            let part = match args.first() {
                Some(Expr::Column(parts)) if parts.len() == 1 => parts[0].to_lowercase(),
                Some(Expr::String { value, .. }) => value.to_lowercase(),
                _ => return Err(SqlError::new(155, 15, format!("'{}' is not a recognized {} option.", "?", name.to_lowercase())).batch_abort()),
            };
            let rest = self.args(&args[1..], scope)?;
            return date_function(name, &part, &rest);
        }
        let v = self.args(args, scope)?;
        let arg = |i: usize| v.get(i).cloned().unwrap_or(Value::Null);
        Ok(match name {
            // ----- null handling -----
            "ISNULL" => {
                count(2)?;
                if v[0].is_null() {
                    match self.static_type(&args[0], scope) {
                        Some(ty) => v[1].convert(&ty)?,
                        None => v[1].clone(),
                    }
                } else {
                    v[0].clone()
                }
            }
            "COALESCE" => v.into_iter().find(|x| !x.is_null()).unwrap_or(Value::Null),
            "NULLIF" => {
                count(2)?;
                if compare(&v[0], &v[1])? == Some(Ordering::Equal) {
                    Value::Null
                } else {
                    v[0].clone()
                }
            }
            "IIF" => {
                count(3)?;
                // The condition was evaluated as a value; re-evaluate as a predicate.
                if self.eval(&args[0], scope)?.truth() == Some(true) {
                    v[1].clone()
                } else {
                    v[2].clone()
                }
            }
            "CHOOSE" => {
                let index = arg(0).as_i64()?.unwrap_or(0);
                if index >= 1 && (index as usize) < v.len() {
                    v[index as usize].clone()
                } else {
                    Value::Null
                }
            }
            // ----- strings -----
            "LEN" => match text(&arg(0)) {
                Some(s) => Value::Int(s.trim_end_matches(' ').chars().count() as i64),
                None => Value::Null,
            },
            "DATALENGTH" => match arg(0) {
                Value::Null => Value::Null,
                Value::Str(s) => {
                    let unicode = self.static_type(&args[0], scope).is_none_or(|t| t.is_unicode());
                    Value::Int(if unicode { s.encode_utf16().count() as i64 * 2 } else { s.chars().count() as i64 })
                }
                Value::Bytes(b) => Value::Int(b.len() as i64),
                other => Value::Int(other.natural_type().storage_length()),
            },
            "UPPER" => map_text(&arg(0), |s| s.to_uppercase()),
            "LOWER" => map_text(&arg(0), |s| s.to_lowercase()),
            "LTRIM" => map_text(&arg(0), |s| s.trim_start_matches(' ').to_string()),
            "RTRIM" => map_text(&arg(0), |s| s.trim_end_matches(' ').to_string()),
            "TRIM" => match (text(&arg(0)), v.get(1).and_then(Value::as_string)) {
                (Some(s), None) => Value::Str(s.trim_matches(' ').to_string()),
                (Some(s), Some(chars)) => Value::Str(s.trim_matches(|c| chars.contains(c)).to_string()),
                _ => Value::Null,
            },
            "SUBSTRING" => {
                count(3)?;
                match (text(&v[0]), v[1].as_i64()?, v[2].as_i64()?) {
                    (Some(s), Some(start), Some(length)) => {
                        if length < 0 {
                            return Err(SqlError::new(537, 16, "Invalid length parameter passed to the LEFT or SUBSTRING function."));
                        }
                        let end = start + length;
                        let start = start.max(1);
                        if end <= start {
                            Value::Str(String::new())
                        } else {
                            Value::Str(s.chars().skip(start as usize - 1).take((end - start) as usize).collect())
                        }
                    }
                    _ => Value::Null,
                }
            }
            "LEFT" | "RIGHT" => {
                count(2)?;
                match (text(&v[0]), v[1].as_i64()?) {
                    (Some(s), Some(n)) => {
                        if n < 0 {
                            return Err(SqlError::new(536, 16, format!("Invalid length parameter passed to the {} function.", name.to_lowercase())));
                        }
                        let chars: Vec<char> = s.chars().collect();
                        let n = (n as usize).min(chars.len());
                        Value::Str(if name == "LEFT" { chars[..n].iter().collect() } else { chars[chars.len() - n..].iter().collect() })
                    }
                    _ => Value::Null,
                }
            }
            "CHARINDEX" => match (text(&arg(1)), text(&arg(0))) {
                (Some(haystack), Some(needle)) => {
                    let start = match v.get(2) {
                        Some(s) => s.as_i64()?.unwrap_or(1).max(1) as usize,
                        None => 1,
                    };
                    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
                    let needle: Vec<char> = needle.to_lowercase().chars().collect();
                    let mut found = 0;
                    if !needle.is_empty() && start <= hay.len() {
                        for i in (start - 1)..=hay.len().saturating_sub(needle.len()) {
                            if hay[i..].starts_with(&needle) {
                                found = i + 1;
                                break;
                            }
                        }
                    }
                    Value::Int(found as i64)
                }
                _ => Value::Null,
            },
            "PATINDEX" => match (text(&arg(0)), text(&arg(1))) {
                (Some(pattern), Some(s)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let inner = pattern.trim_start_matches('%').trim_end_matches('%').to_string();
                    let mut found = 0;
                    for i in 0..chars.len() {
                        for j in i..=chars.len() {
                            let candidate: String = chars[i..j].iter().collect();
                            if like(&candidate, &inner, None) {
                                found = i + 1;
                                break;
                            }
                        }
                        if found > 0 {
                            break;
                        }
                    }
                    Value::Int(found as i64)
                }
                _ => Value::Null,
            },
            "REPLACE" => match (text(&arg(0)), text(&arg(1)), text(&arg(2))) {
                (Some(s), Some(from), Some(to)) => Value::Str(replace_ci(&s, &from, &to)),
                _ => Value::Null,
            },
            "REPLICATE" => match (text(&arg(0)), arg(1).as_i64()?) {
                (Some(s), Some(n)) if n >= 0 => Value::Str(s.repeat(n as usize)),
                _ => Value::Null,
            },
            "SPACE" => match arg(0).as_i64()? {
                Some(n) if n >= 0 => Value::Str(" ".repeat(n as usize)),
                _ => Value::Null,
            },
            "REVERSE" => map_text(&arg(0), |s| s.chars().rev().collect()),
            "STUFF" => match (text(&arg(0)), arg(1).as_i64()?, arg(2).as_i64()?, text(&arg(3))) {
                (Some(s), Some(start), Some(length), insert) => {
                    let chars: Vec<char> = s.chars().collect();
                    if start < 1 || start as usize > chars.len() || length < 0 {
                        Value::Null
                    } else {
                        let start = start as usize - 1;
                        let end = (start + length as usize).min(chars.len());
                        let mut out: String = chars[..start].iter().collect();
                        out.push_str(&insert.unwrap_or_default());
                        out.extend(&chars[end..]);
                        Value::Str(out)
                    }
                }
                _ => Value::Null,
            },
            "CONCAT" => Value::Str(v.iter().map(|x| x.as_string().unwrap_or_default()).collect()),
            "CONCAT_WS" => {
                let separator = text(&arg(0)).unwrap_or_default();
                Value::Str(v.iter().skip(1).filter_map(Value::as_string).collect::<Vec<_>>().join(&separator))
            }
            "QUOTENAME" => match text(&arg(0)) {
                Some(s) => {
                    let quote = v.get(1).and_then(Value::as_string).unwrap_or_else(|| "[".into());
                    match quote.as_str() {
                        "'" => Value::Str(format!("'{}'", s.replace('\'', "''"))),
                        "\"" => Value::Str(format!("\"{}\"", s.replace('"', "\"\""))),
                        _ => Value::Str(format!("[{}]", s.replace(']', "]]"))),
                    }
                }
                None => Value::Null,
            },
            "CHAR" | "NCHAR" => match arg(0).as_i64()? {
                Some(n) => char::from_u32(n as u32).map_or(Value::Null, |c| Value::Str(c.to_string())),
                None => Value::Null,
            },
            "ASCII" | "UNICODE" => match text(&arg(0)).and_then(|s| s.chars().next()) {
                Some(c) => Value::Int(c as i64),
                None => Value::Null,
            },
            "STR" => match arg(0) {
                Value::Null => Value::Null,
                other => {
                    let length = v.get(1).map(|l| l.as_i64()).transpose()?.flatten().unwrap_or(10) as usize;
                    let decimals = v.get(2).map(|l| l.as_i64()).transpose()?.flatten().unwrap_or(0) as usize;
                    let f = other.convert(&SqlType::Float)?;
                    let Value::Float(f) = f else { return Ok(Value::Null) };
                    Value::Str(format!("{f:>length$.decimals$}"))
                }
            },
            "FORMAT" => match (&arg(0), text(&arg(1))) {
                (Value::Null, _) => Value::Null,
                (value, Some(format)) => Value::Str(format_value(value, &format)),
                _ => Value::Null,
            },
            "STRING_ESCAPE" => match text(&arg(0)) {
                Some(s) => Value::Str(serde_json::to_string(&s).unwrap_or_default().trim_matches('"').to_string()),
                None => Value::Null,
            },
            "TRANSLATE" => match (text(&arg(0)), text(&arg(1)), text(&arg(2))) {
                (Some(s), Some(from), Some(to)) => {
                    let from: Vec<char> = from.chars().collect();
                    let to: Vec<char> = to.chars().collect();
                    if from.len() != to.len() {
                        return Err(SqlError::new(9828, 16, "The second and third arguments of the TRANSLATE built-in function must contain an equal number of characters."));
                    }
                    Value::Str(s.chars().map(|c| from.iter().position(|f| *f == c).map_or(c, |i| to[i])).collect())
                }
                _ => Value::Null,
            },
            // ----- numbers -----
            "ABS" => match arg(0) {
                Value::Int(i) => Value::Int(i.abs()),
                Value::Decimal(d) => Value::Decimal(d.abs()),
                Value::Float(f) => Value::Float(f.abs()),
                Value::Null => Value::Null,
                other => other.convert(&SqlType::Float).map(|f| match f {
                    Value::Float(f) => Value::Float(f.abs()),
                    x => x,
                })?,
            },
            "CEILING" | "FLOOR" => match arg(0) {
                Value::Int(i) => Value::Int(i),
                Value::Decimal(d) => Value::Decimal(if name == "CEILING" { d.ceil() } else { d.floor() }),
                Value::Float(f) => Value::Float(if name == "CEILING" { f.ceil() } else { f.floor() }),
                Value::Null => Value::Null,
                other => other.convert(&SqlType::Float)?,
            },
            "ROUND" => {
                let digits = arg(1).as_i64()?.unwrap_or(0);
                let truncate = v.get(2).map(|x| x.as_i64()).transpose()?.flatten().unwrap_or(0) != 0;
                match arg(0) {
                    Value::Null => Value::Null,
                    Value::Int(i) if digits >= 0 => Value::Int(i),
                    Value::Int(i) => {
                        let factor = 10i64.pow((-digits) as u32);
                        Value::Int(if truncate { i / factor * factor } else { ((i as f64 / factor as f64).round() as i64) * factor })
                    }
                    Value::Decimal(d) => {
                        let scale = d.scale();
                        let mut r = if digits >= 0 {
                            if truncate {
                                d.trunc_with_scale(digits as u32)
                            } else {
                                d.round_dp_with_strategy(digits as u32, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
                            }
                        } else {
                            let factor = Decimal::from(10i64.pow((-digits) as u32));
                            let q = d / factor;
                            (if truncate { q.trunc() } else { q.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero) }) * factor
                        };
                        r.rescale(scale);
                        Value::Decimal(r)
                    }
                    other => {
                        let Value::Float(f) = other.convert(&SqlType::Float)? else { return Ok(Value::Null) };
                        let factor = 10f64.powi(digits as i32);
                        Value::Float(if truncate { (f * factor).trunc() / factor } else { (f * factor).round() / factor })
                    }
                }
            }
            "POWER" | "SQRT" | "EXP" | "LOG" | "LOG10" | "SIN" | "COS" | "TAN" | "SQUARE" => {
                let x = match arg(0).convert(&SqlType::Float)? {
                    Value::Float(f) => f,
                    _ => return Ok(Value::Null),
                };
                let result = match name {
                    "POWER" => {
                        let y = match arg(1).convert(&SqlType::Float)? {
                            Value::Float(f) => f,
                            _ => return Ok(Value::Null),
                        };
                        let r = x.powf(y);
                        if matches!(arg(0), Value::Int(_)) {
                            return Ok(Value::Int(r as i64));
                        }
                        r
                    }
                    "SQRT" => x.sqrt(),
                    "EXP" => x.exp(),
                    "LOG" => match v.get(1) {
                        Some(b) => match b.convert(&SqlType::Float)? {
                            Value::Float(b) => x.log(b),
                            _ => return Ok(Value::Null),
                        },
                        None => x.ln(),
                    },
                    "LOG10" => x.log10(),
                    "SIN" => x.sin(),
                    "COS" => x.cos(),
                    "TAN" => x.tan(),
                    _ => x * x,
                };
                Value::Float(result)
            }
            "SIGN" => match arg(0) {
                Value::Null => Value::Null,
                Value::Int(i) => Value::Int(i.signum()),
                Value::Decimal(d) => Value::Decimal(if d.is_zero() { Decimal::ZERO } else if d.is_sign_negative() { Decimal::NEGATIVE_ONE } else { Decimal::ONE }),
                other => match other.convert(&SqlType::Float)? {
                    Value::Float(f) => Value::Float(if f == 0.0 { 0.0 } else { f.signum() }),
                    x => x,
                },
            },
            "PI" => Value::Float(std::f64::consts::PI),
            "RAND" => Value::Float(rand_float()),
            "ISNUMERIC" => Value::Int(match arg(0) {
                Value::Null => 0,
                Value::Str(s) => s.trim().parse::<f64>().is_ok() as i64,
                Value::Int(_) | Value::Decimal(_) | Value::Float(_) | Value::Bit(_) => 1,
                _ => 0,
            }),
            // ----- dates -----
            "GETDATE" | "CURRENT_TIMESTAMP" | "SYSDATETIME" => Value::DateTime(chrono::Local::now().naive_local()),
            "GETUTCDATE" | "SYSUTCDATETIME" => Value::DateTime(chrono::Utc::now().naive_utc()),
            "SYSDATETIMEOFFSET" => Value::DateTimeOffset(chrono::Local::now().fixed_offset()),
            "YEAR" | "MONTH" | "DAY" => {
                count(1)?;
                match arg(0).convert(&SqlType::DateTime2(7))? {
                    Value::DateTime(dt) => Value::Int(match name {
                        "YEAR" => dt.year() as i64,
                        "MONTH" => dt.month() as i64,
                        _ => dt.day() as i64,
                    }),
                    _ => Value::Null,
                }
            }
            "EOMONTH" => match arg(0).convert(&SqlType::Date)? {
                Value::Date(d) => {
                    let months = v.get(1).map(|m| m.as_i64()).transpose()?.flatten().unwrap_or(0);
                    let d = add_months(d.and_time(NaiveTime::MIN), months).date();
                    let first_next = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1) };
                    first_next.and_then(|f| f.pred_opt()).map_or(Value::Null, Value::Date)
                }
                _ => Value::Null,
            },
            "DATEFROMPARTS" => match (arg(0).as_i64()?, arg(1).as_i64()?, arg(2).as_i64()?) {
                (Some(y), Some(m), Some(d)) => NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
                    .map(Value::Date)
                    .ok_or_else(|| SqlError::new(289, 16, "Cannot construct data type date, some of the arguments have values which are not valid."))?,
                _ => Value::Null,
            },
            "DATETIME2FROMPARTS" | "DATETIMEFROMPARTS" => {
                let parts: Vec<i64> = v.iter().take(7).map(|x| x.as_i64().ok().flatten().unwrap_or(0)).collect();
                let get = |i: usize| parts.get(i).copied().unwrap_or(0);
                NaiveDate::from_ymd_opt(get(0) as i32, get(1) as u32, get(2) as u32)
                    .and_then(|d| d.and_hms_opt(get(3) as u32, get(4) as u32, get(5) as u32))
                    .map(Value::DateTime)
                    .ok_or_else(|| SqlError::new(289, 16, "Cannot construct data type datetime2, some of the arguments have values which are not valid."))?
            }
            "ISDATE" => Value::Int(match arg(0) {
                Value::Str(s) => crate::value::parse_datetime(&s).is_some() as i64,
                Value::DateTime(_) | Value::Date(_) => 1,
                _ => 0,
            }),
            // ----- identifiers -----
            "NEWID" | "NEWSEQUENTIALID" => Value::Guid(uuid::Uuid::new_v4()),
            // ----- JSON -----
            "JSON_VALUE" => super::json::json_value(&arg(0), &arg(1))?,
            "JSON_QUERY" => super::json::json_query(&arg(0), v.get(1).cloned())?,
            "ISJSON" => super::json::is_json(&arg(0)),
            "JSON_MODIFY" => super::json::json_modify(&arg(0), &arg(1), &arg(2), args.get(2))?,
            "JSON_OBJECT" | "JSON_ARRAY" => {
                return Err(SqlError::new(40517, 16, format!("{name} is not supported by the Azure emulator's SQL data plane yet.")).batch_abort())
            }
            // ----- errors and transactions -----
            "ERROR_MESSAGE" => self.caught.last().map_or(Value::Null, |e| Value::Str(e.message.clone())),
            "ERROR_NUMBER" => self.caught.last().map_or(Value::Null, |e| Value::Int(e.number as i64)),
            "ERROR_SEVERITY" => self.caught.last().map_or(Value::Null, |e| Value::Int(e.class as i64)),
            "ERROR_STATE" => self.caught.last().map_or(Value::Null, |e| Value::Int(e.state as i64)),
            "ERROR_LINE" => self.caught.last().map_or(Value::Null, |e| Value::Int(e.line as i64)),
            "ERROR_PROCEDURE" => self.caught.last().map_or(Value::Null, |e| if e.procedure.is_empty() { Value::Null } else { Value::Str(e.procedure.clone()) }),
            "XACT_STATE" => Value::Int(self.session.transaction.is_some() as i64),
            "SESSION_CONTEXT" => self.session.context.get(&text(&arg(0)).unwrap_or_default().to_lowercase()).cloned().unwrap_or(Value::Null),
            // ----- metadata -----
            "SCOPE_IDENTITY" => match &self.frame().scope_identity {
                Value::Null => Value::Null,
                v => v.convert(&SqlType::Decimal(38, 0))?,
            },
            "IDENT_CURRENT" => self.ident_current(&text(&arg(0)).unwrap_or_default())?,
            "DB_ID" => match v.first() {
                None => Value::Int(self.state.database(&self.current_db()).map_or(0, |d| d.id)),
                Some(name) => match text(name).and_then(|n| self.state.database(&n).map(|d| d.id)) {
                    Some(id) => Value::Int(id),
                    None => Value::Null,
                },
            },
            "DB_NAME" => match v.first() {
                None => Value::Str(self.current_db()),
                Some(id) => {
                    let id = id.as_i64()?;
                    self.state.databases.values().find(|d| Some(d.id) == id).map_or(Value::Null, |d| Value::Str(d.name.clone()))
                }
            },
            "OBJECT_ID" => self.object_id(&text(&arg(0)).unwrap_or_default(), v.get(1).and_then(Value::as_string).as_deref()),
            "OBJECT_NAME" => self.object_name(arg(0).as_i64()?.unwrap_or(0)),
            "OBJECT_SCHEMA_NAME" => self.object_schema_name(arg(0).as_i64()?.unwrap_or(0)),
            "OBJECT_DEFINITION" => self.object_definition(arg(0).as_i64()?.unwrap_or(0)),
            "SCHEMA_ID" => {
                let name = v.first().and_then(Value::as_string).unwrap_or_else(|| "dbo".into());
                let db = self.current_db();
                match self.state.database(&db) {
                    Some(d) if d.has_schema(&name) => Value::Int(d.schema_id(&name)),
                    _ => Value::Null,
                }
            }
            "SCHEMA_NAME" => match v.first() {
                None => Value::Str("dbo".into()),
                Some(id) => {
                    let id = id.as_i64()?.unwrap_or(0);
                    let db = self.current_db();
                    self.state.database(&db).and_then(|d| d.schemas.iter().find(|s| d.schema_id(s) == id).cloned()).map_or(Value::Null, Value::Str)
                }
            },
            "COL_LENGTH" => self.col_length(&text(&arg(0)).unwrap_or_default(), &text(&arg(1)).unwrap_or_default()),
            "COL_NAME" => self.col_name(arg(0).as_i64()?.unwrap_or(0), arg(1).as_i64()?.unwrap_or(0)),
            "SUSER_SNAME" | "SUSER_NAME" | "ORIGINAL_LOGIN" => Value::Str(self.session.login.clone()),
            "USER_NAME" | "CURRENT_USER" => Value::Str("dbo".into()),
            "HOST_NAME" => Value::Str(self.session.host_name.clone()),
            "APP_NAME" => Value::Str(self.session.app_name.clone()),
            "SERVERPROPERTY" => server_property(&text(&arg(0)).unwrap_or_default()),
            "DATABASEPROPERTYEX" => match text(&arg(1)).unwrap_or_default().to_lowercase().as_str() {
                "status" if text(&arg(0)).is_some_and(|n| self.state.database(&n).is_some()) => Value::Str("ONLINE".into()),
                "collation" => Value::Str("SQL_Latin1_General_CP1_CI_AS".into()),
                "updateability" => Value::Str("READ_WRITE".into()),
                "useraccess" => Value::Str("MULTI_USER".into()),
                _ => Value::Null,
            },
            "HAS_DBACCESS" => Value::Int(1),
            "IS_MEMBER" | "IS_SRVROLEMEMBER" | "IS_ROLEMEMBER" => Value::Int(1),
            "HASHBYTES" | "CHECKSUM" | "BINARY_CHECKSUM" | "COMPRESS" | "DECOMPRESS" => {
                return Err(SqlError::new(40517, 16, format!("{name} is not supported by the Azure emulator's SQL data plane yet.")).batch_abort())
            }
            "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "NTILE" | "LAG" | "LEAD" => {
                return Err(SqlError::new(10753, 15, format!("The function '{}' must have an OVER clause.", name.to_lowercase())).batch_abort())
            }
            _ => {
                return Err(SqlError::new(195, 15, format!("'{name}' is not a recognized built-in function name.")).batch_abort());
            }
        })
    }

    /// The declared type of an expression, where it is known without running it.
    pub(super) fn static_type(&self, expr: &Expr, scope: &Scope) -> Option<SqlType> {
        match expr {
            Expr::Column(parts) => scope.column_type(parts).map(|(t, _)| t),
            Expr::Var(name) => match self.frame().vars.get(&name.to_lowercase()) {
                Some(Variable::Scalar { ty, .. }) => Some(ty.clone()),
                _ => None,
            },
            Expr::String { value, national } => {
                let length = value.chars().count().max(1);
                if *national {
                    Some(SqlType::NVarChar(u16::try_from(length).ok().filter(|l| *l <= 4000)))
                } else {
                    Some(SqlType::VarChar(u16::try_from(length).ok().filter(|l| *l <= 8000)))
                }
            }
            Expr::Literal(Value::Null) => None,
            Expr::Literal(v) => Some(v.natural_type()),
            Expr::Cast { ty, .. } => Some(ty.clone()),
            Expr::Compare { .. } | Expr::And(..) | Expr::Or(..) | Expr::Not(_) | Expr::Exists(_) | Expr::IsNull { .. } | Expr::Like { .. } | Expr::InList { .. } | Expr::Between { .. } => {
                Some(SqlType::Bit)
            }
            Expr::Arith { op, left, right } => {
                let l = self.static_type(left, scope)?;
                let r = self.static_type(right, scope)?;
                if *op == ArithOp::Add && l.is_string() && r.is_string() {
                    let unicode = l.is_unicode() || r.is_unicode();
                    let length = match (l.max_length(), r.max_length()) {
                        (Some(a), Some(b)) if a + b <= if unicode { 4000 } else { 8000 } => Some((a + b) as u16),
                        _ => None,
                    };
                    return Some(if unicode { SqlType::NVarChar(length) } else { SqlType::VarChar(length) });
                }
                let wider = if l.precedence() >= r.precedence() { l } else { r };
                match wider {
                    SqlType::TinyInt | SqlType::SmallInt | SqlType::Bit => Some(SqlType::Int),
                    t if t.is_string() => None,
                    t => Some(t),
                }
            }
            Expr::Unary { expr, .. } => self.static_type(expr, scope),
            Expr::Case { branches, otherwise, .. } => {
                let mut best: Option<SqlType> = None;
                for t in branches.iter().map(|(_, then)| then).chain(otherwise.as_deref()).filter_map(|e| self.static_type(e, scope)) {
                    best = Some(match best {
                        Some(b) if b.precedence() >= t.precedence() => {
                            if let (Some(a), Some(c)) = (b.max_length(), t.max_length()) {
                                if c > a && b.is_string() {
                                    t_with_length(&b, c)
                                } else {
                                    b
                                }
                            } else {
                                b
                            }
                        }
                        _ => t,
                    });
                }
                best
            }
            Expr::Subquery(query) if query.for_json.is_some() => Some(SqlType::NVarChar(None)),
            Expr::Function { name, args, .. } => {
                let first = || args.first().and_then(|a| self.static_type(a, scope));
                match name.as_str() {
                    "COUNT" | "LEN" | "CHARINDEX" | "DATEDIFF" | "DATEPART" | "YEAR" | "MONTH" | "DAY" | "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "ISJSON"
                    | "DB_ID" | "OBJECT_ID" | "XACT_STATE" | "ERROR_NUMBER" | "ERROR_SEVERITY" | "ERROR_STATE" | "ERROR_LINE" | "ISNUMERIC" | "ISDATE"
                    | "ASCII" | "UNICODE" | "PATINDEX" | "SCHEMA_ID" => Some(SqlType::Int),
                    "COUNT_BIG" | "DATEDIFF_BIG" => Some(SqlType::BigInt),
                    "SCOPE_IDENTITY" | "IDENT_CURRENT" => Some(SqlType::Decimal(38, 0)),
                    "JSON_VALUE" => Some(SqlType::NVarChar(Some(4000))),
                    "JSON_QUERY" | "JSON_MODIFY" | "STRING_AGG" | "STRING_AGG#WITHIN" | "CONCAT" | "CONCAT_WS" | "REPLACE" | "REPLICATE" | "STUFF"
                    | "FORMAT" | "OBJECT_DEFINITION" | "ERROR_MESSAGE" => Some(SqlType::NVarChar(None)),
                    "GETDATE" | "GETUTCDATE" | "CURRENT_TIMESTAMP" => Some(SqlType::DateTime),
                    "SYSDATETIME" | "SYSUTCDATETIME" => Some(SqlType::DateTime2(7)),
                    "SYSDATETIMEOFFSET" => Some(SqlType::DateTimeOffset(7)),
                    "NEWID" | "NEWSEQUENTIALID" => Some(SqlType::UniqueIdentifier),
                    "DB_NAME" | "OBJECT_NAME" | "SCHEMA_NAME" | "SUSER_SNAME" | "SUSER_NAME" | "USER_NAME" | "HOST_NAME" | "APP_NAME" | "ERROR_PROCEDURE"
                    | "QUOTENAME" | "OBJECT_SCHEMA_NAME" | "COL_NAME" => Some(SqlType::NVarChar(Some(128))),
                    "EOMONTH" | "DATEFROMPARTS" => Some(SqlType::Date),
                    "DATEADD" => args.get(2).and_then(|a| self.static_type(a, scope)),
                    "ISNULL" | "MIN" | "MAX" | "UPPER" | "LOWER" | "LTRIM" | "RTRIM" | "TRIM" | "LEFT" | "RIGHT" | "SUBSTRING" | "REVERSE" | "ABS"
                    | "CEILING" | "FLOOR" | "ROUND" | "NULLIF" => first(),
                    "SUM" => match first() {
                        Some(SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::Bit) => Some(SqlType::Int),
                        Some(SqlType::Decimal(_, s)) => Some(SqlType::Decimal(38, s)),
                        other => other,
                    },
                    "AVG" => match first() {
                        Some(SqlType::TinyInt | SqlType::SmallInt | SqlType::Int | SqlType::Bit) => Some(SqlType::Int),
                        Some(SqlType::Decimal(_, s)) => Some(SqlType::Decimal(38, s.max(6))),
                        other => other,
                    },
                    "COALESCE" => args.iter().filter_map(|a| self.static_type(a, scope)).max_by_key(|t| t.precedence()),
                    "IIF" => args.get(1).and_then(|a| self.static_type(a, scope)),
                    _ => None,
                }
            }
            Expr::Global(name) => match name.as_str() {
                "ROWCOUNT" | "TRANCOUNT" | "SPID" | "ERROR" => Some(SqlType::Int),
                "IDENTITY" => Some(SqlType::Decimal(38, 0)),
                "VERSION" | "SERVERNAME" => Some(SqlType::NVarChar(Some(300))),
                _ => None,
            },
            _ => None,
        }
    }
}

fn t_with_length(t: &SqlType, length: usize) -> SqlType {
    match t {
        SqlType::VarChar(_) => SqlType::VarChar(u16::try_from(length).ok()),
        SqlType::NVarChar(_) => SqlType::NVarChar(u16::try_from(length).ok()),
        other => other.clone(),
    }
}

fn map_text(v: &Value, f: impl Fn(&str) -> String) -> Value {
    match v.as_string() {
        Some(s) => Value::Str(f(&s)),
        None => Value::Null,
    }
}

fn rand_float() -> f64 {
    let bytes = uuid::Uuid::new_v4();
    let n = u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap_or([0; 8]));
    (n >> 11) as f64 / (1u64 << 53) as f64
}

/// `REPLACE` under a case-insensitive collation.
fn replace_ci(s: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return s.to_string();
    }
    let lower = s.to_lowercase();
    let needle = from.to_lowercase();
    if lower.len() != s.len() {
        return s.replace(from, to);
    }
    let mut out = String::new();
    let mut i = 0;
    while let Some(found) = lower[i..].find(&needle) {
        out.push_str(&s[i..i + found]);
        out.push_str(to);
        i += found + needle.len();
    }
    out.push_str(&s[i..]);
    out
}

/// `LIKE`, case-insensitive: `%`, `_`, `[set]`, `[^set]` and an escape character.
pub fn like(value: &str, pattern: &str, escape: Option<char>) -> bool {
    let v: Vec<char> = value.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let v: Vec<char> = {
        // Trailing spaces in the value are ignored when the pattern does not end in a space.
        let mut v = v;
        if p.last() != Some(&' ') {
            while v.last() == Some(&' ') {
                v.pop();
            }
        }
        v
    };
    fn matches(v: &[char], p: &[char], escape: Option<char>) -> bool {
        if p.is_empty() {
            return v.is_empty();
        }
        if Some(p[0]) == escape && p.len() > 1 {
            return !v.is_empty() && v[0] == p[1] && matches(&v[1..], &p[2..], escape);
        }
        match p[0] {
            '%' => (0..=v.len()).any(|i| matches(&v[i..], &p[1..], escape)),
            '_' => !v.is_empty() && matches(&v[1..], &p[1..], escape),
            '[' => {
                let Some(close) = p.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) else {
                    return !v.is_empty() && v[0] == '[' && matches(&v[1..], &p[1..], escape);
                };
                if v.is_empty() {
                    return false;
                }
                let set = &p[1..close];
                let (negate, set) = if set.first() == Some(&'^') { (true, &set[1..]) } else { (false, set) };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        if set[i] <= v[0] && v[0] <= set[i + 2] {
                            hit = true;
                        }
                        i += 3;
                    } else {
                        if set[i] == v[0] {
                            hit = true;
                        }
                        i += 1;
                    }
                }
                hit != negate && matches(&v[1..], &p[close + 1..], escape)
            }
            c => !v.is_empty() && v[0] == c && matches(&v[1..], &p[1..], escape),
        }
    }
    matches(&v, &p, escape)
}

fn server_property(name: &str) -> Value {
    match name.to_lowercase().as_str() {
        "productversion" => Value::Str("16.0.1000.6".into()),
        "productmajorversion" => Value::Str("16".into()),
        "productminorversion" => Value::Str("0".into()),
        "productlevel" => Value::Str("RTM".into()),
        "edition" => Value::Str("Developer Edition (64-bit)".into()),
        "engineedition" => Value::Int(3),
        "machinename" | "servername" | "instancename" => Value::Str("azure-emulator".into()),
        "collation" => Value::Str("SQL_Latin1_General_CP1_CI_AS".into()),
        "isclustered" | "ishadrenabled" | "isintegratedsecurityonly" | "isfulltextinstalled" => Value::Int(0),
        _ => Value::Null,
    }
}

fn add_months(dt: NaiveDateTime, months: i64) -> NaiveDateTime {
    let total = dt.year() as i64 * 12 + dt.month0() as i64 + months;
    let year = total.div_euclid(12) as i32;
    let month = total.rem_euclid(12) as u32 + 1;
    let mut day = dt.day();
    loop {
        if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
            return d.and_time(dt.time());
        }
        day -= 1;
        if day == 0 {
            return dt;
        }
    }
}

fn canonical_part(part: &str) -> Option<&'static str> {
    Some(match part {
        "year" | "yy" | "yyyy" => "year",
        "quarter" | "qq" | "q" => "quarter",
        "month" | "mm" | "m" => "month",
        "dayofyear" | "dy" | "y" => "dayofyear",
        "day" | "dd" | "d" => "day",
        "week" | "wk" | "ww" => "week",
        "weekday" | "dw" | "w" => "weekday",
        "hour" | "hh" => "hour",
        "minute" | "mi" | "n" => "minute",
        "second" | "ss" | "s" => "second",
        "millisecond" | "ms" => "millisecond",
        "microsecond" | "mcs" => "microsecond",
        "nanosecond" | "ns" => "nanosecond",
        _ => return None,
    })
}

fn date_function(name: &str, part: &str, args: &[Value]) -> Result<Value, SqlError> {
    let Some(part) = canonical_part(part) else {
        return Err(SqlError::new(155, 15, format!("'{part}' is not a recognized {} option.", name.to_lowercase())).batch_abort());
    };
    let to_datetime = |v: &Value| -> Result<Option<NaiveDateTime>, SqlError> {
        match v {
            Value::Null => Ok(None),
            Value::DateTimeOffset(dt) => Ok(Some(dt.naive_local())),
            other => match other.convert(&SqlType::DateTime2(7))? {
                Value::DateTime(dt) => Ok(Some(dt)),
                _ => Ok(None),
            },
        }
    };
    match name {
        "DATEADD" => {
            let n = match args.first() {
                Some(v) => v.as_i64()?,
                None => None,
            };
            let original = args.get(1).cloned().unwrap_or(Value::Null);
            let (Some(n), Some(dt)) = (n, to_datetime(&original)?) else { return Ok(Value::Null) };
            let result = match part {
                "year" => add_months(dt, n * 12),
                "quarter" => add_months(dt, n * 3),
                "month" => add_months(dt, n),
                "day" | "dayofyear" | "weekday" => dt + Duration::days(n),
                "week" => dt + Duration::weeks(n),
                "hour" => dt + Duration::hours(n),
                "minute" => dt + Duration::minutes(n),
                "second" => dt + Duration::seconds(n),
                "millisecond" => dt + Duration::milliseconds(n),
                "microsecond" => dt + Duration::microseconds(n),
                _ => dt + Duration::nanoseconds(n),
            };
            Ok(match original {
                Value::Date(_) => Value::Date(result.date()),
                Value::Time(_) => Value::Time(result.time()),
                Value::DateTimeOffset(o) => Value::DateTimeOffset(result.and_local_timezone(*o.offset()).single().unwrap_or(o)),
                Value::Str(_) => Value::DateTime(crate::value::round_datetime(result)),
                _ => Value::DateTime(result),
            })
        }
        "DATEDIFF" | "DATEDIFF_BIG" => {
            let (Some(a), Some(b)) = (to_datetime(args.first().unwrap_or(&Value::Null))?, to_datetime(args.get(1).unwrap_or(&Value::Null))?) else {
                return Ok(Value::Null);
            };
            let months = |d: &NaiveDateTime| d.year() as i64 * 12 + d.month0() as i64;
            let diff = match part {
                "year" => (b.year() - a.year()) as i64,
                "quarter" => (b.year() as i64 * 4 + b.month0() as i64 / 3) - (a.year() as i64 * 4 + a.month0() as i64 / 3),
                "month" => months(&b) - months(&a),
                "day" | "dayofyear" | "weekday" => (b.date() - a.date()).num_days(),
                "week" => {
                    let week_start = |d: NaiveDate| d - Duration::days(d.weekday().num_days_from_sunday() as i64);
                    (week_start(b.date()) - week_start(a.date())).num_days() / 7
                }
                "hour" => (b.date().and_hms_opt(b.hour(), 0, 0).unwrap_or(b) - a.date().and_hms_opt(a.hour(), 0, 0).unwrap_or(a)).num_hours(),
                "minute" => (b.date().and_hms_opt(b.hour(), b.minute(), 0).unwrap_or(b) - a.date().and_hms_opt(a.hour(), a.minute(), 0).unwrap_or(a)).num_minutes(),
                "second" => (b.with_nanosecond(0).unwrap_or(b) - a.with_nanosecond(0).unwrap_or(a)).num_seconds(),
                "millisecond" => (b - a).num_milliseconds(),
                "microsecond" => (b - a).num_microseconds().unwrap_or(i64::MAX),
                _ => (b - a).num_nanoseconds().unwrap_or(i64::MAX),
            };
            if name == "DATEDIFF" && i32::try_from(diff).is_err() {
                return Err(SqlError::new(535, 16, "The datediff function resulted in an overflow. The number of dateparts separating two date/time instances is too large. Try to use datediff with a less precise datepart."));
            }
            Ok(Value::Int(diff))
        }
        "DATEPART" | "DATENAME" => {
            let Some(dt) = to_datetime(args.first().unwrap_or(&Value::Null))? else { return Ok(Value::Null) };
            let n = match part {
                "year" => dt.year() as i64,
                "quarter" => dt.month0() as i64 / 3 + 1,
                "month" => dt.month() as i64,
                "dayofyear" => dt.ordinal() as i64,
                "day" => dt.day() as i64,
                "week" => {
                    let jan1 = NaiveDate::from_ymd_opt(dt.year(), 1, 1).unwrap_or(dt.date());
                    ((dt.ordinal() as i64 - 1 + jan1.weekday().num_days_from_sunday() as i64) / 7) + 1
                }
                "weekday" => dt.weekday().num_days_from_sunday() as i64 + 1,
                "hour" => dt.hour() as i64,
                "minute" => dt.minute() as i64,
                "second" => dt.second() as i64,
                "millisecond" => (dt.nanosecond() / 1_000_000) as i64,
                "microsecond" => (dt.nanosecond() / 1_000) as i64,
                _ => dt.nanosecond() as i64,
            };
            if name == "DATENAME" {
                return Ok(Value::Str(match part {
                    "month" => dt.format("%B").to_string(),
                    "weekday" => dt.format("%A").to_string(),
                    _ => n.to_string(),
                }));
            }
            Ok(Value::Int(n))
        }
        _ => {
            let Some(dt) = to_datetime(args.first().unwrap_or(&Value::Null))? else { return Ok(Value::Null) };
            let truncated = match part {
                "year" => NaiveDate::from_ymd_opt(dt.year(), 1, 1).map(|d| d.and_time(NaiveTime::MIN)),
                "quarter" => NaiveDate::from_ymd_opt(dt.year(), dt.month0() / 3 * 3 + 1, 1).map(|d| d.and_time(NaiveTime::MIN)),
                "month" => NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).map(|d| d.and_time(NaiveTime::MIN)),
                "day" | "dayofyear" => Some(dt.date().and_time(NaiveTime::MIN)),
                "hour" => dt.date().and_hms_opt(dt.hour(), 0, 0),
                "minute" => dt.date().and_hms_opt(dt.hour(), dt.minute(), 0),
                "second" => dt.with_nanosecond(0),
                _ => Some(dt),
            };
            Ok(truncated.map_or(Value::Null, Value::DateTime))
        }
    }
}

/// `CONVERT` with a style number for dates and times.
pub fn convert_with_style(v: &Value, ty: &SqlType, style: Option<i64>) -> Result<Value, SqlError> {
    if let (Some(style), true) = (style, ty.is_string()) {
        let dt = match v {
            Value::DateTime(dt) => Some(*dt),
            Value::Date(d) => Some(d.and_time(NaiveTime::MIN)),
            Value::DateTimeOffset(dt) => Some(dt.naive_local()),
            _ => None,
        };
        if let Some(dt) = dt {
            let text = match style {
                1 => dt.format("%m/%d/%y").to_string(),
                101 => dt.format("%m/%d/%Y").to_string(),
                3 => dt.format("%d/%m/%y").to_string(),
                103 => dt.format("%d/%m/%Y").to_string(),
                12 => dt.format("%y%m%d").to_string(),
                112 => dt.format("%Y%m%d").to_string(),
                8 | 108 => dt.format("%H:%M:%S").to_string(),
                20 | 120 => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                21 | 121 => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
                23 => dt.format("%Y-%m-%d").to_string(),
                126 | 127 => {
                    let mut s = dt.format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
                    if s.ends_with(".000") {
                        s.truncate(s.len() - 4);
                    }
                    if style == 127 {
                        s.push('Z');
                    }
                    s
                }
                _ => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            };
            return Value::Str(text).convert(ty);
        }
        if let (Value::Bytes(b), 1 | 2) = (v, style) {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            return Value::Str(if style == 1 { format!("0x{hex}") } else { hex }).convert(ty);
        }
    }
    if let (Some(1 | 2), Value::Str(s), SqlType::Binary(_) | SqlType::VarBinary(_)) = (style, v, ty) {
        let hex = s.trim_start_matches("0x").trim_start_matches("0X");
        let bytes: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
        return bytes.map(Value::Bytes).ok_or_else(|| SqlError::new(8114, 16, "Error converting data type varchar to varbinary."));
    }
    v.convert(ty)
}

fn format_value(value: &Value, format: &str) -> String {
    match value {
        Value::DateTime(dt) => {
            let f = format.replace("yyyy", "%Y").replace("MM", "%m").replace("dd", "%d").replace("HH", "%H").replace("mm", "%M").replace("ss", "%S");
            dt.format(&f).to_string()
        }
        Value::Date(d) => {
            let f = format.replace("yyyy", "%Y").replace("MM", "%m").replace("dd", "%d");
            d.format(&f).to_string()
        }
        Value::Int(i) if format.chars().all(|c| c == '0') => format!("{:0width$}", i, width = format.len()),
        Value::Decimal(d) if format.starts_with('N') || format.starts_with('F') => {
            let places: u32 = format[1..].parse().unwrap_or(2);
            d.round_dp(places).to_string()
        }
        Value::Float(f) if format.starts_with('N') || format.starts_with('F') => {
            let places: usize = format[1..].parse().unwrap_or(2);
            format!("{f:.places$}")
        }
        other => other.to_text(),
    }
}

// Silence unused-import warnings for helpers only some builds use.
#[allow(dead_code)]
fn _unused(_: Decimal, _: Option<f64>) -> Option<Decimal> {
    let _ = base_1900();
    let _ = compare_text("", "");
    Decimal::from_f64(0.0).and_then(|d| d.to_f64()).and_then(Decimal::from_f64)
}
