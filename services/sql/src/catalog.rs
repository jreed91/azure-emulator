//! What the SQL data plane stores: logins, databases, tables and procedures.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::value::{SqlType, Value};

/// The SQL data plane's state, shared by every connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SqlState {
    /// SQL logins by lowercased name.
    pub logins: BTreeMap<String, Login>,
    /// Databases by lowercased name.
    pub databases: BTreeMap<String, Database>,
    pub next_database_id: i64,
    pub next_object_id: i64,
    /// Whether `sa` accepts any password, until a password is configured.
    #[serde(default)]
    pub any_sa_password: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Login {
    pub name: String,
    pub password: String,
    pub default_database: String,
}

impl Default for SqlState {
    fn default() -> Self {
        let mut state = SqlState {
            logins: BTreeMap::new(),
            databases: BTreeMap::new(),
            next_database_id: 1,
            next_object_id: 1_000_000_000,
            any_sa_password: true,
        };
        for name in ["master", "tempdb", "model", "msdb"] {
            state.create_database(name);
        }
        state.logins.insert(
            "sa".into(),
            Login { name: "sa".into(), password: String::new(), default_database: "master".into() },
        );
        state
    }
}

impl SqlState {
    pub fn create_database(&mut self, name: &str) -> &mut Database {
        let id = self.next_database_id;
        self.next_database_id += 1;
        let created = chrono::Utc::now().naive_utc();
        self.databases.entry(name.to_lowercase()).or_insert_with(|| Database {
            name: name.to_string(),
            id,
            created,
            schemas: vec!["dbo".into(), "sys".into(), "INFORMATION_SCHEMA".into(), "guest".into()],
            tables: BTreeMap::new(),
            procedures: BTreeMap::new(),
        })
    }

    pub fn database(&self, name: &str) -> Option<&Database> {
        self.databases.get(&name.to_lowercase())
    }

    pub fn database_mut(&mut self, name: &str) -> Option<&mut Database> {
        self.databases.get_mut(&name.to_lowercase())
    }

    pub fn new_object_id(&mut self) -> i64 {
        self.next_object_id += 1;
        self.next_object_id
    }

    /// Sets a login's password, creating the login if needed. Setting the `sa`
    /// password stops `sa` accepting any password.
    pub fn set_login(&mut self, name: &str, password: &str) {
        if name.eq_ignore_ascii_case("sa") {
            self.any_sa_password = false;
        }
        self.logins.insert(
            name.to_lowercase(),
            Login { name: name.to_string(), password: password.to_string(), default_database: "master".into() },
        );
    }

    /// Checks a login's password; returns the login's own name.
    pub fn authenticate(&self, name: &str, password: &str) -> Option<&Login> {
        let login = self.logins.get(&name.to_lowercase())?;
        if login.password == password || (name.eq_ignore_ascii_case("sa") && self.any_sa_password) {
            Some(login)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Database {
    pub name: String,
    pub id: i64,
    pub created: chrono::NaiveDateTime,
    pub schemas: Vec<String>,
    /// Tables by lowercased `schema.name`.
    pub tables: BTreeMap<String, Table>,
    /// Procedures by lowercased `schema.name`.
    pub procedures: BTreeMap<String, StoredProcedure>,
}

impl Database {
    pub fn has_schema(&self, schema: &str) -> bool {
        self.schemas.iter().any(|s| s.eq_ignore_ascii_case(schema))
    }

    pub fn schema_id(&self, schema: &str) -> i64 {
        match schema.to_ascii_lowercase().as_str() {
            "dbo" => 1,
            "guest" => 2,
            "information_schema" => 3,
            "sys" => 4,
            _ => self.schemas.iter().position(|s| s.eq_ignore_ascii_case(schema)).map_or(0, |i| i as i64 + 5),
        }
    }
}

pub fn object_key(schema: &str, name: &str) -> String {
    format!("{}.{}", schema.to_lowercase(), name.to_lowercase())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredProcedure {
    pub schema: String,
    pub name: String,
    pub object_id: i64,
    /// The `CREATE PROCEDURE` text, parsed again when the procedure runs.
    pub definition: String,
    pub created: chrono::NaiveDateTime,
    pub modified: chrono::NaiveDateTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
    pub identity: Option<Identity>,
    /// The default's constraint name and expression text.
    pub default: Option<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
    /// The last value given out, if any.
    pub current: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConstraintRule {
    PrimaryKey(Vec<usize>),
    Unique(Vec<usize>),
    /// Columns here, the table they reference (database-local `schema.name`) and its columns.
    ForeignKey { columns: Vec<usize>, table: String, ref_columns: Vec<String>, cascade: bool },
    /// The condition's text.
    Check(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Constraint {
    pub name: String,
    pub object_id: i64,
    pub rule: ConstraintRule,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<usize>,
    pub unique: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub object_id: i64,
    pub columns: Vec<Column>,
    pub constraints: Vec<Constraint>,
    pub indexes: Vec<Index>,
    /// Rows by an internal, ever-increasing row id.
    pub rows: BTreeMap<u64, Vec<Value>>,
    pub next_row: u64,
    #[serde(default)]
    pub identity_insert: bool,
    pub created: chrono::NaiveDateTime,
}

impl Table {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn primary_key(&self) -> Option<&[usize]> {
        self.constraints.iter().find_map(|c| match &c.rule {
            ConstraintRule::PrimaryKey(columns) => Some(columns.as_slice()),
            _ => None,
        })
    }

    /// Row ids in the order a scan returns them: by primary key when there is one.
    pub fn scan_order(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.rows.keys().copied().collect();
        if let Some(key) = self.primary_key() {
            let key = key.to_vec();
            ids.sort_by(|a, b| {
                let (ra, rb) = (&self.rows[a], &self.rows[b]);
                for &c in &key {
                    let ordering = crate::value::sort_order(&ra[c], &rb[c]);
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        ids
    }
}
