import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { BulkOperationType, CosmosClient } from "@azure/cosmos";

import { startEmulator } from "./emulator.js";

let emulator;
let client;
const account = "/resourceGroups/cosmos-js/providers/Microsoft.DocumentDB/databaseAccounts/cosmos-js";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/cosmos-js", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", account, { location: "eastus", properties: { databaseAccountOfferType: "Standard", locations: [{ locationName: "eastus" }] } }, "2024-11-15");
  const { connectionStrings } = await emulator.arm("POST", `${account}/listConnectionStrings`, undefined, "2024-11-15");
  client = new CosmosClient(connectionStrings[0].connectionString);
});

after(() => {
  client?.dispose();
  emulator?.stop();
});

test("items: create, read, replace, upsert, patch, delete", async () => {
  const { database } = await client.databases.createIfNotExists({ id: "app" });
  const { container } = await database.containers.createIfNotExists({ id: "orders", partitionKey: { paths: ["/tenant"] } });
  await container.items.create({ id: "1", tenant: "a", total: 10, tags: ["new"] });
  await assert.rejects(container.items.create({ id: "1", tenant: "a" }), { code: 409 });
  const { resource } = await container.item("1", "a").read();
  assert.equal(resource.total, 10);
  assert.ok(resource._etag);

  await container.item("1", "a").replace({ ...resource, total: 11 });
  await assert.rejects(container.item("1", "a").replace({ ...resource, total: 12 }, { accessCondition: { type: "IfMatch", condition: resource._etag } }), { code: 412 });
  await container.items.upsert({ id: "2", tenant: "a", total: 30 });
  const { resource: patched } = await container.item("1", "a").patch([
    { op: "incr", path: "/total", value: 4 },
    { op: "add", path: "/tags/-", value: "paid" },
  ]);
  assert.equal(patched.total, 15);
  assert.deepEqual(patched.tags, ["new", "paid"]);
  await container.item("2", "a").delete();
  const { statusCode } = await container.item("2", "a").read();
  assert.equal(statusCode, 404);
});

test("queries: within a partition and across partitions", async () => {
  const { database } = await client.databases.createIfNotExists({ id: "app" });
  const { container } = await database.containers.createIfNotExists({ id: "people", partitionKey: { paths: ["/city"] } });
  for (const [id, city, age] of [["ann", "Oslo", 31], ["bob", "Rome", 25], ["cid", "Oslo", 40]]) {
    await container.items.create({ id, city, age });
  }
  const oslo = await container.items.query({ query: "SELECT * FROM c WHERE c.age > @min", parameters: [{ name: "@min", value: 30 }] }, { partitionKey: "Oslo" }).fetchAll();
  assert.deepEqual(oslo.resources.map((r) => r.id).sort(), ["ann", "cid"]);
  const ordered = await container.items.query("SELECT VALUE c.id FROM c ORDER BY c.age DESC").fetchAll();
  assert.deepEqual(ordered.resources, ["cid", "ann", "bob"]);
  const count = await container.items.query("SELECT VALUE COUNT(1) FROM c").fetchAll();
  assert.deepEqual(count.resources, [3]);
  const paged = container.items.query("SELECT * FROM c", { maxItemCount: 2 });
  const first = await paged.fetchNext();
  assert.equal(first.resources.length, 2);
  assert.ok(first.hasMoreResults);
  const all = await container.items.readAll().fetchAll();
  assert.equal(all.resources.length, 3);
});

test("transactional batch and bulk", async () => {
  const { database } = await client.databases.createIfNotExists({ id: "app" });
  const { container } = await database.containers.createIfNotExists({ id: "carts", partitionKey: { paths: ["/user"] } });
  const result = await container.items.batch(
    [
      { operationType: BulkOperationType.Create, resourceBody: { id: "c1", user: "u1", n: 1 } },
      { operationType: BulkOperationType.Upsert, resourceBody: { id: "c2", user: "u1", n: 2 } },
    ],
    "u1",
  );
  assert.deepEqual(result.result.map((r) => r.statusCode), [201, 201]);
  const failed = await container.items.batch(
    [
      { operationType: BulkOperationType.Create, resourceBody: { id: "c3", user: "u1" } },
      { operationType: BulkOperationType.Create, resourceBody: { id: "c1", user: "u1" } },
    ],
    "u1",
  );
  assert.equal(failed.code, 207);
  const { statusCode } = await container.item("c3", "u1").read();
  assert.equal(statusCode, 404, "a failed batch applies nothing");
});

test("the management API sees data-plane databases, and keys are enforced", async () => {
  const { value } = await emulator.arm("GET", `${account}/sqlDatabases`, undefined, "2024-11-15");
  assert.ok(value.some((d) => d.name === "app"));
  const keys = await emulator.arm("POST", `${account}/listKeys`, undefined, "2024-11-15");
  const endpoint = (await emulator.arm("GET", account, undefined, "2024-11-15")).properties.documentEndpoint;
  const reader = new CosmosClient({ endpoint, key: keys.primaryReadonlyMasterKey });
  const { resources } = await reader.database("app").container("orders").items.readAll().fetchAll();
  assert.ok(resources.length >= 1);
  await assert.rejects(reader.database("app").container("orders").items.create({ id: "x", tenant: "z" }), { code: 403 });
  const forged = new CosmosClient({ endpoint, key: Buffer.alloc(64, 7).toString("base64") });
  await assert.rejects(forged.database("app").read(), { code: 401 });
  reader.dispose();
  forged.dispose();
});

test("a provisioned account takes a hand-written localhost connection string", async () => {
  // The key the Cosmos DB emulator documents, so existing settings keep working.
  const key = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
  const provisioned = await startEmulator({
    cosmos: [{
      resourceGroup: "local",
      account: "local-cosmos",
      key,
      databases: [{ id: "app", containers: [{ id: "orders", partitionKey: "/tenant" }] }],
    }],
  });
  const local = new CosmosClient(`AccountEndpoint=${provisioned.base}/;AccountKey=${key};`);
  try {
    const container = local.database("app").container("orders");
    await container.items.create({ id: "1", tenant: "a" });
    const { resources } = await container.items.query("SELECT VALUE c.id FROM c").fetchAll();
    assert.deepEqual(resources, ["1"]);
  } finally {
    local.dispose();
    provisioned.stop();
  }
});

test("bulk operations across partitions, each succeeding or failing on its own", async () => {
  const { database } = await client.databases.createIfNotExists({ id: "app" });
  const { container } = await database.containers.createIfNotExists({ id: "stock", partitionKey: { paths: ["/warehouse"] } });
  await container.items.create({ id: "s0", warehouse: "w2", count: 1 });
  const results = await container.items.executeBulkOperations([
    { operationType: BulkOperationType.Create, partitionKey: "w1", resourceBody: { id: "s1", warehouse: "w1", count: 5 } },
    { operationType: BulkOperationType.Upsert, partitionKey: "w2", resourceBody: { id: "s2", warehouse: "w2", count: 7 } },
    // Already there: this one fails, and the others still apply.
    { operationType: BulkOperationType.Create, partitionKey: "w2", resourceBody: { id: "s0", warehouse: "w2", count: 9 } },
    { operationType: BulkOperationType.Read, id: "s0", partitionKey: "w2" },
    { operationType: BulkOperationType.Delete, id: "s0", partitionKey: "w2" },
  ]);
  assert.deepEqual(results.map((r) => r.response?.statusCode ?? r.error?.code), [201, 201, 409, 200, 204]);
  assert.equal(results[3].response.resourceBody.count, 1);
  const { resource: s1 } = await container.item("s1", "w1").read();
  assert.equal(s1.count, 5);
  const { resource: s2 } = await container.item("s2", "w2").read();
  assert.equal(s2.count, 7);
  const { statusCode } = await container.item("s0", "w2").read();
  assert.equal(statusCode, 404);
});
