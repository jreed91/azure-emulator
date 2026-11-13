import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { QueueServiceClient, StorageSharedKeyCredential } from "@azure/storage-queue";

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
