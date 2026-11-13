// Starts one emulator for the test process, on free ports, and talks to its
// control plane with plain fetch.
import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const TENANT = "11111111-1111-1111-1111-111111111111";
const SUBSCRIPTION = "00000000-0000-0000-0000-000000000000";
const binary = process.env.AZURE_EMULATOR_BIN ?? resolve(import.meta.dirname, "../../target/debug/azure-emulator");

function freePort() {
  return new Promise((done, fail) => {
    const server = createServer().listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => done(port));
    });
    server.on("error", fail);
  });
}

export async function startEmulator() {
  const [port, amqpPort] = [await freePort(), await freePort()];
  const home = mkdtempSync(join(tmpdir(), "azure-emulator-"));
  const child = spawn(binary, ["--http", "--port", String(port), "--amqp-port", String(amqpPort)], {
    env: { ...process.env, AZURE_EMULATOR_HOME: home },
    stdio: "ignore",
  });
  const base = `http://localhost:${port}`;
  for (let i = 0; ; i++) {
    try {
      await fetch(`${base}/${TENANT}/v2.0/.well-known/openid-configuration`);
      break;
    } catch (error) {
      if (i > 100) throw error;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  const form = new URLSearchParams({ grant_type: "client_credentials", client_id: "a", client_secret: "b", scope: `${base}/.default` });
  const { access_token: token } = await (await fetch(`${base}/${TENANT}/oauth2/v2.0/token`, { method: "POST", body: form })).json();

  async function arm(method, path, body, apiVersion = "2022-10-01-preview") {
    const response = await fetch(`${base}/subscriptions/${SUBSCRIPTION}${path}?api-version=${apiVersion}`, {
      method,
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: body && JSON.stringify(body),
    });
    const text = await response.text();
    if (response.status >= 400) throw new Error(`${method} ${path}: ${response.status} ${text}`);
    return text ? JSON.parse(text) : null;
  }

  return { base, amqpPort, arm, stop: () => child.kill() };
}
