import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { AzureNamedKeyCredential, AzureSASCredential, TableClient, TableServiceClient, generateTableSas, odata } from "@azure/data-tables";

import { startEmulator } from "./emulator.js";

let emulator;
let endpoint;
let credential;
const account = "/resourceGroups/tables-js/providers/Microsoft.Storage/storageAccounts/tablesjs";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/tables-js", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", account, { location: "eastus", kind: "StorageV2", sku: { name: "Standard_LRS" } }, "2023-05-01");
  const { keys } = await emulator.arm("POST", `${account}/listKeys`, undefined, "2023-05-01");
  endpoint = (await emulator.arm("GET", account, undefined, "2023-05-01")).properties.primaryEndpoints.table;
  credential = new AzureNamedKeyCredential("tablesjs", keys[0].value);
});

after(() => emulator?.stop());

test("tables, entities, queries and transactions", async () => {
  const service = new TableServiceClient(endpoint, credential, { allowInsecureConnection: true });
  await service.createTable("Products");
  const names = [];
  for await (const table of service.listTables()) names.push(table.name);
  assert.deepEqual(names, ["Products"]);

  const table = new TableClient(endpoint, "Products", credential, { allowInsecureConnection: true });
  await table.createEntity({ partitionKey: "fruit", rowKey: "apple", price: 1.25, stock: 10, big: { value: "9007199254740993", type: "Int64" }, picked: new Date("2024-01-02T03:04:05Z") });
  await table.createEntity({ partitionKey: "fruit", rowKey: "pear", price: 2, stock: 0 });
  await assert.rejects(table.createEntity({ partitionKey: "fruit", rowKey: "pear" }), (e) => e.statusCode === 409);

  const apple = await table.getEntity("fruit", "apple");
  assert.equal(apple.price, 1.25);
  assert.equal(apple.big, 9007199254740993n);
  assert.equal(apple.picked.toISOString(), "2024-01-02T03:04:05.000Z");

  const inStock = [];
  for await (const e of table.listEntities({ queryOptions: { filter: odata`PartitionKey eq ${"fruit"} and stock gt ${0}` } })) inStock.push(e.rowKey);
  assert.deepEqual(inStock, ["apple"]);

  await table.updateEntity({ partitionKey: "fruit", rowKey: "pear", stock: 4 }, "Merge");
  assert.equal((await table.getEntity("fruit", "pear")).price, 2);
  await assert.rejects(table.updateEntity({ partitionKey: "fruit", rowKey: "pear", stock: 5 }, "Merge", { etag: apple.etag }), (e) => e.statusCode === 412);

  await table.submitTransaction([
    ["create", { partitionKey: "fruit", rowKey: "plum", stock: 3 }],
    ["delete", { partitionKey: "fruit", rowKey: "pear" }],
    ["upsert", { partitionKey: "fruit", rowKey: "apple", stock: 9 }, "Merge"],
  ]);
  const rows = [];
  for await (const e of table.listEntities()) rows.push([e.rowKey, e.stock]);
  assert.deepEqual(rows, [["apple", 9], ["plum", 3]]);

  await service.deleteTable("Products");
  await assert.rejects(table.getEntity("fruit", "apple"), (e) => e.statusCode === 404);
});

function tableClient(name) {
  return new TableClient(endpoint, name, credential, { allowInsecureConnection: true });
}

test("select, top and paging", async () => {
  const table = tableClient("Pages");
  await table.createTable();
  for (let i = 0; i < 7; i++) await table.createEntity({ partitionKey: "p", rowKey: `r${i}`, n: i, extra: "x" });
  const selected = [];
  for await (const e of table.listEntities({ queryOptions: { select: ["n"] } })) selected.push(Object.keys(e).filter((k) => !["etag", "timestamp", "partitionKey", "rowKey"].includes(k)));
  assert.ok(selected.every((keys) => keys.length === 1 && keys[0] === "n"), JSON.stringify(selected));

  const pages = [];
  for await (const page of table.listEntities().byPage({ maxPageSize: 3 })) pages.push(page.map((e) => e.n));
  assert.deepEqual(pages, [[0, 1, 2], [3, 4, 5], [6]]);
  await table.deleteTable();
});

test("replace drops properties, and a failed transaction changes nothing", async () => {
  const table = tableClient("Atomic");
  await table.createTable();
  await table.createEntity({ partitionKey: "p", rowKey: "a", keep: 1, drop: 2 });
  await table.upsertEntity({ partitionKey: "p", rowKey: "a", keep: 3 }, "Replace");
  const replaced = await table.getEntity("p", "a");
  assert.equal(replaced.keep, 3);
  assert.equal(replaced.drop, undefined);

  await assert.rejects(
    table.submitTransaction([
      ["create", { partitionKey: "p", rowKey: "b" }],
      ["create", { partitionKey: "p", rowKey: "a" }],
    ]),
    (e) => e.statusCode === 409,
  );
  await assert.rejects(table.getEntity("p", "b"), (e) => e.statusCode === 404);
  await assert.rejects(table.createEntity({ partitionKey: "p", rowKey: "has/slash" }), (e) => e.statusCode === 400);
  await table.deleteTable();
});

test("table SAS and access policies", async () => {
  const table = tableClient("Signed");
  await table.createTable();
  await table.createEntity({ partitionKey: "p", rowKey: "a", v: 1 });
  const expiresOn = new Date(Date.now() + 3600_000);
  await table.setAccessPolicy([{ id: "readers", accessPolicy: { permission: "r", expiry: expiresOn } }]);
  const [policy] = await table.getAccessPolicy();
  assert.equal(policy.id, "readers");
  assert.equal(policy.accessPolicy.permission, "r");

  const sas = generateTableSas("Signed", credential, { permissions: { query: true }, expiresOn });
  const reader = new TableClient(endpoint, "Signed", new AzureSASCredential(sas), { allowInsecureConnection: true });
  assert.equal((await reader.getEntity("p", "a")).v, 1);
  await assert.rejects(reader.createEntity({ partitionKey: "p", rowKey: "b" }), (e) => e.statusCode === 403);
  await table.deleteTable();
});

test("table service properties", async () => {
  const service = new TableServiceClient(endpoint, credential, { allowInsecureConnection: true });
  const cors = [{ allowedOrigins: "*", allowedMethods: "GET", allowedHeaders: "*", exposedHeaders: "*", maxAgeInSeconds: 5 }];
  await service.setProperties({ cors });
  const properties = await service.getProperties();
  assert.equal(properties.cors[0].maxAgeInSeconds, 5);
  assert.equal(properties.logging.version, "1.0", "the logging settings are kept");
  await service.setProperties({ cors: [] });
});
