import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import test from "node:test";
import { mergeThreads as mergeThreadsImpl } from "../bin/merge-context.js";
const mergeThreads = (client, options) =>
  mergeThreadsImpl(client, { mode: "legacy", semantic: false, ...options });
import { AppServerClient } from "../bin/merge-rpc.js";
import { hash, loadSnapshot } from "../bin/merge-history.js";
import {
  parseImageItem,
  parseSemanticItem,
  readEvidence,
} from "../bin/merge-evidence.js";
import { fixtureState, message } from "./fixtures/semantic.js";

const nativeBinary = process.env.CODEX_MERGE_TEST_BINARY;
const exec = promisify(execFile);
const image = {
  type: "input_image",
  image_url:
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
};

async function fixtureProvider() {
  const requests = [];
  let invalid = false;
  const server = createServer(async (req, res) => {
    try {
      const chunks = [];
      for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString());
      const prompt = body.input
        .flatMap((item) => item.content ?? [])
        .find(
          (part) =>
            part.type === "input_text" &&
            part.text?.startsWith("Produce the merged task state"),
        );
      let text = "I will preserve the API and investigate caching";
      if (prompt) {
        requests.push(body);
        const task = JSON.parse(prompt.text.split("\n").at(-1));
        const state = fixtureState(task, task.requestedGoal ?? undefined);
        if (invalid) state.facts[0].refs = ["nonexistent-reference"];
        text = JSON.stringify(state);
      }
      const id = "resp_merge_" + requests.length;
      const item = {
        type: "message",
        id: "msg_" + requests.length,
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
          usage: { input_tokens: 100, output_tokens: 500, total_tokens: 600 },
        },
      });
      res.end();
    } catch (error) {
      res.writeHead(500, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          error: { message: error.message, type: "fixture_error" },
        }),
      );
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    requests,
    setInvalid: () => {
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

for (const historyMode of ["legacy", "paginated"]) {
  test(
    "native semantic merge (" +
      historyMode +
      "): model stream, evidence lookup, restart, reuse, and increments",
    { skip: !nativeBinary, timeout: 90000 },
    async (t) => {
      const home = await mkdtemp(
        path.join(os.tmpdir(), "codex-merge-semantic-native-"),
      );
      const env = { ...process.env, CODEX_HOME: home };
      const provider = await fixtureProvider();
      const config = [
        'model="merge-fixture"',
        'model_provider="merge_fixture"',
        'model_providers.merge_fixture.name="Semantic fixture"',
        'model_providers.merge_fixture.base_url="' + provider.url + '"',
        'model_providers.merge_fixture.wire_api="responses"',
        "model_providers.merge_fixture.requires_openai_auth=false",
        "model_providers.merge_fixture.supports_websockets=false",
        'model_reasoning_effort="low"',
        'mcp_servers.configured.command="node"',
        "mcp_servers.configured.enabled=false",
      ];
      let client = new AppServerClient(nativeBinary, { env, config });
      t.after(async () => {
        await client.close();
        await provider.close();
        await rm(home, { recursive: true, force: true });
      });
      await client.initialize();
      const { thread: base } = await client.request("thread/start", {
        cwd: process.cwd(),
        historyMode,
      });
      await client.runTurn(
        {
          threadId: base.id,
          input: [
            { type: "text", text: "Add caching without changing the API" },
          ],
        },
        { timeoutMs: 15000 },
      );
      const branches = [];
      for (const label of ["B", "C"]) {
        const { thread } = await client.request("thread/fork", {
          threadId: base.id,
          excludeTurns: true,
        });
        branches.push(thread);
        const request = message(
          label === "B"
            ? "Measure cache performance"
            : "Check invalidation correctness",
          "request-" + label,
        );
        if (label === "B") request.content.push(image);
        await client.request("thread/inject_items", {
          threadId: thread.id,
          items: [
            request,
            {
              type: "function_call",
              call_id: "same-call",
              name: "test_cache",
              arguments: "{}",
            },
            {
              type: "function_call_output",
              call_id: "same-call",
              output:
                label === "B" ? "Performance passed" : "FAILED: stale data",
            },
            message(
              label === "B"
                ? "Caching improves performance"
                : "Cache invalidation still needs a fix",
              "answer-" + label,
              "assistant",
            ),
          ],
        });
      }
      const threadIds = [base.id, ...branches.map((b) => b.id)];
      const original = await Promise.all(
        threadIds.map(async (id) => {
          const { thread } = await client.request("thread/read", {
            threadId: id,
          });
          return {
            path: thread.path,
            checksum: hash(await readFile(thread.path)),
          };
        }),
      );
      const result = await mergeThreads(client, {
        threadIds,
        semantic: true,
        name: "Semantic handoff",
        analysisTimeoutMs: 15000,
      });
      assert.equal(result.analysisExecuted, true);
      assert.equal(result.importedItems, 8);
      assert.equal(result.importedImages, 1);
      assert.equal(result.state.conflicts[0].status, "unresolved");
      assert.equal(provider.requests.length, 1);
      assert.equal(provider.requests[0].text.format.type, "json_schema");
      assert.equal(provider.requests[0].model, "merge-fixture");
      const tools = JSON.stringify(provider.requests[0].tools ?? []);
      assert.doesNotMatch(tools, /exec_command|apply_patch|spawn_agent|mcp__/);
      for (const source of original)
        assert.equal(hash(await readFile(source.path)), source.checksum);
      const saved = await loadSnapshot(client, result.threadId);
      const marker = saved.records
        .map((r) => JSON.parse(r.json))
        .find(parseSemanticItem);
      assert.ok(marker);
      const imageMarker = saved.records
        .map((r) => JSON.parse(r.json))
        .find(parseImageItem);
      assert.deepEqual(imageMarker.content[1], image);
      const capsule = parseSemanticItem(marker);
      const archive = await readEvidence(capsule.evidence);
      assert.equal(archive.records.length, 10);
      const failed = archive.records.find((r) =>
        r.json.includes("FAILED: stale data"),
      );
      assert.ok(
        result.state.facts.some((fact) => fact.refs.includes(failed.ref)),
      );

      const launcher = fileURLToPath(
        new URL("../bin/codex.js", import.meta.url),
      );
      const lookup = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          "evidence",
          result.evidence.path,
          failed.ref,
          "--json",
        ],
        { env },
      );
      assert.equal(JSON.parse(lookup.stdout)[0].json, failed.json);
      const missing = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          "evidence",
          result.evidence.path,
          "unknown",
          "--json",
        ],
        { env },
      ).then(
        () => null,
        (e) => e,
      );
      assert.match(missing.stderr, /Unknown evidence reference/);

      await client.close();
      client = new AppServerClient(nativeBinary, { env, config });
      await client.initialize();
      await client.request("thread/resume", {
        threadId: result.threadId,
        excludeTurns: true,
      });
      const repeated = await mergeThreads(client, {
        threadIds: [result.threadId, ...threadIds.slice(1)],
        semantic: true,
        analysisTimeoutMs: 15000,
      });
      assert.equal(repeated.reusedAnalysis, true);
      assert.equal(repeated.importedItems, 0);
      assert.equal(provider.requests.length, 1);
      const repeatedSnapshot = await loadSnapshot(client, repeated.threadId);
      assert.equal(
        repeatedSnapshot.records.filter((r) =>
          parseSemanticItem(JSON.parse(r.json)),
        ).length,
        1,
      );

      await client.request("thread/resume", {
        threadId: branches[0].id,
        excludeTurns: true,
      });
      await client.request("thread/inject_items", {
        threadId: branches[0].id,
        items: [
          message(
            "Proposed invalidation fix requires retesting",
            "new-request-B",
          ),
          message(
            "A fix is proposed but not verified",
            "new-answer-B",
            "assistant",
          ),
        ],
      });
      const incremental = await mergeThreads(client, {
        threadIds: [repeated.threadId, branches[0].id],
        semantic: true,
        analysisTimeoutMs: 15000,
      });
      assert.equal(incremental.importedItems, 2);
      assert.equal(incremental.analysisExecuted, true);
      assert.equal(provider.requests.length, 2);
      assert.match(JSON.stringify(incremental.state), /requires retesting/);
      const sourceOnly = await mergeThreads(client, {
        threadIds: [incremental.threadId, branches[1].id],
      });
      assert.equal(sourceOnly.importedItems, 0);
      assert.equal(provider.requests.length, 2);

      const graph = await mergeThreads(client, {
        threadIds: [
          incremental.threadId,
          ...branches.map((branch) => branch.id),
        ],
        mode: "auto",
      });
      assert.equal(graph.graph, true);
      assert.equal(graph.importedItems, 0);
      assert.equal(provider.requests.length, 2);
      const graphArchive = await readEvidence(graph.evidence);
      for (const originalRecord of archive.records)
        assert.ok(
          graphArchive.records.some((r) => r.json === originalRecord.json),
        );

      const before = (await client.request("thread/list", { limit: 100 })).data
        .map((thread) => thread.id)
        .sort();
      provider.setInvalid();
      await assert.rejects(
        mergeThreads(client, {
          threadIds,
          semantic: true,
          goal: "Recheck correctness",
          analysisTimeoutMs: 15000,
        }),
        /unknown evidence/,
      );
      const after = (await client.request("thread/list", { limit: 100 })).data
        .map((thread) => thread.id)
        .sort();
      assert.deepEqual(after, before);
    },
  );
}
