import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { QueueClient, QueueSASPermissions, QueueServiceClient, StorageSharedKeyCredential } from "@azure/storage-queue";

import { startEmulator } from "./emulator.js";

let emulator;
let service;
const account = "/resourceGroups/queues-js/providers/Microsoft.Storage/storageAccounts/queuesjs";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/queues-js", { location: "eastus" }, "2021-04-01");
  const created = await emulator.arm("PUT", account, { location: "eastus", kind: "StorageV2", sku: { name: "Standard_LRS" } }, "2023-05-01");
  const { keys } = await emulator.arm("POST", `${account}/listKeys`, undefined, "2023-05-01");
  const endpoint = created?.properties?.primaryEndpoints?.queue ?? (await emulator.arm("GET", account, undefined, "2023-05-01")).properties.primaryEndpoints.queue;
  service = new QueueServiceClient(endpoint, new StorageSharedKeyCredential("queuesjs", keys[0].value));
});

after(() => emulator?.stop());

test("send, receive, update and delete messages", async () => {
  const queue = service.getQueueClient("orders");
  await queue.create({ metadata: { team: "billing" } });
  await queue.sendMessage("one");
  await queue.sendMessage("two");
  const { approximateMessagesCount, metadata } = await queue.getProperties();
  assert.equal(approximateMessagesCount, 2);
  assert.deepEqual(metadata, { team: "billing" });

  const peeked = await queue.peekMessages({ numberOfMessages: 5 });
  assert.deepEqual(peeked.peekedMessageItems.map((m) => m.messageText), ["one", "two"]);

  const { receivedMessageItems: [first] } = await queue.receiveMessages({ visibilityTimeout: 30 });
  assert.equal(first.messageText, "one");
  const updated = await queue.updateMessage(first.messageId, first.popReceipt, "one, edited", 0);
  const { receivedMessageItems } = await queue.receiveMessages({ numberOfMessages: 5 });
  assert.deepEqual(receivedMessageItems.map((m) => m.messageText).sort(), ["one, edited", "two"]);
  for (const message of receivedMessageItems) {
    await queue.deleteMessage(message.messageId, message.popReceipt);
  }
  assert.ok(updated.popReceipt);
  assert.equal((await queue.getProperties()).approximateMessagesCount, 0);

  const names = [];
  for await (const item of service.listQueues()) names.push(item.name);
  assert.deepEqual(names, ["orders"]);
  await queue.delete();
});

test("visibility, time to live and clearing", async () => {
  const queue = service.getQueueClient("timing");
  await queue.create();
  await queue.sendMessage("hidden", { visibilityTimeout: 30 });
  await queue.sendMessage("short", { messageTimeToLive: 1 });
  await queue.sendMessage("forever", { messageTimeToLive: -1 });
  const peeked = await queue.peekMessages({ numberOfMessages: 32 });
  assert.deepEqual(peeked.peekedMessageItems.map((m) => m.messageText), ["short", "forever"], "an invisible message is not peeked");
  await new Promise((resolve) => setTimeout(resolve, 1500));
  const { receivedMessageItems } = await queue.receiveMessages({ numberOfMessages: 32 });
  assert.deepEqual(receivedMessageItems.map((m) => m.messageText), ["forever"], "an expired message is gone");
  assert.equal(receivedMessageItems[0].expiresOn.getUTCFullYear(), 9999, "-1 never expires");
  assert.equal(receivedMessageItems[0].dequeueCount, 1);
  await queue.clearMessages();
  assert.equal((await queue.getProperties()).approximateMessagesCount, 0);
  await queue.delete();
});

test("refusals: existing queues, missing queues and stale pop receipts", async () => {
  const queue = service.getQueueClient("strict");
  await queue.create({ metadata: { a: "1" } });
  await queue.create({ metadata: { a: "1" } }); // the same metadata: no conflict
  await assert.rejects(queue.create({ metadata: { a: "2" } }), (e) => e.statusCode === 409 && e.code === "QueueAlreadyExists");
  await assert.rejects(service.getQueueClient("missing").getProperties(), (e) => e.statusCode === 404 && e.code === "QueueNotFound");
  await assert.rejects(service.getQueueClient("Bad_Name").create(), (e) => e.statusCode === 400);

  await queue.sendMessage("m");
  const { receivedMessageItems: [message] } = await queue.receiveMessages({ visibilityTimeout: 30 });
  const updated = await queue.updateMessage(message.messageId, message.popReceipt, "m2", 30);
  await assert.rejects(queue.deleteMessage(message.messageId, message.popReceipt), (e) => e.statusCode === 400 && e.code === "PopReceiptMismatch");
  await queue.deleteMessage(message.messageId, updated.popReceipt);
  await assert.rejects(queue.deleteMessage(message.messageId, updated.popReceipt), (e) => e.statusCode === 404 && e.code === "MessageNotFound");
  await queue.delete();
});

test("metadata, listing with a prefix, access policies and SAS", async () => {
  for (const name of ["list-a", "list-b", "other"]) await service.getQueueClient(name).create({ metadata: { name } });
  const listed = [];
  for await (const item of service.listQueues({ prefix: "list-", includeMetadata: true })) listed.push([item.name, item.metadata?.name]);
  assert.deepEqual(listed, [["list-a", "list-a"], ["list-b", "list-b"]]);

  const queue = service.getQueueClient("list-a");
  await queue.setMetadata({ owner: "team" });
  assert.deepEqual((await queue.getProperties()).metadata, { owner: "team" });

  const expiresOn = new Date(Date.now() + 3600_000);
  await queue.setAccessPolicy([{ id: "readers", accessPolicy: { permissions: "rp", expiresOn } }]);
  const policy = await queue.getAccessPolicy();
  assert.deepEqual(policy.signedIdentifiers.map((s) => [s.id, s.accessPolicy.permissions]), [["readers", "rp"]]);

  await queue.sendMessage("signed");
  // Read (r) peeks; process (p) receives and deletes; add (a) sends.
  const reader = new QueueClient(await queue.generateSasUrl({ permissions: QueueSASPermissions.parse("r"), expiresOn }));
  assert.deepEqual((await reader.peekMessages()).peekedMessageItems.map((m) => m.messageText), ["signed"]);
  await assert.rejects(reader.sendMessage("nope"), (e) => e.statusCode === 403);
  await assert.rejects(reader.receiveMessages(), (e) => e.statusCode === 403);
  const processor = new QueueClient(await queue.generateSasUrl({ permissions: QueueSASPermissions.parse("p"), expiresOn }));
  await assert.rejects(processor.peekMessages(), (e) => e.statusCode === 403);

  for (const name of ["list-a", "list-b", "other"]) await service.getQueueClient(name).delete();
});

test("service properties", async () => {
  const cors = [{ allowedOrigins: "https://example.com", allowedMethods: "GET,PUT", allowedHeaders: "*", exposedHeaders: "*", maxAgeInSeconds: 60 }];
  await service.setProperties({ cors });
  const properties = await service.getProperties();
  assert.equal(properties.cors[0].allowedOrigins, "https://example.com");
  assert.equal(properties.cors[0].maxAgeInSeconds, 60);
  // A set replaces only what it names: the CORS rules outlive a metrics change.
  await service.setProperties({ hourMetrics: { version: "1.0", enabled: true, includeAPIs: true, retentionPolicy: { enabled: true, days: 7 } } });
  const again = await service.getProperties();
  assert.equal(again.hourMetrics.enabled, true);
  assert.equal(again.hourMetrics.retentionPolicy.days, 7);
  assert.equal(again.cors.length, 1);
  await service.setProperties({ cors: [] });
  assert.equal((await service.getProperties()).cors.length, 0);
});
