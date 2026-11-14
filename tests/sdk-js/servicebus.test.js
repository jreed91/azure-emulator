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

test("a provisioned namespace takes a hand-written localhost connection string", async () => {
  const provisioned = await startEmulator({
    serviceBus: [{
      resourceGroup: "local",
      namespace: "local-bus",
      keys: { RootManageSharedAccessKey: "SAS_KEY_VALUE" },
      queues: [{ name: "jobs" }],
    }],
  });
  const connection = `Endpoint=sb://localhost:${provisioned.amqpPort}/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true`;
  const local = new ServiceBusClient(connection);
  const forged = new ServiceBusClient(connection.replace("SAS_KEY_VALUE", "WRONG"), { retryOptions: { maxRetries: 0 } });
  try {
    await local.createSender("jobs").sendMessages({ body: "hello" });
    const receiver = local.createReceiver("jobs", { receiveMode: "receiveAndDelete" });
    const got = await receiver.receiveMessages(1, { maxWaitTimeInMs: 2000 });
    assert.deepEqual(got.map((m) => m.body), ["hello"]);
    await assert.rejects(forged.createSender("jobs").sendMessages({ body: "nope" }), { code: "UnauthorizedAccess" });
  } finally {
    await local.close();
    await forged.close();
    provisioned.stop();
  }
});

test("sessions: a named session, the next available one, and state", async () => {
  await queue("sessions", { requiresSession: true });
  const sender = client.createSender("sessions");
  await sender.sendMessages([
    { body: "a1", sessionId: "a" },
    { body: "b1", sessionId: "b" },
    { body: "a2", sessionId: "a" },
  ]);
  await assert.rejects(sender.sendMessages({ body: "no session" }));

  const b = await client.acceptSession("sessions", "b");
  assert.deepEqual((await b.receiveMessages(5, { maxWaitTimeInMs: 2000 })).map((m) => m.body), ["b1"]);
  await b.setSessionState("step-1");
  assert.equal(String(await b.getSessionState()), "step-1");
  await b.renewSessionLock();
  await b.close();

  const next = await client.acceptNextSession("sessions");
  assert.equal(next.sessionId, "a");
  const got = await next.receiveMessages(5, { maxWaitTimeInMs: 2000 });
  assert.deepEqual(got.map((m) => m.body), ["a1", "a2"]);
  for (const message of got) await next.completeMessage(message);
  await next.close();
  await sender.close();
});

test("rules managed over AMQP are the subscription's rules", async () => {
  await emulator.arm("PUT", `${namespace}/topics/rules`, {});
  await emulator.arm("PUT", `${namespace}/topics/rules/subscriptions/audit`, {});
  const rules = client.createRuleManager("rules", "audit");
  const listRules = async () => {
    const all = [];
    for await (const rule of rules.listRules()) all.push(rule);
    return all;
  };
  assert.deepEqual((await listRules()).map((r) => r.name), ["$Default"]);
  await rules.deleteRule("$Default");
  await rules.createRule("errors", { sqlExpression: "level = 'error'" }, { sqlExpression: "SET seen = TRUE" });
  await rules.createRule("orders", { subject: "order", applicationProperties: { region: "eu" } });
  const listed = await listRules();
  assert.deepEqual(listed.map((r) => r.name), ["errors", "orders"]);
  assert.equal(listed[0].filter.sqlExpression, "level = 'error'");
  assert.equal(listed[0].action.sqlExpression, "SET seen = TRUE");
  assert.equal(listed[1].filter.subject, "order");
  // Resource Manager sees the same rules.
  const arm = await emulator.arm("GET", `${namespace}/topics/rules/subscriptions/audit/rules`);
  assert.deepEqual(arm.value.map((r) => r.name).sort(), ["errors", "orders"]);
  await assert.rejects(rules.createRule("bad", { sqlExpression: "level =" }));

  const sender = client.createSender("rules");
  await sender.sendMessages([
    { body: "e", applicationProperties: { level: "error" } },
    { body: "o", subject: "order", applicationProperties: { region: "eu" } },
    { body: "x", applicationProperties: { level: "info" } },
  ]);
  const receiver = client.createReceiver("rules", "audit", { receiveMode: "receiveAndDelete" });
  const got = await receiver.receiveMessages(5, { maxWaitTimeInMs: 2000 });
  assert.deepEqual(got.map((m) => m.body).sort(), ["e", "o"]);
  assert.equal(got.find((m) => m.body === "e").applicationProperties.seen, true);
  await receiver.close();
  await sender.close();
});

test("a queue can forward into a topic", async () => {
  await emulator.arm("PUT", `${namespace}/topics/fanout`, {});
  await emulator.arm("PUT", `${namespace}/topics/fanout/subscriptions/one`, {});
  await emulator.arm("PUT", `${namespace}/topics/fanout/subscriptions/two`, {});
  await queue("inbox", { forwardTo: "fanout" });
  const sender = client.createSender("inbox");
  await sender.sendMessages({ body: "hello" });
  for (const name of ["one", "two"]) {
    const receiver = client.createReceiver("fanout", name, { receiveMode: "receiveAndDelete" });
    assert.deepEqual((await receiver.receiveMessages(1, { maxWaitTimeInMs: 2000 })).map((m) => m.body), ["hello"]);
    await receiver.close();
  }
  await sender.close();
});

test("a settled message's lock cannot be renewed", async () => {
  await queue("locks");
  const sender = client.createSender("locks");
  await sender.sendMessages({ body: "once" });
  const receiver = client.createReceiver("locks");
  const [message] = await receiver.receiveMessages(1, { maxWaitTimeInMs: 2000 });
  const lockedUntil = await receiver.renewMessageLock(message);
  assert.ok(lockedUntil instanceof Date);
  await receiver.completeMessage(message);
  await assert.rejects(receiver.renewMessageLock(message), (e) => e.code === "MessageLockLost");
  await receiver.close();
  await sender.close();
});

test("scheduled messages on a topic, cancelled and delivered", async () => {
  await emulator.arm("PUT", `${namespace}/topics/later`, {});
  await emulator.arm("PUT", `${namespace}/topics/later/subscriptions/all`, {});
  const sender = client.createSender("later");
  const soon = new Date(Date.now() + 1000);
  const [keep, drop] = await sender.scheduleMessages([{ body: "keep" }, { body: "drop" }], soon);
  assert.ok(drop > keep, "each scheduled message gets its own sequence number");
  await sender.cancelScheduledMessages(drop);
  const receiver = client.createReceiver("later", "all", { receiveMode: "receiveAndDelete" });
  const got = await receiver.receiveMessages(5, { maxWaitTimeInMs: 4000 });
  assert.deepEqual(got.map((m) => m.body), ["keep"]);
  assert.ok(got[0].enqueuedTimeUtc >= new Date(soon.getTime() - 50));
  await receiver.close();
  await sender.close();
});

test("peek from a sequence number, and dead-letter a deferred message", async () => {
  await queue("peeking");
  const sender = client.createSender("peeking");
  await sender.sendMessages([{ body: "1" }, { body: "2" }, { body: "3" }]);
  const receiver = client.createReceiver("peeking");
  const all = await receiver.peekMessages(10);
  const from = await receiver.peekMessages(10, { fromSequenceNumber: all[1].sequenceNumber });
  assert.deepEqual(from.map((m) => m.body), ["2", "3"]);

  const [first] = await receiver.receiveMessages(1, { maxWaitTimeInMs: 2000 });
  await receiver.deferMessage(first);
  const [deferred] = await receiver.receiveDeferredMessages(first.sequenceNumber);
  await receiver.deadLetterMessage(deferred, { deadLetterReason: "late", deadLetterErrorDescription: "too slow" });
  const dead = client.createReceiver("peeking", { subQueueType: "deadLetter", receiveMode: "receiveAndDelete" });
  const [letter] = await dead.receiveMessages(1, { maxWaitTimeInMs: 2000 });
  assert.equal(letter.body, "1");
  assert.equal(letter.deadLetterReason, "late");
  assert.equal(letter.deadLetterErrorDescription, "too slow");
  await assert.rejects(receiver.receiveDeferredMessages(first.sequenceNumber.add(1000)), (e) => e.code === "MessageNotFound");
  await dead.close();
  await receiver.close();
  await sender.close();
});

test("correlation filters match typed application properties", async () => {
  await emulator.arm("PUT", `${namespace}/topics/typed`, {});
  await emulator.arm("PUT", `${namespace}/topics/typed/subscriptions/match`, {});
  const rules = client.createRuleManager("typed", "match");
  await rules.deleteRule("$Default");
  await assert.rejects(rules.deleteRule("missing"), (e) => e.code === "MessagingEntityNotFound");
  await rules.createRule("typed", {
    correlationId: "c-1",
    applicationProperties: { count: 3, urgent: true, ratio: 0.5, region: "eu" },
  });
  const sender = client.createSender("typed");
  const props = { count: 3, urgent: true, ratio: 0.5, region: "eu" };
  await sender.sendMessages([
    { body: "match", correlationId: "c-1", applicationProperties: props },
    { body: "wrong count", correlationId: "c-1", applicationProperties: { ...props, count: 4 } },
    { body: "wrong flag", correlationId: "c-1", applicationProperties: { ...props, urgent: false } },
    { body: "no correlation", applicationProperties: props },
  ]);
  const receiver = client.createReceiver("typed", "match", { receiveMode: "receiveAndDelete" });
  const got = await receiver.receiveMessages(5, { maxWaitTimeInMs: 2000 });
  assert.deepEqual(got.map((m) => m.body), ["match"]);
  const [listed] = await (async () => { const all = []; for await (const r of rules.listRules()) all.push(r); return all; })();
  assert.equal(listed.filter.correlationId, "c-1");
  assert.equal(listed.filter.applicationProperties.count, 3);
  await receiver.close();
  await sender.close();
});
