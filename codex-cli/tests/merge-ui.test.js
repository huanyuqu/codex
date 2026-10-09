import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { exampleSources, fakeGraphClient } from "./fixtures/graph.js";
import { parseGraphItem } from "../bin/merge-graph.js";
import { JsonRpcClient } from "../bin/merge-rpc.js";

const helper = fileURLToPath(new URL("../bin/merge-ui.js", import.meta.url));

async function runBridge(
  t,
  args,
  {
    cancelAnalysis = false,
    disconnect = false,
    sources = exampleSources(),
    operation,
  } = {},
) {
  const { client } = await fakeGraphClient(t, sources);
  const originals = await Promise.all(
    [...client.threads.values()].map((thread) => readFile(thread.path)),
  );
  const child = spawn(process.execPath, [helper], {
    env: client.env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const lines = createInterface({ input: child.stdout });
  const messages = [];
  const ids = new Set();
  let stderr = "";
  let turn = 0;
  child.stderr.on("data", (data) => {
    stderr += data;
  });
  const send = (message) => {
    if (!child.stdin.destroyed)
      child.stdin.write(JSON.stringify(message) + "\n");
  };
  child.stdin.on("error", () => {});
  const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
  t.after(() => {
    clearTimeout(timer);
    lines.close();
    child.kill();
  });
  lines.on("line", async (line) => {
    const message = JSON.parse(line);
    messages.push(message);
    if (message.type !== "rpc" || !message.message.method) return;
    const request = message.message;
    try {
      assert.match(request.id, /^context-merge-[\da-f-]+-\d+$/);
      assert.ok(
        !ids.has(request.id),
        "unique request IDs on the shared connection",
      );
      ids.add(request.id);
      if (disconnect) return child.stdin.end();
      let result;
      if (request.method === "turn/start") {
        const id = "turn-" + ++turn;
        if (cancelAnalysis) {
          send({ type: "cancel" });
        } else {
          const reading = await client.runTurn(request.params);
          send({
            type: "rpc",
            message: {
              method: "item/completed",
              params: {
                threadId: request.params.threadId,
                turnId: id,
                item: {
                  type: "agentMessage",
                  id: "reading-" + id,
                  phase: "final_answer",
                  text: reading.text,
                },
              },
            },
          });
          send({
            type: "rpc",
            message: {
              method: "turn/completed",
              params: {
                threadId: request.params.threadId,
                turn: { id, status: "completed", items: [] },
              },
            },
          });
        }
        result = { turn: { id, status: "inProgress" } };
      } else if (request.method === "turn/interrupt") result = {};
      else result = await client.request(request.method, request.params);
      send({ type: "rpc", message: { id: request.id, result } });
    } catch (error) {
      send({
        type: "rpc",
        message: {
          id: request.id,
          error: { code: -32000, message: error.message },
        },
      });
    }
  });
  send({
    type: "start",
    operation,
    primaryThreadId: "A",
    args,
    model: "fixture-reader",
  });
  const exitCode = await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", resolve);
  });
  assert.equal(stderr, "");
  const after = await Promise.all(
    sources.map((source) =>
      readFile(client.threads.get(source.thread.id).path),
    ),
  );
  assert.deepEqual(
    after,
    originals,
    "merge must preserve the original branches",
  );
  return {
    client,
    messages,
    exitCode,
    result: messages.find((m) => m.type === "result")?.result,
  };
}

test("TUI bridge merges short branches with semantic synthesis on the existing connection", async (t) => {
  const { client, result, exitCode } = await runBridge(t, [
    "B",
    "C",
    "--name",
    "Two views",
  ]);
  assert.equal(exitCode, 0);
  assert.ok(result.threadId);
  assert.equal(result.modelCalls, 1);
  assert.ok(result.reading);
  assert.equal(client.threads.get(result.threadId).name, "Two views");
  assert.ok(
    !client.calls.some((call) =>
      ["initialize", "thread/resume"].includes(call.method),
    ),
  );
  const injection = client.calls.find(
    (call) => call.method === "thread/inject_items",
  );
  const capsule = injection.params.items.map(parseGraphItem).find(Boolean);
  assert.equal(capsule.branches.length, 2);
  assert.equal(capsule.parents.length, 3);
});

test("TUI bridge forwards streamed analysis and preserves semantic graph context", async (t) => {
  const { client, result, exitCode } = await runBridge(t, [
    "B",
    "C",
    "--mode",
    "summary",
  ]);
  assert.equal(exitCode, 0);
  assert.ok(result.threadId);
  assert.equal(
    client.calls.filter((call) => call.method === "model").length,
    3,
  );
  assert.ok(
    client.calls.some(
      (call) =>
        call.method === "thread/unsubscribe" &&
        call.params.threadId.startsWith("analysis-"),
    ),
  );
});

test("TUI merge options without IDs request the branch picker", async (t) => {
  const { client, result, exitCode } = await runBridge(t, [
    "--goal",
    "Compare the views",
  ]);
  assert.equal(exitCode, 0);
  assert.deepEqual(result, {
    selectBranches: true,
    args: ["--goal", "Compare the views"],
  });
  assert.equal(client.calls.length, 0);
});

test("TUI merge dry run performs no inference or thread creation", async (t) => {
  const { client, result, exitCode } = await runBridge(t, [
    "B",
    "C",
    "--mode",
    "summary",
    "--dry-run",
  ]);
  assert.equal(exitCode, 0);
  assert.equal(result.dryRun, true);
  assert.ok(
    !client.calls.some((call) =>
      ["model", "thread/start", "thread/fork", "thread/inject_items"].includes(
        call.method,
      ),
    ),
  );
});

test("TUI merge rejects duplicate primary and CLI-only flags before RPC", async (t) => {
  for (const args of [
    ["A", "B"],
    ["B", "--resume"],
    ["B", "--cd", "/tmp"],
    ["B", "--config", "model=x"],
    ["B", "--semantic"],
  ]) {
    const { client, messages, exitCode } = await runBridge(t, args);
    assert.equal(exitCode, 1);
    assert.ok(messages.some((m) => m.type === "error"));
    assert.equal(client.calls.length, 0);
  }
});

test("TUI tree helper lists related branches through read-only RPC and exits", async (t) => {
  const { client, result, exitCode } = await runBridge(t, [], {
    operation: "tree",
  });
  assert.equal(exitCode, 0);
  assert.deepEqual(
    result.rows.map((r) => r.id),
    ["A", "B", "C"],
  );
  assert.equal(result.rows[0].current, true);
  assert.ok(
    client.calls.every((c) =>
      ["thread/list", "thread/read"].includes(c.method),
    ),
  );
});

test("TUI merge cancellation interrupts analysis and creates no merged thread", async (t) => {
  const { client, messages, exitCode, result } = await runBridge(
    t,
    ["B", "C", "--mode", "summary"],
    { cancelAnalysis: true },
  );
  assert.equal(exitCode, 1);
  assert.equal(result, undefined);
  assert.match(messages.find((m) => m.type === "error").message, /cancelled/);
  assert.ok(
    !client.calls.some((call) =>
      ["thread/fork", "thread/inject_items"].includes(call.method),
    ),
  );
  assert.ok(client.calls.some((call) => call.method === "thread/unsubscribe"));
  assert.ok(messages.some((m) => m.message?.method === "turn/interrupt"));
});

test("TUI helper exits promptly when its host connection disappears", async (t) => {
  const { messages, exitCode } = await runBridge(t, ["B", "C"], {
    disconnect: true,
  });
  assert.equal(exitCode, 1);
  assert.ok(messages.some((m) => m.type === "error"));
});

test("shared RPC cancellation before a turn reply interrupts the eventual turn and releases listeners", async () => {
  const abort = new AbortController();
  const sent = [];
  const client = new JsonRpcClient({
    write: (message) => sent.push(message),
    signal: abort.signal,
    idPrefix: "merge-",
  });
  const analysis = client.runTurn({ threadId: "analysis" });
  abort.abort();
  await assert.rejects(analysis, /cancelled/);
  assert.equal(client.notifications.size, 0);
  client.receive(
    JSON.stringify({
      id: sent[0].id,
      result: { turn: { id: "late", status: "inProgress" } },
    }),
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(sent[1].method, "turn/interrupt");
  assert.equal(sent[1].params.turnId, "late");
  client.receive(JSON.stringify({ id: sent[1].id, result: {} }));
  await assert.rejects(client.request("thread/fork", {}), /cancelled/);
  assert.equal(client.pending.size, 0);
});
