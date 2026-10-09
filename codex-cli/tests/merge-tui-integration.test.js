import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn, spawnSync } from "node:child_process";
import { createInterface } from "node:readline";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { AppServerClient } from "../bin/merge-rpc.js";
import { hash, loadSnapshot } from "../bin/merge-history.js";
import { parseGraphItem } from "../bin/merge-graph.js";
import { readEvidence } from "../bin/merge-evidence.js";
import { fixtureReading, message } from "./fixtures/graph.js";

import test from "node:test";
import { fileURLToPath } from "node:url";

const binary = process.env.CODEX_TUI_MERGE_TEST_BINARY;
const hasPty =
  process.platform !== "win32" &&
  spawnSync("python3", ["--version"]).status === 0;

test(
  "native /merge and /tree: default synthesis, branch switching, continuation and cancellation",
  { skip: !binary || !hasPty, timeout: 120000 },
  async (t) => {
    const root = path.resolve(fileURLToPath(new URL("../..", import.meta.url)));
    const home = await mkdtemp(path.join(os.tmpdir(), "codex-tui-merge-"));
    const calls = [];
    let holdAnalysis = false;
    let client;
    let terminal;
    const server = createServer(async (req, res) => {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      calls.push(body);
      const prompt = body.input
        .flatMap((item) => item.content ?? [])
        .find((part) =>
          part.text?.startsWith("Read the historical conversation material"),
        );
      let text = "Fixture response: both viewpoints retain their scopes.";
      if (prompt) {
        const task = JSON.parse(prompt.text.split("\n").at(-1));
        text = JSON.stringify(fixtureReading(task));
      }
      res.writeHead(200, { "content-type": "text/event-stream" });
      const emit = (event) =>
        res.write(
          "event: " + event.type + "\ndata: " + JSON.stringify(event) + "\n\n",
        );
      const id = "response-" + calls.length;
      emit({
        type: "response.created",
        response: { id, object: "response", status: "in_progress", output: [] },
      });
      if (prompt && holdAnalysis) return;
      const item = {
        type: "message",
        id: "msg-" + calls.length,
        role: "assistant",
        phase: "final_answer",
        content: [{ type: "output_text", text, annotations: [] }],
      };
      emit({
        type: "response.output_item.added",
        output_index: 0,
        item: { ...item, content: [] },
      });
      emit({
        type: "response.output_text.delta",
        output_index: 0,
        item_id: item.id,
        content_index: 0,
        delta: text,
      });
      emit({ type: "response.output_item.done", output_index: 0, item });
      emit({
        type: "response.completed",
        response: {
          id,
          object: "response",
          status: "completed",
          output: [item],
          usage: { input_tokens: 100, output_tokens: 30, total_tokens: 130 },
        },
      });
      res.end();
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const url = "http://127.0.0.1:" + server.address().port + "/v1";
    const env = {
      ...process.env,
      CODEX_HOME: home,
      TERM: "xterm-256color",
      CODEX_CONTEXT_GRAPH_BINARY: binary,
      CODEX_CONTEXT_MERGE_NO_DAEMON: "1",
    };
    const config = `model = "graph-fixture"
model_provider = "graph_fixture"
model_reasoning_effort = "low"
model_context_window = 32768
approval_policy = "never"
sandbox_mode = "read-only"
[model_providers.graph_fixture]
name = "Graph fixture"
base_url = "${url}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
[projects."${root}"]
trust_level = "trusted"
`;
    await writeFile(path.join(home, "config.toml"), config);

    function startTerminal(threadId) {
      const child = spawn(
        "python3",
        [
          fileURLToPath(new URL("./fixtures/tui-pty.py", import.meta.url)),
          process.execPath,
          path.join(root, "codex-cli/bin/codex.js"),
          "--no-alt-screen",
          "resume",
          threadId,
        ],
        { env, stdio: ["pipe", "pipe", "pipe"], cwd: root },
      );
      const lines = createInterface({ input: child.stdout });
      let raw = "";
      let stderr = "";
      child.stderr.on("data", (chunk) => {
        stderr += chunk;
      });
      lines.on("line", (line) => {
        const event = JSON.parse(line);
        if (event.type === "output")
          raw += Buffer.from(event.data, "base64").toString("utf8");
      });
      const screen = () =>
        raw
          .replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/g, "")
          .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "");
      const input = (data) =>
        child.stdin.write(
          JSON.stringify({
            type: "input",
            data: Buffer.from(data).toString("base64"),
          }) + "\n",
        );
      const command = async (text) => {
        input("\x1b[200~" + text + "\x1b[201~");
        await delay(180);
        input("\r");
      };
      const wait = async (predicate, timeout = 20000) => {
        const limit = Date.now() + timeout;
        while (!predicate(screen())) {
          if (Date.now() > limit) {
            await writeFile(
              path.join(home, "tui-smoke-last-screen.txt"),
              screen(),
            );
            await writeFile(path.join(home, "tui-smoke-last-raw.txt"), raw);
            throw new Error(
              "Timed out waiting for terminal state\n" +
                screen().slice(-6000) +
                stderr,
            );
          }
          await delay(50);
        }
      };
      return {
        child,
        lines,
        screen,
        input,
        command,
        wait,
        save: async (name) => {
          await writeFile(path.join(home, "" + name + ".txt"), screen());
        },
        stop: async () => {
          child.stdin.write(JSON.stringify({ type: "stop" }) + "\n");
          await delay(150);
          child.kill();
          lines.close();
        },
      };
    }

    const report = { binary, cases: [] };
    try {
      client = new AppServerClient(binary, { env });
      await client.initialize();
      const { thread: a } = await client.request("thread/start", {
        cwd: root,
        historyMode: "legacy",
      });
      await client.runTurn({
        threadId: a.id,
        input: [
          { type: "text", text: "A1: Compare restrained and forceful prose." },
        ],
      });
      const { thread: b } = await client.request("thread/fork", {
        threadId: a.id,
        excludeTurns: true,
      });
      await client.request("thread/name/set", {
        threadId: b.id,
        name: "Branch B restrained prose",
      });
      await client.request("thread/inject_items", {
        threadId: b.id,
        items: [
          message(
            "B2: Restrained prose is a suggestion, not a final choice.",
            "b2",
            "assistant",
          ),
        ],
      });
      await client.runTurn({
        threadId: a.id,
        input: [
          {
            type: "text",
            text: "A2: Preserve reader knowledge and uncertainty.",
          },
        ],
      });
      const { thread: c } = await client.request("thread/fork", {
        threadId: a.id,
        excludeTurns: true,
      });
      await client.request("thread/name/set", {
        threadId: c.id,
        name: "Branch C forceful prose",
      });
      await client.request("thread/inject_items", {
        threadId: c.id,
        items: [
          message(
            "C3: Forceful prose is another candidate; retain uncertainty.",
            "c3",
            "assistant",
          ),
        ],
      });
      await client.runTurn({
        threadId: a.id,
        input: [
          {
            type: "text",
            text: "A3: Keep both candidates without choosing one.",
          },
        ],
      });
      const ids = [a.id, b.id, c.id];
      const originals = await Promise.all(
        ids.map(async (id) => {
          const snapshot = await loadSnapshot(client, id);
          return {
            id,
            path: snapshot.thread.path,
            sha: hash(await readFile(snapshot.thread.path)),
            content: await readFile(snapshot.thread.path, "utf8"),
            records: snapshot.records.map((r) => r.json),
          };
        }),
      );
      await client.close();

      terminal = startTerminal(a.id);
      await terminal.wait((screen) => screen.includes("graph-fixture"));
      await delay(700);
      const beforeTree = terminal.screen().length;
      const beforeTreeCalls = calls.length;
      await terminal.command("/tree");
      await terminal.wait((screen) => {
        const tree = screen.slice(beforeTree);
        return (
          tree.includes("Conversation tree") &&
          tree.includes("Branch B") &&
          tree.includes("Branch C")
        );
      });
      terminal.input("\x1b");
      await delay(200);
      assert.equal(calls.length, beforeTreeCalls, "tree does not call a model");
      await terminal.command("/merge --help");
      await terminal.wait((screen) => screen.includes("Usage: /merge"));
      await terminal.command("/merge " + b.id + " " + c.id + " --dry-run");
      await terminal.wait((screen) => screen.includes("Merge preview:"));
      const beforePickerCalls = calls.length;
      await terminal.command("/merge");
      await terminal.wait((screen) =>
        screen.includes("Merge branches into this chat"),
      );
      assert.ok(terminal.screen().includes("Branch B"));
      assert.ok(terminal.screen().includes("Branch C"));
      for (const key of [" ", "\x1b[B", " ", "\r"]) {
        terminal.input(key);
        await delay(100);
      }
      await terminal.wait((screen) =>
        screen.includes("Branches merged. You can continue here."),
      );
      assert.equal(
        calls.length,
        beforePickerCalls + 1,
        "short merge synthesizes the branches by default",
      );
      const rolloutMark = terminal.screen().length;
      await terminal.command("/rollout");
      await terminal.wait((screen) =>
        screen.slice(rolloutMark).includes("rollout-"),
      );
      const mergedId = terminal
        .screen()
        .slice(rolloutMark)
        .match(/[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}/)?.[0];
      assert.ok(mergedId && !ids.includes(mergedId));
      await terminal.command("Continue comparing both prose candidates.");
      await terminal.wait(() => calls.length > beforePickerCalls);
      await terminal.wait((screen) => screen.includes("Fixture response"));
      await delay(500);
      await terminal.save("tui-smoke-picker");
      await writeFile(
        path.join(home, "tui-smoke-requests.json"),
        JSON.stringify(calls, null, 2),
      );
      const continuation = JSON.stringify(
        calls.find((body) =>
          JSON.stringify(body.input).includes(
            "Continue comparing both prose candidates.",
          ),
        ),
      );
      assert.ok(continuation.includes("Codex conversation graph merge"));
      assert.ok(continuation.includes("B2: Restrained prose"));
      assert.ok(continuation.includes("C3: Forceful prose"));
      const treeMark = terminal.screen().length;
      const beforeSwitchCalls = calls.length;
      await terminal.command("/tree");
      await terminal.wait((screen) =>
        screen.slice(treeMark).includes("Conversation tree"),
      );
      assert.ok(terminal.screen().slice(treeMark).includes("merge ←"));
      // Search the exact branch ID in the live native picker and accept it.
      terminal.input(b.id);
      await delay(200);
      terminal.input("\r");
      await delay(700);
      const verifyRollout = async (id) => {
        const mark = terminal.screen().length;
        await terminal.command("/rollout");
        await terminal.wait((screen) => screen.slice(mark).includes(id));
      };
      await verifyRollout(b.id);
      await terminal.command("/tree " + c.id);
      await delay(700);
      await verifyRollout(c.id);
      await terminal.command("/tree " + mergedId);
      await delay(700);
      await verifyRollout(mergedId);
      assert.equal(
        calls.length,
        beforeSwitchCalls,
        "tree browsing and switching do not call a model",
      );
      await terminal.save("tui-smoke-picker");
      report.cases.push({
        name: "tree before merge, help, preview, default synthesis, continuation, tree picker and source-to-merge switching",
        passed: true,
        modelCallsForMerge: 1,
      });
      await terminal.command("/quit");
      await delay(1000);
      await terminal.stop();

      terminal = startTerminal(a.id);
      await terminal.wait((screen) => screen.includes("graph-fixture"));
      await delay(700);
      const beforeSummary = calls.length;
      await terminal.command(
        "/merge " +
          b.id +
          " " +
          c.id +
          ' --mode summary --name "Combined semantic"',
      );
      await terminal.wait(
        (screen) => screen.includes("Branches merged. You can continue here."),
        30000,
      );
      assert.equal(calls.length - beforeSummary, 3);
      await terminal.save("tui-smoke-semantic");
      report.cases.push({
        name: "inline IDs, summary and semantic reading, switch",
        passed: true,
        modelCallsForMerge: 3,
      });
      await terminal.command("/quit");
      await delay(1000);
      await terminal.stop();

      terminal = startTerminal(a.id);
      await terminal.wait((screen) => screen.includes("graph-fixture"));
      await delay(700);
      holdAnalysis = true;
      const beforeCancel = calls.length;
      await terminal.command(
        "/merge " +
          b.id +
          " " +
          c.id +
          ' --mode summary --goal "fresh cancellation probe"',
      );
      await terminal.wait(() => calls.length > beforeCancel, 15000);
      terminal.input("\x1b");
      await terminal.wait(
        (screen) => screen.includes("Conversation merge cancelled."),
        20000,
      );
      holdAnalysis = false;
      const beforeRollout = terminal.screen().length;
      await terminal.command("/rollout");
      await terminal.wait((screen) =>
        screen.slice(beforeRollout).includes(a.id),
      );
      await terminal.save("tui-smoke-cancel");
      report.cases.push({
        name: "Esc during inference keeps primary and releases analyses",
        passed: true,
      });
      await terminal.command("/quit");
      await delay(1000);
      await terminal.stop();

      client = new AppServerClient(binary, { env });
      await client.initialize();
      for (const source of originals) {
        const after = await readFile(source.path, "utf8");
        if (hash(after) !== source.sha) {
          await writeFile(
            path.join(home, "tui-smoke-source-before.jsonl"),
            source.content,
          );
          await writeFile(
            path.join(home, "tui-smoke-source-after.jsonl"),
            after,
          );
        }
        const snapshot = await loadSnapshot(client, source.id);
        assert.deepEqual(
          snapshot.records.map((r) => r.json),
          source.records,
          source.id + " context",
        );
      }
      const threads = await client.request("thread/list", {
        modelProviders: [],
        limit: 100,
      });
      const merged = [];
      for (const thread of threads.data) {
        if (ids.includes(thread.id)) continue;
        const snapshot = await loadSnapshot(client, thread.id);
        const capsule = snapshot.records
          .map((r) => parseGraphItem(JSON.parse(r.json)))
          .find(Boolean);
        if (capsule) {
          const evidence = await readEvidence(capsule.evidence);
          assert.equal(
            evidence.graph.nodes.find(
              (n) => n.id === evidence.graph.mergeNodeId,
            ).parents.length,
            3,
          );
          merged.push(thread.id);
        }
      }
      assert.equal(merged.length, 2);
      report.sourceContextsUnchanged = true;
      report.savedMerges = merged;
      report.totalModelRequests = calls.length;
      await writeFile(
        path.join(home, "tui-smoke-report.json"),
        JSON.stringify(report, null, 2) + "\n",
      );
      t.diagnostic(JSON.stringify(report));
    } finally {
      if (terminal) await terminal.stop().catch(() => {});
      if (client) await client.close();
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
      await rm(home, { recursive: true, force: true });
    }
  },
);
