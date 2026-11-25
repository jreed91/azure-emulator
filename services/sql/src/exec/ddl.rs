//! Databases, schemas, tables, indexes and procedures.

use std::collections::BTreeMap;

use super::{invalid_object, Executor, SqlError, TableLoc, Undo};
use crate::ast::*;
use crate::catalog::{object_key, Column, Constraint, ConstraintRule, Identity, Index, StoredProcedure, Table};

fn exists_error(name: &str) -> SqlError {
    SqlError::new(2714, 16, format!("There is already an object named '{name}' in the database.")).state(6)
}

fn short_hex(n: i64) -> String {
    format!("{:016X}", (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

impl Executor<'_> {
    pub(super) fn create_database(&mut self, name: &str) -> Result<(), SqlError> {
        if self.session.in_transaction() {
            return Err(SqlError::new(226, 16, "CREATE DATABASE statement not allowed within multi-statement transaction."));
        }
        if self.state.database(name).is_some() {
            return Err(SqlError::new(1801, 16, format!("Database '{name}' already exists. Choose a different database name.")).state(3));
        }
        self.state.create_database(name);
        Ok(())
    }

    pub(super) fn drop_database(&mut self, name: &str, if_exists: bool) -> Result<(), SqlError> {
        if self.session.in_transaction() {
            return Err(SqlError::new(226, 16, "DROP DATABASE statement not allowed within multi-statement transaction."));
        }
        let lower = name.to_lowercase();
        if ["master", "tempdb", "model", "msdb"].contains(&lower.as_str()) {
            return Err(SqlError::new(3708, 16, format!("Cannot drop the database '{name}' because it is a system database.")));
        }
        if self.state.database(name).is_none() {
            if if_exists {
                return Ok(());
            }
            return Err(SqlError::new(3701, 11, format!("Cannot drop the database '{name}', because it does not exist or you do not have permission.")));
        }
        if self.session.database.eq_ignore_ascii_case(name) {
            return Err(SqlError::new(3702, 16, format!("Cannot drop database \"{name}\" because it is currently in use.")).state(4));
        }
        self.state.databases.remove(&lower);
        Ok(())
    }

    pub(super) fn create_schema(&mut self, name: &str) -> Result<(), SqlError> {
        let db = self.current_db();
        let database = self.state.database_mut(&db).ok_or_else(|| invalid_object(&db))?;
        if database.has_schema(name) {
            return Err(SqlError::new(2714, 16, format!("There is already an object named '{name}' in the database.")).state(6));
        }
        database.schemas.push(name.to_string());
        Ok(())
    }

    pub(super) fn drop_schema(&mut self, name: &str, if_exists: bool) -> Result<(), SqlError> {
        let db = self.current_db();
        let database = self.state.database_mut(&db).ok_or_else(|| invalid_object(&db))?;
        if !database.has_schema(name) {
            if if_exists {
                return Ok(());
            }
            return Err(SqlError::new(15151, 16, format!("Cannot drop the schema '{name}', because it does not exist or you do not have permission.")));
        }
        let prefix = format!("{}.", name.to_lowercase());
        if database.tables.keys().chain(database.procedures.keys()).any(|k| k.starts_with(&prefix)) {
            return Err(SqlError::new(3729, 16, format!("Cannot drop schema '{name}' because it is being referenced by an object.")));
        }
        database.schemas.retain(|s| !s.eq_ignore_ascii_case(name));
        Ok(())
    }

    /// Where a new table goes, and its schema.
    fn new_table_loc(&self, name: &ObjectName) -> Result<(TableLoc, String), SqlError> {
        if name.name.starts_with('#') {
            return Ok((TableLoc::Temp(name.name.to_lowercase()), "dbo".into()));
        }
        let db = name.database.clone().unwrap_or_else(|| self.current_db());
        let database = self.state.database(&db).ok_or_else(|| SqlError::new(2702, 16, format!("Database '{db}' does not exist.")))?;
        let schema = name.schema.clone().unwrap_or_else(|| "dbo".into());
        if !database.has_schema(&schema) {
            return Err(SqlError::new(2760, 16, format!("The specified schema name \"{schema}\" either does not exist or you do not have permission to use it.")));
        }
        let schema = database.schemas.iter().find(|s| s.eq_ignore_ascii_case(&schema)).cloned().unwrap_or(schema);
        Ok((TableLoc::Db { db: database.name.to_lowercase(), key: object_key(&schema, &name.name) }, schema))
    }

    /// Builds a table from its definition (also for table variables).
    pub(super) fn build_table(&mut self, schema: &str, name: &str, columns: &[ColumnDef], constraints: &[TableConstraint]) -> Result<Table, SqlError> {
        let object_id = self.state.new_object_id();
        let mut table = Table {
            schema: schema.to_string(),
            name: name.to_string(),
            object_id,
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            rows: BTreeMap::new(),
            next_row: 1,
            identity_insert: false,
            created: chrono::Utc::now().naive_utc(),
        };
        let mut all_constraints: Vec<TableConstraint> = constraints.to_vec();
        for def in columns {
            if table.column_index(&def.name).is_some() {
                return Err(SqlError::new(2705, 16, format!("Column names in each table must be unique. Column name '{}' in table '{name}' is specified more than once.", def.name)));
            }
            if def.identity.is_some() && table.columns.iter().any(|c| c.identity.is_some()) {
                return Err(SqlError::new(2744, 16, format!("Multiple identity columns specified for table '{name}'. Only one identity column per table is allowed.")));
            }
            if def.identity.is_some() && !matches!(def.ty, crate::value::SqlType::TinyInt | crate::value::SqlType::SmallInt | crate::value::SqlType::Int | crate::value::SqlType::BigInt | crate::value::SqlType::Decimal(_, 0)) {
                return Err(SqlError::new(2749, 16, format!("Identity column '{}' must be of data type int, bigint, smallint, tinyint, or decimal or numeric with a scale of 0, and constrained to be nonnullable.", def.name)));
            }
            let default = def.default.as_ref().map(|(constraint, _, text)| {
                (constraint.clone().unwrap_or_else(|| format!("DF__{}__{}__{}", truncate(name, 9), truncate(&def.name, 5), &short_hex(object_id)[..8])), text.clone())
            });
            table.columns.push(Column {
                name: def.name.clone(),
                ty: def.ty.clone(),
                nullable: def.nullable.unwrap_or(true) && def.identity.is_none(),
                identity: def.identity.map(|(seed, increment)| Identity { seed, increment, current: None }),
                default,
            });
            all_constraints.extend(def.constraints.iter().cloned());
        }
        for constraint in &all_constraints {
            self.add_constraint(&mut table, constraint)?;
        }
        Ok(table)
    }

    fn add_constraint(&mut self, table: &mut Table, constraint: &TableConstraint) -> Result<(), SqlError> {
        let columns_of = |table: &Table, names: &[String]| -> Result<Vec<usize>, SqlError> {
            names
                .iter()
                .map(|n| {
                    table.column_index(n).ok_or_else(|| SqlError::new(1911, 16, format!("Column name '{n}' does not exist in the target table or view.")))
                })
                .collect()
        };
        let object_id = self.state.new_object_id();
        let suffix = short_hex(object_id);
        let (rule, default_name) = match &constraint.kind {
            ConstraintKind::PrimaryKey(names) => {
                if table.primary_key().is_some() {
                    return Err(SqlError::new(8110, 16, format!("Cannot add multiple PRIMARY KEY constraints to table '{}'.", table.name)));
                }
                let cols = columns_of(table, names)?;
                for &c in &cols {
                    if table.columns[c].nullable && table.rows.values().any(|r| r[c].is_null()) {
                        return Err(SqlError::new(8111, 16, format!("Cannot define PRIMARY KEY constraint on nullable column in table '{}'.", table.name)));
                    }
                    table.columns[c].nullable = false;
                }
                (ConstraintRule::PrimaryKey(cols), format!("PK__{}__{}", truncate(&table.name, 8), suffix))
            }
            ConstraintKind::Unique(names) => (ConstraintRule::Unique(columns_of(table, names)?), format!("UQ__{}__{}", truncate(&table.name, 8), suffix)),
            ConstraintKind::ForeignKey { columns, table: parent, ref_columns, on_delete_cascade } => {
                let cols = columns_of(table, columns)?;
                let parent_schema = parent.schema.clone().unwrap_or_else(|| "dbo".into());
                let parent_key = object_key(&parent_schema, &parent.name);
                let db = parent.database.clone().unwrap_or_else(|| self.current_db());
                let self_reference = parent.name.eq_ignore_ascii_case(&table.name) && parent_schema.eq_ignore_ascii_case(&table.schema);
                let ref_columns = if self_reference {
                    if ref_columns.is_empty() {
                        table.primary_key().map(|k| k.iter().map(|&i| table.columns[i].name.clone()).collect()).unwrap_or_default()
                    } else {
                        ref_columns.clone()
                    }
                } else if !table.name.starts_with('#') && !table.name.starts_with('@') {
                    let parent_table = self
                        .state
                        .database(&db)
                        .and_then(|d| d.tables.get(&parent_key))
                        .ok_or_else(|| SqlError::new(1767, 16, format!("Foreign key references invalid table '{}'.", parent.display())))?;
                    if ref_columns.is_empty() {
                        parent_table.primary_key().map(|k| k.iter().map(|&i| parent_table.columns[i].name.clone()).collect()).unwrap_or_default()
                    } else {
                        for c in ref_columns {
                            if parent_table.column_index(c).is_none() {
                                return Err(SqlError::new(1770, 16, format!("Foreign key references invalid column '{c}' in referenced table '{}'.", parent.name)));
                            }
                        }
                        ref_columns.clone()
                    }
                } else {
                    ref_columns.clone()
                };
                if ref_columns.len() != cols.len() {
                    return Err(SqlError::new(8139, 16, "Number of referencing columns in foreign key differs from number of referenced columns."));
                }
                (
                    ConstraintRule::ForeignKey { columns: cols, table: parent_key, ref_columns, cascade: *on_delete_cascade },
                    format!("FK__{}__{}", truncate(&table.name, 8), suffix),
                )
            }
            ConstraintKind::Check(_, text) => (ConstraintRule::Check(text.clone()), format!("CK__{}__{}", truncate(&table.name, 8), suffix)),
            ConstraintKind::Default { column, text, .. } => {
                let index = table.column_index(column).ok_or_else(|| SqlError::new(1752, 16, format!("Column '{column}' in table '{}' is invalid for creating a default constraint.", table.name)))?;
                if table.columns[index].default.is_some() {
                    return Err(SqlError::new(1781, 16, "Column already has a DEFAULT bound to it."));
                }
                let name = constraint.name.clone().unwrap_or_else(|| format!("DF__{}__{}__{}", truncate(&table.name, 9), truncate(column, 5), &suffix[..8]));
                table.columns[index].default = Some((name, text.clone()));
                return Ok(());
            }
        };
        let name = constraint.name.clone().unwrap_or(default_name);
        if table.constraints.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
            return Err(exists_error(&name));
        }
        table.constraints.push(Constraint { name, object_id, rule });
        Ok(())
    }

    pub(super) fn create_table(&mut self, create: &CreateTable) -> Result<(), SqlError> {
        let (loc, schema) = self.new_table_loc(&create.name)?;
        if self.table(&loc).is_some() {
            return Err(exists_error(&create.name.name));
        }
        if let TableLoc::Db { db, key } = &loc {
            if self.state.database(db).is_some_and(|d| d.procedures.contains_key(key)) {
                return Err(exists_error(&create.name.name));
            }
        }
        let table = self.build_table(&schema, &create.name.name, &create.columns, &create.constraints)?;
        // Constraint names are unique in a database.
        if let TableLoc::Db { db, .. } = &loc {
            if let Some(database) = self.state.database(db) {
                for constraint in &table.constraints {
                    if database.tables.values().any(|t| t.constraints.iter().any(|c| c.name.eq_ignore_ascii_case(&constraint.name))) {
                        return Err(exists_error(&constraint.name));
                    }
                }
            }
        }
        self.put_table(&loc, table);
        self.log(Undo::Table(loc, None));
        Ok(())
    }

    fn put_table(&mut self, loc: &TableLoc, table: Table) {
        match loc {
            TableLoc::Db { db, key } => {
                if let Some(database) = self.state.database_mut(db) {
                    database.tables.insert(key.clone(), table);
                }
            }
            TableLoc::Temp(key) => {
                self.session.temp.insert(key.clone(), table);
            }
            TableLoc::Var(_) => {}
        }
    }

    fn take_table(&mut self, loc: &TableLoc) -> Option<Table> {
        match loc {
            TableLoc::Db { db, key } => self.state.database_mut(db).and_then(|d| d.tables.remove(key)),
            TableLoc::Temp(key) => self.session.temp.remove(key),
            TableLoc::Var(_) => None,
        }
    }

    pub(super) fn drop_table(&mut self, name: &ObjectName, if_exists: bool) -> Result<(), SqlError> {
        let loc = match self.resolve_table(name) {
            Ok(loc) => loc,
            Err(_) if if_exists => return Ok(()),
            Err(_) => {
                return Err(SqlError::new(3701, 11, format!("Cannot drop the table '{}', because it does not exist or you do not have permission.", name.display())));
            }
        };
        if let TableLoc::Db { db, key } = &loc {
            if let Some(database) = self.state.database(db) {
                for (child_key, child) in &database.tables {
                    if child_key == key {
                        continue;
                    }
                    for constraint in &child.constraints {
                        if matches!(&constraint.rule, ConstraintRule::ForeignKey { table, .. } if table == key) {
                            return Err(SqlError::new(
                                3726,
                                16,
                                format!("Could not drop object '{}' because it is referenced by a FOREIGN KEY constraint.", name.display()),
                            ));
                        }
                    }
                }
            }
        }
        let previous = self.take_table(&loc);
        self.log(Undo::Table(loc, previous.map(Box::new)));
        Ok(())
    }

    pub(super) fn truncate(&mut self, name: &ObjectName) -> Result<(), SqlError> {
        let loc = self.resolve_table(name)?;
        let previous = self.table(&loc).cloned();
        if let Some(table) = self.table_mut(&loc) {
            table.rows.clear();
            for column in &mut table.columns {
                if let Some(identity) = &mut column.identity {
                    identity.current = None;
                }
            }
        }
        self.log(Undo::Table(loc, previous.map(Box::new)));
        Ok(())
    }

    pub(super) fn alter_table(&mut self, name: &ObjectName, action: &AlterAction) -> Result<(), SqlError> {
        let loc = self.resolve_table(name)?;
        let previous = self.table(&loc).cloned().ok_or_else(|| invalid_object(&name.display()))?;
        let mut table = previous.clone();
        match action {
            AlterAction::AddColumns(columns) => {
                for def in columns {
                    if table.column_index(&def.name).is_some() {
                        return Err(SqlError::new(2705, 16, format!("Column names in each table must be unique. Column name '{}' in table '{}' is specified more than once.", def.name, table.name)));
                    }
                    let nullable = def.nullable.unwrap_or(true) && def.identity.is_none();
                    if !nullable && def.default.is_none() && def.identity.is_none() && !table.rows.is_empty() {
                        return Err(SqlError::new(4901, 16, format!("ALTER TABLE only allows columns to be added that can contain nulls, or have a DEFAULT definition specified, or the column being added is an identity or timestamp column, or alternatively if none of the previous conditions are satisfied the table must be empty to allow addition of this column. Column '{}' cannot be added to non-empty table '{}' because it does not satisfy these conditions.", def.name, table.name)));
                    }
                    let built = self.build_table(&table.schema, &table.name, std::slice::from_ref(def), &[])?;
                    let mut column = built.columns.into_iter().next().ok_or_else(|| invalid_object(&def.name))?;
                    column.nullable = nullable;
                    // Existing rows get the default when the column is NOT NULL (or WITH VALUES).
                    let fill = if !nullable && column.default.is_some() {
                        match &column.default {
                            Some((_, text)) => {
                                let expr = crate::parser::parse_expression(text)?;
                                self.eval_plain(&expr)?.convert(&column.ty)?
                            }
                            None => crate::value::Value::Null,
                        }
                    } else {
                        crate::value::Value::Null
                    };
                    let mut counter = None;
                    for row in table.rows.values_mut() {
                        if let Some(identity) = &mut column.identity {
                            let next = identity.current.map_or(identity.seed, |c| c + identity.increment);
                            identity.current = Some(next);
                            counter = Some(next);
                            row.push(crate::value::Value::Int(next));
                        } else {
                            row.push(fill.clone());
                        }
                    }
                    let _ = counter;
                    table.columns.push(column);
                    for constraint in &def.constraints {
                        self.add_constraint(&mut table, constraint)?;
                    }
                }
            }
            AlterAction::AddConstraints(constraints) => {
                for constraint in constraints {
                    self.add_constraint(&mut table, constraint)?;
                }
                // Existing rows must satisfy new keys.
                let shape_rows: Vec<Vec<crate::value::Value>> = table.rows.values().cloned().collect();
                for constraint in &table.constraints {
                    let cols = match &constraint.rule {
                        ConstraintRule::PrimaryKey(c) | ConstraintRule::Unique(c) => c,
                        _ => continue,
                    };
                    for (i, row) in shape_rows.iter().enumerate() {
                        if shape_rows[..i].iter().any(|o| cols.iter().all(|&c| crate::value::same(&o[c], &row[c]))) {
                            return Err(SqlError::new(1505, 16, format!("The CREATE UNIQUE INDEX statement terminated because a duplicate key was found for the object name '{}.{}' and the index name '{}'.", table.schema, table.name, constraint.name)));
                        }
                    }
                }
            }
            AlterAction::DropColumns(names) => {
                for column_name in names {
                    let index = table.column_index(column_name).ok_or_else(|| {
                        SqlError::new(4924, 16, format!("ALTER TABLE DROP COLUMN failed because column '{column_name}' does not exist in table '{}'.", table.name))
                    })?;
                    let used = table.constraints.iter().any(|c| match &c.rule {
                        ConstraintRule::PrimaryKey(cols) | ConstraintRule::Unique(cols) => cols.contains(&index),
                        ConstraintRule::ForeignKey { columns, .. } => columns.contains(&index),
                        ConstraintRule::Check(_) => false,
                    }) || table.columns[index].default.is_some();
                    if used {
                        return Err(SqlError::new(5074, 16, format!("The object is dependent on column '{column_name}'.")));
                    }
                    table.columns.remove(index);
                    for row in table.rows.values_mut() {
                        row.remove(index);
                    }
                    let shift = |cols: &mut Vec<usize>| {
                        for c in cols.iter_mut() {
                            if *c > index {
                                *c -= 1;
                            }
                        }
                    };
                    for constraint in &mut table.constraints {
                        match &mut constraint.rule {
                            ConstraintRule::PrimaryKey(cols) | ConstraintRule::Unique(cols) => shift(cols),
                            ConstraintRule::ForeignKey { columns, .. } => shift(columns),
                            ConstraintRule::Check(_) => {}
                        }
                    }
                    table.indexes.retain(|i| !i.columns.contains(&index));
                    for i in &mut table.indexes {
                        shift(&mut i.columns);
                    }
                }
            }
            AlterAction::DropConstraint(constraint_name) => {
                let before = table.constraints.len();
                table.constraints.retain(|c| !c.name.eq_ignore_ascii_case(constraint_name));
                let mut found = table.constraints.len() != before;
                for column in &mut table.columns {
                    if column.default.as_ref().is_some_and(|(n, _)| n.eq_ignore_ascii_case(constraint_name)) {
                        column.default = None;
                        found = true;
                    }
                }
                if !found {
                    return Err(SqlError::new(3728, 16, format!("'{constraint_name}' is not a constraint.")));
                }
            }
            AlterAction::AlterColumn { name: column_name, ty, nullable } => {
                let index = table.column_index(column_name).ok_or_else(|| SqlError::new(4902, 16, format!("Cannot find the object \"{}\" because it does not exist or you do not have permissions.", table.name)))?;
                for row in table.rows.values_mut() {
                    row[index] = row[index].convert(ty)?;
                }
                table.columns[index].ty = ty.clone();
                if let Some(n) = nullable {
                    if !n && table.rows.values().any(|r| r[index].is_null()) {
                        return Err(SqlError::new(515, 16, format!("Cannot insert the value NULL into column '{column_name}', table '{}'; column does not allow nulls. UPDATE fails.", table.name)));
                    }
                    table.columns[index].nullable = *n;
                }
            }
        }
        if let Some(slot) = self.table_mut(&loc) {
            *slot = table;
        }
        self.log(Undo::Table(loc, Some(Box::new(previous))));
        Ok(())
    }

    pub(super) fn create_index(&mut self, name: &str, table_name: &ObjectName, columns: &[String], unique: bool) -> Result<(), SqlError> {
        let loc = self.resolve_table(table_name)?;
        let previous = self.table(&loc).cloned().ok_or_else(|| invalid_object(&table_name.display()))?;
        if previous.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(name)) {
            return Err(SqlError::new(1913, 16, format!("The operation failed because an index or statistics with name '{name}' already exists on table '{}'.", table_name.display())));
        }
        let mut cols = Vec::new();
        for c in columns {
            cols.push(previous.column_index(c).ok_or_else(|| SqlError::new(1911, 16, format!("Column name '{c}' does not exist in the target table or view.")))?);
        }
        if unique {
            let rows: Vec<&Vec<crate::value::Value>> = previous.rows.values().collect();
            for (i, row) in rows.iter().enumerate() {
                if rows[..i].iter().any(|o| cols.iter().all(|&c| crate::value::same(&o[c], &row[c]))) {
                    return Err(SqlError::new(1505, 16, format!("The CREATE UNIQUE INDEX statement terminated because a duplicate key was found for the object name '{}.{}' and the index name '{name}'.", previous.schema, previous.name)));
                }
            }
        }
        if let Some(table) = self.table_mut(&loc) {
            table.indexes.push(Index { name: name.to_string(), columns: cols, unique });
        }
        self.log(Undo::Table(loc, Some(Box::new(previous))));
        Ok(())
    }

    pub(super) fn drop_index(&mut self, name: &str, table_name: Option<&ObjectName>, if_exists: bool) -> Result<(), SqlError> {
        let Some(table_name) = table_name else {
            return Err(SqlError::new(159, 15, "Must specify the table name and index name for the DROP INDEX statement."));
        };
        let loc = match self.resolve_table(table_name) {
            Ok(loc) => loc,
            Err(_) if if_exists => return Ok(()),
            Err(e) => return Err(e),
        };
        let previous = self.table(&loc).cloned().ok_or_else(|| invalid_object(&table_name.display()))?;
        if !previous.indexes.iter().any(|i| i.name.eq_ignore_ascii_case(name)) {
            if if_exists {
                return Ok(());
            }
            return Err(SqlError::new(3701, 11, format!("Cannot drop the index '{}.{name}', because it does not exist or you do not have permission.", table_name.display())));
        }
        if let Some(table) = self.table_mut(&loc) {
            table.indexes.retain(|i| !i.name.eq_ignore_ascii_case(name));
        }
        self.log(Undo::Table(loc, Some(Box::new(previous))));
        Ok(())
    }

    pub(super) fn create_procedure(&mut self, procedure: &Procedure) -> Result<(), SqlError> {
        if procedure.name.database.as_ref().is_some_and(|d| !d.eq_ignore_ascii_case(&self.current_db())) {
            return Err(SqlError::new(166, 15, "'CREATE/ALTER PROCEDURE' does not allow specifying the database name as a prefix to the object name."));
        }
        let db = self.current_db();
        let schema = procedure.name.schema.clone().unwrap_or_else(|| "dbo".into());
        let key = object_key(&schema, &procedure.name.name);
        let database = self.state.database(&db).ok_or_else(|| invalid_object(&db))?;
        if !database.has_schema(&schema) {
            return Err(SqlError::new(2760, 16, format!("The specified schema name \"{schema}\" either does not exist or you do not have permission to use it.")));
        }
        let existing = database.procedures.get(&key).cloned();
        if database.tables.contains_key(&key) {
            return Err(exists_error(&procedure.name.name));
        }
        if existing.is_some() && !procedure.or_alter && !procedure.alter {
            return Err(exists_error(&procedure.name.name));
        }
        if existing.is_none() && procedure.alter {
            return Err(SqlError::new(208, 16, format!("Invalid object name '{}'.", procedure.name.display())));
        }
        let now = chrono::Utc::now().naive_utc();
        let object_id = match &existing {
            Some(p) => p.object_id,
            None => self.state.new_object_id(),
        };
        let stored = StoredProcedure {
            schema: schema.clone(),
            name: procedure.name.name.clone(),
            object_id,
            definition: procedure.definition.clone(),
            created: existing.as_ref().map_or(now, |p| p.created),
            modified: now,
        };
        let db_key = db.to_lowercase();
        if let Some(database) = self.state.database_mut(&db) {
            database.procedures.insert(key.clone(), stored);
        }
        self.log(Undo::Procedure { db: db_key, key, previous: existing });
        Ok(())
    }

    pub(super) fn drop_procedure(&mut self, name: &ObjectName, if_exists: bool) -> Result<(), SqlError> {
        let db = name.database.clone().unwrap_or_else(|| self.current_db());
        let schema = name.schema.clone().unwrap_or_else(|| "dbo".into());
        let key = object_key(&schema, &name.name);
        let removed = self.state.database_mut(&db).and_then(|d| d.procedures.remove(&key));
        match removed {
            Some(previous) => {
                self.log(Undo::Procedure { db: db.to_lowercase(), key, previous: Some(previous) });
                Ok(())
            }
            None if if_exists => Ok(()),
            None => Err(SqlError::new(3701, 11, format!("Cannot drop the procedure '{}', because it does not exist or you do not have permission.", name.display()))),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
