import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { execFile } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import test from "node:test";
import { AppServerClient } from "../bin/merge-rpc.js";
import { mergeThreads as mergeThreadsImpl } from "../bin/merge-context.js";
// These regression cases isolate graph projection from cross-branch synthesis.
const mergeThreads = (client, options) =>
  mergeThreadsImpl(client, { semantic: false, ...options });
import { hash, loadSnapshot } from "../bin/merge-history.js";
import { expandSnapshot, readEvidence } from "../bin/merge-evidence.js";
import { parseGraphItem } from "../bin/merge-graph.js";
import { fixtureReading, message } from "./fixtures/graph.js";

const binary = process.env.CODEX_MERGE_TEST_BINARY;
const exec = promisify(execFile);
const launcher = fileURLToPath(new URL("../bin/codex.js", import.meta.url));
const image = {
  type: "input_image",
  image_url:
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
};

async function providerFixture() {
  const requests = [];
  const dialogueRequests = [];
  let invalid = false;
  const server = createServer(async (req, res) => {
    try {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      const prompt = body.input
        .flatMap((item) => item.content ?? [])
        .find((part) =>
          part.text?.startsWith("Read the historical conversation material"),
        );
      let text = "主线保留两种写作风格和各自的限定条件。";
      if (prompt) {
        requests.push(body);
        const task = JSON.parse(prompt.text.split("\n").at(-1));
        const reading = fixtureReading(task);
        if (invalid) reading.refs = ["invented-node"];
        text = JSON.stringify(reading);
      } else dialogueRequests.push(body);
      const id = "resp_graph_" + requests.length;
      const item = {
        type: "message",
        id: "msg_graph_" + Math.random().toString(16).slice(2),
        role: "assistant",
        phase: "final_answer",
        content: [{ type: "output_text", text, annotations: [] }],
      };
      res.writeHead(200, { "content-type": "text/event-stream" });
      const send = (event) =>
        res.write(
          "event: " + event.type + "\ndata: " + JSON.stringify(event) + "\n\n",
        );
      send({
        type: "response.created",
        response: { id, object: "response", status: "in_progress", output: [] },
      });
      send({
        type: "response.output_item.added",
        output_index: 0,
        item: { ...item, content: [] },
      });
      send({
        type: "response.output_text.delta",
        item_id: item.id,
        output_index: 0,
        content_index: 0,
        delta: text,
      });
      send({ type: "response.output_item.done", output_index: 0, item });
      send({
        type: "response.completed",
        response: {
          id,
          object: "response",
          status: "completed",
          output: [item],
          usage: { input_tokens: 100, output_tokens: 100, total_tokens: 200 },
        },
      });
      res.end();
    } catch (error) {
      res.writeHead(500);
      res.end(JSON.stringify({ error: { message: error.message } }));
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    requests,
    dialogueRequests,
    invalidate: () => {
      invalid = true;
    },
    url: "http://127.0.0.1:" + server.address().port + "/v1",
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(resolve);
      }),
  };
}

for (const historyMode of ["legacy", "paginated"])
  test(
    "native graph merge (" +
      historyMode +
      "): staggered forks, long branch, images, restart and incremental DAG",
    { skip: !binary, timeout: 120000 },
    async (t) => {
      const home = await mkdtemp(path.join(os.tmpdir(), "codex-graph-native-"));
      const env = { ...process.env, CODEX_HOME: home };
      const provider = await providerFixture();
      const config = [
        'model="graph-fixture"',
        'model_provider="graph_fixture"',
        'model_providers.graph_fixture.name="Graph fixture"',
        'model_providers.graph_fixture.base_url="' + provider.url + '"',
        'model_providers.graph_fixture.wire_api="responses"',
        "model_providers.graph_fixture.requires_openai_auth=false",
        "model_providers.graph_fixture.supports_websockets=false",
        'model_reasoning_effort="low"',
        "model_context_window=16384",
      ];
      let client = new AppServerClient(binary, { env, config });
      t.after(async () => {
        await client.close();
        await provider.close();
        await rm(home, { recursive: true, force: true });
      });
      await client.initialize();
      const { thread: a } = await client.request("thread/start", {
        cwd: process.cwd(),
        historyMode,
      });
      await client.runTurn(
        {
          threadId: a.id,
          input: [{ type: "text", text: "A1：比较两种写作风格。" }],
        },
        { timeoutMs: 15000 },
      );
      const a1 = await loadSnapshot(client, a.id);
      const { thread: b } = await client.request("thread/fork", {
        threadId: a.id,
        excludeTurns: true,
      });
      await client.request("thread/inject_items", {
        threadId: b.id,
        items: [
          message("B偏向克制表达。", "b-question"),
          message(
            "保留人物归属和未解决分歧。".repeat(1800),
            "b-long",
            "assistant",
          ),
        ],
      });
      await client.runTurn(
        {
          threadId: a.id,
          input: [{ type: "text", text: "A2：进一步考虑读者和语气。" }],
        },
        { timeoutMs: 15000 },
      );
      const a2 = await loadSnapshot(client, a.id);
      const { thread: c } = await client.request("thread/fork", {
        threadId: a.id,
        excludeTurns: true,
      });
      const question = message(
        "C提出更有力的表达，但仍需限定条件。",
        "c-question",
      );
      question.content.push(image);
      await client.request("thread/inject_items", {
        threadId: c.id,
        items: [
          question,
          message("这只是另一种候选风格。", "c-answer", "assistant"),
        ],
      });
      await client.runTurn(
        {
          threadId: a.id,
          input: [{ type: "text", text: "A3：两种候选都保留。" }],
        },
        { timeoutMs: 15000 },
      );
      const threadIds = [a.id, b.id, c.id];
      const before = await Promise.all(
        threadIds.map(async (id) => {
          const snapshot = await loadSnapshot(client, id);
          return {
            path: snapshot.thread.path,
            sha: hash(await readFile(snapshot.thread.path)),
          };
        }),
      );
      const preview = await mergeThreads(client, {
        threadIds,
        dryRun: true,
        maxInputBytes: 7000,
      });
      assert.equal(preview.analysisPlanned, true);
      assert.equal(provider.requests.length, 0);
      const result = await mergeThreads(client, {
        threadIds,
        maxInputBytes: 7000,
      });
      assert.deepEqual(
        result.presentations.map((v) => v.presentation),
        ["summary", "inline"],
      );
      assert.ok(result.modelCalls > 1);
      assert.equal(result.importedImages, 1);
      assert.ok(result.estimatedContextTokens <= result.budget.inputLimit);
      for (const original of before)
        assert.equal(hash(await readFile(original.path)), original.sha);
      const archive = await readEvidence(result.evidence);
      const bGraph = archive.graph.branches.find(
        (branch) => branch.threadId === b.id,
      );
      const cGraph = archive.graph.branches.find(
        (branch) => branch.threadId === c.id,
      );
      assert.equal(
        archive.records.find((r) => r.ref === bGraph.forkPoint).json,
        a1.records.at(-1).json,
      );
      assert.equal(
        archive.records.find((r) => r.ref === cGraph.forkPoint).json,
        a2.records.at(-1).json,
      );
      assert.equal(
        archive.graph.nodes.find((n) => n.id === result.mergeNodeId).parents
          .length,
        3,
      );
      const rawLarge = archive.records.find((r) =>
        r.json.includes("保留人物归属"),
      );
      assert.ok(Buffer.byteLength(rawLarge.json) > 50000);
      const saved = await loadSnapshot(client, result.threadId);
      const frame = saved.records
        .map((r) => JSON.parse(r.json))
        .find(parseGraphItem);
      assert.deepEqual(frame.content[1], image);
      for (const request of provider.requests) {
        assert.equal(request.text.format.type, "json_schema");
        assert.doesNotMatch(
          JSON.stringify(request.tools ?? []),
          /exec_command|apply_patch|spawn_agent|mcp__/,
        );
        const prompt = request.input
          .flatMap((item) => item.content ?? [])
          .find((part) =>
            part.text?.startsWith("Read the historical conversation material"),
          );
        assert.ok(
          Buffer.byteLength(
            JSON.stringify([{ type: "text", text: prompt.text }]),
          ) <= 7000,
        );
      }
      const read = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          "evidence",
          result.evidence.path,
          rawLarge.ref,
          "--length-bytes",
          "500",
          "--max-tokens",
          "2048",
          "--json",
        ],
        { env },
      );
      assert.ok(JSON.parse(read.stdout).nextByte);
      const draw = await exec(
        process.execPath,
        [launcher, "merge", "graph", result.evidence.path, "--mermaid"],
        { env },
      );
      assert.match(draw.stdout, /flowchart LR/);
      await client.close();
      client = new AppServerClient(binary, { env, config });
      await client.initialize();
      await client.request("thread/resume", {
        threadId: result.threadId,
        excludeTurns: true,
      });
      const repeated = await mergeThreads(client, {
        threadIds: [result.threadId, b.id, c.id],
      });
      assert.equal(repeated.reusedGraph, true);
      assert.equal(repeated.modelCalls, 0);
      const priorRequests = provider.requests.length;
      await client.request("thread/resume", {
        threadId: b.id,
        excludeTurns: true,
      });
      await client.request("thread/inject_items", {
        threadId: b.id,
        items: [message("B继续保留一个例外。", "b-update")],
      });
      const incremental = await mergeThreads(client, {
        threadIds: [repeated.threadId, b.id],
      });
      assert.equal(incremental.importedItems, 1);
      assert.equal(provider.requests.length, priorRequests);
      const expanded = await expandSnapshot(
        await loadSnapshot(client, incremental.threadId),
      );
      assert.equal(expanded.timeline.at(-1), incremental.mergeNodeId);
      const updated = await readEvidence(incremental.evidence);
      for (const node of archive.graph.nodes)
        assert.deepEqual(
          updated.graph.nodes.find((n) => n.id === node.id),
          node,
        );
      const cli = await exec(
        process.execPath,
        [launcher, "merge", incremental.threadId, c.id, "--dry-run", "--json"],
        { env },
      );
      assert.equal(JSON.parse(cli.stdout).importedItems, 0);
      const beforeSynthesis = provider.requests.length;
      const synthesizedCli = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          a.id,
          c.id,
          ...config.flatMap((value) => ["-c", value]),
          "--goal",
          "Compare both views by default",
          "--name",
          "Default synthesis",
          "--json",
        ],
        { env, timeout: 30000 },
      );
      const synthesized = JSON.parse(synthesizedCli.stdout);
      assert.equal(synthesized.modelCalls, 1);
      assert.ok(synthesized.reading);
      assert.equal(provider.requests.length, beforeSynthesis + 1);
      const listing = async () =>
        (await client.request("thread/list", { limit: 100 })).data
          .map((thread) => thread.id)
          .sort();
      const beforeInvalid = await listing();
      provider.invalidate();
      await assert.rejects(
        mergeThreads(client, {
          threadIds: [a.id, c.id],
          mode: "summary",
          summaryBytes: 1000,
        }),
        /unknown original/,
      );
      assert.deepEqual(await listing(), beforeInvalid);
    },
  );

for (const historyMode of ["legacy", "paginated"])
  test(
    "native compact primary (" +
      historyMode +
      "): fresh context, restart and continuation",
    { skip: !binary, timeout: 120000 },
    async (t) => {
      const home = await mkdtemp(
        path.join(os.tmpdir(), "codex-graph-primary-"),
      );
      const env = { ...process.env, CODEX_HOME: home };
      const provider = await providerFixture();
      const config = [
        'model="graph-fixture"',
        'model_provider="graph_fixture"',
        'model_providers.graph_fixture.name="Graph fixture"',
        'model_providers.graph_fixture.base_url="' + provider.url + '"',
        'model_providers.graph_fixture.wire_api="responses"',
        "model_providers.graph_fixture.requires_openai_auth=false",
        "model_providers.graph_fixture.supports_websockets=false",
        'model_reasoning_effort="low"',
        "model_context_window=16384",
      ];
      let client = new AppServerClient(binary, { env, config });
      t.after(async () => {
        await client.close();
        await provider.close();
        await rm(home, { recursive: true, force: true });
      });
      await client.initialize();
      const { thread: a } = await client.request("thread/start", {
        cwd: process.cwd(),
        historyMode,
      });
      await client.request("thread/inject_items", {
        threadId: a.id,
        items: [message("共同讨论写作风格。", "compact-root")],
      });
      const { thread: b } = await client.request("thread/fork", {
        threadId: a.id,
        excludeTurns: true,
      });
      await client.request("thread/inject_items", {
        threadId: b.id,
        items: [
          message("分支保留克制表达的候选。", "compact-branch", "assistant"),
        ],
      });
      await client.request("thread/inject_items", {
        threadId: a.id,
        items: [
          message(
            "主线持续讨论限定条件。".repeat(1800),
            "compact-large",
            "assistant",
          ),
        ],
      });
      const sources = await Promise.all(
        [a.id, b.id].map(async (id) => {
          const snapshot = await loadSnapshot(client, id);
          return {
            path: snapshot.thread.path,
            sha: hash(await readFile(snapshot.thread.path)),
          };
        }),
      );
      const result = await mergeThreads(client, {
        threadIds: [a.id, b.id],
        maxInputBytes: 7000,
      });
      assert.equal(result.primaryEmbedded, true);
      assert.ok(result.estimatedContextTokens <= result.budget.inputLimit);
      const target = await loadSnapshot(client, result.threadId);
      assert.equal(target.thread.forkedFromId ?? null, null);
      assert.equal(target.thread.historyMode, historyMode);
      assert.equal(target.records.length, 1);
      for (const original of sources)
        assert.equal(hash(await readFile(original.path)), original.sha);
      const originalGraph = await readEvidence(result.evidence);
      assert.ok(
        originalGraph.records.some((r) => Buffer.byteLength(r.json) > 50000),
      );
      await client.close();
      client = new AppServerClient(binary, { env, config });
      await client.initialize();
      await client.request("thread/resume", {
        threadId: result.threadId,
        excludeTurns: true,
      });
      const answer = await client.runTurn(
        {
          threadId: result.threadId,
          input: [{ type: "text", text: "继续比较候选风格，保留归属。" }],
        },
        { timeoutMs: 15000 },
      );
      assert.match(answer.text, /主线/);
      assert.ok(
        provider.dialogueRequests
          .at(-1)
          .input.some((item) =>
            item.content?.some((part) =>
              part.text?.includes("codex-graph-merge-v1"),
            ),
          ),
      );
      const next = await mergeThreads(client, {
        threadIds: [result.threadId, b.id],
      });
      const updated = await readEvidence(next.evidence);
      const continued = updated.records.find((r) =>
        r.json.includes("继续比较候选风格"),
      );
      assert.deepEqual(
        updated.graph.nodes.find((n) => n.id === continued.ref).parents,
        [result.mergeNodeId],
      );
      for (const node of originalGraph.graph.nodes)
        assert.deepEqual(
          updated.graph.nodes.find((n) => n.id === node.id),
          node,
        );
    },
  );
