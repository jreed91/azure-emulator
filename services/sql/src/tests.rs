//! The engine run directly, without TDS.

use crate::catalog::SqlState;
use crate::exec::{Event, Executor, Session};
use crate::value::Value;

pub struct Harness {
    pub state: SqlState,
    pub session: Session,
}

impl Harness {
    pub fn new() -> Self {
        Harness { state: SqlState::default(), session: Session::new(51, "sa", "master") }
    }

    pub fn run(&mut self, sql: &str) -> Vec<Event> {
        let mut out = Vec::new();
        Executor::new(&mut self.state, &mut self.session, &mut out).run_batch(sql);
        out
    }

    /// Runs a batch that must succeed; returns its result sets' rows.
    pub fn ok(&mut self, sql: &str) -> Vec<Vec<Vec<Value>>> {
        let events = self.run(sql);
        let mut sets = Vec::new();
        for event in events {
            match event {
                Event::Error(e) => panic!("{sql}\n=> error {}: {} (line {})", e.number, e.message, e.line),
                Event::ResultSet { rows, .. } => sets.push(rows),
                _ => {}
            }
        }
        sets
    }

    pub fn error(&mut self, sql: &str) -> (i32, String) {
        for event in self.run(sql) {
            if let Event::Error(e) = event {
                return (e.number, e.message);
            }
        }
        panic!("{sql} succeeded");
    }

    pub fn scalar(&mut self, sql: &str) -> Value {
        self.ok(sql).pop().and_then(|s| s.into_iter().next()).and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)
    }
}

fn s(text: &str) -> Value {
    Value::Str(text.into())
}

#[test]
fn databases_tables_and_procedures() {
    let mut h = Harness::new();
    h.ok("IF DB_ID('app') IS NULL CREATE DATABASE app;");
    h.ok("IF DB_ID('app') IS NULL CREATE DATABASE app;");
    h.ok("USE app");
    assert_eq!(h.session.database, "app");
    h.ok(
        "IF OBJECT_ID(N'dbo.Items', N'U') IS NULL
         CREATE TABLE dbo.Items (Id INT IDENTITY(1,1) PRIMARY KEY, Payload NVARCHAR(MAX) NOT NULL);
         IF OBJECT_ID('dbo.Keys') IS NULL CREATE TABLE Keys ([Key] NVARCHAR(100) PRIMARY KEY, Value NVARCHAR(MAX));",
    );
    h.ok(
        "CREATE OR ALTER PROCEDURE dbo.SaveItem
            @Payload NVARCHAR(MAX),
            @Fail BIT = 0,
            @Saved BIT OUTPUT,
            @Message VARCHAR(MAX) OUTPUT
         AS
         BEGIN
            SET NOCOUNT ON;
            DECLARE @name NVARCHAR(100) = JSON_VALUE(@Payload, '$.Name');
            IF EXISTS (SELECT 1 FROM dbo.Items WHERE JSON_VALUE(Payload, '$.Name') = @name)
            BEGIN
                SET @Saved = 0;
                SET @Message = 'duplicate: ' + @name;
                RETURN 1;
            END
            INSERT INTO dbo.Items (Payload) VALUES (@Payload);
            SET @Saved = 1;
            SET @Message = 'saved ' + @name + ' as ' + CAST(SCOPE_IDENTITY() AS VARCHAR(20));
            RETURN 0;
         END",
    );
    let rows = h.ok(
        "DECLARE @s BIT, @m VARCHAR(MAX), @r INT;
         EXEC @r = dbo.SaveItem @Payload = N'{\"Name\":\"a\"}', @Saved = @s OUTPUT, @Message = @m OUTPUT;
         SELECT @r, @s, @m;
         EXEC @r = dbo.SaveItem N'{\"Name\":\"a\"}', 0, @s OUTPUT, @m OUTPUT;
         SELECT @r, @s, @m;",
    );
    assert_eq!(rows[0][0], vec![Value::Int(0), Value::Bit(true), s("saved a as 1")]);
    assert_eq!(rows[1][0], vec![Value::Int(1), Value::Bit(false), s("duplicate: a")]);
    assert_eq!(h.scalar("SELECT COUNT(*) FROM Items WHERE JSON_VALUE(Payload, '$.Name') = 'A'"), Value::Int(1));
}

#[test]
fn transactions_roll_back() {
    let mut h = Harness::new();
    h.ok("CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(10))");
    h.ok("INSERT t VALUES (1, 'one')");
    h.ok("BEGIN TRAN; INSERT t VALUES (2, 'two'); UPDATE t SET name = 'uno' WHERE id = 1; DELETE t WHERE id = 1; ROLLBACK;");
    assert_eq!(h.ok("SELECT id, name FROM t")[0], vec![vec![Value::Int(1), s("one")]]);
    h.ok("BEGIN TRAN; INSERT t VALUES (3, 'three'); SAVE TRAN sp; INSERT t VALUES (4, 'four'); ROLLBACK TRAN sp; COMMIT;");
    assert_eq!(h.scalar("SELECT COUNT(*) FROM t"), Value::Int(2));
    assert_eq!(h.scalar("SELECT @@TRANCOUNT"), Value::Int(0));
}

#[test]
fn errors_are_reported() {
    let mut h = Harness::new();
    h.ok("CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(3) NOT NULL)");
    h.ok("INSERT t VALUES (1, 'a')");
    assert_eq!(h.error("INSERT t VALUES (1, 'b')").0, 2627);
    assert_eq!(h.error("INSERT t VALUES (2, NULL)").0, 515);
    assert_eq!(h.error("INSERT t VALUES (2, 'toolong')").0, 2628);
    assert_eq!(h.error("SELECT nope FROM t").0, 207);
    assert_eq!(h.error("SELECT * FROM missing").0, 208);
    assert_eq!(h.error("SELEC 1").0, 102);
    assert_eq!(h.error("SELECT CAST('x' AS INT)").0, 245);
    let caught = h.ok("BEGIN TRY SELECT 1/0 END TRY BEGIN CATCH SELECT ERROR_NUMBER(), ERROR_MESSAGE() END CATCH");
    assert_eq!(caught[0][0][0], Value::Int(8134));
}

#[test]
fn queries() {
    let mut h = Harness::new();
    h.ok("CREATE TABLE p (id INT PRIMARY KEY, name VARCHAR(20), team INT)");
    h.ok("INSERT INTO p (id, name, team) VALUES (1,'ann',1),(2,'bob',1),(3,'cy',2),(4,'di',NULL)");
    h.ok("CREATE TABLE team (id INT PRIMARY KEY, title NVARCHAR(20))");
    h.ok("INSERT team VALUES (1, N'red'), (2, N'blue')");
    let rows = h.ok("SELECT t.title, COUNT(*) AS n FROM p JOIN team t ON t.id = p.team GROUP BY t.title ORDER BY n DESC")[0].clone();
    assert_eq!(rows, vec![vec![s("red"), Value::Int(2)], vec![s("blue"), Value::Int(1)]]);
    let rows = h.ok("SELECT p.name, t.title FROM p LEFT JOIN team t ON t.id = p.team WHERE p.id > 2 ORDER BY p.id")[0].clone();
    assert_eq!(rows, vec![vec![s("cy"), s("blue")], vec![s("di"), Value::Null]]);
    assert_eq!(h.scalar("SELECT TOP 1 name FROM p ORDER BY id DESC"), s("di"));
    assert_eq!(h.ok("SELECT name FROM p ORDER BY id OFFSET 1 ROWS FETCH NEXT 2 ROWS ONLY")[0].len(), 2);
    assert_eq!(h.scalar("SELECT name FROM p WHERE name LIKE 'B%'"), s("bob"));
    assert_eq!(h.scalar("SELECT (SELECT MAX(id) FROM p) + 1"), Value::Int(5));
    assert_eq!(h.scalar("WITH x AS (SELECT id FROM p WHERE team = 1) SELECT SUM(id) FROM x"), Value::Int(3));
    assert_eq!(h.scalar("SELECT STRING_AGG(name, ',') WITHIN GROUP (ORDER BY id DESC) FROM p"), s("di,cy,bob,ann"));
    assert_eq!(h.scalar("SELECT ROW_NUMBER() OVER (ORDER BY name DESC) FROM p WHERE id = 1"), Value::Int(1));
    assert_eq!(h.scalar("SELECT id FROM p FOR JSON PATH"), s("[{\"id\":1},{\"id\":2},{\"id\":3},{\"id\":4}]"));
    let out = h.ok("INSERT p OUTPUT INSERTED.id, inserted.name VALUES (5, 'ed', NULL)");
    assert_eq!(out[0][0], vec![Value::Int(5), s("ed")]);
    let rows = h.ok("SELECT [key], value FROM OPENJSON(N'{\"a\":1,\"b\":\"x\"}')")[0].clone();
    assert_eq!(rows.len(), 2);
}
