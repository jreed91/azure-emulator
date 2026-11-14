import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { ShareClient, ShareSASPermissions, ShareServiceClient, StorageSharedKeyCredential } from "@azure/storage-file-share";

import { startEmulator } from "./emulator.js";

let emulator;
let service;
const account = "/resourceGroups/files-js/providers/Microsoft.Storage/storageAccounts/filesjs";

before(async () => {
  emulator = await startEmulator();
  await emulator.arm("PUT", "/resourceGroups/files-js", { location: "eastus" }, "2021-04-01");
  await emulator.arm("PUT", account, { location: "eastus", kind: "StorageV2", sku: { name: "Standard_LRS" } }, "2023-05-01");
  const { keys } = await emulator.arm("POST", `${account}/listKeys`, undefined, "2023-05-01");
  const endpoint = (await emulator.arm("GET", account, undefined, "2023-05-01")).properties.primaryEndpoints.file;
  service = new ShareServiceClient(endpoint, new StorageSharedKeyCredential("filesjs", keys[0].value));
});

after(() => emulator?.stop());

test("shares, directories and files", async () => {
  const share = service.getShareClient("docs");
  await share.create({ quota: 5 });
  const directory = share.getDirectoryClient("reports");
  await directory.create();
  const file = directory.getFileClient("a.txt");
  const content = Buffer.from("hello from node");
  await file.create(content.length);
  await file.uploadRange(content, 0, content.length);
  assert.equal((await file.downloadToBuffer()).toString(), "hello from node");
  assert.equal((await file.getProperties()).contentLength, content.length);

  const names = [];
  for await (const item of directory.listFilesAndDirectories()) names.push([item.kind, item.name]);
  assert.deepEqual(names, [["file", "a.txt"]]);

  const { rangeList } = await file.getRangeList();
  assert.deepEqual(rangeList.map((r) => [r.start, r.end]), [[0, content.length - 1]]);

  await file.delete();
  await directory.delete();
  await share.delete();
  const shares = [];
  for await (const item of service.listShares()) shares.push(item.name);
  assert.deepEqual(shares, []);
});

test("file properties, metadata, resize and clearing ranges", async () => {
  const share = service.getShareClient("props");
  await share.create();
  const file = share.rootDirectoryClient.getFileClient("data.bin");
  await file.create(1024, { metadata: { owner: "team" }, fileHttpHeaders: { fileContentType: "application/octet-stream" } });
  await file.uploadRange(Buffer.alloc(512, 1), 0, 512);
  await file.setHttpHeaders({ fileContentType: "text/plain", fileCacheControl: "no-cache" });
  await file.setMetadata({ owner: "other" });
  let properties = await file.getProperties();
  assert.equal(properties.contentType, "text/plain");
  assert.equal(properties.cacheControl, "no-cache");
  assert.deepEqual(properties.metadata, { owner: "other" });

  await file.clearRange(0, 512);
  assert.deepEqual((await file.getRangeList()).rangeList, []);
  await file.resize(10);
  properties = await file.getProperties();
  assert.equal(properties.contentLength, 10);
  assert.deepEqual([...(await file.downloadToBuffer())], Array(10).fill(0));
  await share.delete();
});

test("share quota, prefixes and SAS", async () => {
  const share = service.getShareClient("tuned");
  await share.create({ quota: 1, metadata: { tier: "small" } });
  await share.setQuota(3);
  const properties = await share.getProperties();
  assert.equal(properties.quota, 3);
  assert.deepEqual(properties.metadata, { tier: "small" });

  for (const name of ["log-1.txt", "log-2.txt", "other.txt"]) await share.rootDirectoryClient.getFileClient(name).create(1);
  const logs = [];
  for await (const item of share.rootDirectoryClient.listFilesAndDirectories({ prefix: "log-" })) logs.push(item.name);
  assert.deepEqual(logs, ["log-1.txt", "log-2.txt"]);

  await share.rootDirectoryClient.getFileClient("other.txt").delete();

  const expiresOn = new Date(Date.now() + 3600_000);
  const reader = new ShareClient(share.generateSasUrl({ permissions: ShareSASPermissions.parse("rl"), expiresOn }));
  const listed = [];
  for await (const item of reader.rootDirectoryClient.listFilesAndDirectories()) listed.push(item.name);
  assert.deepEqual(listed, ["log-1.txt", "log-2.txt"]);
  await assert.rejects(reader.rootDirectoryClient.getFileClient("new.txt").create(1), (e) => e.statusCode === 403);
  await share.delete();
});

test("file service properties", async () => {
  const cors = [{ allowedOrigins: "*", allowedMethods: "GET", allowedHeaders: "*", exposedHeaders: "*", maxAgeInSeconds: 9 }];
  await service.setProperties({ cors });
  const properties = await service.getProperties();
  assert.equal(properties.cors[0].maxAgeInSeconds, 9);
  assert.equal(properties.hourMetrics.version, "1.0");
  await service.setProperties({ cors: [] });
});
