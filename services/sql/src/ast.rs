//! The parsed form of the T-SQL the emulator runs.

use crate::value::{ArithOp, SqlType, Value};

/// A possibly qualified object name: `[db.][schema.]name`.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectName {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
}

impl ObjectName {
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(d) = &self.database {
            parts.push(d.as_str());
        }
        if let Some(s) = &self.schema {
            parts.push(s.as_str());
        }
        parts.push(&self.name);
        parts.join(".")
    }
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Select(Box<Query>),
    /// `SELECT @a = x, @b = y [FROM ...]`.
    SelectAssign { assignments: Vec<(String, Option<ArithOp>, Expr)>, query: Box<Query> },
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
    Declare(Vec<Declaration>),
    SetVar { name: String, op: Option<ArithOp>, value: Expr },
    SetOption { option: String, on: bool },
    SetIdentityInsert { table: ObjectName, on: bool },
    /// SET statements that change nothing the emulator models.
    SetIgnored,
    If { condition: Expr, then: Box<Stmt>, otherwise: Option<Box<Stmt>> },
    While { condition: Expr, body: Box<Stmt> },
    Block(Vec<Stmt>),
    TryCatch { body: Vec<Stmt>, handler: Vec<Stmt> },
    Break,
    Continue,
    Return(Option<Expr>),
    CreateDatabase { name: String },
    DropDatabase { name: String, if_exists: bool },
    AlterDatabase,
    Use(String),
    CreateSchema { name: String },
    DropSchema { name: String, if_exists: bool },
    CreateTable(Box<CreateTable>),
    DropTable { names: Vec<ObjectName>, if_exists: bool },
    Truncate(ObjectName),
    AlterTable { table: ObjectName, action: AlterAction },
    CreateIndex { name: String, table: ObjectName, columns: Vec<String>, unique: bool },
    DropIndex { name: String, table: Option<ObjectName>, if_exists: bool },
    CreateProcedure(Box<Procedure>),
    DropProcedure { names: Vec<ObjectName>, if_exists: bool },
    Exec(Box<Exec>),
    /// `EXEC ('text')`: a batch built at run time.
    ExecDynamic(Vec<Expr>),
    BeginTran(Option<String>),
    Commit,
    Rollback(Option<String>),
    SaveTran(String),
    Print(Expr),
    Throw(Option<(Expr, Expr, Expr)>),
    Raiserror { message: Expr, severity: Expr, state: Expr, args: Vec<Expr> },
    /// `WITH x AS (...)` before a statement.
    With { ctes: Vec<Cte>, body: Box<Stmt> },
}

#[derive(Clone, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: Query,
}

#[derive(Clone, Debug)]
pub struct Declaration {
    pub name: String,
    pub kind: DeclKind,
}

#[derive(Clone, Debug)]
pub enum DeclKind {
    Scalar { ty: SqlType, init: Option<Expr> },
    Table(Vec<ColumnDef>, Vec<TableConstraint>),
}

#[derive(Clone, Debug)]
pub struct Procedure {
    pub name: ObjectName,
    pub or_alter: bool,
    pub alter: bool,
    pub params: Vec<ProcParam>,
    pub body: Vec<Stmt>,
    /// The whole `CREATE PROCEDURE` text, for `OBJECT_DEFINITION`.
    pub definition: String,
}

#[derive(Clone, Debug)]
pub struct ProcParam {
    pub name: String,
    pub ty: SqlType,
    pub default: Option<Expr>,
    pub output: bool,
}

#[derive(Clone, Debug)]
pub struct Exec {
    pub return_var: Option<String>,
    pub procedure: ObjectName,
    pub args: Vec<ExecArg>,
}

#[derive(Clone, Debug)]
pub struct ExecArg {
    pub name: Option<String>,
    pub value: ExecValue,
    pub output: bool,
}

#[derive(Clone, Debug)]
pub enum ExecValue {
    Expr(Expr),
    Default,
}

#[derive(Clone, Debug)]
pub struct CreateTable {
    pub name: ObjectName,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
}

#[derive(Clone, Debug)]
pub struct ColumnDef {
    pub name: String,
    pub ty: SqlType,
    pub nullable: Option<bool>,
    pub identity: Option<(i64, i64)>,
    pub default: Option<(Option<String>, Expr, String)>,
    pub constraints: Vec<TableConstraint>,
}

#[derive(Clone, Debug)]
pub struct TableConstraint {
    pub name: Option<String>,
    pub kind: ConstraintKind,
}

#[derive(Clone, Debug)]
pub enum ConstraintKind {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    ForeignKey { columns: Vec<String>, table: ObjectName, ref_columns: Vec<String>, on_delete_cascade: bool },
    /// The condition and its text.
    Check(Expr, String),
    /// `DEFAULT x FOR column`, with the expression's text.
    Default { column: String, expr: Expr, text: String },
}

#[derive(Clone, Debug)]
pub enum AlterAction {
    AddColumns(Vec<ColumnDef>),
    AddConstraints(Vec<TableConstraint>),
    DropColumns(Vec<String>),
    DropConstraint(String),
    AlterColumn { name: String, ty: SqlType, nullable: Option<bool> },
}

#[derive(Clone, Debug)]
pub struct Insert {
    pub table: ObjectName,
    pub columns: Vec<String>,
    pub output: Option<Vec<SelectItem>>,
    pub source: InsertSource,
}

#[derive(Clone, Debug)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
    Exec(Box<Exec>),
    DefaultValues,
}

#[derive(Clone, Debug)]
pub struct Update {
    pub table: ObjectName,
    pub top: Option<Expr>,
    pub assignments: Vec<(Vec<String>, Option<ArithOp>, Expr)>,
    pub output: Option<Vec<SelectItem>>,
    pub from: Vec<TableRef>,
    pub condition: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct Delete {
    pub table: ObjectName,
    pub top: Option<Expr>,
    pub output: Option<Vec<SelectItem>>,
    pub from: Vec<TableRef>,
    pub condition: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub body: QueryBody,
    pub order_by: Vec<OrderItem>,
    pub offset: Option<Expr>,
    pub fetch: Option<Expr>,
    pub for_json: Option<ForJson>,
}

#[derive(Clone, Debug)]
pub struct ForJson {
    pub path: bool,
    pub root: Option<String>,
    pub include_nulls: bool,
    pub without_array_wrapper: bool,
}

#[derive(Clone, Debug)]
pub enum QueryBody {
    Select(Box<Select>),
    SetOp { op: SetOp, all: bool, left: Box<QueryBody>, right: Box<QueryBody> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SetOp {
    Union,
    Except,
    Intersect,
}

#[derive(Clone, Debug)]
pub struct Select {
    pub distinct: bool,
    pub top: Option<(Expr, bool)>,
    pub items: Vec<SelectItem>,
    pub into: Option<ObjectName>,
    pub from: Vec<TableRef>,
    pub condition: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum SelectItem {
    /// `*` or `alias.*`.
    Wildcard(Option<String>),
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Clone, Debug)]
pub struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Clone, Debug)]
pub enum TableRef {
    Table { name: ObjectName, alias: Option<String> },
    Derived { query: Box<Query>, alias: String, columns: Vec<String> },
    Values { rows: Vec<Vec<Expr>>, alias: String, columns: Vec<String> },
    Function { name: String, args: Vec<Expr>, alias: Option<String>, with: Vec<(String, SqlType, Option<String>, bool)> },
    Join { left: Box<TableRef>, right: Box<TableRef>, kind: JoinKind, on: Option<Expr> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
    CrossApply,
    OuterApply,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Literal(Value),
    /// A string literal, kept apart so `'x'` is `varchar` and `N'x'` `nvarchar`.
    String { value: String, national: bool },
    Var(String),
    /// `@@NAME`.
    Global(String),
    Column(Vec<String>),
    Unary { op: char, expr: Box<Expr> },
    Arith { op: ArithOp, left: Box<Expr>, right: Box<Expr> },
    Compare { op: CmpOp, left: Box<Expr>, right: Box<Expr> },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull { expr: Box<Expr>, negated: bool },
    Like { expr: Box<Expr>, pattern: Box<Expr>, escape: Option<Box<Expr>>, negated: bool },
    InList { expr: Box<Expr>, list: Vec<Expr>, negated: bool },
    InQuery { expr: Box<Expr>, query: Box<Query>, negated: bool },
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },
    Exists(Box<Query>),
    Subquery(Box<Query>),
    Case { operand: Option<Box<Expr>>, branches: Vec<(Expr, Expr)>, otherwise: Option<Box<Expr>> },
    Cast { expr: Box<Expr>, ty: SqlType, style: Option<Box<Expr>>, try_: bool },
    Function { name: String, args: Vec<Expr>, distinct: bool, star: bool, over: Option<Box<Over>> },
    /// A bare word that names a date part or other keyword argument (`DATEADD(day, ...)`).
    Keyword(String),
    /// `NEXT VALUE FOR` and the like are not supported; `DEFAULT` in VALUES.
    Default,
}

#[derive(Clone, Debug)]
pub struct Over {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderItem>,
}
