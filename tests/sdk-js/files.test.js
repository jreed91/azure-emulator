import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { ShareServiceClient, StorageSharedKeyCredential } from "@azure/storage-file-share";

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
