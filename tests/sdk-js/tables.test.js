import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { AzureNamedKeyCredential, TableClient, TableServiceClient, odata } from "@azure/data-tables";

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
