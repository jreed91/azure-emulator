import assert from "node:assert/strict";
import { Readable } from "node:stream";
import { after, before, test } from "node:test";

import { BlobServiceClient, StorageSharedKeyCredential } from "@azure/storage-blob";

import { startEmulator } from "./emulator.js";

let emulator;
let service;
const account = "jsblobs";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/blob-js", { location: "eastus" }, "2021-04-01");
  const path = `/resourceGroups/blob-js/providers/Microsoft.Storage/storageAccounts/${account}`;
  const created = await emulator.arm("PUT", path, { location: "eastus", kind: "StorageV2", sku: { name: "Standard_LRS" } }, "2023-01-01");
  const { keys } = await emulator.arm("POST", `${path}/listKeys`, undefined, "2023-01-01");
  service = new BlobServiceClient(created.properties.primaryEndpoints.blob, new StorageSharedKeyCredential(account, keys[0].value));
});

after(() => emulator?.stop());

test("uploadStream and uploadData send blocks and commit them", async () => {
  const container = service.getContainerClient("streams");
  await container.create();
  const data = Buffer.alloc(10_000, 0);
  for (let i = 0; i < data.length; i++) data[i] = i % 251;

  const streamed = container.getBlockBlobClient("streamed.bin");
  await streamed.uploadStream(Readable.from([data.subarray(0, 4000), data.subarray(4000)]), 1024, 4, {
    blobHTTPHeaders: { blobContentType: "application/x-test" },
    metadata: { source: "stream" },
  });
  assert.deepEqual(await streamed.downloadToBuffer(), data);
  const { committedBlocks } = await streamed.getBlockList("committed");
  assert.equal(committedBlocks.length, 10);
  const properties = await streamed.getProperties();
  assert.equal(properties.contentType, "application/x-test");
  assert.deepEqual(properties.metadata, { source: "stream" });

  const uploaded = container.getBlockBlobClient("data.bin");
  await uploaded.uploadData(data, { blockSize: 2048, maxSingleShotSize: 2048, concurrency: 3 });
  assert.deepEqual(await uploaded.downloadToBuffer(), data);
  assert.equal((await uploaded.getBlockList("committed")).committedBlocks.length, 5);
});

test("leases, snapshots and copies", async () => {
  const container = service.getContainerClient("features");
  await container.create();
  const blob = container.getBlockBlobClient("doc.txt");
  await blob.upload("v1", 2);

  const leases = blob.getBlobLeaseClient();
  await leases.acquireLease(15);
  await assert.rejects(blob.upload("v2", 2), (e) => e.statusCode === 412 && e.code === "LeaseIdMissing");
  await blob.upload("v2", 2, { conditions: { leaseId: leases.leaseId } });
  const leased = await blob.getProperties();
  assert.deepEqual([leased.leaseState, leased.leaseStatus, leased.leaseDuration], ["leased", "locked", "fixed"]);
  await leases.releaseLease();

  const { snapshot } = await blob.createSnapshot();
  await blob.upload("v3", 2);
  const old = await blob.withSnapshot(snapshot).downloadToBuffer();
  assert.equal(old.toString(), "v2");
  const names = [];
  for await (const item of container.listBlobsFlat({ includeSnapshots: true })) names.push([item.name, item.snapshot ?? null]);
  assert.deepEqual(names, [["doc.txt", snapshot], ["doc.txt", null]]);

  const copy = container.getBlobClient("copy.txt");
  const poller = await copy.beginCopyFromURL(blob.url);
  const result = await poller.pollUntilDone();
  assert.equal(result.copyStatus, "success");
  assert.equal((await copy.downloadToBuffer()).toString(), "v3");
  assert.equal((await copy.getProperties()).copyStatus, "success");

  const fromUrl = container.getBlockBlobClient("from-url.txt");
  await fromUrl.syncUploadFromURL(blob.withSnapshot(snapshot).url);
  assert.equal((await fromUrl.downloadToBuffer()).toString(), "v2");

  await assert.rejects(blob.delete(), (e) => e.statusCode === 409 && e.code === "SnapshotsPresent");
  await blob.delete({ deleteSnapshots: "include" });
});
