//! Runs parsed T-SQL against the shared state, one session at a time.

mod ddl;
mod dml;
mod expr;
mod json;
mod query;
mod system;

use std::collections::{BTreeMap, HashMap};

use crate::ast::*;
use crate::catalog::{object_key, SqlState, StoredProcedure, Table};
use crate::parser::{parse_batch, parse_param_list};
use crate::value::{SqlType, Value};

pub use query::{Layout, LayoutColumn};

/// An error as SQL Server reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlError {
    pub number: i32,
    pub class: u8,
    pub state: u8,
    pub message: String,
    pub line: u32,
    pub procedure: String,
    /// Ends the batch, not just the statement.
    pub abort_batch: bool,
}

impl SqlError {
    pub fn new(number: i32, class: u8, message: impl Into<String>) -> Self {
        SqlError { number, class, state: 1, message: message.into(), line: 0, procedure: String::new(), abort_batch: false }
    }

    pub fn batch_abort(mut self) -> Self {
        self.abort_batch = true;
        self
    }

    pub fn state(mut self, state: u8) -> Self {
        self.state = state;
        self
    }
}

pub fn invalid_object(name: &str) -> SqlError {
    SqlError::new(208, 16, format!("Invalid object name '{name}'.")).batch_abort()
}

/// A request failed; its error has already been reported as an [`Event::Error`].
#[derive(Debug)]
pub struct Reported;

/// A column of a result set.
#[derive(Clone, Debug)]
pub struct ColumnMeta {
    pub name: String,
    pub ty: SqlType,
    pub nullable: bool,
    pub identity: bool,
}

/// What running a request produces, in order; the TDS layer turns each into tokens.
#[derive(Clone, Debug)]
pub enum Event {
    ResultSet { columns: Vec<ColumnMeta>, rows: Vec<Vec<Value>> },
    /// The end of a statement. `count` is the rows it affected or returned, when reported.
    Done { count: Option<u64>, in_proc: bool, error: bool },
    /// The end of a procedure run by `EXEC` inside a batch.
    DoneProc { count: Option<u64>, error: bool },
    ReturnStatus(i32),
    Info { number: i32, class: u8, state: u8, message: String, line: u32, procedure: String },
    Error(SqlError),
    Database { new: String, old: String },
    BeginTransaction(u64),
    CommitTransaction(u64),
    RollbackTransaction(u64),
}

/// Where a table lives.
#[derive(Clone, Debug, PartialEq)]
pub enum TableLoc {
    Db { db: String, key: String },
    Temp(String),
    Var(String),
}

/// One change to undo when a transaction rolls back.
#[derive(Clone, Debug)]
enum Undo {
    Inserted(TableLoc, u64),
    Deleted(TableLoc, u64, Vec<Value>),
    Updated(TableLoc, u64, Vec<Value>),
    /// A table as it was before DDL (`None`: it did not exist).
    Table(TableLoc, Option<Box<Table>>),
    Procedure { db: String, key: String, previous: Option<StoredProcedure> },
}

#[derive(Debug)]
struct Transaction {
    count: u32,
    descriptor: u64,
    undo: Vec<Undo>,
    savepoints: Vec<(String, usize)>,
}

/// One connection's state.
#[derive(Debug)]
pub struct Session {
    pub spid: u16,
    pub login: String,
    pub database: String,
    pub app_name: String,
    pub host_name: String,
    transaction: Option<Transaction>,
    next_descriptor: u64,
    xact_abort: bool,
    nocount: bool,
    /// `@@IDENTITY`.
    identity: Value,
    /// `#temp` tables by lowercased name.
    temp: BTreeMap<String, Table>,
    context: HashMap<String, Value>,
    rowcount: i64,
    last_error: i32,
}

impl Session {
    pub fn new(spid: u16, login: &str, database: &str) -> Self {
        Session {
            spid,
            login: login.to_string(),
            database: database.to_string(),
            app_name: String::new(),
            host_name: String::new(),
            transaction: None,
            next_descriptor: (spid as u64) << 32,
            xact_abort: false,
            nocount: false,
            identity: Value::Null,
            temp: BTreeMap::new(),
            context: HashMap::new(),
            rowcount: 0,
            last_error: 0,
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    pub fn transaction_descriptor(&self) -> u64 {
        self.transaction.as_ref().map_or(0, |t| t.descriptor)
    }
}

/// A variable.
#[derive(Clone, Debug)]
pub enum Variable {
    Scalar { ty: SqlType, value: Value },
    Table(Box<Table>),
}

/// The variables and context of one batch or procedure.
#[derive(Debug)]
struct Frame {
    vars: HashMap<String, Variable>,
    database: String,
    procedure: Option<String>,
    scope_identity: Value,
    ctes: Vec<(String, query::Rowset)>,
}

impl Frame {
    fn new(database: &str, procedure: Option<String>) -> Self {
        Frame { vars: HashMap::new(), database: database.to_string(), procedure, scope_identity: Value::Null, ctes: Vec::new() }
    }
}

/// What a statement tells the block running it.
enum Flow {
    Next,
    Break,
    Continue,
    Return(Option<i64>),
}

/// An argument to a procedure.
#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Value,
    pub ty: Option<SqlType>,
    pub output: bool,
    pub default: bool,
}

/// An output parameter's value after a procedure ran: the argument's position, name, value and type.
#[derive(Clone, Debug)]
pub struct OutputValue {
    pub ordinal: usize,
    pub name: String,
    pub value: Value,
    pub ty: SqlType,
}

pub struct Executor<'a> {
    state: &'a mut SqlState,
    session: &'a mut Session,
    out: &'a mut Vec<Event>,
    frames: Vec<Frame>,
    /// The errors being handled by the `CATCH` blocks running now.
    caught: Vec<SqlError>,
    try_depth: usize,
    /// Set while a DML statement reads its target table, to expose row ids.
    rowid_target: Option<TableLoc>,
    windows: HashMap<usize, Vec<Value>>,
    /// Requests are run with every statement reported as inside a procedure (RPC).
    rpc: bool,
}

pub const MAX_NESTING: usize = 32;

impl<'a> Executor<'a> {
    pub fn new(state: &'a mut SqlState, session: &'a mut Session, out: &'a mut Vec<Event>) -> Self {
        Executor { state, session, out, frames: Vec::new(), caught: Vec::new(), try_depth: 0, rowid_target: None, windows: HashMap::new(), rpc: false }
    }

    fn frame(&self) -> &Frame {
        self.frames.last().expect("a frame while running")
    }

    fn frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a frame while running")
    }

    fn current_db(&self) -> String {
        self.frames.last().map_or_else(|| self.session.database.clone(), |f| f.database.clone())
    }

    fn in_proc(&self) -> bool {
        self.rpc || self.frames.len() > 1
    }

    fn done(&mut self, count: Option<u64>) {
        let count = if self.session.nocount { None } else { count };
        let in_proc = self.in_proc();
        self.out.push(Event::Done { count, in_proc, error: false });
    }

    fn report(&mut self, mut error: SqlError) {
        if error.procedure.is_empty() {
            if let Some(name) = self.frames.last().and_then(|f| f.procedure.clone()) {
                error.procedure = name;
            }
        }
        self.session.last_error = error.number;
        let in_proc = self.in_proc();
        self.out.push(Event::Error(error));
        self.out.push(Event::Done { count: None, in_proc, error: true });
    }

    // ----- entry points -----

    /// Runs a SQL batch.
    pub fn run_batch(&mut self, text: &str) {
        let statements = match parse_batch(text) {
            Ok(statements) => statements,
            Err(error) => {
                self.report(error);
                return;
            }
        };
        let database = self.session.database.clone();
        self.frames.push(Frame::new(&database, None));
        if let Err(error) = self.block(&statements).map(|_| ()) {
            self.finish_error(error);
        }
        self.frames.pop();
    }

    /// Runs `sp_executesql`: a statement, its parameter declarations and their values.
    pub fn execute_sql(&mut self, text: &str, params: &str, args: Vec<Arg>) -> Result<Vec<OutputValue>, Reported> {
        self.rpc = true;
        let result = self.execute_sql_inner(text, params, args, 0);
        match result {
            Ok(outputs) => Ok(outputs),
            Err(error) => {
                self.finish_error(error);
                Err(Reported)
            }
        }
    }

    fn execute_sql_inner(&mut self, text: &str, params: &str, args: Vec<Arg>, ordinal_base: usize) -> Result<Vec<OutputValue>, SqlError> {
        let declared = if params.trim().is_empty() { Vec::new() } else { parse_param_list(params)? };
        let statements = parse_batch(text)?;
        let database = self.current_db();
        let mut frame = Frame::new(&database, None);
        let bindings = bind_args(&declared, &args, "sp_executesql", true)?;
        for (param, value) in declared.iter().zip(bindings.iter()) {
            let value = match value {
                Some(v) => v.convert(&param.ty)?,
                None => match &param.default {
                    Some(_) | None => Value::Null,
                },
            };
            frame.vars.insert(param.name.to_lowercase(), Variable::Scalar { ty: param.ty.clone(), value });
        }
        if self.frames.len() >= MAX_NESTING {
            return Err(nesting_error());
        }
        self.frames.push(frame);
        let result = self.block(&statements);
        let frame = self.frames.pop().expect("the pushed frame");
        result?;
        Ok(collect_outputs(&declared, &args, &frame, ordinal_base))
    }

    /// Runs a procedure called by RPC. Returns its status and output parameters.
    pub fn call(&mut self, name: &str, args: Vec<Arg>) -> Result<(i32, Vec<OutputValue>), Reported> {
        self.rpc = true;
        let database = self.session.database.clone();
        self.frames.push(Frame::new(&database, None));
        let object = match crate::parser::parse_expression(name) {
            Ok(Expr::Column(parts)) => object_from_parts(parts),
            _ => ObjectName { database: None, schema: None, name: name.to_string() },
        };
        let result = self.invoke(&object, args);
        self.frames.pop();
        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                self.finish_error(error);
                Err(Reported)
            }
        }
    }

    /// Reports a batch-ending error, rolling back first under `XACT_ABORT`.
    fn finish_error(&mut self, error: SqlError) {
        if self.session.xact_abort && self.session.transaction.is_some() {
            self.rollback_all();
        }
        self.report(error);
    }

    // ----- transactions (also used by the transaction manager requests) -----

    pub fn begin_transaction(&mut self) {
        match &mut self.session.transaction {
            Some(t) => t.count += 1,
            None => {
                self.session.next_descriptor += 1;
                let descriptor = self.session.next_descriptor;
                self.session.transaction = Some(Transaction { count: 1, descriptor, undo: Vec::new(), savepoints: Vec::new() });
                self.out.push(Event::BeginTransaction(descriptor));
            }
        }
    }

    pub fn commit(&mut self) -> Result<(), SqlError> {
        let Some(transaction) = &mut self.session.transaction else {
            return Err(SqlError::new(3902, 16, "The COMMIT TRANSACTION request has no corresponding BEGIN TRANSACTION.").state(1));
        };
        transaction.count -= 1;
        if transaction.count == 0 {
            let descriptor = transaction.descriptor;
            self.session.transaction = None;
            self.out.push(Event::CommitTransaction(descriptor));
        }
        Ok(())
    }

    pub fn rollback(&mut self, name: Option<&str>) -> Result<(), SqlError> {
        let Some(transaction) = &mut self.session.transaction else {
            return Err(SqlError::new(3903, 16, "The ROLLBACK TRANSACTION request has no corresponding BEGIN TRANSACTION.").state(1));
        };
        if let Some(name) = name {
            if let Some(position) = transaction.savepoints.iter().rposition(|(n, _)| n.eq_ignore_ascii_case(name)) {
                let mark = transaction.savepoints[position].1;
                transaction.savepoints.truncate(position + 1);
                let undo: Vec<Undo> = transaction.undo.drain(mark..).collect();
                for change in undo.into_iter().rev() {
                    self.apply_undo(change);
                }
                return Ok(());
            }
        }
        self.rollback_all();
        Ok(())
    }

    pub fn save_transaction(&mut self, name: &str) -> Result<(), SqlError> {
        let Some(transaction) = &mut self.session.transaction else {
            return Err(SqlError::new(628, 16, "Cannot issue SAVE TRANSACTION when there is no active transaction."));
        };
        let mark = transaction.undo.len();
        transaction.savepoints.push((name.to_string(), mark));
        Ok(())
    }

    /// Rolls back the whole transaction, if one is open.
    pub fn rollback_all(&mut self) {
        if let Some(transaction) = self.session.transaction.take() {
            for change in transaction.undo.into_iter().rev() {
                self.apply_undo(change);
            }
            self.out.push(Event::RollbackTransaction(transaction.descriptor));
        }
    }

    fn log(&mut self, change: Undo) {
        // Table variables are not part of transactions.
        let is_var = match &change {
            Undo::Inserted(loc, _) | Undo::Deleted(loc, _, _) | Undo::Updated(loc, _, _) | Undo::Table(loc, _) => matches!(loc, TableLoc::Var(_)),
            Undo::Procedure { .. } => false,
        };
        if is_var {
            return;
        }
        if let Some(transaction) = &mut self.session.transaction {
            transaction.undo.push(change);
        }
    }

    fn apply_undo(&mut self, change: Undo) {
        match change {
            Undo::Inserted(loc, id) => {
                if let Some(table) = self.table_mut(&loc) {
                    table.rows.remove(&id);
                }
            }
            Undo::Deleted(loc, id, row) | Undo::Updated(loc, id, row) => {
                if let Some(table) = self.table_mut(&loc) {
                    table.rows.insert(id, row);
                }
            }
            Undo::Table(loc, previous) => match loc {
                TableLoc::Db { db, key } => {
                    if let Some(database) = self.state.database_mut(&db) {
                        match previous {
                            Some(table) => {
                                database.tables.insert(key, *table);
                            }
                            None => {
                                database.tables.remove(&key);
                            }
                        }
                    }
                }
                TableLoc::Temp(key) => match previous {
                    Some(table) => {
                        self.session.temp.insert(key, *table);
                    }
                    None => {
                        self.session.temp.remove(&key);
                    }
                },
                TableLoc::Var(_) => {}
            },
            Undo::Procedure { db, key, previous } => {
                if let Some(database) = self.state.database_mut(&db) {
                    match previous {
                        Some(p) => {
                            database.procedures.insert(key, p);
                        }
                        None => {
                            database.procedures.remove(&key);
                        }
                    }
                }
            }
        }
    }

    // ----- tables -----

    fn table(&self, loc: &TableLoc) -> Option<&Table> {
        match loc {
            TableLoc::Db { db, key } => self.state.database(db).and_then(|d| d.tables.get(key)),
            TableLoc::Temp(key) => self.session.temp.get(key),
            TableLoc::Var(name) => self.frames.iter().rev().find_map(|f| match f.vars.get(name) {
                Some(Variable::Table(t)) => Some(t.as_ref()),
                _ => None,
            }),
        }
    }

    fn table_mut(&mut self, loc: &TableLoc) -> Option<&mut Table> {
        match loc {
            TableLoc::Db { db, key } => self.state.database_mut(db).and_then(|d| d.tables.get_mut(key)),
            TableLoc::Temp(key) => self.session.temp.get_mut(key),
            TableLoc::Var(name) => self.frames.iter_mut().rev().find_map(|f| match f.vars.get_mut(name) {
                Some(Variable::Table(t)) => Some(t.as_mut()),
                _ => None,
            }),
        }
    }

    /// Finds a table by name: a table variable, a `#temp` table or a database table.
    fn resolve_table(&self, name: &ObjectName) -> Result<TableLoc, SqlError> {
        if name.name.starts_with('@') && name.schema.is_none() {
            let key = name.name.to_lowercase();
            return match self.frame().vars.get(&key) {
                Some(Variable::Table(_)) => Ok(TableLoc::Var(key)),
                _ => Err(SqlError::new(1087, 15, format!("Must declare the table variable \"{}\".", name.name)).batch_abort()),
            };
        }
        if name.name.starts_with('#') {
            let key = name.name.to_lowercase();
            if self.session.temp.contains_key(&key) {
                return Ok(TableLoc::Temp(key));
            }
            return Err(invalid_object(&name.name));
        }
        let db = name.database.clone().unwrap_or_else(|| self.current_db());
        let Some(database) = self.state.database(&db) else {
            return Err(SqlError::new(2702, 16, format!("Database '{db}' does not exist.")).batch_abort());
        };
        let schema = name.schema.as_deref().unwrap_or("dbo");
        let key = object_key(schema, &name.name);
        if database.tables.contains_key(&key) {
            Ok(TableLoc::Db { db: database.name.to_lowercase(), key })
        } else {
            Err(invalid_object(&name.display()))
        }
    }

    // ----- statements -----

    fn block(&mut self, statements: &[Stmt]) -> Result<Flow, SqlError> {
        for statement in statements {
            match self.statement(statement) {
                Ok(Flow::Next) => {}
                Ok(flow) => return Ok(flow),
                Err(mut error) => {
                    if error.line == 0 {
                        error.line = statement.line;
                    }
                    if error.procedure.is_empty() {
                        if let Some(name) = self.frame().procedure.clone() {
                            error.procedure = name;
                        }
                    }
                    if self.session.xact_abort && self.session.transaction.is_some() {
                        error.abort_batch = true;
                    }
                    if error.abort_batch || self.try_depth > 0 {
                        return Err(error);
                    }
                    self.report(error);
                }
            }
        }
        Ok(Flow::Next)
    }

    fn statement(&mut self, statement: &Stmt) -> Result<Flow, SqlError> {
        match &statement.kind {
            StmtKind::Select(query) => {
                let rowset = self.run_query(query, None)?;
                self.session.rowcount = rowset.rows.len() as i64;
                let count = rowset.rows.len() as u64;
                self.emit_rowset(rowset, query.for_json.as_ref());
                self.done(Some(count));
            }
            StmtKind::SelectAssign { assignments, query } => {
                self.select_assign(assignments, query)?;
            }
            StmtKind::Insert(insert) => {
                let count = self.insert(insert)?;
                self.session.rowcount = count as i64;
                self.done(Some(count));
            }
            StmtKind::Update(update) => {
                let count = self.update(update)?;
                self.session.rowcount = count as i64;
                self.done(Some(count));
            }
            StmtKind::Delete(delete) => {
                let count = self.delete(delete)?;
                self.session.rowcount = count as i64;
                self.done(Some(count));
            }
            StmtKind::With { ctes, body } => {
                let saved = self.frame().ctes.len();
                for cte in ctes {
                    let mut rowset = self.run_query(&cte.query, None)?;
                    if !cte.columns.is_empty() {
                        if cte.columns.len() != rowset.layout.columns.len() {
                            return Err(SqlError::new(8158, 16, format!("'{}' has more columns than were specified in the column list.", cte.name)));
                        }
                        for (column, name) in rowset.layout.columns.iter_mut().zip(&cte.columns) {
                            column.name = name.clone();
                        }
                    }
                    self.frame_mut().ctes.push((cte.name.to_lowercase(), rowset));
                }
                let result = self.statement(body);
                self.frame_mut().ctes.truncate(saved);
                return result;
            }
            StmtKind::Declare(declarations) => {
                for declaration in declarations {
                    let key = declaration.name.to_lowercase();
                    let variable = match &declaration.kind {
                        DeclKind::Scalar { ty, init } => {
                            let value = match init {
                                Some(e) => self.eval_plain(e)?.convert(ty)?,
                                None => Value::Null,
                            };
                            Variable::Scalar { ty: ty.clone(), value }
                        }
                        DeclKind::Table(columns, constraints) => {
                            let table = self.build_table("dbo", &declaration.name, columns, constraints)?;
                            Variable::Table(Box::new(table))
                        }
                    };
                    self.frame_mut().vars.insert(key, variable);
                }
            }
            StmtKind::SetVar { name, op, value } => {
                let value = self.eval_plain(value)?;
                self.assign(name, *op, value)?;
            }
            StmtKind::SetOption { option, on } => match option.as_str() {
                "NOCOUNT" => self.session.nocount = *on,
                "XACT_ABORT" => self.session.xact_abort = *on,
                _ => {}
            },
            StmtKind::SetIdentityInsert { table, on } => {
                let loc = self.resolve_table(table)?;
                if let Some(t) = self.table_mut(&loc) {
                    t.identity_insert = *on;
                }
            }
            StmtKind::SetIgnored | StmtKind::AlterDatabase => {}
            StmtKind::If { condition, then, otherwise } => {
                let truth = self.eval_plain(condition)?.truth() == Some(true);
                if truth {
                    return self.statement(then);
                } else if let Some(otherwise) = otherwise {
                    return self.statement(otherwise);
                }
            }
            StmtKind::While { condition, body } => {
                let mut iterations = 0u64;
                while self.eval_plain(condition)?.truth() == Some(true) {
                    iterations += 1;
                    if iterations > 10_000_000 {
                        return Err(SqlError::new(50000, 16, "The emulator stopped a WHILE loop after 10,000,000 iterations.").batch_abort());
                    }
                    match self.statement(body)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Next | Flow::Continue => {}
                    }
                }
            }
            StmtKind::Block(statements) => return self.block(statements),
            StmtKind::TryCatch { body, handler } => {
                self.try_depth += 1;
                let result = self.block(body);
                self.try_depth -= 1;
                match result {
                    Ok(flow) => return Ok(flow),
                    Err(error) => {
                        self.session.last_error = error.number;
                        self.caught.push(error);
                        let result = self.block(handler);
                        self.caught.pop();
                        return result;
                    }
                }
            }
            StmtKind::Break => return Ok(Flow::Break),
            StmtKind::Continue => return Ok(Flow::Continue),
            StmtKind::Return(value) => {
                let value = match value {
                    Some(e) => self.eval_plain(e)?.convert(&SqlType::Int)?.as_i64()?,
                    None => None,
                };
                return Ok(Flow::Return(Some(value.unwrap_or(0))));
            }
            StmtKind::CreateDatabase { name } => self.create_database(name)?,
            StmtKind::DropDatabase { name, if_exists } => self.drop_database(name, *if_exists)?,
            StmtKind::Use(name) => {
                let Some(database) = self.state.database(name) else {
                    return Err(SqlError::new(911, 16, format!("Database '{name}' does not exist. Make sure that the name is entered correctly.")).batch_abort());
                };
                let new = database.name.clone();
                let old = std::mem::replace(&mut self.session.database, new.clone());
                self.frame_mut().database = new.clone();
                self.out.push(Event::Database { new: new.clone(), old });
                self.out.push(Event::Info { number: 5701, class: 0, state: 1, message: format!("Changed database context to '{new}'."), line: statement.line, procedure: String::new() });
            }
            StmtKind::CreateSchema { name } => self.create_schema(name)?,
            StmtKind::DropSchema { name, if_exists } => self.drop_schema(name, *if_exists)?,
            StmtKind::CreateTable(create) => self.create_table(create)?,
            StmtKind::DropTable { names, if_exists } => {
                for name in names {
                    self.drop_table(name, *if_exists)?;
                }
            }
            StmtKind::Truncate(name) => self.truncate(name)?,
            StmtKind::AlterTable { table, action } => self.alter_table(table, action)?,
            StmtKind::CreateIndex { name, table, columns, unique } => self.create_index(name, table, columns, *unique)?,
            StmtKind::DropIndex { name, table, if_exists } => self.drop_index(name, table.as_ref(), *if_exists)?,
            StmtKind::CreateProcedure(procedure) => self.create_procedure(procedure)?,
            StmtKind::DropProcedure { names, if_exists } => {
                for name in names {
                    self.drop_procedure(name, *if_exists)?;
                }
            }
            StmtKind::Exec(exec) => {
                self.exec_statement(exec)?;
            }
            StmtKind::ExecDynamic(parts) => {
                let mut text = String::new();
                for part in parts {
                    if let Some(s) = self.eval_plain(part)?.as_string() {
                        text.push_str(&s);
                    }
                }
                let statements = parse_batch(&text)?;
                if self.frames.len() >= MAX_NESTING {
                    return Err(nesting_error());
                }
                let database = self.current_db();
                self.frames.push(Frame::new(&database, None));
                let result = self.block(&statements);
                self.frames.pop();
                result?;
            }
            StmtKind::BeginTran(_) => self.begin_transaction(),
            StmtKind::Commit => self.commit()?,
            StmtKind::Rollback(name) => {
                let name = match name {
                    Some(n) if n.starts_with('@') => self.eval_plain(&Expr::Var(n.clone()))?.as_string(),
                    other => other.clone(),
                };
                self.rollback(name.as_deref())?;
            }
            StmtKind::SaveTran(name) => {
                let name = if name.starts_with('@') { self.eval_plain(&Expr::Var(name.clone()))?.as_string().unwrap_or_default() } else { name.clone() };
                self.save_transaction(&name)?;
            }
            StmtKind::Print(expr) => {
                let message = self.eval_plain(expr)?.as_string().unwrap_or_default();
                self.out.push(Event::Info { number: 0, class: 0, state: 1, message, line: statement.line, procedure: self.frame().procedure.clone().unwrap_or_default() });
            }
            StmtKind::Throw(args) => {
                let mut error = match args {
                    None => match self.caught.last() {
                        Some(error) => error.clone(),
                        None => return Err(SqlError::new(10704, 15, "To rethrow an error, a THROW statement must be used inside a CATCH block. Insert the THROW statement inside a CATCH block, or add error parameters to the THROW statement.").batch_abort()),
                    },
                    Some((number, message, state)) => {
                        let number = self.eval_plain(number)?.as_i64()?.unwrap_or(50000);
                        if number < 50000 {
                            return Err(SqlError::new(35100, 16, format!("Error number {number} in the THROW statement is outside the valid range. Specify an error number in the valid range of 50000 to 2147483647.")).batch_abort());
                        }
                        let message = self.eval_plain(message)?.as_string().unwrap_or_default();
                        let state = self.eval_plain(state)?.as_i64()?.unwrap_or(1).clamp(0, 255) as u8;
                        SqlError::new(number as i32, 16, message).state(state)
                    }
                };
                error.abort_batch = true;
                error.line = statement.line;
                error.procedure = self.frame().procedure.clone().unwrap_or_default();
                return Err(error);
            }
            StmtKind::Raiserror { message, severity, state, args } => {
                let message_value = self.eval_plain(message)?;
                let severity = self.eval_plain(severity)?.as_i64()?.unwrap_or(16).clamp(0, 25) as u8;
                let state = self.eval_plain(state)?.as_i64()?.unwrap_or(1).clamp(0, 255) as u8;
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval_plain(arg)?);
                }
                let (number, text) = match message_value {
                    Value::Int(n) => (n as i32, format!("Error {n} raised.")),
                    other => (50000, format_raiserror(&other.as_string().unwrap_or_default(), &values)),
                };
                if severity <= 10 {
                    self.out.push(Event::Info { number, class: severity, state, message: text, line: statement.line, procedure: self.frame().procedure.clone().unwrap_or_default() });
                } else {
                    let mut error = SqlError::new(number, severity, text).state(state);
                    error.line = statement.line;
                    return Err(error);
                }
            }
        }
        Ok(Flow::Next)
    }

    fn assign(&mut self, name: &str, op: Option<crate::value::ArithOp>, value: Value) -> Result<(), SqlError> {
        let key = name.to_lowercase();
        let current = match self.frame().vars.get(&key) {
            Some(Variable::Scalar { ty, value }) => (ty.clone(), value.clone()),
            Some(Variable::Table(_)) => return Err(SqlError::new(137, 15, format!("Must declare the scalar variable \"{name}\".")).batch_abort()),
            None => return Err(SqlError::new(137, 15, format!("Must declare the scalar variable \"{name}\".")).batch_abort()),
        };
        let value = match op {
            Some(op) => crate::value::arithmetic(op, &current.1, &value)?,
            None => value,
        };
        let value = value.convert(&current.0)?;
        self.frame_mut().vars.insert(key, Variable::Scalar { ty: current.0, value });
        Ok(())
    }

    fn select_assign(&mut self, assignments: &[(String, Option<crate::value::ArithOp>, Expr)], query: &Query) -> Result<(), SqlError> {
        let QueryBody::Select(select) = &query.body else {
            return Err(SqlError::new(141, 15, "A SELECT statement that assigns a value to a variable must not be combined with data-retrieval operations."));
        };
        // Evaluate the assignments as select items, then assign from each row in turn.
        let items: Vec<SelectItem> = assignments.iter().map(|(_, _, e)| SelectItem::Expr { expr: e.clone(), alias: None }).collect();
        let shaped = Query {
            body: QueryBody::Select(Box::new(Select { items, ..(**select).clone() })),
            order_by: query.order_by.clone(),
            offset: query.offset.clone(),
            fetch: query.fetch.clone(),
            for_json: None,
        };
        let rowset = self.run_query(&shaped, None)?;
        let count = rowset.rows.len();
        for row in rowset.rows {
            for ((name, op, _), value) in assignments.iter().zip(row) {
                self.assign(name, *op, value)?;
            }
        }
        self.session.rowcount = count as i64;
        Ok(())
    }

    fn emit_rowset(&mut self, rowset: query::Rowset, for_json: Option<&ForJson>) {
        if let Some(options) = for_json {
            let text = json::for_json(&rowset, options);
            let columns = vec![ColumnMeta { name: "JSON_F52E2B61-18A1-11d1-B105-00805F49916B".into(), ty: SqlType::NVarChar(None), nullable: true, identity: false }];
            let rows = if rowset.rows.is_empty() && !options.without_array_wrapper && options.root.is_none() { Vec::new() } else { vec![vec![Value::Str(text)]] };
            self.out.push(Event::ResultSet { columns, rows });
            return;
        }
        let columns = rowset
            .layout
            .columns
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| ColumnMeta { name: c.name.clone(), ty: c.ty.clone(), nullable: c.nullable, identity: c.identity })
            .collect();
        let hidden: Vec<bool> = rowset.layout.columns.iter().map(|c| c.hidden).collect();
        let rows = if hidden.iter().any(|h| *h) {
            rowset.rows.into_iter().map(|r| r.into_iter().zip(&hidden).filter(|(_, h)| !**h).map(|(v, _)| v).collect()).collect()
        } else {
            rowset.rows
        };
        self.out.push(Event::ResultSet { columns, rows });
    }

    // ----- procedures -----

    fn exec_statement(&mut self, exec: &Exec) -> Result<(), SqlError> {
        let mut args = Vec::new();
        for arg in &exec.args {
            let (value, ty, default) = match &arg.value {
                ExecValue::Default => (Value::Null, None, true),
                ExecValue::Expr(e) => {
                    let ty = match e {
                        Expr::Var(v) => match self.frame().vars.get(&v.to_lowercase()) {
                            Some(Variable::Scalar { ty, .. }) => Some(ty.clone()),
                            _ => None,
                        },
                        _ => None,
                    };
                    (self.eval_plain(e)?, ty, false)
                }
            };
            if arg.output && !matches!(arg.value, ExecValue::Expr(Expr::Var(_))) {
                return Err(SqlError::new(179, 15, "Cannot use the OUTPUT option when passing a constant to a stored procedure.").batch_abort());
            }
            args.push(Arg { name: arg.name.clone(), value, ty, output: arg.output, default });
        }
        let (status, outputs) = self.invoke(&exec.procedure, args)?;
        for output in outputs {
            if let Some(ExecArg { value: ExecValue::Expr(Expr::Var(var)), .. }) = exec.args.get(output.ordinal) {
                self.assign(var, None, output.value)?;
            }
        }
        if let Some(var) = &exec.return_var {
            self.assign(var, None, Value::Int(status as i64))?;
        }
        if !self.in_proc() {
            self.out.push(Event::ReturnStatus(status));
            self.out.push(Event::DoneProc { count: None, error: false });
        }
        Ok(())
    }

    /// Runs a procedure (stored or system) with arguments.
    fn invoke(&mut self, name: &ObjectName, args: Vec<Arg>) -> Result<(i32, Vec<OutputValue>), SqlError> {
        if self.frames.len() >= MAX_NESTING {
            return Err(nesting_error());
        }
        let lower = name.name.to_lowercase();
        if name.schema.as_deref().is_none_or(|s| s.eq_ignore_ascii_case("sys") || s.eq_ignore_ascii_case("dbo")) {
            if let Some(result) = self.system_procedure(&lower, &args)? {
                return Ok(result);
            }
        }
        let db = name.database.clone().unwrap_or_else(|| self.current_db());
        let schema = name.schema.clone().unwrap_or_else(|| "dbo".into());
        let Some(database) = self.state.database(&db) else {
            return Err(SqlError::new(2812, 16, format!("Could not find stored procedure '{}'.", name.display())));
        };
        let database_name = database.name.clone();
        let Some(stored) = database.procedures.get(&object_key(&schema, &name.name)).cloned() else {
            return Err(SqlError::new(2812, 16, format!("Could not find stored procedure '{}'.", name.display())));
        };
        let statements = parse_batch(&stored.definition)?;
        let Some(Stmt { kind: StmtKind::CreateProcedure(procedure), .. }) = statements.into_iter().next() else {
            return Err(SqlError::new(2812, 16, format!("Could not find stored procedure '{}'.", name.display())));
        };
        let bindings = bind_args(&procedure.params, &args, &stored.name, false)?;
        let mut frame = Frame::new(&database_name, Some(stored.name.clone()));
        for (param, value) in procedure.params.iter().zip(bindings) {
            let value = match value {
                Some(v) => v,
                None => match &param.default {
                    Some(e) => {
                        self.frames.push(Frame::new(&database_name, None));
                        let v = self.eval_plain(e);
                        self.frames.pop();
                        v?
                    }
                    None => {
                        return Err(SqlError::new(
                            201,
                            16,
                            format!("Procedure or function '{}' expects parameter '{}', which was not supplied.", stored.name, param.name),
                        )
                        .state(4))
                    }
                },
            };
            let value = value.convert(&param.ty)?;
            frame.vars.insert(param.name.to_lowercase(), Variable::Scalar { ty: param.ty.clone(), value });
        }
        self.frames.push(frame);
        let saved_nocount = self.session.nocount;
        let result = self.block(&procedure.body);
        self.session.nocount = saved_nocount;
        let frame = self.frames.pop().expect("the pushed frame");
        let status = match result? {
            Flow::Return(Some(v)) => v as i32,
            _ => 0,
        };
        Ok((status, collect_outputs(&procedure.params, &args, &frame, 0)))
    }

    fn system_procedure(&mut self, name: &str, args: &[Arg]) -> Result<Option<(i32, Vec<OutputValue>)>, SqlError> {
        match name {
            "sp_executesql" => {
                let text = args.first().and_then(|a| a.value.as_string()).unwrap_or_default();
                let params = args.get(1).and_then(|a| a.value.as_string()).unwrap_or_default();
                let rest: Vec<Arg> = args.iter().skip(2).cloned().collect();
                let outputs = self.execute_sql_inner(&text, &params, rest, 2)?;
                Ok(Some((0, outputs)))
            }
            "sp_reset_connection" => Ok(Some((0, Vec::new()))),
            "sp_set_session_context" => {
                let key = args.first().and_then(|a| a.value.as_string()).unwrap_or_default();
                let value = args.get(1).map(|a| a.value.clone()).unwrap_or(Value::Null);
                self.session.context.insert(key.to_lowercase(), value);
                Ok(Some((0, Vec::new())))
            }
            "sp_rename" => {
                Err(SqlError::new(40517, 16, "sp_rename is not supported by the Azure emulator's SQL data plane yet."))
            }
            "sp_getapplock" | "sp_releaseapplock" => Ok(Some((0, Vec::new()))),
            "sp_prepare" | "sp_execute" | "sp_prepexec" | "sp_unprepare" | "sp_cursoropen" => {
                Err(SqlError::new(40517, 16, format!("{name} is not supported by the Azure emulator's SQL data plane yet.")))
            }
            _ => Ok(None),
        }
    }

    /// Reports an error found outside a batch (a transaction manager request, a bad RPC).
    pub fn report_error(&mut self, error: SqlError) {
        self.report(error);
    }

    /// `sp_reset_connection`: the session as it was after login (the caller rolls back first).
    pub fn reset_session(&mut self, database: &str) {
        let old = std::mem::replace(&mut self.session.database, database.to_string());
        if !old.eq_ignore_ascii_case(database) {
            self.out.push(Event::Database { new: database.to_string(), old });
        }
        self.session.nocount = false;
        self.session.xact_abort = false;
        self.session.temp.clear();
        self.session.context.clear();
        self.session.identity = Value::Null;
        self.session.rowcount = 0;
        self.session.last_error = 0;
    }
}

fn nesting_error() -> SqlError {
    SqlError::new(217, 16, "Maximum stored procedure, function, trigger, or view nesting level exceeded (limit 32).").batch_abort()
}

/// Matches arguments to parameters: positional first, then by name.
fn bind_args(params: &[ProcParam], args: &[Arg], procedure: &str, executesql: bool) -> Result<Vec<Option<Value>>, SqlError> {
    let mut values: Vec<Option<Value>> = vec![None; params.len()];
    let mut named = false;
    for (i, arg) in args.iter().enumerate() {
        let index = match &arg.name {
            Some(name) if !name.is_empty() => {
                named = true;
                match params.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
                    Some(index) => index,
                    None if executesql => continue,
                    None => return Err(SqlError::new(8145, 16, format!("{name} is not a parameter for procedure {procedure}."))),
                }
            }
            _ => {
                if named {
                    return Err(SqlError::new(119, 15, "Must pass parameter number 2 and subsequent parameters as '@name = value'. After the form '@name = value' has been used, all subsequent parameters must be passed in the form '@name = value'."));
                }
                if i >= params.len() {
                    return Err(SqlError::new(8144, 16, format!("Procedure or function {procedure} has too many arguments specified.")));
                }
                i
            }
        };
        if arg.output && !params[index].output {
            return Err(SqlError::new(
                8162,
                16,
                format!("The formal parameter \"{}\" was not declared as an OUTPUT parameter, but the actual parameter passed in requested output.", params[index].name),
            ));
        }
        if !arg.default {
            values[index] = Some(arg.value.clone());
        }
    }
    Ok(values)
}

/// The output parameters' values, for the arguments passed as `OUTPUT`.
fn collect_outputs(params: &[ProcParam], args: &[Arg], frame: &Frame, ordinal_base: usize) -> Vec<OutputValue> {
    let mut outputs = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if !arg.output {
            continue;
        }
        let param = match &arg.name {
            Some(name) if !name.is_empty() => params.iter().find(|p| p.name.eq_ignore_ascii_case(name)),
            _ => params.get(i),
        };
        let Some(param) = param else { continue };
        if !param.output {
            continue;
        }
        if let Some(Variable::Scalar { ty, value }) = frame.vars.get(&param.name.to_lowercase()) {
            outputs.push(OutputValue { ordinal: i + ordinal_base, name: arg.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| param.name.clone()), value: value.clone(), ty: ty.clone() });
        }
    }
    outputs
}

fn object_from_parts(mut parts: Vec<String>) -> ObjectName {
    let name = parts.pop().unwrap_or_default();
    let schema = parts.pop().filter(|s| !s.is_empty());
    let database = parts.pop().filter(|s| !s.is_empty());
    ObjectName { database, schema, name }
}

/// `RAISERROR`'s printf-style message: `%s`, `%d`, `%i`, `%u` and `%%`.
fn format_raiserror(template: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        // Skip flags, width and precision.
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() || matches!(d, '-' | '+' | ' ' | '#' | '.' | '*') {
                chars.next();
            } else {
                break;
            }
        }
        match chars.next() {
            Some('s' | 'd' | 'i' | 'u' | 'x' | 'X' | 'o') => {
                let value = args.next().map(|v| if v.is_null() { "(null)".to_string() } else { v.to_text() }).unwrap_or_else(|| "(null)".into());
                out.push_str(&value);
            }
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}
