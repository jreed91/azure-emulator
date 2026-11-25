import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import sql from "mssql";

import { startEmulator } from "./emulator.js";

const PASSWORD = "Str0ng!Passw0rd";
let emulator;
let pool;

function config(overrides = {}) {
  return {
    server: "localhost",
    port: emulator.sqlPort,
    user: "sa",
    password: PASSWORD,
    database: "app",
    options: { encrypt: true, trustServerCertificate: true },
    pool: { max: 4 },
    ...overrides,
  };
}

before(async () => {
  emulator = await startEmulator({ sql: { saPassword: PASSWORD, databases: ["app"] } });
  pool = await new sql.ConnectionPool(config()).connect();
  await pool.request().batch(`
IF OBJECT_ID('dbo.Orders') IS NULL
  CREATE TABLE dbo.Orders (Id INT IDENTITY PRIMARY KEY, Body NVARCHAR(MAX));
IF OBJECT_ID('dbo.Customers') IS NULL
  CREATE TABLE dbo.Customers (Name NVARCHAR(100) PRIMARY KEY, Body NVARCHAR(MAX));`);
  await pool.request().batch(`
CREATE OR ALTER PROCEDURE dbo.PlaceOrder
  @order NVARCHAR(MAX),
  @success BIT OUTPUT,
  @message VARCHAR(MAX) OUTPUT
AS
BEGIN
  DECLARE @customer NVARCHAR(100) = JSON_VALUE(@order, '$.Customer');
  IF EXISTS (SELECT 1 FROM dbo.Customers WHERE Name = @customer)
  BEGIN
    SET @success = 0;
    SET @message = 'customer ' + @customer + ' already ordered';
    RETURN 1;
  END
  INSERT INTO dbo.Customers (Name, Body) VALUES (@customer, @order);
  INSERT INTO dbo.Orders (Body) VALUES (@order);
  SET @success = 1;
  SET @message = 'ordered for ' + @customer;
  RETURN 0;
END`);
});

after(async () => {
  await pool?.close();
  emulator?.stop();
});

function placeOrder(request, customer) {
  request.input("order", sql.NVarChar(sql.MAX), JSON.stringify({ Customer: customer, Items: [1, 2] }));
  request.output("success", sql.Bit);
  request.output("message", sql.VarChar(sql.MAX));
  return request.execute("dbo.PlaceOrder");
}

test("a login with TLS and the server certificate trusted reaches the database it names", async () => {
  const result = await pool.request().query("SELECT DB_NAME() AS db, SUSER_SNAME() AS login");
  assert.deepEqual(result.recordset, [{ db: "app", login: "sa" }]);
});

test("a wrong password is refused with login error 18456", async () => {
  await assert.rejects(new sql.ConnectionPool(config({ password: "wrong" })).connect(), (error) => {
    assert.match(error.message, /Login failed for user 'sa'/);
    return true;
  });
});

test("CREATE DATABASE runs once behind IF DB_ID(...) IS NULL", async () => {
  for (let i = 0; i < 2; i++) {
    await pool.request().batch("IF DB_ID('reports') IS NULL CREATE DATABASE reports");
  }
  const result = await pool.request().query("SELECT COUNT(*) AS n FROM sys.databases WHERE name = 'reports'");
  assert.equal(result.recordset[0].n, 1);
});

test("a procedure returns OUTPUT parameters and a return value", async () => {
  let result = await placeOrder(pool.request(), "contoso");
  assert.deepEqual(result.output, { success: true, message: "ordered for contoso" });
  assert.equal(result.returnValue, 0);
  result = await placeOrder(pool.request(), "contoso");
  assert.deepEqual(result.output, { success: false, message: "customer contoso already ordered" });
  assert.equal(result.returnValue, 1);
});

test("parameterized SELECT, INSERT and DELETE go through sp_executesql", async () => {
  const insert = await pool
    .request()
    .input("body", sql.NVarChar(sql.MAX), JSON.stringify({ Customer: "fabrikam", Total: 12.5 }))
    .query("INSERT INTO dbo.Orders (Body) OUTPUT inserted.Id VALUES (@body)");
  const id = insert.recordset[0].Id;
  const select = await pool
    .request()
    .input("id", sql.Int, id)
    .query("SELECT JSON_VALUE(Body, '$.Customer') AS customer, CAST(JSON_VALUE(Body, '$.Total') AS DECIMAL(10,2)) AS total FROM dbo.Orders WHERE Id = @id");
  assert.deepEqual(select.recordset, [{ customer: "fabrikam", total: 12.5 }]);
  const remove = await pool.request().input("id", sql.Int, id).query("DELETE FROM dbo.Orders WHERE Id = @id");
  assert.deepEqual(remove.rowsAffected, [1]);
});

test("rows a procedure wrote inside a rolled-back transaction are gone", async () => {
  const transaction = new sql.Transaction(pool);
  await transaction.begin();
  const result = await placeOrder(new sql.Request(transaction), "northwind");
  assert.equal(result.output.success, true);
  await transaction.rollback();
  const customers = await pool.request().input("name", sql.NVarChar(100), "northwind").query("SELECT COUNT(*) AS n FROM dbo.Customers WHERE Name = @name");
  assert.equal(customers.recordset[0].n, 0);
  const orders = await pool.request().query("SELECT COUNT(*) AS n FROM dbo.Orders WHERE JSON_VALUE(Body, '$.Customer') = 'northwind'");
  assert.equal(orders.recordset[0].n, 0);
});

test("a committed transaction keeps its rows", async () => {
  const transaction = new sql.Transaction(pool);
  await transaction.begin();
  await placeOrder(new sql.Request(transaction), "adventureworks");
  await transaction.commit();
  const result = await pool.request().query("SELECT COUNT(*) AS n FROM dbo.Customers WHERE Name = 'ADVENTUREWORKS '");
  assert.equal(result.recordset[0].n, 1, "names compare case-insensitively, ignoring trailing spaces");
});

test("errors carry SQL Server's numbers", async () => {
  await assert.rejects(pool.request().query("SELECT * FROM dbo.Missing"), (error) => {
    assert.equal(error.number, 208);
    return true;
  });
  await assert.rejects(pool.request().query("INSERT INTO dbo.Customers (Name) VALUES (N'contoso')"), (error) => {
    assert.equal(error.number, 2627);
    return true;
  });
});
