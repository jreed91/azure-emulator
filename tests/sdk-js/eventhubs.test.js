import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { earliestEventPosition, EventHubConsumerClient, EventHubProducerClient } from "@azure/event-hubs";

import { startEmulator } from "./emulator.js";

let emulator;
let connection;
const namespace = "/resourceGroups/hubs-js/providers/Microsoft.EventHub/namespaces/hubs-js";
const api = "2024-01-01";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/hubs-js", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", namespace, { location: "eastus", sku: { name: "Standard" } }, api);
  await emulator.arm("PUT", `${namespace}/eventhubs/orders`, { properties: { partitionCount: 2 } }, api);
  await emulator.arm("PUT", `${namespace}/eventhubs/orders/consumergroups/audit`, {}, api);
  const keys = await emulator.arm("POST", `${namespace}/authorizationRules/RootManageSharedAccessKey/listKeys`, undefined, api);
  connection = keys.primaryConnectionString;
});

after(() => emulator?.stop());

/** Receives from every partition until `count` events arrive (or 5 s pass). */
async function receive(group, count, options = {}) {
  const consumer = new EventHubConsumerClient(group, connection, "orders");
  const events = [];
  await new Promise((resolve) => {
    const timer = setTimeout(resolve, 5000);
    const subscription = consumer.subscribe(
      {
        processEvents: async (batch) => {
          events.push(...batch);
          if (events.length >= count) {
            clearTimeout(timer);
            resolve();
          }
        },
        processError: async () => {},
      },
      { startPosition: earliestEventPosition, maxWaitTimeInSeconds: 1, ...options },
    );
    timer.unref?.();
    consumer.subscription = subscription;
  });
  await consumer.close();
  return events;
}

test("send and receive across partitions", async () => {
  const producer = new EventHubProducerClient(connection, "orders");
  const properties = await producer.getEventHubProperties();
  assert.deepEqual(properties.partitionIds, ["0", "1"]);

  const keyed = await producer.createBatch({ partitionKey: "customer-7" });
  keyed.tryAdd({ body: "first" });
  keyed.tryAdd({ body: "second" });
  await producer.sendBatch(keyed);
  await producer.sendBatch([{ body: "direct" }], { partitionId: "0" });

  const partition0 = await producer.getPartitionProperties("0");
  assert.equal(partition0.isEmpty, false);
  await producer.close();

  const events = await receive("$Default", 3);
  assert.deepEqual(events.map((e) => e.body).sort(), ["direct", "first", "second"]);
  const keyedEvents = events.filter((e) => e.partitionKey === "customer-7");
  assert.equal(keyedEvents.length, 2);
  assert.deepEqual(keyedEvents.map((e) => e.sequenceNumber), [0, 1]);
  // Reading takes nothing away: another consumer group sees the same events.
  assert.equal((await receive("audit", 3)).length, 3);
});

test("an unknown consumer group is refused", async () => {
  const consumer = new EventHubConsumerClient("missing", connection, "orders");
  const errors = [];
  const subscription = consumer.subscribe(
    { processEvents: async () => {}, processError: async (err) => errors.push(err) },
    { startPosition: earliestEventPosition, maxWaitTimeInSeconds: 1 },
  );
  await new Promise((r) => setTimeout(r, 2000));
  await subscription.close();
  await consumer.close();
  assert.ok(errors.length > 0);
});

test("a provisioned namespace takes a hand-written localhost connection string", async () => {
  const provisioned = await startEmulator({
    eventHubs: [{
      resourceGroup: "local",
      namespace: "local-hubs",
      keys: { RootManageSharedAccessKey: "SAS_KEY_VALUE" },
      hubs: [{ name: "telemetry", partitionCount: 2, consumerGroups: ["analytics"] }],
    }],
  });
  const local = `Endpoint=sb://localhost:${provisioned.amqpPort}/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true`;
  const producer = new EventHubProducerClient(local, "telemetry");
  try {
    assert.deepEqual((await producer.getEventHubProperties()).partitionIds, ["0", "1"]);
    await producer.sendBatch([{ body: "hello" }], { partitionId: "1" });
    assert.equal((await producer.getPartitionProperties("1")).lastEnqueuedSequenceNumber, 0);
  } finally {
    await producer.close();
    provisioned.stop();
  }
});
