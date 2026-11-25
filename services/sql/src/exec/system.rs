//! Catalog views (`sys.*`, `INFORMATION_SCHEMA.*`) and metadata functions.

use super::query::{Layout, LayoutColumn, Rowset};
use super::{Executor, SqlError};
use crate::ast::{Expr, ObjectName};
use crate::catalog::{ConstraintRule, Database};
use crate::parser::parse_expression;
use crate::value::{SqlType, Value};

fn layout(columns: &[(&str, SqlType)]) -> Layout {
    Layout {
        columns: columns
            .iter()
            .map(|(name, ty)| LayoutColumn { qualifier: None, name: name.to_string(), ty: ty.clone(), nullable: true, identity: false, hidden: false })
            .collect(),
    }
}

fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

const NAME: SqlType = SqlType::NVarChar(Some(128));

fn constraint_type(rule: &ConstraintRule) -> (&'static str, &'static str) {
    match rule {
        ConstraintRule::PrimaryKey(_) => ("PK", "PRIMARY_KEY_CONSTRAINT"),
        ConstraintRule::Unique(_) => ("UQ", "UNIQUE_CONSTRAINT"),
        ConstraintRule::ForeignKey { .. } => ("F", "FOREIGN_KEY_CONSTRAINT"),
        ConstraintRule::Check(_) => ("C", "CHECK_CONSTRAINT"),
    }
}

/// Every object in a database: (object id, schema, name, type, type description, parent id, created).
fn objects(database: &Database) -> Vec<(i64, String, String, &'static str, &'static str, i64, chrono::NaiveDateTime)> {
    let mut out = Vec::new();
    for table in database.tables.values() {
        out.push((table.object_id, table.schema.clone(), table.name.clone(), "U", "USER_TABLE", 0, table.created));
        for c in &table.constraints {
            let (t, d) = constraint_type(&c.rule);
            out.push((c.object_id, table.schema.clone(), c.name.clone(), t, d, table.object_id, table.created));
        }
        for c in &table.columns {
            if let Some((name, _)) = &c.default {
                let id = (table.object_id ^ (name.len() as i64 * 7919)) + 1;
                out.push((id, table.schema.clone(), name.clone(), "D", "DEFAULT_CONSTRAINT", table.object_id, table.created));
            }
        }
    }
    for p in database.procedures.values() {
        out.push((p.object_id, p.schema.clone(), p.name.clone(), "P", "SQL_STORED_PROCEDURE", 0, p.created));
    }
    out
}

impl Executor<'_> {
    /// A catalog view by name, if `name` is one.
    pub(super) fn system_view(&mut self, name: &ObjectName) -> Result<Option<Rowset>, SqlError> {
        let Some(schema) = &name.schema else { return Ok(None) };
        let schema = schema.to_lowercase();
        if schema != "sys" && schema != "information_schema" {
            return Ok(None);
        }
        let db_name = name.database.clone().unwrap_or_else(|| self.current_db());
        let Some(database) = self.state.database(&db_name) else {
            return Err(SqlError::new(2702, 16, format!("Database '{db_name}' does not exist.")).batch_abort());
        };
        let view = format!("{schema}.{}", name.name.to_lowercase());
        let rowset = match view.as_str() {
            "sys.databases" => Rowset {
                layout: layout(&[
                    ("name", NAME),
                    ("database_id", SqlType::Int),
                    ("create_date", SqlType::DateTime),
                    ("compatibility_level", SqlType::TinyInt),
                    ("collation_name", NAME),
                    ("state", SqlType::TinyInt),
                    ("state_desc", SqlType::NVarChar(Some(60))),
                    ("user_access_desc", SqlType::NVarChar(Some(60))),
                    ("is_read_only", SqlType::Bit),
                ]),
                rows: {
                    let mut dbs: Vec<&Database> = self.state.databases.values().collect();
                    dbs.sort_by_key(|d| d.id);
                    dbs.iter()
                        .map(|d| {
                            vec![
                                s(&d.name),
                                Value::Int(d.id),
                                Value::DateTime(d.created),
                                Value::Int(160),
                                s("SQL_Latin1_General_CP1_CI_AS"),
                                Value::Int(0),
                                s("ONLINE"),
                                s("MULTI_USER"),
                                Value::Bit(false),
                            ]
                        })
                        .collect()
                },
            },
            "sys.schemas" => Rowset {
                layout: layout(&[("name", NAME), ("schema_id", SqlType::Int), ("principal_id", SqlType::Int)]),
                rows: database.schemas.iter().map(|sch| vec![s(sch), Value::Int(database.schema_id(sch)), Value::Int(1)]).collect(),
            },
            "sys.objects" | "sys.all_objects" | "sys.tables" | "sys.procedures" => {
                let filter = match view.as_str() {
                    "sys.tables" => Some("U"),
                    "sys.procedures" => Some("P"),
                    _ => None,
                };
                Rowset {
                    layout: layout(&[
                        ("name", NAME),
                        ("object_id", SqlType::Int),
                        ("schema_id", SqlType::Int),
                        ("parent_object_id", SqlType::Int),
                        ("type", SqlType::Char(2)),
                        ("type_desc", SqlType::NVarChar(Some(60))),
                        ("create_date", SqlType::DateTime),
                        ("modify_date", SqlType::DateTime),
                        ("is_ms_shipped", SqlType::Bit),
                    ]),
                    rows: objects(database)
                        .into_iter()
                        .filter(|o| filter.is_none_or(|f| f == o.3))
                        .map(|(id, sch, name, t, d, parent, created)| {
                            vec![
                                s(&name),
                                Value::Int(id),
                                Value::Int(database.schema_id(&sch)),
                                Value::Int(parent),
                                Value::Str(format!("{t:<2}")),
                                s(d),
                                Value::DateTime(created),
                                Value::DateTime(created),
                                Value::Bit(false),
                            ]
                        })
                        .collect(),
                }
            }
            "sys.columns" | "sys.all_columns" => Rowset {
                layout: layout(&[
                    ("object_id", SqlType::Int),
                    ("name", NAME),
                    ("column_id", SqlType::Int),
                    ("max_length", SqlType::SmallInt),
                    ("is_nullable", SqlType::Bit),
                    ("is_identity", SqlType::Bit),
                ]),
                rows: database
                    .tables
                    .values()
                    .flat_map(|t| {
                        t.columns.iter().enumerate().map(move |(i, c)| {
                            vec![Value::Int(t.object_id), s(&c.name), Value::Int(i as i64 + 1), Value::Int(c.ty.storage_length()), Value::Bit(c.nullable), Value::Bit(c.identity.is_some())]
                        })
                    })
                    .collect(),
            },
            "sys.indexes" => Rowset {
                layout: layout(&[("object_id", SqlType::Int), ("name", NAME), ("index_id", SqlType::Int), ("type_desc", SqlType::NVarChar(Some(60))), ("is_unique", SqlType::Bit), ("is_primary_key", SqlType::Bit)]),
                rows: database
                    .tables
                    .values()
                    .flat_map(|t| {
                        let mut rows = Vec::new();
                        let mut id = 1;
                        for c in &t.constraints {
                            if let ConstraintRule::PrimaryKey(_) | ConstraintRule::Unique(_) = c.rule {
                                let primary = matches!(c.rule, ConstraintRule::PrimaryKey(_));
                                rows.push(vec![Value::Int(t.object_id), s(&c.name), Value::Int(id), s(if primary { "CLUSTERED" } else { "NONCLUSTERED" }), Value::Bit(true), Value::Bit(primary)]);
                                id += 1;
                            }
                        }
                        for i in &t.indexes {
                            rows.push(vec![Value::Int(t.object_id), s(&i.name), Value::Int(id), s("NONCLUSTERED"), Value::Bit(i.unique), Value::Bit(false)]);
                            id += 1;
                        }
                        rows
                    })
                    .collect(),
            },
            "sys.foreign_keys" => Rowset {
                layout: layout(&[("name", NAME), ("object_id", SqlType::Int), ("parent_object_id", SqlType::Int), ("referenced_object_id", SqlType::Int), ("delete_referential_action_desc", SqlType::NVarChar(Some(60)))]),
                rows: database
                    .tables
                    .values()
                    .flat_map(|t| {
                        t.constraints.iter().filter_map(move |c| match &c.rule {
                            ConstraintRule::ForeignKey { table, cascade, .. } => Some(vec![
                                s(&c.name),
                                Value::Int(c.object_id),
                                Value::Int(t.object_id),
                                database.tables.get(table).map_or(Value::Null, |p| Value::Int(p.object_id)),
                                s(if *cascade { "CASCADE" } else { "NO_ACTION" }),
                            ]),
                            _ => None,
                        })
                    })
                    .collect(),
            },
            "sys.sql_modules" => Rowset {
                layout: layout(&[("object_id", SqlType::Int), ("definition", SqlType::NVarChar(None))]),
                rows: database.procedures.values().map(|p| vec![Value::Int(p.object_id), s(&p.definition)]).collect(),
            },
            "information_schema.tables" => Rowset {
                layout: layout(&[("TABLE_CATALOG", NAME), ("TABLE_SCHEMA", NAME), ("TABLE_NAME", NAME), ("TABLE_TYPE", SqlType::VarChar(Some(10)))]),
                rows: database.tables.values().map(|t| vec![s(&database.name), s(&t.schema), s(&t.name), s("BASE TABLE")]).collect(),
            },
            "information_schema.columns" => Rowset {
                layout: layout(&[
                    ("TABLE_CATALOG", NAME),
                    ("TABLE_SCHEMA", NAME),
                    ("TABLE_NAME", NAME),
                    ("COLUMN_NAME", NAME),
                    ("ORDINAL_POSITION", SqlType::Int),
                    ("COLUMN_DEFAULT", SqlType::NVarChar(Some(4000))),
                    ("IS_NULLABLE", SqlType::VarChar(Some(3))),
                    ("DATA_TYPE", NAME),
                    ("CHARACTER_MAXIMUM_LENGTH", SqlType::Int),
                ]),
                rows: database
                    .tables
                    .values()
                    .flat_map(|t| {
                        let db = database.name.clone();
                        t.columns.iter().enumerate().map(move |(i, c)| {
                            vec![
                                s(&db),
                                s(&t.schema),
                                s(&t.name),
                                s(&c.name),
                                Value::Int(i as i64 + 1),
                                c.default.as_ref().map_or(Value::Null, |(_, d)| Value::Str(format!("({d})"))),
                                s(if c.nullable { "YES" } else { "NO" }),
                                s(c.ty.name()),
                                if c.ty.is_string() || matches!(c.ty, SqlType::Binary(_) | SqlType::VarBinary(_)) {
                                    Value::Int(c.ty.max_length().map_or(-1, |n| n as i64))
                                } else {
                                    Value::Null
                                },
                            ]
                        })
                    })
                    .collect(),
            },
            "information_schema.routines" => Rowset {
                layout: layout(&[("ROUTINE_CATALOG", NAME), ("ROUTINE_SCHEMA", NAME), ("ROUTINE_NAME", NAME), ("ROUTINE_TYPE", SqlType::NVarChar(Some(20))), ("ROUTINE_DEFINITION", SqlType::NVarChar(Some(4000)))]),
                rows: database
                    .procedures
                    .values()
                    .map(|p| vec![s(&database.name), s(&p.schema), s(&p.name), s("PROCEDURE"), Value::Str(p.definition.chars().take(4000).collect())])
                    .collect(),
            },
            "information_schema.schemata" => Rowset {
                layout: layout(&[("CATALOG_NAME", NAME), ("SCHEMA_NAME", NAME), ("SCHEMA_OWNER", NAME)]),
                rows: database.schemas.iter().map(|sch| vec![s(&database.name), s(sch), s("dbo")]).collect(),
            },
            _ => {
                return Err(SqlError::new(
                    208,
                    16,
                    format!("Invalid object name '{}'. (The Azure emulator's SQL data plane does not model this catalog view.)", name.display()),
                )
                .batch_abort())
            }
        };
        Ok(Some(rowset))
    }

    fn parse_object(&self, text: &str) -> Option<ObjectName> {
        let text = text.replace("..", ".dbo.");
        match parse_expression(&text) {
            Ok(Expr::Column(parts)) => Some(super::object_from_parts(parts)),
            _ => None,
        }
    }

    pub(super) fn object_id(&self, name: &str, kind: Option<&str>) -> Value {
        let Some(object) = self.parse_object(name) else { return Value::Null };
        let kind = kind.map(|k| k.trim().to_ascii_uppercase());
        if object.name.starts_with('#') {
            if kind.as_deref().is_some_and(|k| k != "U") {
                return Value::Null;
            }
            return self.session.temp.get(&object.name.to_lowercase()).map_or(Value::Null, |t| Value::Int(t.object_id));
        }
        let db = object.database.clone().unwrap_or_else(|| self.current_db());
        let Some(database) = self.state.database(&db) else { return Value::Null };
        let schema = object.schema.clone().unwrap_or_else(|| "dbo".into());
        objects(database)
            .into_iter()
            .find(|o| o.1.eq_ignore_ascii_case(&schema) && o.2.eq_ignore_ascii_case(&object.name) && kind.as_deref().is_none_or(|k| k == o.3))
            .map_or(Value::Null, |o| Value::Int(o.0))
    }

    fn find_object(&self, id: i64) -> Option<(String, String, Option<String>)> {
        let db = self.current_db();
        let database = self.state.database(&db)?;
        let found = objects(database).into_iter().find(|o| o.0 == id)?;
        let definition = database.procedures.values().find(|p| p.object_id == id).map(|p| p.definition.clone());
        Some((found.1, found.2, definition))
    }

    pub(super) fn object_name(&self, id: i64) -> Value {
        self.find_object(id).map_or(Value::Null, |(_, name, _)| Value::Str(name))
    }

    pub(super) fn object_schema_name(&self, id: i64) -> Value {
        self.find_object(id).map_or(Value::Null, |(schema, _, _)| Value::Str(schema))
    }

    pub(super) fn object_definition(&self, id: i64) -> Value {
        self.find_object(id).and_then(|(_, _, d)| d).map_or(Value::Null, Value::Str)
    }

    pub(super) fn col_length(&self, table: &str, column: &str) -> Value {
        let Some(object) = self.parse_object(table) else { return Value::Null };
        let Ok(loc) = self.resolve_table(&object) else { return Value::Null };
        self.table(&loc)
            .and_then(|t| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(column)))
            .map_or(Value::Null, |c| Value::Int(c.ty.storage_length()))
    }

    pub(super) fn col_name(&self, table_id: i64, column_id: i64) -> Value {
        let db = self.current_db();
        self.state
            .database(&db)
            .and_then(|d| d.tables.values().find(|t| t.object_id == table_id))
            .and_then(|t| t.columns.get((column_id - 1).max(0) as usize))
            .map_or(Value::Null, |c| Value::Str(c.name.clone()))
    }
}
