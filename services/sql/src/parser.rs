//! A recursive-descent parser for the T-SQL subset the emulator runs.

use crate::ast::*;
use crate::exec::SqlError;
use crate::lexer::{tokenize, Tok, Token};
use crate::value::{ArithOp, SqlType, Value};

/// Words that end an expression or start a clause, so never an implicit alias.
const RESERVED: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "APPLY", "AS", "ASC", "BEGIN", "BETWEEN", "BREAK", "BY", "CASE", "CATCH", "CHECK", "CLOSE",
    "COLLATE", "COMMIT", "CONSTRAINT", "CONTINUE", "CREATE", "CROSS", "DEALLOCATE", "DECLARE", "DEFAULT", "DELETE", "DENY", "DESC",
    "DISTINCT", "DROP", "ELSE", "END", "ESCAPE", "EXCEPT", "EXEC", "EXECUTE", "EXISTS", "FETCH", "FOR", "FOREIGN", "FROM", "FULL",
    "GO", "GOTO", "GRANT", "GROUP", "HAVING", "IF", "IN", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE",
    "MERGE", "NOT", "NULL", "OFFSET", "ON", "OPEN", "OPTION", "OR", "ORDER", "OUTER", "OUTPUT", "PIVOT", "PRIMARY", "PRINT",
    "RAISERROR", "REFERENCES", "RETURN", "REVOKE", "RIGHT", "ROLLBACK", "SAVE", "SELECT", "SET", "THEN", "THROW", "TOP", "TRUNCATE",
    "UNION", "UNIQUE", "UNPIVOT", "UPDATE", "USE", "VALUES", "WAITFOR", "WHEN", "WHERE", "WHILE", "WITH",
];

/// Words that start a statement, ending the one before it.
const STATEMENT_START: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "DECLARE", "SET", "IF", "ELSE", "WHILE", "BEGIN", "END", "RETURN", "CREATE", "ALTER", "DROP",
    "USE", "EXEC", "EXECUTE", "PRINT", "THROW", "RAISERROR", "COMMIT", "ROLLBACK", "SAVE", "TRUNCATE", "WITH", "BREAK", "CONTINUE",
    "MERGE", "GO", "GOTO", "WAITFOR", "OPEN", "CLOSE", "FETCH", "DEALLOCATE", "GRANT", "DENY", "REVOKE",
];

pub struct Parser<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, SqlError>;

/// Parses one batch.
pub fn parse_batch(text: &str) -> PResult<Vec<Stmt>> {
    let mut parser = Parser { text, tokens: tokenize(text)?, pos: 0 };
    parser.batch()
}

/// Parses the parameter list of `sp_executesql` (`@a int, @b nvarchar(10) OUTPUT`).
pub fn parse_param_list(text: &str) -> PResult<Vec<ProcParam>> {
    let mut parser = Parser { text, tokens: tokenize(text)?, pos: 0 };
    let params = parser.proc_params()?;
    parser.expect_eof()?;
    Ok(params)
}

/// Parses an expression on its own, such as a stored default.
pub fn parse_expression(text: &str) -> PResult<Expr> {
    let mut parser = Parser { text, tokens: tokenize(text)?, pos: 0 };
    let expr = parser.expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

fn unsupported(what: &str, line: u32) -> SqlError {
    let mut error = SqlError::new(
        40517,
        16,
        format!("{what} is not supported by the Azure emulator's SQL data plane yet."),
    );
    error.line = line;
    error
}

impl Parser<'_> {
    // ----- token helpers -----

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)].tok
    }

    fn line(&self) -> u32 {
        self.tokens[self.pos].line
    }

    fn advance(&mut self) -> Tok {
        let tok = self.tokens[self.pos].tok.clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    fn error_here(&self) -> SqlError {
        let token = &self.tokens[self.pos];
        let near = match &token.tok {
            Tok::Eof => {
                let mut error = SqlError::new(102, 15, "Incorrect syntax near the end of the batch.".to_string());
                error.number = 102;
                error.line = token.line;
                return error;
            }
            _ => &self.text[token.start..token.end],
        };
        let mut error = SqlError::new(102, 15, format!("Incorrect syntax near '{near}'."));
        error.line = token.line;
        error
    }

    fn is_kw(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(word))
    }

    fn is_kw_at(&self, n: usize, word: &str) -> bool {
        matches!(self.peek_at(n), Tok::Word(w) if w.eq_ignore_ascii_case(word))
    }

    fn eat_kw(&mut self, word: &str) -> bool {
        if self.is_kw(word) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, word: &str) -> PResult<()> {
        if self.eat_kw(word) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    fn is_sym(&self, sym: &str) -> bool {
        matches!(self.peek(), Tok::Sym(s) if *s == sym)
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if self.is_sym(sym) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, sym: &str) -> PResult<()> {
        if self.eat_sym(sym) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    fn expect_eof(&mut self) -> PResult<()> {
        while self.eat_sym(";") {}
        if matches!(self.peek(), Tok::Eof) {
            Ok(())
        } else {
            Err(self.error_here())
        }
    }

    fn at_statement_end(&self) -> bool {
        match self.peek() {
            Tok::Eof => true,
            Tok::Sym(";") => true,
            Tok::Word(w) => STATEMENT_START.iter().any(|k| w.eq_ignore_ascii_case(k)),
            _ => false,
        }
    }

    fn is_reserved(word: &str) -> bool {
        RESERVED.iter().any(|k| word.eq_ignore_ascii_case(k))
    }

    /// An identifier: a non-reserved word or a delimited name.
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Word(w) if !Self::is_reserved(&w) => {
                self.advance();
                Ok(w)
            }
            Tok::Quoted(q) => {
                self.advance();
                Ok(q)
            }
            _ => Err(self.error_here()),
        }
    }

    /// An identifier, reserved words allowed (after a dot, or as a column name in a list).
    fn any_ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Word(w) => {
                self.advance();
                Ok(w)
            }
            Tok::Quoted(q) => {
                self.advance();
                Ok(q)
            }
            _ => Err(self.error_here()),
        }
    }

    fn object_name(&mut self) -> PResult<ObjectName> {
        let mut parts = vec![self.any_ident()?];
        while self.is_sym(".") {
            self.advance();
            if self.is_sym(".") {
                // db..table
                parts.push(String::new());
                continue;
            }
            parts.push(self.any_ident()?);
        }
        let name = parts.pop().unwrap_or_default();
        let schema = parts.pop().filter(|s| !s.is_empty());
        let database = parts.pop().filter(|s| !s.is_empty());
        if !parts.is_empty() {
            // A four-part (linked server) name.
            return Err(unsupported("A linked server name", self.line()));
        }
        Ok(ObjectName { database, schema, name })
    }

    fn var(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Var(v) if !v.starts_with("@@") => {
                self.advance();
                Ok(v)
            }
            _ => Err(self.error_here()),
        }
    }

    fn integer(&mut self) -> PResult<i64> {
        let negative = self.eat_sym("-");
        match self.advance() {
            Tok::Int(s) => s.parse::<i64>().map(|n| if negative { -n } else { n }).map_err(|_| self.error_here()),
            _ => Err(self.error_here()),
        }
    }

    // ----- batches and statements -----

    fn batch(&mut self) -> PResult<Vec<Stmt>> {
        while self.eat_sym(";") {}
        let creates_procedure = (self.is_kw("CREATE") && (self.is_kw_at(1, "PROC") || self.is_kw_at(1, "PROCEDURE")))
            || (self.is_kw("CREATE") && self.is_kw_at(1, "OR") && self.is_kw_at(2, "ALTER"))
            || (self.is_kw("ALTER") && (self.is_kw_at(1, "PROC") || self.is_kw_at(1, "PROCEDURE")));
        if creates_procedure {
            let line = self.line();
            let procedure = self.procedure()?;
            return Ok(vec![Stmt { kind: StmtKind::CreateProcedure(Box::new(procedure)), line }]);
        }
        let mut statements = Vec::new();
        loop {
            while self.eat_sym(";") {}
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            statements.push(self.statement()?);
        }
        Ok(statements)
    }

    fn statement(&mut self) -> PResult<Stmt> {
        let line = self.line();
        let kind = self.statement_kind()?;
        Ok(Stmt { kind, line })
    }

    fn statement_kind(&mut self) -> PResult<StmtKind> {
        let word = match self.peek().clone() {
            Tok::Word(w) => w.to_ascii_uppercase(),
            Tok::Sym("(") => return self.select_statement(),
            _ => return Err(self.error_here()),
        };
        match word.as_str() {
            "SELECT" => self.select_statement(),
            "WITH" => self.with_statement(),
            "INSERT" => Ok(StmtKind::Insert(Box::new(self.insert()?))),
            "UPDATE" => Ok(StmtKind::Update(Box::new(self.update()?))),
            "DELETE" => Ok(StmtKind::Delete(Box::new(self.delete()?))),
            "DECLARE" => self.declare(),
            "SET" => self.set(),
            "IF" => {
                self.advance();
                let condition = self.expr()?;
                let then = Box::new(self.statement()?);
                while self.is_sym(";") && self.is_kw_at(1, "ELSE") {
                    self.advance();
                }
                let otherwise = if self.eat_kw("ELSE") { Some(Box::new(self.statement()?)) } else { None };
                Ok(StmtKind::If { condition, then, otherwise })
            }
            "WHILE" => {
                self.advance();
                let condition = self.expr()?;
                let body = Box::new(self.statement()?);
                Ok(StmtKind::While { condition, body })
            }
            "BEGIN" => self.begin(),
            "BREAK" => {
                self.advance();
                Ok(StmtKind::Break)
            }
            "CONTINUE" => {
                self.advance();
                Ok(StmtKind::Continue)
            }
            "RETURN" => {
                self.advance();
                if self.at_statement_end() {
                    Ok(StmtKind::Return(None))
                } else {
                    Ok(StmtKind::Return(Some(self.expr()?)))
                }
            }
            "CREATE" => self.create(),
            "ALTER" => self.alter(),
            "DROP" => self.drop(),
            "TRUNCATE" => {
                self.advance();
                self.expect_kw("TABLE")?;
                Ok(StmtKind::Truncate(self.object_name()?))
            }
            "USE" => {
                self.advance();
                Ok(StmtKind::Use(self.any_ident()?))
            }
            "EXEC" | "EXECUTE" => self.exec_statement(),
            "PRINT" => {
                self.advance();
                Ok(StmtKind::Print(self.expr()?))
            }
            "THROW" => {
                self.advance();
                if self.at_statement_end() {
                    return Ok(StmtKind::Throw(None));
                }
                let number = self.expr()?;
                self.expect_sym(",")?;
                let message = self.expr()?;
                self.expect_sym(",")?;
                let state = self.expr()?;
                Ok(StmtKind::Throw(Some((number, message, state))))
            }
            "RAISERROR" => {
                self.advance();
                self.expect_sym("(")?;
                let message = self.expr()?;
                self.expect_sym(",")?;
                let severity = self.expr()?;
                self.expect_sym(",")?;
                let state = self.expr()?;
                let mut args = Vec::new();
                while self.eat_sym(",") {
                    args.push(self.expr()?);
                }
                self.expect_sym(")")?;
                if self.eat_kw("WITH") {
                    loop {
                        self.any_ident()?;
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                }
                Ok(StmtKind::Raiserror { message, severity, state, args })
            }
            "COMMIT" => {
                self.advance();
                let _ = self.eat_kw("TRAN") || self.eat_kw("TRANSACTION") || self.eat_kw("WORK");
                self.optional_tran_name();
                Ok(StmtKind::Commit)
            }
            "ROLLBACK" => {
                self.advance();
                let _ = self.eat_kw("TRAN") || self.eat_kw("TRANSACTION") || self.eat_kw("WORK");
                Ok(StmtKind::Rollback(self.optional_tran_name()))
            }
            "SAVE" => {
                self.advance();
                if !(self.eat_kw("TRAN") || self.eat_kw("TRANSACTION")) {
                    return Err(self.error_here());
                }
                Ok(StmtKind::SaveTran(self.optional_tran_name().ok_or_else(|| self.error_here())?))
            }
            "MERGE" => Err(unsupported("MERGE", self.line())),
            "WAITFOR" => Err(unsupported("WAITFOR", self.line())),
            "GRANT" | "DENY" | "REVOKE" => {
                // Permissions are not modelled: every login can do everything.
                while !matches!(self.peek(), Tok::Eof | Tok::Sym(";")) {
                    if let Tok::Word(w) = self.peek() {
                        if STATEMENT_START.iter().any(|k| w.eq_ignore_ascii_case(k)) && !w.eq_ignore_ascii_case(&word) {
                            break;
                        }
                    }
                    self.advance();
                }
                Ok(StmtKind::SetIgnored)
            }
            "DECLARE_CURSOR" | "OPEN" | "CLOSE" | "FETCH" | "DEALLOCATE" => Err(unsupported("Cursors", self.line())),
            _ => Err(self.error_here()),
        }
    }

    fn optional_tran_name(&mut self) -> Option<String> {
        match self.peek().clone() {
            Tok::Word(w) if !STATEMENT_START.iter().any(|k| w.eq_ignore_ascii_case(k)) && !Self::is_reserved(&w) => {
                self.advance();
                Some(w)
            }
            Tok::Quoted(q) => {
                self.advance();
                Some(q)
            }
            Tok::Var(v) => {
                self.advance();
                Some(v)
            }
            _ => None,
        }
    }

    fn begin(&mut self) -> PResult<StmtKind> {
        self.advance();
        if self.eat_kw("TRAN") || self.eat_kw("TRANSACTION") {
            let name = self.optional_tran_name();
            if self.eat_kw("WITH") {
                self.expect_kw("MARK")?;
                if matches!(self.peek(), Tok::Str(_) | Tok::NStr(_)) {
                    self.advance();
                }
            }
            return Ok(StmtKind::BeginTran(name));
        }
        if self.eat_kw("DISTRIBUTED") {
            let _ = self.eat_kw("TRAN") || self.eat_kw("TRANSACTION");
            return Ok(StmtKind::BeginTran(self.optional_tran_name()));
        }
        if self.eat_kw("TRY") {
            let body = self.statements_until(&["END"])?;
            self.expect_kw("END")?;
            self.expect_kw("TRY")?;
            while self.eat_sym(";") {}
            self.expect_kw("BEGIN")?;
            self.expect_kw("CATCH")?;
            let handler = self.statements_until(&["END"])?;
            self.expect_kw("END")?;
            self.expect_kw("CATCH")?;
            return Ok(StmtKind::TryCatch { body, handler });
        }
        let body = self.statements_until(&["END"])?;
        self.expect_kw("END")?;
        Ok(StmtKind::Block(body))
    }

    fn statements_until(&mut self, ends: &[&str]) -> PResult<Vec<Stmt>> {
        let mut statements = Vec::new();
        loop {
            while self.eat_sym(";") {}
            if ends.iter().any(|e| self.is_kw(e)) {
                return Ok(statements);
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(self.error_here());
            }
            statements.push(self.statement()?);
        }
    }

    fn select_statement(&mut self) -> PResult<StmtKind> {
        // `SELECT @a = ...` assigns instead of returning rows.
        let mut n = 1;
        if self.is_kw_at(1, "TOP") {
            n = if matches!(self.peek_at(2), Tok::Sym("(")) { 6 } else { 3 };
        }
        if self.is_kw("SELECT") && matches!(self.peek_at(n), Tok::Var(v) if !v.starts_with("@@")) {
            let assigns = matches!(self.peek_at(n + 1), Tok::Sym(s) if s.ends_with('=') && *s != "<=" && *s != ">=" && *s != "!=");
            if assigns {
                return self.select_assign();
            }
        }
        let query = self.query()?;
        if let QueryBody::Select(select) = &query.body {
            if select.into.is_some() {
                return Err(unsupported("SELECT ... INTO", self.line()));
            }
        }
        Ok(StmtKind::Select(Box::new(query)))
    }

    fn select_assign(&mut self) -> PResult<StmtKind> {
        self.expect_kw("SELECT")?;
        let top = self.top()?;
        let mut assignments = Vec::new();
        loop {
            let name = self.var()?;
            let op = self.assign_op()?;
            let value = self.expr()?;
            assignments.push((name, op, value));
            if !self.eat_sym(",") {
                break;
            }
        }
        let mut select = Select { distinct: false, top, items: Vec::new(), into: None, from: Vec::new(), condition: None, group_by: Vec::new(), having: None };
        self.select_tail(&mut select)?;
        let mut query = Query { body: QueryBody::Select(Box::new(select)), order_by: Vec::new(), offset: None, fetch: None, for_json: None };
        self.query_tail(&mut query)?;
        Ok(StmtKind::SelectAssign { assignments, query: Box::new(query) })
    }

    fn assign_op(&mut self) -> PResult<Option<ArithOp>> {
        let op = match self.advance() {
            Tok::Sym("=") => None,
            Tok::Sym("+=") => Some(ArithOp::Add),
            Tok::Sym("-=") => Some(ArithOp::Sub),
            Tok::Sym("*=") => Some(ArithOp::Mul),
            Tok::Sym("/=") => Some(ArithOp::Div),
            Tok::Sym("%=") => Some(ArithOp::Mod),
            Tok::Sym("&=") => Some(ArithOp::BitAnd),
            Tok::Sym("|=") => Some(ArithOp::BitOr),
            Tok::Sym("^=") => Some(ArithOp::BitXor),
            _ => {
                self.pos -= 1;
                return Err(self.error_here());
            }
        };
        Ok(op)
    }

    fn with_statement(&mut self) -> PResult<StmtKind> {
        self.expect_kw("WITH")?;
        let mut ctes = Vec::new();
        loop {
            let name = self.ident()?;
            let mut columns = Vec::new();
            if self.eat_sym("(") {
                loop {
                    columns.push(self.any_ident()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            self.expect_kw("AS")?;
            self.expect_sym("(")?;
            let query = self.query()?;
            self.expect_sym(")")?;
            ctes.push(Cte { name, columns, query });
            if !self.eat_sym(",") {
                break;
            }
        }
        let body = self.statement()?;
        if !matches!(body.kind, StmtKind::Select(_) | StmtKind::Insert(_) | StmtKind::Update(_) | StmtKind::Delete(_) | StmtKind::SelectAssign { .. }) {
            return Err(self.error_here());
        }
        Ok(StmtKind::With { ctes, body: Box::new(body) })
    }

    fn declare(&mut self) -> PResult<StmtKind> {
        self.expect_kw("DECLARE")?;
        let mut declarations = Vec::new();
        loop {
            let name = match self.peek().clone() {
                Tok::Var(v) => {
                    self.advance();
                    v
                }
                Tok::Word(_) if self.is_kw_at(1, "CURSOR") => return Err(unsupported("Cursors", self.line())),
                _ => return Err(self.error_here()),
            };
            self.eat_kw("AS");
            if self.is_kw("CURSOR") {
                return Err(unsupported("Cursors", self.line()));
            }
            if self.eat_kw("TABLE") {
                self.expect_sym("(")?;
                let (columns, constraints) = self.table_elements()?;
                declarations.push(Declaration { name, kind: DeclKind::Table(columns, constraints) });
            } else {
                let ty = self.data_type()?;
                let init = if self.eat_sym("=") { Some(self.expr()?) } else { None };
                declarations.push(Declaration { name, kind: DeclKind::Scalar { ty, init } });
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(StmtKind::Declare(declarations))
    }

    fn set(&mut self) -> PResult<StmtKind> {
        self.expect_kw("SET")?;
        if let Tok::Var(v) = self.peek().clone() {
            if !v.starts_with("@@") {
                self.advance();
                let op = self.assign_op()?;
                let value = self.expr()?;
                return Ok(StmtKind::SetVar { name: v, op, value });
            }
        }
        if self.eat_kw("TRANSACTION") {
            self.expect_kw("ISOLATION")?;
            self.expect_kw("LEVEL")?;
            for _ in 0..2 {
                if matches!(self.peek(), Tok::Word(w) if ["READ", "COMMITTED", "UNCOMMITTED", "REPEATABLE", "SERIALIZABLE", "SNAPSHOT"].iter().any(|k| w.eq_ignore_ascii_case(k)))
                {
                    self.advance();
                }
            }
            return Ok(StmtKind::SetIgnored);
        }
        if self.eat_kw("IDENTITY_INSERT") {
            let table = self.object_name()?;
            let on = self.on_off()?;
            return Ok(StmtKind::SetIdentityInsert { table, on });
        }
        let mut options = vec![self.any_ident()?.to_ascii_uppercase()];
        while self.eat_sym(",") {
            options.push(self.any_ident()?.to_ascii_uppercase());
        }
        // The value: ON, OFF, a number, a word or a string.
        let value = self.advance();
        let on = match &value {
            Tok::Word(w) if w.eq_ignore_ascii_case("ON") => Some(true),
            Tok::Word(w) if w.eq_ignore_ascii_case("OFF") => Some(false),
            Tok::Word(_) | Tok::Int(_) | Tok::Str(_) | Tok::NStr(_) | Tok::Var(_) | Tok::Quoted(_) => None,
            Tok::Sym("-") => {
                self.advance();
                None
            }
            _ => {
                self.pos -= 1;
                return Err(self.error_here());
            }
        };
        match (options.as_slice(), on) {
            ([single], Some(on)) if single == "NOCOUNT" || single == "XACT_ABORT" => Ok(StmtKind::SetOption { option: single.clone(), on }),
            _ => Ok(StmtKind::SetIgnored),
        }
    }

    fn on_off(&mut self) -> PResult<bool> {
        if self.eat_kw("ON") {
            Ok(true)
        } else if self.eat_kw("OFF") {
            Ok(false)
        } else {
            Err(self.error_here())
        }
    }

    // ----- DDL -----

    fn create(&mut self) -> PResult<StmtKind> {
        let line = self.line();
        self.expect_kw("CREATE")?;
        if self.eat_kw("DATABASE") {
            let name = self.any_ident()?;
            self.skip_database_options();
            return Ok(StmtKind::CreateDatabase { name });
        }
        if self.eat_kw("TABLE") {
            let name = self.object_name()?;
            self.expect_sym("(")?;
            let (columns, constraints) = self.table_elements()?;
            self.skip_storage_options()?;
            return Ok(StmtKind::CreateTable(Box::new(CreateTable { name, columns, constraints })));
        }
        if self.eat_kw("SCHEMA") {
            let name = self.any_ident()?;
            if self.eat_kw("AUTHORIZATION") {
                self.any_ident()?;
            }
            return Ok(StmtKind::CreateSchema { name });
        }
        let unique = self.eat_kw("UNIQUE");
        let _ = self.eat_kw("CLUSTERED") || self.eat_kw("NONCLUSTERED");
        if self.eat_kw("INDEX") {
            let name = self.any_ident()?;
            self.expect_kw("ON")?;
            let table = self.object_name()?;
            let columns = self.column_list_with_order()?;
            if self.eat_kw("INCLUDE") {
                self.paren_ident_list()?;
            }
            if self.eat_kw("WHERE") {
                self.expr()?;
            }
            self.skip_storage_options()?;
            return Ok(StmtKind::CreateIndex { name, table, columns, unique });
        }
        if self.is_kw("PROC") || self.is_kw("PROCEDURE") || self.is_kw("OR") {
            let mut error = SqlError::new(111, 15, "'CREATE/ALTER PROCEDURE' must be the first statement in a query batch.");
            error.line = line;
            return Err(error);
        }
        if self.is_kw("VIEW") || self.is_kw("FUNCTION") || self.is_kw("TRIGGER") || self.is_kw("TYPE") || self.is_kw("SEQUENCE") || self.is_kw("SYNONYM") {
            let what = format!("CREATE {}", self.any_ident()?.to_ascii_uppercase());
            return Err(unsupported(&what, line));
        }
        if self.is_kw("LOGIN") || self.is_kw("USER") || self.is_kw("ROLE") {
            let what = format!("CREATE {}", self.any_ident()?.to_ascii_uppercase());
            return Err(unsupported(&what, line));
        }
        Err(self.error_here())
    }

    fn skip_database_options(&mut self) {
        // COLLATE name, CONTAINMENT = NONE, ON (...) and the like: accepted, not modelled.
        while !self.at_statement_end() {
            self.advance();
        }
    }

    /// `ON [PRIMARY]`, `WITH (...)`, `TEXTIMAGE_ON ...` after a table or index.
    fn skip_storage_options(&mut self) -> PResult<()> {
        loop {
            if self.eat_kw("ON") || self.eat_kw("TEXTIMAGE_ON") {
                self.any_ident()?;
                if self.eat_sym("(") {
                    self.any_ident()?;
                    self.expect_sym(")")?;
                }
            } else if self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
                self.advance();
                self.skip_parens()?;
            } else {
                return Ok(());
            }
        }
    }

    fn skip_parens(&mut self) -> PResult<()> {
        self.expect_sym("(")?;
        let mut depth = 1;
        while depth > 0 {
            match self.advance() {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => depth -= 1,
                Tok::Eof => return Err(self.error_here()),
                _ => {}
            }
        }
        Ok(())
    }

    fn paren_ident_list(&mut self) -> PResult<Vec<String>> {
        self.expect_sym("(")?;
        let mut names = Vec::new();
        loop {
            names.push(self.any_ident()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(names)
    }

    fn column_list_with_order(&mut self) -> PResult<Vec<String>> {
        self.expect_sym("(")?;
        let mut names = Vec::new();
        loop {
            names.push(self.any_ident()?);
            let _ = self.eat_kw("ASC") || self.eat_kw("DESC");
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(names)
    }

    /// The elements of `CREATE TABLE (...)` after the opening parenthesis.
    fn table_elements(&mut self) -> PResult<(Vec<ColumnDef>, Vec<TableConstraint>)> {
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.is_kw("CONSTRAINT") || self.is_kw("PRIMARY") || self.is_kw("UNIQUE") || self.is_kw("FOREIGN") || self.is_kw("CHECK") {
                constraints.push(self.table_constraint()?);
            } else if self.is_kw("INDEX") {
                // An inline index: accepted, not modelled.
                self.advance();
                self.any_ident()?;
                let _ = self.eat_kw("CLUSTERED") || self.eat_kw("NONCLUSTERED");
                self.column_list_with_order()?;
            } else {
                columns.push(self.column_def()?);
            }
            if !self.eat_sym(",") {
                break;
            }
            if self.is_sym(")") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok((columns, constraints))
    }

    fn column_def(&mut self) -> PResult<ColumnDef> {
        let line = self.line();
        let name = self.any_ident()?;
        if self.is_kw("AS") {
            return Err(unsupported("A computed column", line));
        }
        let ty = self.data_type()?;
        let mut column = ColumnDef { name, ty, nullable: None, identity: None, default: None, constraints: Vec::new() };
        loop {
            if self.eat_kw("COLLATE") {
                self.any_ident()?;
            } else if self.eat_kw("NULL") {
                column.nullable = Some(true);
            } else if self.is_kw("NOT") && self.is_kw_at(1, "NULL") {
                self.advance();
                self.advance();
                column.nullable = Some(false);
            } else if self.eat_kw("IDENTITY") {
                let mut seed = (1, 1);
                if self.eat_sym("(") {
                    seed.0 = self.integer()?;
                    self.expect_sym(",")?;
                    seed.1 = self.integer()?;
                    self.expect_sym(")")?;
                }
                column.identity = Some(seed);
            } else if self.eat_kw("ROWGUIDCOL") || self.eat_kw("SPARSE") || self.eat_kw("FILESTREAM") {
            } else if self.is_kw("CONSTRAINT") || self.is_kw("PRIMARY") || self.is_kw("UNIQUE") || self.is_kw("REFERENCES") || self.is_kw("FOREIGN") || self.is_kw("CHECK") || self.is_kw("DEFAULT") {
                let constraint_name = if self.eat_kw("CONSTRAINT") { Some(self.any_ident()?) } else { None };
                if self.eat_kw("DEFAULT") {
                    let start = self.tokens[self.pos].start;
                    let expr = self.additive()?;
                    let text = self.text[start..self.tokens[self.pos - 1].end].to_string();
                    column.default = Some((constraint_name, expr, text));
                    continue;
                }
                let kind = if self.is_kw("REFERENCES") {
                    self.references(vec![column.name.clone()])?
                } else {
                    self.constraint_body(Some(&column.name))?
                };
                column.constraints.push(TableConstraint { name: constraint_name, kind });
            } else {
                break;
            }
        }
        Ok(column)
    }

    fn table_constraint(&mut self) -> PResult<TableConstraint> {
        let name = if self.eat_kw("CONSTRAINT") { Some(self.any_ident()?) } else { None };
        let kind = self.constraint_body(None)?;
        Ok(TableConstraint { name, kind })
    }

    /// `PRIMARY KEY (...)`, `UNIQUE (...)`, `FOREIGN KEY (...) REFERENCES ...`, `CHECK (...)`,
    /// `DEFAULT x FOR col`. On a column, the column list is that column.
    fn constraint_body(&mut self, column: Option<&str>) -> PResult<ConstraintKind> {
        if self.eat_kw("PRIMARY") {
            self.expect_kw("KEY")?;
            let _ = self.eat_kw("CLUSTERED") || self.eat_kw("NONCLUSTERED");
            let columns = match column {
                Some(c) if !self.is_sym("(") => vec![c.to_string()],
                _ => self.column_list_with_order()?,
            };
            self.skip_storage_options()?;
            return Ok(ConstraintKind::PrimaryKey(columns));
        }
        if self.eat_kw("UNIQUE") {
            let _ = self.eat_kw("CLUSTERED") || self.eat_kw("NONCLUSTERED");
            let columns = match column {
                Some(c) if !self.is_sym("(") => vec![c.to_string()],
                _ => self.column_list_with_order()?,
            };
            self.skip_storage_options()?;
            return Ok(ConstraintKind::Unique(columns));
        }
        if self.eat_kw("FOREIGN") {
            self.expect_kw("KEY")?;
            let columns = match column {
                Some(c) if !self.is_sym("(") => vec![c.to_string()],
                _ => self.paren_ident_list()?,
            };
            return self.references(columns);
        }
        if self.eat_kw("CHECK") {
            let _ = self.is_kw("NOT") && self.is_kw_at(1, "FOR") && {
                self.advance();
                self.advance();
                self.expect_kw("REPLICATION").is_ok()
            };
            self.expect_sym("(")?;
            let start = self.tokens[self.pos].start;
            let expr = self.expr()?;
            let text = self.text[start..self.tokens[self.pos - 1].end].to_string();
            self.expect_sym(")")?;
            return Ok(ConstraintKind::Check(expr, text));
        }
        if self.eat_kw("DEFAULT") {
            let start = self.tokens[self.pos].start;
            let expr = self.additive()?;
            let text = self.text[start..self.tokens[self.pos - 1].end].to_string();
            self.expect_kw("FOR")?;
            let column = self.any_ident()?;
            return Ok(ConstraintKind::Default { column, expr, text });
        }
        Err(self.error_here())
    }

    fn references(&mut self, columns: Vec<String>) -> PResult<ConstraintKind> {
        self.expect_kw("REFERENCES")?;
        let table = self.object_name()?;
        let ref_columns = if self.is_sym("(") { self.paren_ident_list()? } else { Vec::new() };
        let mut on_delete_cascade = false;
        while self.is_kw("ON") && (self.is_kw_at(1, "DELETE") || self.is_kw_at(1, "UPDATE")) {
            self.advance();
            let delete = self.eat_kw("DELETE");
            if !delete {
                self.advance();
            }
            if self.eat_kw("CASCADE") {
                if delete {
                    on_delete_cascade = true;
                } else {
                    return Err(unsupported("ON UPDATE CASCADE", self.line()));
                }
            } else if self.eat_kw("NO") {
                self.expect_kw("ACTION")?;
            } else if self.eat_kw("SET") {
                return Err(unsupported("ON DELETE SET NULL / SET DEFAULT", self.line()));
            } else {
                return Err(self.error_here());
            }
        }
        if self.is_kw("NOT") && self.is_kw_at(1, "FOR") {
            self.advance();
            self.advance();
            self.expect_kw("REPLICATION")?;
        }
        Ok(ConstraintKind::ForeignKey { columns, table, ref_columns, on_delete_cascade })
    }

    fn data_type(&mut self) -> PResult<SqlType> {
        let line = self.line();
        let mut name = self.any_ident()?;
        if self.is_sym(".") {
            // sys.int, dbo.MyType
            self.advance();
            name = self.any_ident()?;
        }
        if name.eq_ignore_ascii_case("double") && self.eat_kw("PRECISION") {
            name = "float".to_string();
        }
        if (name.eq_ignore_ascii_case("national") || name.eq_ignore_ascii_case("char") || name.eq_ignore_ascii_case("character")) && self.eat_kw("VARYING") {
            name = "varchar".to_string();
        }
        let mut args = Vec::new();
        if self.eat_sym("(") {
            loop {
                if self.eat_kw("MAX") {
                    args.push(None);
                } else {
                    let n = self.integer()?;
                    args.push(Some(u32::try_from(n).map_err(|_| self.error_here())?));
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        SqlType::from_name(&name, &args).ok_or_else(|| {
            let mut error = SqlError::new(2715, 16, format!("Column, parameter, or variable #1: Cannot find data type {name}."));
            error.line = line;
            error
        })
    }

    fn alter(&mut self) -> PResult<StmtKind> {
        let line = self.line();
        self.expect_kw("ALTER")?;
        if self.eat_kw("DATABASE") {
            self.skip_database_options();
            return Ok(StmtKind::AlterDatabase);
        }
        if self.is_kw("PROC") || self.is_kw("PROCEDURE") {
            let mut error = SqlError::new(111, 15, "'CREATE/ALTER PROCEDURE' must be the first statement in a query batch.");
            error.line = line;
            return Err(error);
        }
        if !self.eat_kw("TABLE") {
            if let Tok::Word(w) = self.peek().clone() {
                return Err(unsupported(&format!("ALTER {}", w.to_ascii_uppercase()), line));
            }
            return Err(self.error_here());
        }
        let table = self.object_name()?;
        if self.eat_kw("WITH") {
            let _ = self.eat_kw("CHECK") || self.eat_kw("NOCHECK");
        }
        if self.eat_kw("ADD") {
            let mut columns = Vec::new();
            let mut constraints = Vec::new();
            loop {
                if self.is_kw("CONSTRAINT") || self.is_kw("PRIMARY") || self.is_kw("UNIQUE") || self.is_kw("FOREIGN") || self.is_kw("CHECK") || self.is_kw("DEFAULT") {
                    constraints.push(self.table_constraint()?);
                } else {
                    columns.push(self.column_def()?);
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            if !columns.is_empty() && !constraints.is_empty() {
                return Err(unsupported("Adding columns and constraints in one ALTER TABLE", line));
            }
            if columns.is_empty() {
                return Ok(StmtKind::AlterTable { table, action: AlterAction::AddConstraints(constraints) });
            }
            return Ok(StmtKind::AlterTable { table, action: AlterAction::AddColumns(columns) });
        }
        if self.eat_kw("DROP") {
            if self.eat_kw("CONSTRAINT") {
                if self.eat_kw("IF") {
                    self.expect_kw("EXISTS")?;
                }
                return Ok(StmtKind::AlterTable { table, action: AlterAction::DropConstraint(self.any_ident()?) });
            }
            self.expect_kw("COLUMN")?;
            if self.eat_kw("IF") {
                self.expect_kw("EXISTS")?;
            }
            let mut names = vec![self.any_ident()?];
            while self.eat_sym(",") {
                names.push(self.any_ident()?);
            }
            return Ok(StmtKind::AlterTable { table, action: AlterAction::DropColumns(names) });
        }
        if self.eat_kw("ALTER") {
            self.expect_kw("COLUMN")?;
            let name = self.any_ident()?;
            let ty = self.data_type()?;
            let mut nullable = None;
            if self.eat_kw("COLLATE") {
                self.any_ident()?;
            }
            if self.eat_kw("NULL") {
                nullable = Some(true);
            } else if self.is_kw("NOT") {
                self.advance();
                self.expect_kw("NULL")?;
                nullable = Some(false);
            }
            return Ok(StmtKind::AlterTable { table, action: AlterAction::AlterColumn { name, ty, nullable } });
        }
        if self.eat_kw("CHECK") || self.eat_kw("NOCHECK") {
            self.expect_kw("CONSTRAINT")?;
            self.any_ident()?;
            return Ok(StmtKind::SetIgnored);
        }
        Err(self.error_here())
    }

    fn drop(&mut self) -> PResult<StmtKind> {
        let line = self.line();
        self.expect_kw("DROP")?;
        let what = self.any_ident()?.to_ascii_uppercase();
        let if_exists = if self.eat_kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        match what.as_str() {
            "DATABASE" => Ok(StmtKind::DropDatabase { name: self.any_ident()?, if_exists }),
            "SCHEMA" => Ok(StmtKind::DropSchema { name: self.any_ident()?, if_exists }),
            "TABLE" | "PROC" | "PROCEDURE" => {
                let mut names = vec![self.object_name()?];
                while self.eat_sym(",") {
                    names.push(self.object_name()?);
                }
                if what == "TABLE" {
                    Ok(StmtKind::DropTable { names, if_exists })
                } else {
                    Ok(StmtKind::DropProcedure { names, if_exists })
                }
            }
            "INDEX" => {
                let first = self.object_name()?;
                if self.eat_kw("ON") {
                    let table = self.object_name()?;
                    Ok(StmtKind::DropIndex { name: first.name, table: Some(table), if_exists })
                } else {
                    // DROP INDEX table.index
                    let table = first.schema.clone().map(|t| ObjectName { database: None, schema: first.database.clone(), name: t });
                    Ok(StmtKind::DropIndex { name: first.name, table, if_exists })
                }
            }
            other => Err(unsupported(&format!("DROP {other}"), line)),
        }
    }

    fn procedure(&mut self) -> PResult<Procedure> {
        let start = self.tokens[self.pos].start;
        let mut or_alter = false;
        let mut alter = false;
        if self.eat_kw("CREATE") {
            if self.eat_kw("OR") {
                self.expect_kw("ALTER")?;
                or_alter = true;
            }
        } else {
            self.expect_kw("ALTER")?;
            alter = true;
        }
        if !(self.eat_kw("PROC") || self.eat_kw("PROCEDURE")) {
            if let Tok::Word(w) = self.peek().clone() {
                return Err(unsupported(&format!("CREATE OR ALTER {}", w.to_ascii_uppercase()), self.line()));
            }
            return Err(self.error_here());
        }
        let name = self.object_name()?;
        let parenthesized = self.eat_sym("(");
        let params = if parenthesized && self.is_sym(")") { Vec::new() } else { self.proc_params()? };
        if parenthesized {
            self.expect_sym(")")?;
        }
        if self.eat_kw("WITH") {
            loop {
                if self.eat_kw("EXECUTE") || self.eat_kw("EXEC") {
                    self.expect_kw("AS")?;
                    self.advance();
                } else {
                    self.any_ident()?;
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        if self.eat_kw("FOR") {
            self.expect_kw("REPLICATION")?;
        }
        self.expect_kw("AS")?;
        let mut body = Vec::new();
        loop {
            while self.eat_sym(";") {}
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            body.push(self.statement()?);
        }
        let definition = self.text[start..].trim_end().to_string();
        Ok(Procedure { name, or_alter, alter, params, body, definition })
    }

    fn proc_params(&mut self) -> PResult<Vec<ProcParam>> {
        let mut params = Vec::new();
        if !matches!(self.peek(), Tok::Var(_)) {
            return Ok(params);
        }
        loop {
            let name = self.var()?;
            self.eat_kw("AS");
            let ty = self.data_type()?;
            self.eat_kw("VARYING");
            let mut default = None;
            let mut output = false;
            if self.eat_kw("NULL") {
                // `@p int NULL`: nullable, the default anyway.
            }
            if self.eat_sym("=") {
                default = Some(self.unary()?);
            }
            loop {
                if self.eat_kw("OUT") || self.eat_kw("OUTPUT") {
                    output = true;
                } else if self.eat_kw("READONLY") {
                } else {
                    break;
                }
            }
            params.push(ProcParam { name, ty, default, output });
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(params)
    }

    fn exec_statement(&mut self) -> PResult<StmtKind> {
        self.advance();
        if self.eat_sym("(") {
            let mut parts = vec![self.expr()?];
            while self.eat_sym("+") {
                parts.push(self.expr()?);
            }
            self.expect_sym(")")?;
            return Ok(StmtKind::ExecDynamic(parts));
        }
        Ok(StmtKind::Exec(Box::new(self.exec_body()?)))
    }

    fn exec_body(&mut self) -> PResult<Exec> {
        let mut return_var = None;
        if let (Tok::Var(v), Tok::Sym("=")) = (self.peek().clone(), self.peek_at(1).clone()) {
            self.advance();
            self.advance();
            return_var = Some(v);
        }
        let procedure = self.object_name()?;
        let mut args = Vec::new();
        if !self.at_statement_end() {
            loop {
                let mut name = None;
                if let (Tok::Var(v), Tok::Sym("=")) = (self.peek().clone(), self.peek_at(1).clone()) {
                    self.advance();
                    self.advance();
                    name = Some(v);
                }
                let value = if self.eat_kw("DEFAULT") {
                    ExecValue::Default
                } else if let Tok::Word(w) = self.peek().clone() {
                    // A bare word passed to a procedure is a string.
                    if !Self::is_reserved(&w) && !matches!(self.peek_at(1), Tok::Sym("(") | Tok::Sym(".")) {
                        self.advance();
                        ExecValue::Expr(Expr::String { value: w, national: true })
                    } else {
                        ExecValue::Expr(self.expr()?)
                    }
                } else {
                    ExecValue::Expr(self.expr()?)
                };
                let output = self.eat_kw("OUTPUT") || self.eat_kw("OUT");
                args.push(ExecArg { name, value, output });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        if self.is_kw("WITH") && self.is_kw_at(1, "RECOMPILE") {
            self.advance();
            self.advance();
        }
        Ok(Exec { return_var, procedure, args })
    }

    // ----- DML -----

    fn output_clause(&mut self) -> PResult<Option<Vec<SelectItem>>> {
        if !self.eat_kw("OUTPUT") {
            return Ok(None);
        }
        let items = self.select_items()?;
        if self.is_kw("INTO") {
            return Err(unsupported("OUTPUT ... INTO", self.line()));
        }
        Ok(Some(items))
    }

    fn insert(&mut self) -> PResult<Insert> {
        self.expect_kw("INSERT")?;
        if self.is_kw("TOP") {
            return Err(unsupported("INSERT TOP", self.line()));
        }
        self.eat_kw("INTO");
        let table = self.object_name()?;
        if self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
            self.advance();
            self.skip_parens()?;
        }
        let mut columns = Vec::new();
        if self.is_sym("(") && !self.is_kw_at(1, "SELECT") && !self.is_kw_at(1, "WITH") {
            columns = self.paren_ident_list_qualified()?;
        }
        let output = self.output_clause()?;
        let source = if self.eat_kw("VALUES") {
            let mut rows = Vec::new();
            loop {
                self.expect_sym("(")?;
                let mut row = Vec::new();
                loop {
                    if self.eat_kw("DEFAULT") {
                        row.push(Expr::Default);
                    } else {
                        row.push(self.expr()?);
                    }
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                rows.push(row);
                if !self.eat_sym(",") {
                    break;
                }
            }
            InsertSource::Values(rows)
        } else if self.eat_kw("DEFAULT") {
            self.expect_kw("VALUES")?;
            InsertSource::DefaultValues
        } else if self.is_kw("EXEC") || self.is_kw("EXECUTE") {
            self.advance();
            InsertSource::Exec(Box::new(self.exec_body()?))
        } else if self.is_kw("SELECT") || self.is_sym("(") {
            InsertSource::Query(Box::new(self.query()?))
        } else {
            return Err(self.error_here());
        };
        Ok(Insert { table, columns, output, source })
    }

    fn paren_ident_list_qualified(&mut self) -> PResult<Vec<String>> {
        self.expect_sym("(")?;
        let mut names = Vec::new();
        loop {
            let mut name = self.any_ident()?;
            while self.eat_sym(".") {
                name = self.any_ident()?;
            }
            names.push(name);
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(names)
    }

    fn dml_top(&mut self) -> PResult<Option<Expr>> {
        if self.eat_kw("TOP") {
            self.expect_sym("(")?;
            let e = self.expr()?;
            self.expect_sym(")")?;
            if self.eat_kw("PERCENT") {
                return Err(unsupported("TOP PERCENT", self.line()));
            }
            return Ok(Some(e));
        }
        Ok(None)
    }

    fn update(&mut self) -> PResult<Update> {
        self.expect_kw("UPDATE")?;
        let top = self.dml_top()?;
        let table = self.object_name()?;
        if self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
            self.advance();
            self.skip_parens()?;
        }
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            if matches!(self.peek(), Tok::Var(_)) {
                return Err(unsupported("Assigning a variable in UPDATE ... SET", self.line()));
            }
            let mut target = vec![self.any_ident()?];
            while self.eat_sym(".") {
                target.push(self.any_ident()?);
            }
            let op = self.assign_op()?;
            let value = if self.eat_kw("DEFAULT") { Expr::Default } else { self.expr()? };
            assignments.push((target, op, value));
            if !self.eat_sym(",") {
                break;
            }
        }
        let output = self.output_clause()?;
        let from = if self.eat_kw("FROM") { self.table_refs()? } else { Vec::new() };
        let condition = if self.eat_kw("WHERE") { Some(self.where_condition()?) } else { None };
        self.skip_option_clause()?;
        Ok(Update { table, top, assignments, output, from, condition })
    }

    fn delete(&mut self) -> PResult<Delete> {
        self.expect_kw("DELETE")?;
        let top = self.dml_top()?;
        self.eat_kw("FROM");
        let table = self.object_name()?;
        if self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
            self.advance();
            self.skip_parens()?;
        }
        let output = self.output_clause()?;
        let from = if self.eat_kw("FROM") { self.table_refs()? } else { Vec::new() };
        let condition = if self.eat_kw("WHERE") { Some(self.where_condition()?) } else { None };
        self.skip_option_clause()?;
        Ok(Delete { table, top, output, from, condition })
    }

    fn where_condition(&mut self) -> PResult<Expr> {
        if self.is_kw("CURRENT") && self.is_kw_at(1, "OF") {
            return Err(unsupported("Cursors", self.line()));
        }
        self.expr()
    }

    fn skip_option_clause(&mut self) -> PResult<()> {
        if self.is_kw("OPTION") && matches!(self.peek_at(1), Tok::Sym("(")) {
            self.advance();
            self.skip_parens()?;
        }
        Ok(())
    }

    // ----- queries -----

    pub fn query(&mut self) -> PResult<Query> {
        let body = self.query_body()?;
        let mut query = Query { body, order_by: Vec::new(), offset: None, fetch: None, for_json: None };
        self.query_tail(&mut query)?;
        Ok(query)
    }

    fn query_tail(&mut self, query: &mut Query) -> PResult<()> {
        if self.is_kw("ORDER") {
            self.advance();
            self.expect_kw("BY")?;
            query.order_by = self.order_items()?;
            if self.eat_kw("OFFSET") {
                query.offset = Some(self.expr()?);
                let _ = self.eat_kw("ROWS") || self.eat_kw("ROW");
                if self.eat_kw("FETCH") {
                    let _ = self.eat_kw("NEXT") || self.eat_kw("FIRST");
                    query.fetch = Some(self.expr()?);
                    let _ = self.eat_kw("ROWS") || self.eat_kw("ROW");
                    self.expect_kw("ONLY")?;
                }
            }
        }
        if self.is_kw("FOR") {
            self.advance();
            if self.eat_kw("XML") {
                return Err(unsupported("FOR XML", self.line()));
            }
            self.expect_kw("JSON")?;
            let path = if self.eat_kw("PATH") {
                true
            } else if self.eat_kw("AUTO") {
                false
            } else {
                return Err(self.error_here());
            };
            let mut for_json = ForJson { path, root: None, include_nulls: false, without_array_wrapper: false };
            while self.eat_sym(",") {
                if self.eat_kw("ROOT") {
                    let mut root = "root".to_string();
                    if self.eat_sym("(") {
                        if let Tok::Str(s) | Tok::NStr(s) = self.advance() {
                            root = s;
                        }
                        self.expect_sym(")")?;
                    }
                    for_json.root = Some(root);
                } else if self.eat_kw("INCLUDE_NULL_VALUES") {
                    for_json.include_nulls = true;
                } else if self.eat_kw("WITHOUT_ARRAY_WRAPPER") {
                    for_json.without_array_wrapper = true;
                } else {
                    return Err(self.error_here());
                }
            }
            query.for_json = Some(for_json);
        }
        self.skip_option_clause()?;
        Ok(())
    }

    fn order_items(&mut self) -> PResult<Vec<OrderItem>> {
        let mut items = Vec::new();
        loop {
            let expr = self.expr()?;
            if self.eat_kw("COLLATE") {
                self.any_ident()?;
            }
            let descending = if self.eat_kw("DESC") {
                true
            } else {
                self.eat_kw("ASC");
                false
            };
            items.push(OrderItem { expr, descending });
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(items)
    }

    fn query_body(&mut self) -> PResult<QueryBody> {
        let mut left = self.query_primary()?;
        loop {
            let op = if self.is_kw("UNION") {
                SetOp::Union
            } else if self.is_kw("EXCEPT") {
                SetOp::Except
            } else if self.is_kw("INTERSECT") {
                SetOp::Intersect
            } else {
                return Ok(left);
            };
            self.advance();
            let all = self.eat_kw("ALL");
            let right = self.query_primary()?;
            left = QueryBody::SetOp { op, all, left: Box::new(left), right: Box::new(right) };
        }
    }

    fn query_primary(&mut self) -> PResult<QueryBody> {
        if self.eat_sym("(") {
            let query = self.query()?;
            self.expect_sym(")")?;
            if !query.order_by.is_empty() {
                return Err(unsupported("ORDER BY inside a parenthesized query", self.line()));
            }
            return Ok(query.body);
        }
        Ok(QueryBody::Select(Box::new(self.select()?)))
    }

    fn top(&mut self) -> PResult<Option<(Expr, bool)>> {
        if !self.eat_kw("TOP") {
            return Ok(None);
        }
        let expr = if self.eat_sym("(") {
            let e = self.expr()?;
            self.expect_sym(")")?;
            e
        } else {
            match self.advance() {
                Tok::Int(s) => Expr::Literal(Value::Int(s.parse().unwrap_or(0))),
                Tok::Var(v) => Expr::Var(v),
                _ => {
                    self.pos -= 1;
                    return Err(self.error_here());
                }
            }
        };
        let percent = self.eat_kw("PERCENT");
        if self.is_kw("WITH") && self.is_kw_at(1, "TIES") {
            return Err(unsupported("TOP ... WITH TIES", self.line()));
        }
        Ok(Some((expr, percent)))
    }

    fn select(&mut self) -> PResult<Select> {
        self.expect_kw("SELECT")?;
        let distinct = if self.eat_kw("DISTINCT") {
            true
        } else {
            self.eat_kw("ALL");
            false
        };
        let top = self.top()?;
        let items = self.select_items()?;
        let into = if self.eat_kw("INTO") { Some(self.object_name()?) } else { None };
        let mut select = Select { distinct, top, items, into, from: Vec::new(), condition: None, group_by: Vec::new(), having: None };
        self.select_tail(&mut select)?;
        Ok(select)
    }

    fn select_tail(&mut self, select: &mut Select) -> PResult<()> {
        if self.eat_kw("FROM") {
            select.from = self.table_refs()?;
        }
        if self.eat_kw("WHERE") {
            select.condition = Some(self.expr()?);
        }
        if self.is_kw("GROUP") {
            self.advance();
            self.expect_kw("BY")?;
            loop {
                select.group_by.push(self.expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        if self.eat_kw("HAVING") {
            select.having = Some(self.expr()?);
        }
        Ok(())
    }

    fn select_items(&mut self) -> PResult<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(items)
    }

    fn select_item(&mut self) -> PResult<SelectItem> {
        if self.eat_sym("*") {
            return Ok(SelectItem::Wildcard(None));
        }
        // alias.* and db.schema.table.* (only the last qualifier is kept)
        let mut n = 0;
        while matches!(self.peek_at(n), Tok::Word(_) | Tok::Quoted(_)) && matches!(self.peek_at(n + 1), Tok::Sym(".")) {
            n += 2;
        }
        if n > 0 && matches!(self.peek_at(n), Tok::Sym("*")) {
            let mut qualifier = String::new();
            for _ in 0..n / 2 {
                qualifier = self.any_ident()?;
                self.expect_sym(".")?;
            }
            self.expect_sym("*")?;
            return Ok(SelectItem::Wildcard(Some(qualifier)));
        }
        // alias = expr
        if matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_) | Tok::Str(_) | Tok::NStr(_)) && matches!(self.peek_at(1), Tok::Sym("=")) {
            let alias = match self.advance() {
                Tok::Word(w) | Tok::Quoted(w) | Tok::Str(w) | Tok::NStr(w) => w,
                _ => unreachable_tok(),
            };
            self.advance();
            let expr = self.expr()?;
            return Ok(SelectItem::Expr { expr, alias: Some(alias) });
        }
        let expr = self.expr()?;
        let alias = self.alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn alias(&mut self) -> PResult<Option<String>> {
        if self.eat_kw("AS") {
            return match self.advance() {
                Tok::Word(w) | Tok::Quoted(w) | Tok::Str(w) | Tok::NStr(w) => Ok(Some(w)),
                _ => {
                    self.pos -= 1;
                    Err(self.error_here())
                }
            };
        }
        match self.peek().clone() {
            Tok::Word(w) if !Self::is_reserved(&w) && !STATEMENT_START.iter().any(|k| w.eq_ignore_ascii_case(k)) => {
                self.advance();
                Ok(Some(w))
            }
            Tok::Quoted(q) | Tok::Str(q) | Tok::NStr(q) => {
                self.advance();
                Ok(Some(q))
            }
            _ => Ok(None),
        }
    }

    fn table_refs(&mut self) -> PResult<Vec<TableRef>> {
        let mut refs = Vec::new();
        loop {
            refs.push(self.table_ref()?);
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(refs)
    }

    fn table_ref(&mut self) -> PResult<TableRef> {
        let mut left = self.table_primary()?;
        loop {
            let kind = if self.is_kw("JOIN") || (self.is_kw("INNER") && self.is_kw_at(1, "JOIN")) {
                self.eat_kw("INNER");
                JoinKind::Inner
            } else if self.is_kw("INNER") || self.is_kw("LEFT") || self.is_kw("RIGHT") || self.is_kw("FULL") {
                let kind = match self.advance() {
                    Tok::Word(w) if w.eq_ignore_ascii_case("LEFT") => JoinKind::Left,
                    Tok::Word(w) if w.eq_ignore_ascii_case("RIGHT") => JoinKind::Right,
                    Tok::Word(w) if w.eq_ignore_ascii_case("FULL") => JoinKind::Full,
                    _ => JoinKind::Inner,
                };
                self.eat_kw("OUTER");
                let _ = self.eat_kw("HASH") || self.eat_kw("LOOP") || self.eat_kw("MERGE");
                if !self.is_kw("JOIN") {
                    return Err(self.error_here());
                }
                kind
            } else if self.is_kw("CROSS") && self.is_kw_at(1, "JOIN") {
                self.advance();
                JoinKind::Cross
            } else if self.is_kw("CROSS") && self.is_kw_at(1, "APPLY") {
                self.advance();
                self.advance();
                let right = self.table_primary()?;
                left = TableRef::Join { left: Box::new(left), right: Box::new(right), kind: JoinKind::CrossApply, on: None };
                continue;
            } else if self.is_kw("OUTER") && self.is_kw_at(1, "APPLY") {
                self.advance();
                self.advance();
                let right = self.table_primary()?;
                left = TableRef::Join { left: Box::new(left), right: Box::new(right), kind: JoinKind::OuterApply, on: None };
                continue;
            } else {
                return Ok(left);
            };
            self.expect_kw("JOIN")?;
            let right = self.table_ref_for_join()?;
            let on = if kind == JoinKind::Cross {
                None
            } else {
                self.expect_kw("ON")?;
                Some(self.expr()?)
            };
            left = TableRef::Join { left: Box::new(left), right: Box::new(right), kind, on };
        }
    }

    /// The right side of a join: a primary, or a parenthesized join.
    fn table_ref_for_join(&mut self) -> PResult<TableRef> {
        self.table_primary()
    }

    fn table_primary(&mut self) -> PResult<TableRef> {
        let line = self.line();
        if self.is_sym("(") {
            if self.is_kw_at(1, "SELECT") || self.is_kw_at(1, "WITH") || matches!(self.peek_at(1), Tok::Sym("(")) {
                self.advance();
                let query = self.query()?;
                self.expect_sym(")")?;
                let alias = self.alias()?.ok_or_else(|| self.error_here())?;
                let columns = if self.is_sym("(") { self.paren_ident_list()? } else { Vec::new() };
                return Ok(TableRef::Derived { query: Box::new(query), alias, columns });
            }
            if self.is_kw_at(1, "VALUES") {
                self.advance();
                self.advance();
                let mut rows = Vec::new();
                loop {
                    self.expect_sym("(")?;
                    let mut row = vec![self.expr()?];
                    while self.eat_sym(",") {
                        row.push(self.expr()?);
                    }
                    self.expect_sym(")")?;
                    rows.push(row);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                let alias = self.alias()?.ok_or_else(|| self.error_here())?;
                let columns = if self.is_sym("(") { self.paren_ident_list()? } else { Vec::new() };
                return Ok(TableRef::Values { rows, alias, columns });
            }
            self.advance();
            let inner = self.table_ref()?;
            self.expect_sym(")")?;
            return Ok(inner);
        }
        if matches!(self.peek(), Tok::Var(_)) {
            let name = self.var()?;
            let alias = self.alias()?;
            return Ok(TableRef::Table { name: ObjectName { database: None, schema: None, name }, alias });
        }
        let name = self.object_name()?;
        if self.is_sym("(") && name.database.is_none() && name.schema.is_none() {
            // A table-valued function.
            let fname = name.name.to_ascii_uppercase();
            if fname != "OPENJSON" && fname != "STRING_SPLIT" {
                return Err(unsupported(&format!("The table-valued function {}", name.name), line));
            }
            self.advance();
            let mut args = vec![self.expr()?];
            while self.eat_sym(",") {
                args.push(self.expr()?);
            }
            self.expect_sym(")")?;
            let mut with = Vec::new();
            if fname == "OPENJSON" && self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
                self.advance();
                self.advance();
                loop {
                    let column = self.any_ident()?;
                    let ty = self.data_type()?;
                    let mut path = None;
                    if let Tok::Str(p) | Tok::NStr(p) = self.peek().clone() {
                        self.advance();
                        path = Some(p);
                    }
                    let as_json = if self.eat_kw("AS") {
                        self.expect_kw("JSON")?;
                        true
                    } else {
                        false
                    };
                    with.push((column, ty, path, as_json));
                    if !self.eat_sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            let alias = self.alias()?;
            return Ok(TableRef::Function { name: fname, args, alias, with });
        }
        let alias = self.alias()?;
        // Table hints: WITH (NOLOCK) or the old (NOLOCK).
        if self.is_kw("WITH") && matches!(self.peek_at(1), Tok::Sym("(")) {
            self.advance();
            self.skip_parens()?;
        } else if self.is_sym("(") && matches!(self.peek_at(1), Tok::Word(w) if ["NOLOCK", "READUNCOMMITTED", "UPDLOCK", "ROWLOCK", "HOLDLOCK", "READPAST", "TABLOCK", "TABLOCKX", "XLOCK", "PAGLOCK", "SERIALIZABLE", "NOWAIT", "READCOMMITTED", "REPEATABLEREAD"].iter().any(|h| w.eq_ignore_ascii_case(h)))
        {
            self.skip_parens()?;
        }
        Ok(TableRef::Table { name, alias })
    }

    // ----- expressions -----

    pub fn expr(&mut self) -> PResult<Expr> {
        let mut left = self.and()?;
        while self.eat_kw("OR") {
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> PResult<Expr> {
        let mut left = self.not()?;
        while self.eat_kw("AND") {
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> PResult<Expr> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> PResult<Expr> {
        if self.is_kw("EXISTS") {
            self.advance();
            self.expect_sym("(")?;
            let query = self.query()?;
            self.expect_sym(")")?;
            return Ok(Expr::Exists(Box::new(query)));
        }
        let left = self.additive()?;
        let op = match self.peek() {
            Tok::Sym("=") => Some(CmpOp::Eq),
            Tok::Sym("<>") | Tok::Sym("!=") => Some(CmpOp::Ne),
            Tok::Sym("<") => Some(CmpOp::Lt),
            Tok::Sym("<=") | Tok::Sym("!>") => Some(CmpOp::Le),
            Tok::Sym(">") => Some(CmpOp::Gt),
            Tok::Sym(">=") | Tok::Sym("!<") => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            if self.is_kw("ALL") || self.is_kw("ANY") || self.is_kw("SOME") {
                return Err(unsupported("ALL / ANY / SOME comparisons", self.line()));
            }
            let right = self.additive()?;
            return Ok(Expr::Compare { op, left: Box::new(left), right: Box::new(right) });
        }
        if self.is_kw("IS") {
            self.advance();
            let negated = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull { expr: Box::new(left), negated });
        }
        let negated = if self.is_kw("NOT") && (self.is_kw_at(1, "IN") || self.is_kw_at(1, "LIKE") || self.is_kw_at(1, "BETWEEN")) {
            self.advance();
            true
        } else {
            false
        };
        if self.eat_kw("IN") {
            self.expect_sym("(")?;
            if self.is_kw("SELECT") || self.is_kw("WITH") {
                let query = self.query()?;
                self.expect_sym(")")?;
                return Ok(Expr::InQuery { expr: Box::new(left), query: Box::new(query), negated });
            }
            let mut list = vec![self.expr()?];
            while self.eat_sym(",") {
                list.push(self.expr()?);
            }
            self.expect_sym(")")?;
            return Ok(Expr::InList { expr: Box::new(left), list, negated });
        }
        if self.eat_kw("LIKE") {
            let pattern = self.additive()?;
            let escape = if self.eat_kw("ESCAPE") { Some(Box::new(self.additive()?)) } else { None };
            return Ok(Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), escape, negated });
        }
        if self.eat_kw("BETWEEN") {
            let low = self.additive()?;
            self.expect_kw("AND")?;
            let high = self.additive()?;
            return Ok(Expr::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated });
        }
        if negated {
            return Err(self.error_here());
        }
        Ok(left)
    }

    fn additive(&mut self) -> PResult<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Sym("+") => ArithOp::Add,
                Tok::Sym("-") => ArithOp::Sub,
                Tok::Sym("&") => ArithOp::BitAnd,
                Tok::Sym("|") => ArithOp::BitOr,
                Tok::Sym("^") => ArithOp::BitXor,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.multiplicative()?;
            left = Expr::Arith { op, left: Box::new(left), right: Box::new(right) };
        }
    }

    fn multiplicative(&mut self) -> PResult<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Sym("*") => ArithOp::Mul,
                Tok::Sym("/") => ArithOp::Div,
                Tok::Sym("%") => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.unary()?;
            left = Expr::Arith { op, left: Box::new(left), right: Box::new(right) };
        }
    }

    fn unary(&mut self) -> PResult<Expr> {
        if self.eat_sym("-") {
            let inner = self.unary()?;
            return Ok(match inner {
                Expr::Literal(Value::Int(i)) => Expr::Literal(Value::Int(-i)),
                Expr::Literal(Value::Decimal(d)) => Expr::Literal(Value::Decimal(-d)),
                Expr::Literal(Value::Float(f)) => Expr::Literal(Value::Float(-f)),
                other => Expr::Unary { op: '-', expr: Box::new(other) },
            });
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        if self.eat_sym("~") {
            return Ok(Expr::Unary { op: '~', expr: Box::new(self.unary()?) });
        }
        let expr = self.primary()?;
        if self.eat_kw("COLLATE") {
            self.any_ident()?;
        }
        if self.is_kw("AT") && self.is_kw_at(1, "TIME") {
            return Err(unsupported("AT TIME ZONE", self.line()));
        }
        Ok(expr)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Int(s) => {
                self.advance();
                Ok(match s.parse::<i64>() {
                    Ok(i) if i32::try_from(i).is_ok() => Expr::Literal(Value::Int(i)),
                    _ => Expr::Literal(Value::Decimal(s.parse().map_err(|_| self.error_here())?)),
                })
            }
            Tok::Number(s) => {
                self.advance();
                if s.contains(['e', 'E']) {
                    Ok(Expr::Literal(Value::Float(s.parse().map_err(|_| self.error_here())?)))
                } else {
                    Ok(Expr::Literal(Value::Decimal(s.parse().map_err(|_| self.error_here())?)))
                }
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::String { value: s, national: false })
            }
            Tok::NStr(s) => {
                self.advance();
                Ok(Expr::String { value: s, national: true })
            }
            Tok::Binary(b) => {
                self.advance();
                Ok(Expr::Literal(Value::Bytes(b)))
            }
            Tok::Var(v) => {
                self.advance();
                if let Some(global) = v.strip_prefix("@@") {
                    Ok(Expr::Global(global.to_ascii_uppercase()))
                } else {
                    Ok(Expr::Var(v))
                }
            }
            Tok::Sym("(") => {
                self.advance();
                if self.is_kw("SELECT") || self.is_kw("WITH") {
                    let query = self.query()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::Subquery(Box::new(query)));
                }
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Sym("$") => Err(self.error_here()),
            Tok::Word(w) => {
                let upper = w.to_ascii_uppercase();
                match upper.as_str() {
                    "NULL" => {
                        self.advance();
                        Ok(Expr::Literal(Value::Null))
                    }
                    "CASE" => self.case(),
                    "CAST" | "TRY_CAST" if matches!(self.peek_at(1), Tok::Sym("(")) => {
                        self.advance();
                        self.advance();
                        let expr = self.expr()?;
                        self.expect_kw("AS")?;
                        let ty = self.data_type()?;
                        self.expect_sym(")")?;
                        Ok(Expr::Cast { expr: Box::new(expr), ty, style: None, try_: upper == "TRY_CAST" })
                    }
                    "CONVERT" | "TRY_CONVERT" if matches!(self.peek_at(1), Tok::Sym("(")) => {
                        self.advance();
                        self.advance();
                        let ty = self.data_type()?;
                        self.expect_sym(",")?;
                        let expr = self.expr()?;
                        let style = if self.eat_sym(",") { Some(Box::new(self.expr()?)) } else { None };
                        self.expect_sym(")")?;
                        Ok(Expr::Cast { expr: Box::new(expr), ty, style, try_: upper == "TRY_CONVERT" })
                    }
                    "CURRENT_TIMESTAMP" => {
                        self.advance();
                        Ok(Expr::Function { name: "GETDATE".into(), args: Vec::new(), distinct: false, star: false, over: None })
                    }
                    "CURRENT_USER" | "SESSION_USER" | "SYSTEM_USER" | "USER" if !matches!(self.peek_at(1), Tok::Sym("(")) => {
                        self.advance();
                        Ok(Expr::Function { name: "SUSER_SNAME".into(), args: Vec::new(), distinct: false, star: false, over: None })
                    }
                    _ if matches!(self.peek_at(1), Tok::Sym("(")) && !Self::is_reserved(&w) || matches!(upper.as_str(), "LEFT" | "RIGHT") && matches!(self.peek_at(1), Tok::Sym("(")) => {
                        self.function_call()
                    }
                    _ if Self::is_reserved(&w) => Err(self.error_here()),
                    _ => self.column_ref(),
                }
            }
            Tok::Quoted(_) => self.column_ref(),
            _ => {
                let mut e = self.error_here();
                e.line = line;
                Err(e)
            }
        }
    }

    fn column_ref(&mut self) -> PResult<Expr> {
        let mut parts = vec![self.any_ident()?];
        while self.is_sym(".") && !matches!(self.peek_at(1), Tok::Sym("*")) {
            self.advance();
            parts.push(self.any_ident()?);
        }
        if self.is_sym("(") && parts.len() > 1 {
            return Err(unsupported(&format!("The user-defined function {}", parts.join(".")), self.line()));
        }
        Ok(Expr::Column(parts))
    }

    fn function_call(&mut self) -> PResult<Expr> {
        let name = self.any_ident()?.to_ascii_uppercase();
        self.expect_sym("(")?;
        let mut args = Vec::new();
        let mut distinct = false;
        let mut star = false;
        if self.eat_sym("*") {
            star = true;
        } else if !self.is_sym(")") {
            distinct = self.eat_kw("DISTINCT");
            if !distinct {
                self.eat_kw("ALL");
            }
            loop {
                args.push(self.expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        self.expect_sym(")")?;
        if self.eat_kw("WITHIN") {
            self.expect_kw("GROUP")?;
            self.expect_sym("(")?;
            self.expect_kw("ORDER")?;
            self.expect_kw("BY")?;
            let order = self.order_items()?;
            self.expect_sym(")")?;
            let over = Over { partition_by: Vec::new(), order_by: order };
            return Ok(Expr::Function { name, args, distinct, star, over: Some(Box::new(over)) }).map(|f| match f {
                Expr::Function { name, args, distinct, star, over } => Expr::Function { name: format!("{name}#WITHIN"), args, distinct, star, over },
                other => other,
            });
        }
        let over = if self.eat_kw("OVER") {
            self.expect_sym("(")?;
            let mut partition_by = Vec::new();
            if self.eat_kw("PARTITION") {
                self.expect_kw("BY")?;
                loop {
                    partition_by.push(self.expr()?);
                    if !self.eat_sym(",") {
                        break;
                    }
                }
            }
            let mut order_by = Vec::new();
            if self.eat_kw("ORDER") {
                self.expect_kw("BY")?;
                order_by = self.order_items()?;
            }
            if self.is_kw("ROWS") || self.is_kw("RANGE") {
                return Err(unsupported("Window frames", self.line()));
            }
            self.expect_sym(")")?;
            Some(Box::new(Over { partition_by, order_by }))
        } else {
            None
        };
        Ok(Expr::Function { name, args, distinct, star, over })
    }

    fn case(&mut self) -> PResult<Expr> {
        self.expect_kw("CASE")?;
        let operand = if self.is_kw("WHEN") { None } else { Some(Box::new(self.expr()?)) };
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let when = self.expr()?;
            self.expect_kw("THEN")?;
            let then = self.expr()?;
            branches.push((when, then));
        }
        if branches.is_empty() {
            return Err(self.error_here());
        }
        let otherwise = if self.eat_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr::Case { operand, branches, otherwise })
    }
}

fn unreachable_tok() -> String {
    String::new()
}
