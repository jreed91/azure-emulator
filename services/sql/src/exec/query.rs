//! `SELECT`: sources, joins, filtering, grouping, ordering and set operations.

use std::cmp::Ordering;

use super::{invalid_object, Executor, SqlError, TableLoc};
use crate::ast::*;
use crate::value::{same, sort_order, SqlType, Value};

/// One column of a row being processed.
#[derive(Clone, Debug)]
pub struct LayoutColumn {
    /// The table alias or name it can be qualified with, lowercased.
    pub qualifier: Option<String>,
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
    pub identity: bool,
    /// Not shown in results: a DML target's row id.
    pub hidden: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub columns: Vec<LayoutColumn>,
}

pub const ROWID: &str = "\u{0}rowid";

impl Layout {
    /// Finds a column by a possibly qualified name.
    pub fn resolve(&self, parts: &[String]) -> Result<Option<usize>, SqlError> {
        let (qualifier, name) = match parts {
            [name] => (None, name),
            [.., q, name] => (Some(q.to_lowercase()), name),
            [] => return Ok(None),
        };
        let mut found = None;
        for (i, column) in self.columns.iter().enumerate() {
            if !column.name.eq_ignore_ascii_case(name) {
                continue;
            }
            if let Some(q) = &qualifier {
                if column.qualifier.as_deref() != Some(q.as_str()) {
                    continue;
                }
            }
            if found.is_some() {
                return Err(SqlError::new(209, 16, format!("Ambiguous column name '{name}'.")).batch_abort());
            }
            found = Some(i);
        }
        Ok(found)
    }

    fn has_qualifier(&self, q: &str) -> bool {
        self.columns.iter().any(|c| c.qualifier.as_deref() == Some(q))
    }
}

/// Rows with their layout.
#[derive(Clone, Debug, Default)]
pub struct Rowset {
    pub layout: Layout,
    pub rows: Vec<Vec<Value>>,
}

/// What an expression can see: the current row, its group, and the rows of enclosing queries.
#[derive(Clone, Copy)]
pub struct Scope<'s> {
    pub layout: &'s Layout,
    pub row: &'s [Value],
    pub group: Option<&'s [Vec<Value>]>,
    pub outer: Option<&'s Scope<'s>>,
    pub row_index: usize,
}

pub static EMPTY_LAYOUT: Layout = Layout { columns: Vec::new() };

impl<'s> Scope<'s> {
    pub fn empty() -> Scope<'static> {
        Scope { layout: &EMPTY_LAYOUT, row: &[], group: None, outer: None, row_index: 0 }
    }

    pub fn lookup(&self, parts: &[String]) -> Result<Option<Value>, SqlError> {
        if let Some(i) = self.layout.resolve(parts)? {
            return Ok(Some(self.row.get(i).cloned().unwrap_or(Value::Null)));
        }
        match self.outer {
            Some(outer) => outer.lookup(parts),
            None => Ok(None),
        }
    }

    pub fn column_type(&self, parts: &[String]) -> Option<(SqlType, bool)> {
        if let Ok(Some(i)) = self.layout.resolve(parts) {
            let c = &self.layout.columns[i];
            return Some((c.ty.clone(), c.nullable));
        }
        self.outer.and_then(|o| o.column_type(parts))
    }

    fn knows_qualifier(&self, q: &str) -> bool {
        self.layout.has_qualifier(q) || self.outer.is_some_and(|o| o.knows_qualifier(q))
    }
}

pub fn unknown_column(parts: &[String], scope: &Scope) -> SqlError {
    if parts.len() > 1 && !scope.knows_qualifier(&parts[parts.len() - 2].to_lowercase()) {
        return SqlError::new(4104, 16, format!("The multi-part identifier \"{}\" could not be bound.", parts.join("."))).batch_abort();
    }
    SqlError::new(207, 16, format!("Invalid column name '{}'.", parts.last().cloned().unwrap_or_default())).batch_abort()
}

const AGGREGATES: &[&str] = &["COUNT", "COUNT_BIG", "SUM", "AVG", "MIN", "MAX", "STRING_AGG", "STRING_AGG#WITHIN", "STDEV", "VAR", "CHECKSUM_AGG"];

pub fn is_aggregate(name: &str) -> bool {
    AGGREGATES.contains(&name)
}

/// Whether the expression uses an aggregate of this query (not of a subquery).
pub fn has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args, over, .. } => ((over.is_none() || name.ends_with("#WITHIN")) && is_aggregate(name)) || args.iter().any(has_aggregate),
        Expr::Unary { expr, .. } | Expr::Not(expr) | Expr::IsNull { expr, .. } => has_aggregate(expr),
        Expr::Arith { left, right, .. } | Expr::Compare { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => has_aggregate(left) || has_aggregate(right),
        Expr::Like { expr, pattern, .. } => has_aggregate(expr) || has_aggregate(pattern),
        Expr::InList { expr, list, .. } => has_aggregate(expr) || list.iter().any(has_aggregate),
        Expr::InQuery { expr, .. } => has_aggregate(expr),
        Expr::Between { expr, low, high, .. } => has_aggregate(expr) || has_aggregate(low) || has_aggregate(high),
        Expr::Case { operand, branches, otherwise } => {
            operand.as_deref().is_some_and(has_aggregate) || branches.iter().any(|(a, b)| has_aggregate(a) || has_aggregate(b)) || otherwise.as_deref().is_some_and(has_aggregate)
        }
        Expr::Cast { expr, .. } => has_aggregate(expr),
        _ => false,
    }
}

fn collect_windows<'e>(expr: &'e Expr, found: &mut Vec<&'e Expr>) {
    match expr {
        Expr::Function { name, args, over, .. } => {
            if over.is_some() && !name.ends_with("#WITHIN") {
                found.push(expr);
            }
            for a in args {
                collect_windows(a, found);
            }
        }
        Expr::Unary { expr, .. } | Expr::Not(expr) | Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => collect_windows(expr, found),
        Expr::Arith { left, right, .. } | Expr::Compare { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => {
            collect_windows(left, found);
            collect_windows(right, found);
        }
        Expr::Case { operand, branches, otherwise } => {
            if let Some(o) = operand {
                collect_windows(o, found);
            }
            for (a, b) in branches {
                collect_windows(a, found);
                collect_windows(b, found);
            }
            if let Some(o) = otherwise {
                collect_windows(o, found);
            }
        }
        _ => {}
    }
}

impl Executor<'_> {
    /// Runs a query; `outer` is the row of an enclosing query, for correlated subqueries.
    pub(super) fn run_query(&mut self, query: &Query, outer: Option<&Scope>) -> Result<Rowset, SqlError> {
        match &query.body {
            QueryBody::Select(select) => self.run_select(select, &query.order_by, query.offset.as_ref(), query.fetch.as_ref(), outer),
            body => {
                let mut rowset = self.run_body(body, outer)?;
                if !query.order_by.is_empty() {
                    let mut keyed = Vec::new();
                    for row in rowset.rows.drain(..) {
                        let mut keys = Vec::new();
                        for item in &query.order_by {
                            keys.push(self.order_key_output(&item.expr, &rowset.layout, &row, outer)?);
                        }
                        keyed.push((keys, row));
                    }
                    sort_keyed(&mut keyed, &query.order_by);
                    rowset.rows = keyed.into_iter().map(|(_, r)| r).collect();
                }
                self.apply_offset_fetch(&mut rowset.rows, query.offset.as_ref(), query.fetch.as_ref())?;
                Ok(rowset)
            }
        }
    }

    fn run_body(&mut self, body: &QueryBody, outer: Option<&Scope>) -> Result<Rowset, SqlError> {
        match body {
            QueryBody::Select(select) => self.run_select(select, &[], None, None, outer),
            QueryBody::SetOp { op, all, left, right } => {
                let mut left = self.run_body(left, outer)?;
                let right = self.run_body(right, outer)?;
                if left.layout.columns.len() != right.layout.columns.len() {
                    return Err(SqlError::new(205, 16, "All queries combined using a UNION, INTERSECT or EXCEPT operator must have an equal number of expressions in their target lists.").batch_abort());
                }
                for (l, r) in left.layout.columns.iter_mut().zip(&right.layout.columns) {
                    if r.ty.precedence() > l.ty.precedence() {
                        l.ty = r.ty.clone();
                    }
                    l.nullable |= r.nullable;
                }
                let rows_equal = |a: &[Value], b: &[Value]| a.iter().zip(b).all(|(x, y)| same(x, y));
                let rows = match op {
                    SetOp::Union => {
                        let mut rows = left.rows;
                        rows.extend(right.rows);
                        if !all {
                            dedup(&mut rows);
                        }
                        rows
                    }
                    SetOp::Except => {
                        let mut rows: Vec<Vec<Value>> = left.rows.into_iter().filter(|l| !right.rows.iter().any(|r| rows_equal(l, r))).collect();
                        dedup(&mut rows);
                        rows
                    }
                    SetOp::Intersect => {
                        let mut rows: Vec<Vec<Value>> = left.rows.into_iter().filter(|l| right.rows.iter().any(|r| rows_equal(l, r))).collect();
                        dedup(&mut rows);
                        rows
                    }
                };
                Ok(Rowset { layout: left.layout, rows })
            }
        }
    }

    fn apply_offset_fetch(&mut self, rows: &mut Vec<Vec<Value>>, offset: Option<&Expr>, fetch: Option<&Expr>) -> Result<(), SqlError> {
        if let Some(offset) = offset {
            let n = self.eval_plain(offset)?.as_i64()?.unwrap_or(0).max(0) as usize;
            if n >= rows.len() {
                rows.clear();
            } else {
                rows.drain(..n);
            }
        }
        if let Some(fetch) = fetch {
            let n = self.eval_plain(fetch)?.as_i64()?.unwrap_or(0).max(0) as usize;
            rows.truncate(n);
        }
        Ok(())
    }

    fn order_key_output(&mut self, expr: &Expr, layout: &Layout, row: &[Value], outer: Option<&Scope>) -> Result<Value, SqlError> {
        if let Expr::Literal(Value::Int(n)) = expr {
            return row.get((*n as usize).wrapping_sub(1)).cloned().ok_or_else(|| {
                SqlError::new(108, 16, format!("The ORDER BY position number {n} is out of range of the number of items in the select list.")).batch_abort()
            });
        }
        let scope = Scope { layout, row, group: None, outer, row_index: 0 };
        self.eval(expr, &scope)
    }

    /// The rows of a `FROM` list, joined.
    pub(super) fn rows_from(&mut self, from: &[TableRef], outer: Option<&Scope>) -> Result<Rowset, SqlError> {
        if from.is_empty() {
            return Ok(Rowset { layout: Layout::default(), rows: vec![Vec::new()] });
        }
        let mut result = self.table_ref_rows(&from[0], outer)?;
        for item in &from[1..] {
            let right = self.table_ref_rows(item, outer)?;
            result = cross(result, right);
        }
        Ok(result)
    }

    fn table_ref_rows(&mut self, item: &TableRef, outer: Option<&Scope>) -> Result<Rowset, SqlError> {
        match item {
            TableRef::Table { name, alias } => self.named_rows(name, alias.as_deref()),
            TableRef::Derived { query, alias, columns } => {
                let mut rowset = self.run_query(query, outer)?;
                rename(&mut rowset, alias, columns)?;
                Ok(rowset)
            }
            TableRef::Values { rows, alias, columns } => {
                let mut values = Vec::new();
                for row in rows {
                    let mut out = Vec::new();
                    for e in row {
                        out.push(self.eval(e, &match outer {
                    Some(o) => *o,
                    None => Scope::empty(),
                })?);
                    }
                    values.push(out);
                }
                let width = values.first().map_or(0, |r| r.len());
                let layout = Layout {
                    columns: (0..width)
                        .map(|i| LayoutColumn {
                            qualifier: None,
                            name: columns.get(i).cloned().unwrap_or_default(),
                            ty: values.iter().map(|r| &r[i]).find(|v| !v.is_null()).map_or(SqlType::Int, Value::natural_type),
                            nullable: true,
                            identity: false,
                            hidden: false,
                        })
                        .collect(),
                };
                let mut rowset = Rowset { layout, rows: values };
                rename(&mut rowset, alias, columns)?;
                Ok(rowset)
            }
            TableRef::Function { name, args, alias, with } => {
                let scope = match outer {
                    Some(o) => *o,
                    None => Scope::empty(),
                };
                let mut values = Vec::new();
                for a in args {
                    values.push(self.eval(a, &scope)?);
                }
                let mut rowset = match name.as_str() {
                    "OPENJSON" => super::json::openjson(&values, with)?,
                    _ => {
                        let text = values.first().and_then(Value::as_string);
                        let separator = values.get(1).and_then(Value::as_string).unwrap_or_default();
                        let rows = match text {
                            Some(text) if !separator.is_empty() => text.split(separator.as_str()).map(|s| vec![Value::Str(s.to_string())]).collect(),
                            Some(text) => vec![vec![Value::Str(text)]],
                            None => Vec::new(),
                        };
                        Rowset {
                            layout: Layout {
                                columns: vec![LayoutColumn { qualifier: None, name: "value".into(), ty: SqlType::NVarChar(Some(4000)), nullable: true, identity: false, hidden: false }],
                            },
                            rows,
                        }
                    }
                };
                let alias = alias.clone().unwrap_or_else(|| name.clone());
                rename(&mut rowset, &alias, &[])?;
                Ok(rowset)
            }
            TableRef::Join { left, right, kind, on } => {
                let left = self.table_ref_rows(left, outer)?;
                if matches!(kind, JoinKind::CrossApply | JoinKind::OuterApply) {
                    let mut rows = Vec::new();
                    let mut right_layout: Option<Layout> = None;
                    for l in &left.rows {
                        let scope = Scope { layout: &left.layout, row: l, group: None, outer, row_index: 0 };
                        let r = self.table_ref_rows(right, Some(&scope))?;
                        if r.rows.is_empty() && *kind == JoinKind::OuterApply {
                            let mut row = l.clone();
                            row.extend(std::iter::repeat_n(Value::Null, r.layout.columns.len()));
                            rows.push(row);
                        }
                        for rr in &r.rows {
                            let mut row = l.clone();
                            row.extend(rr.iter().cloned());
                            rows.push(row);
                        }
                        right_layout.get_or_insert(r.layout);
                    }
                    let right_layout = match right_layout {
                        Some(layout) => layout,
                        None => {
                            // No left rows: the right side's shape, from one run without a row.
                            let empty: Vec<Value> = vec![Value::Null; left.layout.columns.len()];
                            let scope = Scope { layout: &left.layout, row: &empty, group: None, outer, row_index: 0 };
                            self.table_ref_rows(right, Some(&scope)).map(|r| r.layout).unwrap_or_default()
                        }
                    };
                    let mut layout = left.layout;
                    layout.columns.extend(right_layout.columns);
                    return Ok(Rowset { layout, rows });
                }
                let right = self.table_ref_rows(right, outer)?;
                let mut layout = left.layout.clone();
                layout.columns.extend(right.layout.columns.iter().cloned());
                if matches!(kind, JoinKind::Left | JoinKind::Full | JoinKind::Right) {
                    for c in &mut layout.columns {
                        c.nullable = true;
                    }
                }
                let left_width = left.layout.columns.len();
                let right_width = right.layout.columns.len();
                let mut rows = Vec::new();
                let mut right_matched = vec![false; right.rows.len()];
                for l in &left.rows {
                    let mut matched = false;
                    for (j, r) in right.rows.iter().enumerate() {
                        let mut row = l.clone();
                        row.extend(r.iter().cloned());
                        let keep = match on {
                            Some(on) => {
                                let scope = Scope { layout: &layout, row: &row, group: None, outer, row_index: 0 };
                                self.eval(on, &scope)?.truth() == Some(true)
                            }
                            None => true,
                        };
                        if keep {
                            matched = true;
                            right_matched[j] = true;
                            rows.push(row);
                        }
                    }
                    if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
                        let mut row = l.clone();
                        row.extend(std::iter::repeat_n(Value::Null, right_width));
                        rows.push(row);
                    }
                }
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    for (j, r) in right.rows.iter().enumerate() {
                        if !right_matched[j] {
                            let mut row = vec![Value::Null; left_width];
                            row.extend(r.iter().cloned());
                            rows.push(row);
                        }
                    }
                }
                Ok(Rowset { layout, rows })
            }
        }
    }

    /// The rows of a named source: a CTE, a table variable, a temp table, a system view or a table.
    fn named_rows(&mut self, name: &ObjectName, alias: Option<&str>) -> Result<Rowset, SqlError> {
        let qualifier = alias.unwrap_or(&name.name).to_lowercase();
        if name.schema.is_none() && name.database.is_none() {
            let key = name.name.to_lowercase();
            if let Some((_, rowset)) = self.frames.iter().rev().flat_map(|f| f.ctes.iter().rev()).find(|(n, _)| *n == key) {
                let mut rowset = rowset.clone();
                for c in &mut rowset.layout.columns {
                    c.qualifier = Some(qualifier.clone());
                }
                return Ok(rowset);
            }
        }
        if let Some(rowset) = self.system_view(name)? {
            let mut rowset = rowset;
            for c in &mut rowset.layout.columns {
                c.qualifier = Some(qualifier.clone());
            }
            return Ok(rowset);
        }
        let loc = self.resolve_table(name)?;
        let with_rowid = self.rowid_target.as_ref() == Some(&loc);
        let table = self.table(&loc).ok_or_else(|| invalid_object(&name.display()))?;
        let mut columns: Vec<LayoutColumn> = table
            .columns
            .iter()
            .map(|c| LayoutColumn { qualifier: Some(qualifier.clone()), name: c.name.clone(), ty: c.ty.clone(), nullable: c.nullable, identity: c.identity.is_some(), hidden: false })
            .collect();
        let order = table.scan_order();
        let mut rows = Vec::with_capacity(order.len());
        for id in order {
            let mut row = table.rows[&id].clone();
            if with_rowid {
                row.push(Value::Int(id as i64));
            }
            rows.push(row);
        }
        if with_rowid {
            columns.push(LayoutColumn { qualifier: Some(qualifier), name: ROWID.into(), ty: SqlType::BigInt, nullable: false, identity: false, hidden: true });
        }
        Ok(Rowset { layout: Layout { columns }, rows })
    }

    fn run_select(&mut self, select: &Select, order_by: &[OrderItem], offset: Option<&Expr>, fetch: Option<&Expr>, outer: Option<&Scope>) -> Result<Rowset, SqlError> {
        let source = self.rows_from(&select.from, outer)?;
        let mut rows = Vec::new();
        for row in source.rows {
            let keep = match &select.condition {
                Some(condition) => {
                    let scope = Scope { layout: &source.layout, row: &row, group: None, outer, row_index: 0 };
                    self.eval(condition, &scope)?.truth() == Some(true)
                }
                None => true,
            };
            if keep {
                rows.push(row);
            }
        }
        let layout = source.layout;

        // The output columns: wildcards expanded.
        let mut items: Vec<(Expr, String)> = Vec::new();
        for item in &select.items {
            match item {
                SelectItem::Wildcard(qualifier) => {
                    let q = qualifier.as_ref().map(|q| q.to_lowercase());
                    if let Some(q) = &q {
                        if !layout.has_qualifier(q) {
                            return Err(SqlError::new(4104, 16, format!("The multi-part identifier \"{q}\" could not be bound.")).batch_abort());
                        }
                    }
                    if select.from.is_empty() {
                        return Err(SqlError::new(263, 16, "Must specify table to select from.").batch_abort());
                    }
                    for c in &layout.columns {
                        if c.hidden || (q.is_some() && c.qualifier != q) {
                            continue;
                        }
                        let mut parts = Vec::new();
                        if let Some(cq) = &c.qualifier {
                            parts.push(cq.clone());
                        }
                        parts.push(c.name.clone());
                        items.push((Expr::Column(parts), c.name.clone()));
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let name = match (alias, expr) {
                        (Some(a), _) => a.clone(),
                        (None, Expr::Column(parts)) => {
                            let scope = Scope { layout: &layout, row: &[], group: None, outer, row_index: 0 };
                            match layout.resolve(parts)? {
                                Some(i) => layout.columns[i].name.clone(),
                                None if scope.column_type(parts).is_some() => parts.last().cloned().unwrap_or_default(),
                                None => parts.last().cloned().unwrap_or_default(),
                            }
                        }
                        _ => String::new(),
                    };
                    items.push((expr.clone(), name));
                }
            }
        }

        let grouped = !select.group_by.is_empty()
            || items.iter().any(|(e, _)| has_aggregate(e))
            || select.having.as_ref().is_some_and(has_aggregate)
            || order_by.iter().any(|o| has_aggregate(&o.expr));

        // Each output row, with the source row (or group) it came from for ORDER BY.
        let mut out_types: Vec<Option<SqlType>> = items.iter().map(|(e, _)| self.static_type(e, &Scope { layout: &layout, row: &[], group: None, outer, row_index: 0 })).collect();
        let output: Vec<(Vec<Value>, Vec<Value>)> = if grouped {
            let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
            for row in rows {
                let scope = Scope { layout: &layout, row: &row, group: None, outer, row_index: 0 };
                let mut key = Vec::new();
                for e in &select.group_by {
                    key.push(self.eval(e, &scope)?);
                }
                match groups.iter_mut().find(|(k, _)| k.iter().zip(&key).all(|(a, b)| same(a, b))) {
                    Some((_, members)) => members.push(row),
                    None => groups.push((key, vec![row])),
                }
            }
            if groups.is_empty() && select.group_by.is_empty() {
                groups.push((Vec::new(), Vec::new()));
            }
            let mut keyed = Vec::new();
            for (_, members) in &groups {
                let first = members.first().cloned().unwrap_or_else(|| vec![Value::Null; layout.columns.len()]);
                let scope = Scope { layout: &layout, row: &first, group: Some(members), outer, row_index: 0 };
                if let Some(having) = &select.having {
                    if self.eval(having, &scope)?.truth() != Some(true) {
                        continue;
                    }
                }
                let mut values = Vec::new();
                for (e, _) in &items {
                    values.push(self.eval(e, &scope)?);
                }
                let mut keys = Vec::new();
                for item in order_by {
                    keys.push(self.order_key(&item.expr, &items, &values, &scope)?);
                }
                keyed.push((keys, values));
            }
            if !order_by.is_empty() {
                sort_keyed(&mut keyed, order_by);
            }
            keyed.into_iter().map(|(k, v)| (v, k)).collect()
        } else {
            // Window functions, computed over the filtered rows.
            let mut windows = Vec::new();
            for (e, _) in &items {
                collect_windows(e, &mut windows);
            }
            for item in order_by {
                collect_windows(&item.expr, &mut windows);
            }
            for w in windows {
                let values = self.window_values(w, &layout, &rows, outer)?;
                self.windows.insert(w as *const Expr as usize, values);
            }
            let mut keyed = Vec::new();
            for (index, row) in rows.iter().enumerate() {
                let scope = Scope { layout: &layout, row, group: None, outer, row_index: index };
                let mut values = Vec::new();
                for (e, _) in &items {
                    values.push(self.eval(e, &scope)?);
                }
                let mut keys = Vec::new();
                for item in order_by {
                    keys.push(self.order_key(&item.expr, &items, &values, &scope)?);
                }
                keyed.push((keys, values));
            }
            if !order_by.is_empty() {
                sort_keyed(&mut keyed, order_by);
            }
            keyed.into_iter().map(|(k, v)| (v, k)).collect()
        };

        let mut out_rows: Vec<Vec<Value>> = output.into_iter().map(|(v, _)| v).collect();
        if select.distinct {
            dedup(&mut out_rows);
        }
        if let Some((top, percent)) = &select.top {
            let n = self.eval_plain(top)?.as_i64()?.unwrap_or(0).max(0) as usize;
            let n = if *percent { (out_rows.len() * n).div_ceil(100) } else { n };
            out_rows.truncate(n);
        }
        self.apply_offset_fetch(&mut out_rows, offset, fetch)?;

        let mut columns = Vec::new();
        for (i, (e, name)) in items.iter().enumerate() {
            let ty = out_types[i].take().unwrap_or_else(|| out_rows.iter().map(|r| &r[i]).find(|v| !v.is_null()).map_or(SqlType::Int, Value::natural_type));
            let (nullable, identity) = match e {
                Expr::Column(parts) => match layout.resolve(parts).ok().flatten() {
                    Some(c) => (layout.columns[c].nullable, layout.columns[c].identity),
                    None => (true, false),
                },
                _ => (true, false),
            };
            columns.push(LayoutColumn { qualifier: None, name: name.clone(), ty, nullable, identity, hidden: false });
        }
        Ok(Rowset { layout: Layout { columns }, rows: out_rows })
    }

    /// An ORDER BY key: a select-list alias, a position, or an expression over the source row.
    fn order_key(&mut self, expr: &Expr, items: &[(Expr, String)], values: &[Value], scope: &Scope) -> Result<Value, SqlError> {
        if let Expr::Literal(Value::Int(n)) = expr {
            return values.get((*n as usize).wrapping_sub(1)).cloned().ok_or_else(|| {
                SqlError::new(108, 16, format!("The ORDER BY position number {n} is out of range of the number of items in the select list.")).batch_abort()
            });
        }
        if let Expr::Column(parts) = expr {
            if parts.len() == 1 {
                if let Some(i) = items.iter().position(|(_, name)| name.eq_ignore_ascii_case(&parts[0])) {
                    // A source column of the same name wins only when the item is that column.
                    if scope.layout.resolve(parts).ok().flatten().is_none() || matches!(&items[i].0, Expr::Column(p) if p.last().is_some_and(|l| l.eq_ignore_ascii_case(&parts[0]))) {
                        return Ok(values[i].clone());
                    }
                }
            }
        }
        self.eval(expr, scope)
    }

    fn window_values(&mut self, expr: &Expr, layout: &Layout, rows: &[Vec<Value>], outer: Option<&Scope>) -> Result<Vec<Value>, SqlError> {
        let Expr::Function { name, args, over: Some(over), distinct, star } = expr else {
            return Ok(vec![Value::Null; rows.len()]);
        };
        let mut partitions: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let scope = Scope { layout, row, group: None, outer, row_index: i };
            let mut key = Vec::new();
            for e in &over.partition_by {
                key.push(self.eval(e, &scope)?);
            }
            match partitions.iter_mut().find(|(k, _)| k.iter().zip(&key).all(|(a, b)| same(a, b))) {
                Some((_, members)) => members.push(i),
                None => partitions.push((key, vec![i])),
            }
        }
        let mut result = vec![Value::Null; rows.len()];
        for (_, mut members) in partitions {
            let mut keys: Vec<Vec<Value>> = Vec::new();
            for &i in &members {
                let scope = Scope { layout, row: &rows[i], group: None, outer, row_index: i };
                let mut k = Vec::new();
                for item in &over.order_by {
                    k.push(self.eval(&item.expr, &scope)?);
                }
                keys.push(k);
            }
            let mut order: Vec<usize> = (0..members.len()).collect();
            order.sort_by(|&a, &b| compare_keys(&keys[a], &keys[b], &over.order_by));
            members = order.iter().map(|&i| members[i]).collect();
            let sorted_keys: Vec<&Vec<Value>> = order.iter().map(|&i| &keys[i]).collect();
            match name.as_str() {
                "ROW_NUMBER" => {
                    for (n, &i) in members.iter().enumerate() {
                        result[i] = Value::Int(n as i64 + 1);
                    }
                }
                "RANK" | "DENSE_RANK" => {
                    let mut rank = 0;
                    let mut dense = 0;
                    for (n, &i) in members.iter().enumerate() {
                        if n == 0 || compare_keys(sorted_keys[n], sorted_keys[n - 1], &over.order_by) != Ordering::Equal {
                            rank = n as i64 + 1;
                            dense += 1;
                        }
                        result[i] = Value::Int(if name == "RANK" { rank } else { dense });
                    }
                }
                agg if is_aggregate(agg) => {
                    let all_rows: Vec<Vec<Value>> = members.iter().map(|&i| rows[i].clone()).collect();
                    let call = Expr::Function { name: name.clone(), args: args.clone(), distinct: *distinct, star: *star, over: None };
                    for (n, &i) in members.iter().enumerate() {
                        // With ORDER BY the frame runs from the start to the current row's peers.
                        let end = if over.order_by.is_empty() {
                            all_rows.len()
                        } else {
                            let mut end = n + 1;
                            while end < members.len() && compare_keys(sorted_keys[end], sorted_keys[n], &over.order_by) == Ordering::Equal {
                                end += 1;
                            }
                            end
                        };
                        let group = &all_rows[..end];
                        let scope = Scope { layout, row: &rows[i], group: Some(group), outer, row_index: i };
                        result[i] = self.eval(&call, &scope)?;
                    }
                }
                "LAG" | "LEAD" => {
                    let offset = match args.get(1) {
                        Some(e) => self.eval_plain(e)?.as_i64()?.unwrap_or(1),
                        None => 1,
                    };
                    for (n, &i) in members.iter().enumerate() {
                        let target = if name == "LAG" { n as i64 - offset } else { n as i64 + offset };
                        result[i] = if target >= 0 && (target as usize) < members.len() {
                            let row = &rows[members[target as usize]];
                            let scope = Scope { layout, row, group: None, outer, row_index: members[target as usize] };
                            self.eval(&args[0], &scope)?
                        } else {
                            match args.get(2) {
                                Some(e) => self.eval_plain(e)?,
                                None => Value::Null,
                            }
                        };
                    }
                }
                other => return Err(SqlError::new(40517, 16, format!("The window function {other} is not supported by the Azure emulator's SQL data plane yet.")).batch_abort()),
            }
        }
        Ok(result)
    }
}

fn compare_keys(a: &[Value], b: &[Value], order: &[OrderItem]) -> Ordering {
    for (i, item) in order.iter().enumerate() {
        let mut o = sort_order(&a[i], &b[i]);
        if item.descending {
            o = o.reverse();
        }
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

fn sort_keyed(keyed: &mut [(Vec<Value>, Vec<Value>)], order: &[OrderItem]) {
    keyed.sort_by(|(a, _), (b, _)| compare_keys(a, b, order));
}

pub fn dedup(rows: &mut Vec<Vec<Value>>) {
    let mut unique: Vec<Vec<Value>> = Vec::new();
    for row in rows.drain(..) {
        if !unique.iter().any(|u| u.iter().zip(&row).all(|(a, b)| same(a, b))) {
            unique.push(row);
        }
    }
    *rows = unique;
}

fn cross(left: Rowset, right: Rowset) -> Rowset {
    let mut layout = left.layout;
    layout.columns.extend(right.layout.columns);
    let mut rows = Vec::new();
    for l in &left.rows {
        for r in &right.rows {
            let mut row = l.clone();
            row.extend(r.iter().cloned());
            rows.push(row);
        }
    }
    Rowset { layout, rows }
}

fn rename(rowset: &mut Rowset, alias: &str, columns: &[String]) -> Result<(), SqlError> {
    if !columns.is_empty() && columns.len() != rowset.layout.columns.len() {
        return Err(SqlError::new(8158, 16, format!("'{alias}' has more columns than were specified in the column list.")).batch_abort());
    }
    for (i, c) in rowset.layout.columns.iter_mut().enumerate() {
        c.qualifier = Some(alias.to_lowercase());
        if let Some(name) = columns.get(i) {
            c.name = name.clone();
        }
        if c.name.is_empty() {
            return Err(SqlError::new(8155, 16, format!("No column name was specified for column {} of '{alias}'.", i + 1)).batch_abort());
        }
    }
    Ok(())
}

impl Executor<'_> {
    pub(super) fn loc_display(&self, loc: &TableLoc) -> String {
        match loc {
            TableLoc::Db { key, .. } => key.clone(),
            TableLoc::Temp(k) | TableLoc::Var(k) => k.clone(),
        }
    }
}
