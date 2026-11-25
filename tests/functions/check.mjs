// Reads what the Functions host left in storage, over the same path-style
// connection string, and checks it. `secrets` prints the host's master key.
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(new URL("../sdk-js/package.json", import.meta.url));
const { BlobServiceClient } = require("@azure/storage-blob");
const { QueueServiceClient } = require("@azure/storage-queue");
const { TableClient } = require("@azure/data-tables");

const [command, connectionString] = process.argv.slice(2);
const blobs = BlobServiceClient.fromConnectionString(connectionString);

async function names(iterable) {
  const out = [];
  for await (const item of iterable) out.push(item.name);
  return out.sort();
}

if (command === "secrets") {
  const secrets = blobs.getContainerClient("azure-webjobs-secrets");
  const host = (await names(secrets.listBlobsFlat())).find((name) => name.endsWith("/host.json"));
  console.log(JSON.parse((await secrets.getBlobClient(host).downloadToBuffer()).toString()).masterKey.value);
} else if (command === "upload") {
  const inbox = blobs.getContainerClient("inbox");
  await inbox.createIfNotExists();
  await inbox.getBlockBlobClient("hello.txt").upload("hi", 2);
} else if (command === "check") {
  const hosts = blobs.getContainerClient("azure-webjobs-hosts");
  const locks = [];
  for await (const blob of hosts.listBlobsFlat({ prefix: "locks/" })) locks.push([blob.name.split("/").pop(), blob.properties.leaseState]);
  assert.deepEqual(locks.filter(([, state]) => state === "leased").map(([name]) => name).sort(),
    ["Host.Functions.Tick.Listener", "WebJobs.Internal.Blobs.Listener", "host"], "the host holds its lock leases");
  assert.ok((await names(hosts.listBlobsFlat({ prefix: "blobreceipts/" }))).some((n) => n.endsWith("/inbox/hello.txt")), "a blob receipt");
  assert.deepEqual(await names(blobs.getContainerClient("outputs").listBlobsFlat()), ["one.txt", "two.txt"]);

  const queues = QueueServiceClient.fromConnectionString(connectionString);
  const count = async (name) => (await queues.getQueueClient(name).getProperties()).approximateMessagesCount;
  assert.equal(await count("work-items"), 0);
  assert.equal(await count("work-items-poison"), 1, "the failing message is moved to the poison queue");
  assert.equal(await count("done-items"), 2);
  assert.equal(await count("blob-seen"), 1);

  const table = TableClient.fromConnectionString(connectionString, "processed", { allowInsecureConnection: true });
  assert.deepEqual((await names({ async *[Symbol.asyncIterator]() { for await (const e of table.listEntities()) yield { name: e.rowKey }; } })), ["one", "two"]);
  console.log("storage state as expected");
} else {
  throw new Error(`unknown command ${command}`);
}
