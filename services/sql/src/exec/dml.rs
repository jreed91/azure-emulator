//! `INSERT`, `UPDATE` and `DELETE`, with constraints and `OUTPUT`.

use std::collections::BTreeSet;

use super::query::{Layout, LayoutColumn, Rowset, Scope, ROWID};
use super::{Event, Executor, SqlError, TableLoc, Undo};
use crate::ast::*;
use crate::catalog::{Column, ConstraintRule};
use crate::parser::parse_expression;
use crate::value::{same, SqlType, StoreError, Value};

/// A row's id in its table, with the row as the statement sees it.
type RowWithId = (u64, Vec<Value>);

/// A table's shape, copied so its rows can be changed while it is consulted.
struct Shape {
    loc: TableLoc,
    database: String,
    schema: String,
    name: String,
    columns: Vec<Column>,
}

impl Shape {
    fn qualified(&self) -> String {
        format!("{}.{}.{}", self.database, self.schema, self.name)
    }
}

fn layout_of(columns: &[Column], qualifier: &str) -> Layout {
    Layout {
        columns: columns
            .iter()
            .map(|c| LayoutColumn { qualifier: Some(qualifier.to_lowercase()), name: c.name.clone(), ty: c.ty.clone(), nullable: c.nullable, identity: c.identity.is_some(), hidden: false })
            .collect(),
    }
}

impl Executor<'_> {
    fn shape(&self, loc: &TableLoc) -> Result<Shape, SqlError> {
        let table = self.table(loc).ok_or_else(|| super::invalid_object(&self.loc_display(loc)))?;
        let database = match loc {
            TableLoc::Db { db, .. } => self.state.database(db).map_or_else(|| db.clone(), |d| d.name.clone()),
            _ => "tempdb".into(),
        };
        Ok(Shape { loc: loc.clone(), database, schema: table.schema.clone(), name: table.name.clone(), columns: table.columns.clone() })
    }

    fn column_default(&mut self, column: &Column) -> Result<Value, SqlError> {
        match &column.default {
            Some((_, text)) => {
                let expr = parse_expression(text)?;
                self.eval_plain(&expr)?.convert(&column.ty)
            }
            None => Ok(Value::Null),
        }
    }

    /// Converts a value for a column, with SQL Server's errors for values that do not fit.
    fn store_value(&self, shape: &Shape, column: &Column, value: &Value) -> Result<Value, SqlError> {
        let value = match value.store(&column.ty) {
            Ok(v) => v,
            Err(StoreError::Truncated(cut)) => {
                return Err(SqlError::new(
                    2628,
                    16,
                    format!("String or binary data would be truncated in table '{}', column '{}'. Truncated value: '{}'.", shape.qualified(), column.name, cut),
                ))
            }
            Err(StoreError::Conversion(e)) => return Err(e),
        };
        Ok(value)
    }

    fn null_check(&self, shape: &Shape, row: &[Value], verb: &str) -> Result<(), SqlError> {
        for (column, value) in shape.columns.iter().zip(row) {
            if value.is_null() && !column.nullable {
                return Err(SqlError::new(
                    515,
                    16,
                    format!("Cannot insert the value NULL into column '{}', table '{}'; column does not allow nulls. {verb} fails.", column.name, shape.qualified()),
                )
                .state(2));
            }
        }
        Ok(())
    }

    /// Checks CHECK, PRIMARY KEY, UNIQUE and FOREIGN KEY constraints for rows about
    /// to be written. `changed` holds the row ids being replaced.
    fn validate(&mut self, shape: &Shape, rows: &[Vec<Value>], changed: &BTreeSet<u64>, verb: &str) -> Result<(), SqlError> {
        let table = self.table(&shape.loc).ok_or_else(|| super::invalid_object(&shape.name))?.clone();
        // CHECK constraints.
        let layout = layout_of(&shape.columns, &shape.name);
        for constraint in &table.constraints {
            if let ConstraintRule::Check(text) = &constraint.rule {
                let expr = parse_expression(text)?;
                for row in rows {
                    let scope = Scope { layout: &layout, row, group: None, outer: None, row_index: 0 };
                    if self.eval(&expr, &scope)?.truth() == Some(false) {
                        return Err(SqlError::new(
                            547,
                            16,
                            format!(
                                "The {verb} statement conflicted with the CHECK constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\".",
                                constraint.name, shape.database, shape.schema, shape.name
                            ),
                        ));
                    }
                }
            }
        }
        // Keys.
        let mut keys: Vec<(String, Vec<usize>, bool)> = Vec::new();
        for constraint in &table.constraints {
            match &constraint.rule {
                ConstraintRule::PrimaryKey(cols) => keys.push((constraint.name.clone(), cols.clone(), true)),
                ConstraintRule::Unique(cols) => keys.push((constraint.name.clone(), cols.clone(), true)),
                _ => {}
            }
        }
        for index in table.indexes.iter().filter(|i| i.unique) {
            keys.push((index.name.clone(), index.columns.clone(), false));
        }
        for (name, cols, is_constraint) in &keys {
            for (i, row) in rows.iter().enumerate() {
                let key: Vec<&Value> = cols.iter().map(|&c| &row[c]).collect();
                let equal = |other: &Vec<Value>| cols.iter().zip(&key).all(|(&c, k)| same(&other[c], k));
                let clash = table.rows.iter().any(|(id, other)| !changed.contains(id) && equal(other)) || rows[..i].iter().any(equal);
                if clash {
                    let shown = key.iter().map(|v| if v.is_null() { "<NULL>".to_string() } else { v.to_text() }).collect::<Vec<_>>().join(", ");
                    let object = format!("{}.{}", shape.schema, shape.name);
                    let is_primary = table.constraints.iter().any(|c| c.name == *name && matches!(c.rule, ConstraintRule::PrimaryKey(_)));
                    return Err(if *is_constraint {
                        SqlError::new(
                            2627,
                            14,
                            format!(
                                "Violation of {} constraint '{name}'. Cannot insert duplicate key in object '{object}'. The duplicate key value is ({shown}).",
                                if is_primary { "PRIMARY KEY" } else { "UNIQUE KEY" }
                            ),
                        )
                    } else {
                        SqlError::new(2601, 14, format!("Cannot insert duplicate key row in object '{object}' with unique index '{name}'. The duplicate key value is ({shown})."))
                    });
                }
            }
        }
        // Foreign keys this table holds.
        if let TableLoc::Db { db, .. } = &shape.loc {
            for constraint in &table.constraints {
                let ConstraintRule::ForeignKey { columns, table: parent_key, ref_columns, .. } = &constraint.rule else { continue };
                let Some(parent) = self.state.database(db).and_then(|d| d.tables.get(parent_key)) else { continue };
                let ref_idx: Vec<usize> = ref_columns.iter().filter_map(|c| parent.column_index(c)).collect();
                for row in rows {
                    let values: Vec<&Value> = columns.iter().map(|&c| &row[c]).collect();
                    if values.iter().any(|v| v.is_null()) {
                        continue;
                    }
                    let found = parent.rows.values().any(|p| ref_idx.iter().zip(&values).all(|(&c, v)| same(&p[c], v)));
                    if !found {
                        return Err(SqlError::new(
                            547,
                            16,
                            format!(
                                "The {verb} statement conflicted with the FOREIGN KEY constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.",
                                constraint.name,
                                shape.database,
                                parent.schema,
                                parent.name,
                                ref_columns.first().cloned().unwrap_or_default()
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Evaluates an `OUTPUT` list over `inserted` and `deleted` rows and emits it.
    fn emit_output(&mut self, items: &[SelectItem], shape: &Shape, inserted: &[Option<Vec<Value>>], deleted: &[Option<Vec<Value>>]) -> Result<(), SqlError> {
        let mut layout = layout_of(&shape.columns, "inserted");
        layout.columns.extend(layout_of(&shape.columns, "deleted").columns);
        let width = shape.columns.len();
        let mut expanded: Vec<(Expr, String)> = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard(Some(q)) if q.eq_ignore_ascii_case("inserted") || q.eq_ignore_ascii_case("deleted") => {
                    for c in &shape.columns {
                        expanded.push((Expr::Column(vec![q.to_lowercase(), c.name.clone()]), c.name.clone()));
                    }
                }
                SelectItem::Wildcard(_) => return Err(SqlError::new(102, 15, "Incorrect syntax near '*'.")),
                SelectItem::Expr { expr, alias } => {
                    let name = alias.clone().unwrap_or_else(|| match expr {
                        Expr::Column(parts) => parts.last().cloned().unwrap_or_default(),
                        _ => String::new(),
                    });
                    expanded.push((expr.clone(), name));
                }
            }
        }
        let count = inserted.len().max(deleted.len());
        let mut rows = Vec::new();
        for i in 0..count {
            let mut row = inserted.get(i).cloned().flatten().unwrap_or_else(|| vec![Value::Null; width]);
            row.extend(deleted.get(i).cloned().flatten().unwrap_or_else(|| vec![Value::Null; width]));
            let scope = Scope { layout: &layout, row: &row, group: None, outer: None, row_index: i };
            let mut out = Vec::new();
            for (e, _) in &expanded {
                out.push(self.eval(e, &scope)?);
            }
            rows.push(out);
        }
        let empty = Scope { layout: &layout, row: &[], group: None, outer: None, row_index: 0 };
        let columns = expanded
            .iter()
            .enumerate()
            .map(|(i, (e, name))| LayoutColumn {
                qualifier: None,
                name: name.clone(),
                ty: self.static_type(e, &empty).unwrap_or_else(|| rows.iter().map(|r: &Vec<Value>| &r[i]).find(|v| !v.is_null()).map_or(SqlType::Int, Value::natural_type)),
                nullable: true,
                identity: false,
                hidden: false,
            })
            .collect();
        self.emit_rowset(Rowset { layout: Layout { columns }, rows }, None);
        Ok(())
    }

    pub(super) fn insert(&mut self, insert: &Insert) -> Result<u64, SqlError> {
        let loc = self.resolve_table(&insert.table)?;
        let shape = self.shape(&loc)?;
        let identity_insert = self.table(&loc).is_some_and(|t| t.identity_insert);
        let identity_column = shape.columns.iter().position(|c| c.identity.is_some());
        let targets: Vec<usize> = if insert.columns.is_empty() {
            if identity_insert && identity_column.is_some() {
                return Err(SqlError::new(
                    8101,
                    16,
                    format!("An explicit value for the identity column in table '{}' can only be specified when a column list is used and IDENTITY_INSERT is ON.", shape.name),
                ));
            }
            (0..shape.columns.len()).filter(|&i| Some(i) != identity_column).collect()
        } else {
            let mut targets = Vec::new();
            for name in &insert.columns {
                let index = shape.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
                    SqlError::new(207, 16, format!("Invalid column name '{name}'.")).batch_abort()
                })?;
                if targets.contains(&index) {
                    return Err(SqlError::new(264, 16, format!("The column name '{name}' is specified more than once in the SET clause or column list of an INSERT. A column cannot be assigned more than one value in the same clause. Modify the clause to make sure that a column is updated only once. If the SET clause updates columns of a view, column name '{name}' may appear twice in the view definition.")));
                }
                targets.push(index);
            }
            targets
        };
        if let Some(identity) = identity_column {
            if targets.contains(&identity) && !identity_insert {
                return Err(SqlError::new(544, 16, format!("Cannot insert explicit value for identity column in table '{}' when IDENTITY_INSERT is set to OFF.", shape.name)));
            }
        }

        // The values, `None` for DEFAULT.
        let source: Vec<Vec<Option<Value>>> = match &insert.source {
            InsertSource::Values(rows) => {
                let mut out = Vec::new();
                for row in rows {
                    if row.len() != targets.len() {
                        return Err(if row.len() < targets.len() {
                            SqlError::new(109, 15, "There are more columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement.")
                        } else {
                            SqlError::new(110, 15, "There are fewer columns in the INSERT statement than values specified in the VALUES clause. The number of values in the VALUES clause must match the number of columns specified in the INSERT statement.")
                        }
                        .batch_abort());
                    }
                    let mut values = Vec::new();
                    for e in row {
                        values.push(match e {
                            Expr::Default => None,
                            e => Some(self.eval_plain(e)?),
                        });
                    }
                    out.push(values);
                }
                out
            }
            InsertSource::Query(query) => {
                let rowset = self.run_query(query, None)?;
                let width = rowset.layout.columns.iter().filter(|c| !c.hidden).count();
                if width != targets.len() {
                    return Err(if width < targets.len() {
                        SqlError::new(120, 15, "The select list for the INSERT statement contains fewer items than the insert list. The number of SELECT values must match the number of INSERT columns.")
                    } else {
                        SqlError::new(121, 15, "The select list for the INSERT statement contains more items than the insert list. The number of SELECT values must match the number of INSERT columns.")
                    }
                    .batch_abort());
                }
                rowset.rows.into_iter().map(|r| r.into_iter().map(Some).collect()).collect()
            }
            InsertSource::Exec(exec) => {
                let mark = self.out.len();
                self.exec_statement(exec)?;
                let produced: Vec<Event> = self.out.drain(mark..).collect();
                let mut rows = Vec::new();
                for event in produced {
                    match event {
                        Event::ResultSet { rows: r, columns } => {
                            if columns.len() != targets.len() {
                                return Err(SqlError::new(213, 16, "Column name or number of supplied values does not match table definition."));
                            }
                            rows.extend(r.into_iter().map(|row| row.into_iter().map(Some).collect::<Vec<_>>()));
                        }
                        Event::ReturnStatus(_) | Event::DoneProc { .. } | Event::Done { .. } => {}
                        other => self.out.push(other),
                    }
                }
                rows
            }
            InsertSource::DefaultValues => vec![Vec::new()],
        };

        let mut new_rows = Vec::new();
        let mut last_identity = None;
        for values in source {
            let mut row = vec![Value::Null; shape.columns.len()];
            let mut given = vec![false; shape.columns.len()];
            for (&target, value) in targets.iter().zip(values) {
                if let Some(value) = value {
                    row[target] = self.store_value(&shape, &shape.columns[target], &value)?;
                    given[target] = true;
                }
            }
            for (i, column) in shape.columns.iter().enumerate() {
                if given[i] {
                    if column.identity.is_some() {
                        // An explicit identity value moves the counter forward.
                        if let (Some(v), Some(table)) = (row[i].as_i64()?, self.table_mut(&loc)) {
                            if let Some(identity) = table.columns[i].identity.as_mut() {
                                let ahead = identity.current.is_none_or(|c| if identity.increment > 0 { v > c } else { v < c });
                                if ahead {
                                    identity.current = Some(v);
                                }
                            }
                        }
                    }
                    continue;
                }
                if column.identity.is_some() {
                    let next = {
                        let table = self.table_mut(&loc).ok_or_else(|| super::invalid_object(&shape.name))?;
                        let identity = table.columns[i].identity.as_mut().ok_or_else(|| super::invalid_object(&shape.name))?;
                        let next = identity.current.map_or(identity.seed, |c| c + identity.increment);
                        identity.current = Some(next);
                        next
                    };
                    row[i] = Value::Int(next).convert(&column.ty)?;
                    last_identity = Some(Value::Int(next));
                } else {
                    row[i] = self.column_default(column)?;
                }
            }
            self.null_check(&shape, &row, "INSERT")?;
            new_rows.push(row);
        }
        self.validate(&shape, &new_rows, &BTreeSet::new(), "INSERT")?;

        let count = new_rows.len() as u64;
        let mut ids = Vec::new();
        {
            let table = self.table_mut(&loc).ok_or_else(|| super::invalid_object(&shape.name))?;
            for row in &new_rows {
                let id = table.next_row;
                table.next_row += 1;
                table.rows.insert(id, row.clone());
                ids.push(id);
            }
        }
        for id in ids {
            self.log(Undo::Inserted(loc.clone(), id));
        }
        if let Some(identity) = last_identity {
            self.frame_mut().scope_identity = identity.clone();
            self.session.identity = identity;
        }
        if let Some(items) = &insert.output {
            let inserted: Vec<Option<Vec<Value>>> = new_rows.into_iter().map(Some).collect();
            self.emit_output(items, &shape, &inserted, &[])?;
        }
        Ok(count)
    }

    /// The target table of an UPDATE or DELETE, which may be an alias in its FROM list.
    fn dml_target(&self, name: &ObjectName, from: &[TableRef]) -> Result<(TableLoc, String), SqlError> {
        fn find<'f>(refs: &'f [TableRef], alias: &str) -> Option<&'f ObjectName> {
            for r in refs {
                match r {
                    TableRef::Table { name, alias: Some(a) } if a.eq_ignore_ascii_case(alias) => return Some(name),
                    TableRef::Join { left, right, .. } => {
                        if let Some(found) = find(std::slice::from_ref(left), alias).or_else(|| find(std::slice::from_ref(right), alias)) {
                            return Some(found);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        if name.schema.is_none() && name.database.is_none() {
            if let Some(real) = find(from, &name.name) {
                return Ok((self.resolve_table(real)?, name.name.to_lowercase()));
            }
        }
        Ok((self.resolve_table(name)?, name.name.to_lowercase()))
    }

    /// The rows a DML statement changes: (row id, the source row), with its layout.
    fn dml_rows(&mut self, loc: &TableLoc, qualifier: &str, name: &ObjectName, from: &[TableRef], condition: Option<&Expr>, top: Option<&Expr>) -> Result<(Layout, Vec<RowWithId>), SqlError> {
        self.rowid_target = Some(loc.clone());
        let source = if from.is_empty() {
            self.rows_from(&[TableRef::Table { name: name.clone(), alias: None }], None)
        } else {
            self.rows_from(from, None)
        };
        self.rowid_target = None;
        let source = source?;
        let rowid = source
            .layout
            .columns
            .iter()
            .position(|c| c.name == ROWID && c.qualifier.as_deref() == Some(qualifier))
            .or_else(|| source.layout.columns.iter().position(|c| c.name == ROWID))
            .ok_or_else(|| SqlError::new(4104, 16, format!("The multi-part identifier \"{}\" could not be bound.", name.display())).batch_abort())?;
        let limit = match top {
            Some(e) => Some(self.eval_plain(e)?.as_i64()?.unwrap_or(0).max(0) as usize),
            None => None,
        };
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        for row in source.rows {
            if limit.is_some_and(|l| rows.len() >= l) {
                break;
            }
            let keep = match condition {
                Some(c) => {
                    let scope = Scope { layout: &source.layout, row: &row, group: None, outer: None, row_index: 0 };
                    self.eval(c, &scope)?.truth() == Some(true)
                }
                None => true,
            };
            if !keep {
                continue;
            }
            let Value::Int(id) = row[rowid] else { continue };
            if seen.insert(id as u64) {
                rows.push((id as u64, row));
            }
        }
        Ok((source.layout, rows))
    }

    pub(super) fn update(&mut self, update: &Update) -> Result<u64, SqlError> {
        let (loc, qualifier) = self.dml_target(&update.table, &update.from)?;
        let shape = self.shape(&loc)?;
        let mut targets = Vec::new();
        for (parts, op, expr) in &update.assignments {
            let name = parts.last().cloned().unwrap_or_default();
            let index = shape.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&name)).ok_or_else(|| SqlError::new(207, 16, format!("Invalid column name '{name}'.")).batch_abort())?;
            if shape.columns[index].identity.is_some() {
                return Err(SqlError::new(8102, 16, format!("Cannot update identity column '{}'.", shape.columns[index].name)));
            }
            targets.push((index, *op, expr));
        }
        let (layout, rows) = self.dml_rows(&loc, &qualifier, &update.table, &update.from, update.condition.as_ref(), update.top.as_ref())?;
        let mut changes = Vec::new();
        for (id, source_row) in rows {
            let old = self.table(&loc).and_then(|t| t.rows.get(&id)).cloned().ok_or_else(|| super::invalid_object(&shape.name))?;
            let mut new = old.clone();
            let scope = Scope { layout: &layout, row: &source_row, group: None, outer: None, row_index: 0 };
            for (index, op, expr) in &targets {
                let value = match expr {
                    Expr::Default => self.column_default(&shape.columns[*index])?,
                    e => self.eval(e, &scope)?,
                };
                let value = match op {
                    Some(op) => crate::value::arithmetic(*op, &old[*index], &value)?,
                    None => value,
                };
                new[*index] = self.store_value(&shape, &shape.columns[*index], &value)?;
            }
            self.null_check(&shape, &new, "UPDATE")?;
            changes.push((id, old, new));
        }
        let changed: BTreeSet<u64> = changes.iter().map(|(id, _, _)| *id).collect();
        let new_rows: Vec<Vec<Value>> = changes.iter().map(|(_, _, n)| n.clone()).collect();
        self.validate(&shape, &new_rows, &changed, "UPDATE")?;
        self.check_referenced_keys(&shape, &changes)?;
        let count = changes.len() as u64;
        for (id, old, new) in &changes {
            if let Some(table) = self.table_mut(&loc) {
                table.rows.insert(*id, new.clone());
            }
            self.log(Undo::Updated(loc.clone(), *id, old.clone()));
        }
        if let Some(items) = &update.output {
            let inserted: Vec<Option<Vec<Value>>> = changes.iter().map(|(_, _, n)| Some(n.clone())).collect();
            let deleted: Vec<Option<Vec<Value>>> = changes.iter().map(|(_, o, _)| Some(o.clone())).collect();
            self.emit_output(items, &shape, &inserted, &deleted)?;
        }
        Ok(count)
    }

    /// An update may not change a key that other tables' foreign keys point at.
    fn check_referenced_keys(&self, shape: &Shape, changes: &[(u64, Vec<Value>, Vec<Value>)]) -> Result<(), SqlError> {
        let TableLoc::Db { db, key } = &shape.loc else { return Ok(()) };
        let Some(database) = self.state.database(db) else { return Ok(()) };
        for (child_key, child) in &database.tables {
            for constraint in &child.constraints {
                let ConstraintRule::ForeignKey { columns, table: parent, ref_columns, .. } = &constraint.rule else { continue };
                if parent != key {
                    continue;
                }
                let ref_idx: Vec<usize> = ref_columns.iter().filter_map(|c| shape.columns.iter().position(|x| x.name.eq_ignore_ascii_case(c))).collect();
                for (_, old, new) in changes {
                    if ref_idx.iter().all(|&i| same(&old[i], &new[i])) {
                        continue;
                    }
                    let referenced = child.rows.values().any(|r| columns.iter().zip(&ref_idx).all(|(&c, &p)| same(&r[c], &old[p])));
                    if referenced {
                        let _ = child_key;
                        return Err(SqlError::new(
                            547,
                            16,
                            format!(
                                "The UPDATE statement conflicted with the REFERENCE constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.",
                                constraint.name,
                                shape.database,
                                child.schema,
                                child.name,
                                columns.first().map(|&c| child.columns[c].name.clone()).unwrap_or_default()
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn delete(&mut self, delete: &Delete) -> Result<u64, SqlError> {
        let (loc, qualifier) = self.dml_target(&delete.table, &delete.from)?;
        let shape = self.shape(&loc)?;
        let (_, rows) = self.dml_rows(&loc, &qualifier, &delete.table, &delete.from, delete.condition.as_ref(), delete.top.as_ref())?;
        let ids: Vec<u64> = rows.into_iter().map(|(id, _)| id).collect();
        // Rows of other tables to delete with these (ON DELETE CASCADE), checked first.
        let mut plan: Vec<(TableLoc, Vec<u64>)> = Vec::new();
        self.plan_delete(&loc, &shape, &ids, &mut plan, 0)?;
        let mut deleted_rows = Vec::new();
        for (target, ids) in plan.iter().rev() {
            for id in ids {
                let removed = self.table_mut(target).and_then(|t| t.rows.remove(id));
                if let Some(row) = removed {
                    if target == &loc {
                        deleted_rows.push(row.clone());
                    }
                    self.log(Undo::Deleted(target.clone(), *id, row));
                }
            }
        }
        if let Some(items) = &delete.output {
            let deleted: Vec<Option<Vec<Value>>> = deleted_rows.into_iter().map(Some).collect();
            self.emit_output(items, &shape, &[], &deleted)?;
        }
        Ok(ids.len() as u64)
    }

    fn plan_delete(&self, loc: &TableLoc, shape: &Shape, ids: &[u64], plan: &mut Vec<(TableLoc, Vec<u64>)>, depth: usize) -> Result<(), SqlError> {
        plan.push((loc.clone(), ids.to_vec()));
        let TableLoc::Db { db, key } = loc else { return Ok(()) };
        if depth > 32 || ids.is_empty() {
            return Ok(());
        }
        let Some(database) = self.state.database(db) else { return Ok(()) };
        let Some(table) = database.tables.get(key) else { return Ok(()) };
        let parents: Vec<&Vec<Value>> = ids.iter().filter_map(|id| table.rows.get(id)).collect();
        for (child_key, child) in &database.tables {
            for constraint in &child.constraints {
                let ConstraintRule::ForeignKey { columns, table: parent, ref_columns, cascade } = &constraint.rule else { continue };
                if parent != key {
                    continue;
                }
                let ref_idx: Vec<usize> = ref_columns.iter().filter_map(|c| table.column_index(c)).collect();
                let child_loc = TableLoc::Db { db: db.clone(), key: child_key.clone() };
                let already: BTreeSet<u64> = plan.iter().filter(|(l, _)| *l == child_loc).flat_map(|(_, ids)| ids.iter().copied()).collect();
                let referencing: Vec<u64> = child
                    .rows
                    .iter()
                    .filter(|(id, r)| !already.contains(id) && parents.iter().any(|p| columns.iter().zip(&ref_idx).all(|(&c, &pi)| !r[c].is_null() && same(&r[c], &p[pi]))))
                    .map(|(id, _)| *id)
                    .collect();
                if referencing.is_empty() {
                    continue;
                }
                if !cascade {
                    return Err(SqlError::new(
                        547,
                        16,
                        format!(
                            "The DELETE statement conflicted with the REFERENCE constraint \"{}\". The conflict occurred in database \"{}\", table \"{}.{}\", column '{}'.",
                            constraint.name,
                            shape.database,
                            child.schema,
                            child.name,
                            columns.first().map(|&c| child.columns[c].name.clone()).unwrap_or_default()
                        ),
                    ));
                }
                let child_shape = Shape { loc: child_loc.clone(), database: shape.database.clone(), schema: child.schema.clone(), name: child.name.clone(), columns: child.columns.clone() };
                self.plan_delete(&child_loc, &child_shape, &referencing, plan, depth + 1)?;
            }
        }
        Ok(())
    }

    /// `IDENT_CURRENT('table')`.
    pub(super) fn ident_current(&self, name: &str) -> Result<Value, SqlError> {
        let Ok(Expr::Column(parts)) = parse_expression(name) else { return Ok(Value::Null) };
        let object = super::object_from_parts(parts);
        let Ok(loc) = self.resolve_table(&object) else { return Ok(Value::Null) };
        let Some(table) = self.table(&loc) else { return Ok(Value::Null) };
        Ok(table
            .columns
            .iter()
            .find_map(|c| c.identity.as_ref())
            .map_or(Value::Null, |i| Value::Decimal(rust_decimal::Decimal::from(i.current.unwrap_or(i.seed)))))
    }
}
