// Path-style storage addressing, as Azurite serves it: one port per service,
// and the account as the first path segment, so a connection string written
// for Azurite works with only its ports changed.
import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { BlobServiceClient } from "@azure/storage-blob";
import { QueueServiceClient } from "@azure/storage-queue";
import { TableClient, TableServiceClient } from "@azure/data-tables";

import { startEmulator } from "./emulator.js";

let emulator;
let connectionString;
const name = "pathstylejs";
const account = `/resourceGroups/path-style-js/providers/Microsoft.Storage/storageAccounts/${name}`;

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/path-style-js", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", account, { location: "eastus", kind: "StorageV2", sku: { name: "Standard_LRS" } }, "2023-05-01");
  const { keys } = await emulator.arm("POST", `${account}/listKeys`, undefined, "2023-05-01");
  const { blob, queue, table } = emulator.storagePorts;
  connectionString = [
    "DefaultEndpointsProtocol=http",
    `AccountName=${name}`,
    `AccountKey=${keys[0].value}`,
    `BlobEndpoint=http://127.0.0.1:${blob}/${name}`,
    `QueueEndpoint=http://127.0.0.1:${queue}/${name}`,
    `TableEndpoint=http://127.0.0.1:${table}/${name}`,
  ].join(";");
});

after(() => emulator?.stop());

test("blobs, leases and copies over a path-style connection string", async () => {
  const service = BlobServiceClient.fromConnectionString(connectionString);
  const container = service.getContainerClient("azure-webjobs-hosts");
  await container.create();
  const blob = container.getBlockBlobClient("locks/host-id/host");
  await blob.upload("", 0);
  const lease = blob.getBlobLeaseClient();
  await lease.acquireLease(15);
  await assert.rejects(blob.upload("taken", 5), (error) => error.statusCode === 412);
  await lease.renewLease();
  await lease.releaseLease();

  const copy = container.getBlockBlobClient("copy");
  await (await copy.beginCopyFromURL(blob.url)).pollUntilDone();
  assert.equal((await copy.getProperties()).contentLength, 0);

  const names = [];
  for await (const item of service.listContainers()) names.push(item.name);
  assert.deepEqual(names, ["azure-webjobs-hosts"]);
  const blobs = [];
  for await (const item of container.listBlobsFlat({ prefix: "locks/" })) blobs.push(item.name);
  assert.deepEqual(blobs, ["locks/host-id/host"]);
});

test("queues over a path-style connection string", async () => {
  const service = QueueServiceClient.fromConnectionString(connectionString);
  const queue = service.getQueueClient("work-items");
  await queue.create();
  await queue.sendMessage("hello");
  const { receivedMessageItems: [message] } = await queue.receiveMessages();
  assert.equal(message.messageText, "hello");
  await queue.deleteMessage(message.messageId, message.popReceipt);
  const names = [];
  for await (const item of service.listQueues()) names.push(item.name);
  assert.deepEqual(names, ["work-items"]);
});

test("tables over a path-style connection string", async () => {
  const options = { allowInsecureConnection: true };
  const service = TableServiceClient.fromConnectionString(connectionString, options);
  await service.createTable("events");
  const tables = [];
  for await (const item of service.listTables()) tables.push(item.name);
  assert.deepEqual(tables, ["events"]);

  const client = TableClient.fromConnectionString(connectionString, "events", options);
  await client.createEntity({ partitionKey: "p", rowKey: "1", value: 42 });
  const entity = await client.getEntity("p", "1");
  assert.equal(entity.value, 42);
  const rows = [];
  for await (const row of client.listEntities({ queryOptions: { filter: "PartitionKey eq 'p'" } })) rows.push(row.rowKey);
  assert.deepEqual(rows, ["1"]);
  await client.submitTransaction([["upsert", { partitionKey: "p", rowKey: "2" }], ["delete", { partitionKey: "p", rowKey: "1" }]]);
  const after = [];
  for await (const row of client.listEntities()) after.push(row.rowKey);
  assert.deepEqual(after, ["2"]);
});

test("a path that names no account is rejected", async () => {
  const response = await fetch(`http://127.0.0.1:${emulator.storagePorts.blob}/?comp=list`);
  assert.equal(response.status, 400);
  assert.equal(response.headers.get("x-ms-error-code"), "InvalidUri");
});
