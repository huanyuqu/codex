import assert from "node:assert/strict";
import { readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import * as zlib from "node:zlib";
import { promisify } from "node:util";
import { contextTree } from "../bin/context-tree.js";
import { mergeThreads } from "../bin/merge-context.js";
import { loadSnapshot } from "../bin/merge-history.js";
import {
  exampleSources,
  fakeGraphClient,
  message,
  record,
  source,
} from "./fixtures/graph.js";

test("tree includes ancestors, siblings and descendants, excludes unrelated chats, and paginates", async (t) => {
  const sources = [
    ...exampleSources(),
    source("D", [record(message("unrelated"), "D")]),
  ];
  for (let i = 0; i < 110; i++) sources.push(source("child-" + i, [], "B"));
  const { client } = await fakeGraphClient(t, sources);
  const result = await contextTree(client, "child-70");
  assert.equal(result.rows.length, 113);
  assert.ok(!result.rows.some((r) => r.id === "D"));
  assert.deepEqual(result.rows.find((r) => r.id === "B").parents, ["A"]);
  assert.deepEqual(result.rows.find((r) => r.id === "child-70").parents, ["B"]);
  assert.equal(result.rows.filter((r) => r.current).length, 1);
  assert.equal(result.rows.find((r) => r.current).id, "child-70");
  assert.ok(client.calls.filter((c) => c.method === "thread/list").length > 2);
  assert.ok(
    client.calls.every((c) =>
      ["thread/list", "thread/read"].includes(c.method),
    ),
  );
});

test("default merge synthesizes inline branches and binds all merge parents to the saved target", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    goal: "Keep the two views",
    name: "Combined",
  });
  assert.equal(result.modelCalls, 1);
  assert.ok(result.reading);
  assert.ok(result.presentations.every((p) => p.presentation === "inline"));
  const task = JSON.parse(
    client.calls
      .find((c) => c.method === "model")
      .params.input[0].text.split("\n")
      .at(-1),
  );
  assert.equal(task.purpose, "cross-branch-reading");
  assert.equal(task.requestedGoal, "Keep the two views");
  const index = JSON.parse(
    await readFile(
      path.join(directory, "merges", "lineage", result.threadId + ".json"),
    ),
  );
  assert.deepEqual(
    index.sources.map((s) => s.id),
    ["A", "B", "C"],
  );
  for (const id of ["A", "B", "C", result.threadId]) {
    const tree = await contextTree(client, id);
    assert.equal(tree.rows.length, 4);
    const merged = tree.rows.find((r) => r.id === result.threadId);
    assert.equal(merged.kind, "merge");
    assert.deepEqual(merged.parents, ["A", "B", "C"]);
    assert.match(merged.description, /merge ← A \[A\] \+ B \[B\] \+ C \[C\]/);
    assert.deepEqual(tree.rows.find((r) => r.id === "B").parents, ["A"]);
  }
});

test("native list summaries without fork metadata recover parents from plain, paginated and compressed headers", async (t) => {
  const { client } = await fakeGraphClient(t);
  for (const [id, paginated] of [
    ["B", false],
    ["C", true],
  ]) {
    const thread = client.threads.get(id);
    thread.forkedFromId = null;
    const text = await readFile(thread.path, "utf8");
    const header = JSON.stringify({
      type: "session_meta",
      payload: {
        id,
        ...(paginated
          ? {
              history_base: {
                thread_id: "A",
                end_byte_offset: 0,
                end_ordinal_exclusive: 0,
              },
            }
          : { forked_from_id: "A" }),
      },
    });
    await writeFile(
      thread.path,
      header + "\n" + text.split("\n").slice(1).join("\n"),
    );
  }
  if (zlib.zstdCompress) {
    const file = client.threads.get("B").path;
    await writeFile(
      file + ".zst",
      await promisify(zlib.zstdCompress)(await readFile(file)),
    );
    await rm(file);
  }
  const tree = await contextTree(client, "B");
  assert.deepEqual(
    tree.rows.map((r) => r.id),
    ["A", "B", "C"],
  );
  assert.deepEqual(tree.rows.find((r) => r.id === "C").parents, ["A"]);
});

test("fresh reference target for an oversized primary is reachable from every source without inference", async (t) => {
  const { client } = await fakeGraphClient(
    t,
    exampleSources({ primaryLong: true }),
  );
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "reference",
    contextTokens: 8000,
    reserveTokens: 1000,
  });
  assert.equal(result.primaryEmbedded, true);
  assert.equal(result.modelCalls, 0);
  assert.equal(client.threads.get(result.threadId).forkedFromId, null);
  for (const id of ["B", "C", result.threadId]) {
    const tree = await contextTree(client, id);
    assert.deepEqual(tree.rows.find((r) => r.id === result.threadId).parents, [
      "A",
      "B",
      "C",
    ]);
  }
  assert.ok(!client.calls.some((c) => c.method === "model"));
});

test("older graph merges are discovered and inherited graph capsules stay ordinary forks", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "reference",
  });
  const { thread: fork } = await client.request("thread/fork", {
    threadId: result.threadId,
  });
  await rm(path.join(directory, "merges", "lineage"), { recursive: true });
  const tree = await contextTree(client, "B");
  assert.equal(tree.rows.find((r) => r.id === result.threadId).kind, "merge");
  assert.equal(tree.rows.find((r) => r.id === fork.id).kind, "fork");
  assert.deepEqual(tree.rows.find((r) => r.id === fork.id).parents, [
    result.threadId,
  ]);
  assert.deepEqual(tree.warnings, []);
});

test("merge connects independent roots and nested merges retain all direct parents", async (t) => {
  const sources = [
    ...exampleSources(),
    source("D", [record(message("independent idea"), "D")]),
  ];
  const { client } = await fakeGraphClient(t, sources);
  const first = await mergeThreads(client, {
    threadIds: ["A", "B"],
    mode: "reference",
  });
  const second = await mergeThreads(client, {
    threadIds: [first.threadId, "C", "D"],
    mode: "reference",
  });
  const tree = await contextTree(client, "D");
  assert.equal(tree.rows.length, 6);
  assert.deepEqual(tree.rows.find((r) => r.id === first.threadId).parents, [
    "A",
    "B",
  ]);
  assert.deepEqual(tree.rows.find((r) => r.id === second.threadId).parents, [
    first.threadId,
    "C",
    "D",
  ]);
});

test("deleted sources and archived branches remain visible and disabled; missing evidence does not block indexed navigation", async (t) => {
  const { client } = await fakeGraphClient(t);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "reference",
  });
  client.threads.delete("B");
  client.threads.get("C").archived = true;
  await rm(result.evidence.path);
  const tree = await contextTree(client, result.threadId);
  assert.match(
    tree.rows.find((r) => r.id === "B").disabledReason,
    /no longer available/,
  );
  assert.deepEqual(tree.rows.find((r) => r.id === "B").parents, ["A"]);
  assert.match(tree.rows.find((r) => r.id === "C").disabledReason, /Archived/);
  assert.equal(
    tree.rows.find((r) => r.id === result.threadId).disabledReason,
    null,
  );
  assert.deepEqual(tree.rows.find((r) => r.id === result.threadId).parents, [
    "A",
    "B",
    "C",
  ]);
});

test("a malformed lineage warns without losing native forks; repeated cursors fail clearly", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B"],
    mode: "reference",
  });
  await writeFile(
    path.join(directory, "merges", "lineage", result.threadId + ".json"),
    "{}",
  );
  const tree = await contextTree(client, "B");
  assert.equal(tree.rows.length, 4);
  assert.match(tree.warnings[0], /Invalid session merge lineage/);
  const request = client.request;
  client.request = (method, params) =>
    method === "thread/list"
      ? { data: [], nextCursor: "same" }
      : request(method, params);
  await assert.rejects(
    contextTree(client, "A"),
    /repeated conversation list cursor/,
  );
});

test("default synthesis rejects invented evidence before creating a target; dry runs do not persist lineage", async (t) => {
  const { client, directory } = await fakeGraphClient(t, exampleSources(), {
    invalid: true,
  });
  const preview = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    dryRun: true,
  });
  assert.equal(preview.analysisPlanned, true);
  assert.equal(preview.modelCalls, 0);
  await assert.rejects(
    readFile(path.join(directory, "merges", "lineage", "A.json")),
    { code: "ENOENT" },
  );
  await assert.rejects(
    mergeThreads(client, { threadIds: ["A", "B", "C"] }),
    /unknown original references/,
  );
  assert.ok(
    !client.calls.some((c) =>
      ["thread/fork", "thread/inject_items"].includes(c.method),
    ),
  );
  assert.equal((await loadSnapshot(client, "A")).records.length, 3);
});
