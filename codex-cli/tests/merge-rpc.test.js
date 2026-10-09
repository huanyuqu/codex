import assert from "node:assert/strict";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import test from "node:test";
import { AppServerClient, RpcError } from "../bin/merge-rpc.js";

async function server(t) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-merge-rpc-"));
  const file = path.join(dir, "fake-server");
  await writeFile(
    file,
    `#!/usr/bin/env node
const readline = require('node:readline');
const lines = readline.createInterface({input:process.stdin});
lines.on('line', line => {
  const request = JSON.parse(line);
  if(request.id === undefined) return;
  if(request.method === 'timeout') return;
  if(request.method === 'invalid') return process.stdout.write('null\\n');
  if(request.method === 'exit') return process.exit(2);
  if(request.method === 'failure') return process.stdout.write(JSON.stringify({id:request.id,error:{code:-32000,message:'expected failure',data:{why:'test'}}})+'\\n');
  const delay = request.params?.delay ?? 0;
  setTimeout(() => process.stdout.write(JSON.stringify({id:request.id,result:request.params ?? {}})+'\\n'),delay);
});
lines.on('close',()=>process.exit(0));
`,
  );
  await chmod(file, 0o700);
  const client = new AppServerClient(file, { timeoutMs: 1000 });
  t.after(async () => {
    await client.close();
    await rm(dir, { recursive: true, force: true });
  });
  return client;
}

test("RPC correlates out-of-order responses", async (t) => {
  const client = await server(t);
  await client.initialize();
  const replies = await Promise.all([
    client.request("slow", { delay: 30, answer: "slow" }),
    client.request("fast", { answer: "fast" }),
  ]);
  assert.deepEqual(
    replies.map((r) => r.answer),
    ["slow", "fast"],
  );
  assert.equal(client.pending.size, 0);
});

test("RPC errors retain method and structured server error", async (t) => {
  const client = await server(t);
  await assert.rejects(client.request("failure", {}), (error) => {
    assert.ok(error instanceof RpcError);
    assert.equal(error.code, -32000);
    assert.deepEqual(error.data, { why: "test" });
    assert.match(error.message, /failure: expected failure/);
    return true;
  });
  assert.equal(client.pending.size, 0);
});

test("RPC rejects timeouts and clears pending requests", async (t) => {
  const client = await server(t);
  await client.initialize();
  client.timeoutMs = 30;
  await assert.rejects(
    client.request("timeout", {}),
    /Timed out waiting for timeout/,
  );
  assert.equal(client.pending.size, 0);
});

test("RPC invalid responses reject all pending requests", async (t) => {
  const client = await server(t);
  const results = await Promise.allSettled([
    client.request("invalid", {}),
    client.request("timeout", {}),
  ]);
  assert.ok(
    results.every(
      (r) =>
        r.status === "rejected" && /invalid response/.test(r.reason.message),
    ),
  );
  assert.equal(client.pending.size, 0);
});

test("RPC reports a server crash without hanging", async (t) => {
  const client = await server(t);
  await assert.rejects(client.request("exit", {}), /closed \(2\)/);
  assert.equal(client.pending.size, 0);
});

test("RPC reports a missing native executable without hanging", async () => {
  const client = new AppServerClient(
    path.join(os.tmpdir(), "codex-merge-missing-executable"),
  );
  try {
    await assert.rejects(client.initialize(), /Cannot start Codex app-server/);
  } finally {
    await client.close();
  }
});
