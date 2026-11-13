import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { ServiceBusClient } from "@azure/service-bus";

import { startEmulator } from "./emulator.js";

let emulator;
let client;
const namespace = "/resourceGroups/js-tests/providers/Microsoft.ServiceBus/namespaces/js-tests";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/js-tests", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", namespace, { location: "eastus" });
  const keys = await emulator.arm("POST", `${namespace}/AuthorizationRules/RootManageSharedAccessKey/listKeys`);
  client = new ServiceBusClient(keys.primaryConnectionString);
});

after(async () => {
  await client?.close();
  emulator?.stop();
});

async function queue(name, properties = {}) {
  await emulator.arm("PUT", `${namespace}/queues/${name}`, { properties });
}

test("a message batch is sized by the link and sent", async () => {
  await queue("batch");
  const sender = client.createSender("batch");
  const batch = await sender.createMessageBatch();
  assert.equal(batch.maxSizeInBytes, 256 * 1024);
  assert.ok(batch.tryAddMessage({ body: "a" }));
  assert.ok(batch.tryAddMessage({ body: "b" }));
  await sender.sendMessages(batch);
  const receiver = client.createReceiver("batch", { receiveMode: "receiveAndDelete" });
  const got = await receiver.receiveMessages(5, { maxWaitTimeInMs: 2000 });
  assert.deepEqual(got.map((m) => m.body), ["a", "b"]);
  assert.ok(got.every((m) => m.messageId), "the broker assigns message ids");
  await receiver.close();
  await sender.close();
});

test("every abandon counts toward the max delivery count", async () => {
  await queue("poison", { maxDeliveryCount: 3 });
  const sender = client.createSender("poison");
  await sender.sendMessages({ body: "poison" });
  const receiver = client.createReceiver("poison");
  const counts = [];
  for (let i = 0; i < 5; i++) {
    const [message] = await receiver.receiveMessages(1, { maxWaitTimeInMs: 1000 });
    if (!message) break;
    counts.push(message.deliveryCount);
    await receiver.abandonMessage(message);
  }
  assert.deepEqual(counts, [0, 1, 2]);
  const dead = client.createReceiver("poison", { subQueueType: "deadLetter" });
  const [message] = await dead.receiveMessages(1, { maxWaitTimeInMs: 2000 });
  assert.equal(message.deadLetterReason, "MaxDeliveryCountExceeded");
  await dead.completeMessage(message);
  await dead.close();
  await receiver.close();
  await sender.close();
});

test("a dead-lettered message moves to the dead-letter queue", async () => {
  await queue("dead");
  const sender = client.createSender("dead");
  await sender.sendMessages({ body: "bad" });
  const receiver = client.createReceiver("dead");
  const [message] = await receiver.receiveMessages(1, { maxWaitTimeInMs: 2000 });
  await receiver.deadLetterMessage(message, { deadLetterReason: "invalid", deadLetterErrorDescription: "cannot parse" });
  assert.equal((await receiver.peekMessages(10)).length, 0);
  const dead = client.createReceiver("dead", { subQueueType: "deadLetter" });
  const [peeked] = await dead.peekMessages(10);
  assert.equal(peeked.body, "bad");
  assert.equal(peeked.deadLetterReason, "invalid");
  assert.equal(peeked.deadLetterErrorDescription, "cannot parse");
  await dead.close();
  await receiver.close();
  await sender.close();
});

test("complete, defer and receive deferred", async () => {
  await queue("work");
  const sender = client.createSender("work");
  await sender.sendMessages([{ body: "one" }, { body: "two" }]);
  const receiver = client.createReceiver("work");
  const [first, second] = await receiver.receiveMessages(2, { maxWaitTimeInMs: 2000 });
  await receiver.completeMessage(first);
  await receiver.deferMessage(second);
  const [deferred] = await receiver.receiveDeferredMessages(second.sequenceNumber);
  assert.equal(deferred.body, "two");
  await receiver.completeMessage(deferred);
  assert.equal((await receiver.peekMessages(10)).length, 0);
  await receiver.close();
  await sender.close();
});
