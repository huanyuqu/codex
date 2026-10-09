import assert from "node:assert/strict";
import { appendFile, readFile, readdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { contextTree } from "../bin/context-tree.js";
import { mergeThreads } from "../bin/merge-context.js";
import { parseMergeArgs } from "../bin/merge-cli.js";
import {
  expandSnapshot,
  readEvidence,
  saveEvidence,
} from "../bin/merge-evidence.js";
import { buildConversationGraph, GRAPH_PREFIX } from "../bin/merge-graph.js";
import { loadSnapshot } from "../bin/merge-history.js";
import { inspectMergeUpdates } from "../bin/merge-updates.js";
import {
  exampleSources,
  fakeGraphClient,
  message,
  record,
  source,
} from "./fixtures/graph.js";

const append = (client, id, ...items) =>
  client.request("thread/inject_items", { threadId: id, items });
const update = (client, id, options = {}) =>
  mergeThreads(client, { threadIds: [id], update: true, ...options });
const firstMerge = (client) =>
  mergeThreads(client, { threadIds: ["A", "B", "C"], mode: "reference" });
const writes = (calls) =>
  calls.filter(
    (call) =>
      !["thread/read", "thread/list", "config/read"].includes(call.method),
  );

test("tree marks pending source items from both the source and merge, without model calls or writes", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await append(
    client,
    "B",
    message("B的后续修正", "b3"),
    message("B的新结论", "b4", "assistant"),
  );
  await append(client, "C", message("C的补充", "c4"));
  client.calls.length = 0;
  const fromB = await contextTree(client, "B");
  const row = fromB.rows.find((row) => row.id === merged.threadId);
  assert.match(row.name, /3 pending items/);
  assert.match(row.description, /B \+2; C \+1/);
  assert.equal(row.updates.pendingSources, 2);
  const fromM = await contextTree(client, merged.threadId);
  assert.match(fromM.rows.find((row) => row.id === "B").name, /\+2 pending/);
  assert.match(fromM.updateHint, /\/merge --update/);
  assert.deepEqual(writes(client.calls), []);
});

test("update imports only new evidence, keeps mainline work, and tracks unchanged sources across later updates", async (t) => {
  const { client } = await fakeGraphClient(t);
  const first = await firstMerge(client);
  const originalGraph = (await readEvidence(first.evidence)).graph;
  await append(
    client,
    first.threadId,
    message("主线已经选定初稿，请保留", "m-work"),
  );
  await append(
    client,
    "B",
    message("B的新事实", "b3"),
    message("B撤回先前的建议", "b4", "assistant"),
  );
  const before = await Promise.all(
    ["A", "B", "C", first.threadId].map((id) =>
      readFile(client.threads.get(id).path),
    ),
  );
  client.calls.length = 0;
  const second = await update(client, first.threadId);
  assert.equal(second.modelCalls, 1);
  assert.equal(second.importedItems, 2);
  assert.deepEqual(second.sourceThreadIds, [first.threadId, "B"]);
  const archive = await readEvidence(second.evidence);
  for (const node of originalGraph.nodes)
    assert.deepEqual(
      archive.graph.nodes.find((current) => current.id === node.id),
      node,
    );
  assert.equal(
    archive.records.filter((record) => JSON.parse(record.json).id === "b3")
      .length,
    1,
  );
  assert.ok(
    archive.records.some((record) => JSON.parse(record.json).id === "m-work"),
  );
  const after = await Promise.all(
    ["A", "B", "C", first.threadId].map((id) =>
      readFile(client.threads.get(id).path),
    ),
  );
  assert.deepEqual(after, before);
  const native = await loadSnapshot(client, second.threadId);
  const expanded = await expandSnapshot(native);
  assert.ok(
    expanded.records.some((record) => JSON.parse(record.json).id === "m-work"),
  );
  assert.equal(
    (await inspectMergeUpdates(client, second.threadId)).sources.find(
      (source) => source.id === "C",
    ).state,
    "upToDate",
  );
  const noOp = await update(client, second.threadId);
  assert.equal(noOp.noOp, true);
  await append(client, "C", message("C后来发现的事实", "c4"));
  const third = await update(client, second.threadId, { mode: "reference" });
  assert.equal(third.importedItems, 1);
  assert.deepEqual(third.sourceThreadIds, [second.threadId, "C"]);
  assert.equal(
    (await inspectMergeUpdates(client, third.threadId)).sources.find(
      (source) => source.id === "B",
    ).state,
    "upToDate",
  );
});

test("unchanged sources, renamed chats and runtime settings are no-ops with no inference, archives or new chats", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  client.threads.get("B").name = "Renamed B";
  await appendFile(
    client.threads.get("B").path,
    JSON.stringify({
      type: "event_msg",
      payload: { type: "token_count", info: null },
    }) + "\n",
  );
  const files = await readdir(path.join(directory, "merges", "evidence"));
  const size = client.threads.size;
  client.calls.length = 0;
  for (const dryRun of [false, true]) {
    const result = await update(client, merged.threadId, { dryRun });
    assert.equal(result.noOp, true);
    assert.equal(result.threadId, merged.threadId);
    assert.equal(result.modelCalls, 0);
    assert.equal(result.dryRun, dryRun);
  }
  assert.equal(client.threads.size, size);
  assert.deepEqual(
    await readdir(path.join(directory, "merges", "evidence")),
    files,
  );
  assert.deepEqual(writes(client.calls), []);
});

test("importing an older merged graph does not regress an incorporated source update boundary", async (t) => {
  const { client } = await fakeGraphClient(t);
  const first = await firstMerge(client);
  await append(client, "B", message("Already incorporated update", "b3"));
  const second = await update(client, first.threadId, { mode: "reference" });
  const third = await mergeThreads(client, {
    threadIds: [second.threadId, first.threadId],
    mode: "reference",
  });
  client.calls.length = 0;
  assert.equal(
    (await inspectMergeUpdates(client, third.threadId)).sources.find(
      (source) => source.id === "B",
    ).state,
    "upToDate",
  );
  assert.equal((await update(client, third.threadId)).noOp, true);
  assert.deepEqual(writes(client.calls), []);
});

test("pending update previews never call a model, create a target or write evidence", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await append(client, "B", message("新的细节", "b3"));
  const evidence = await readdir(path.join(directory, "merges", "evidence"));
  const lineage = await readdir(path.join(directory, "merges", "lineage"));
  client.calls.length = 0;
  const result = await update(client, merged.threadId, { dryRun: true });
  assert.equal(result.dryRun, true);
  assert.equal(result.importedItems, 1);
  assert.equal(result.updates.pendingSources, 1);
  assert.deepEqual(writes(client.calls), []);
  assert.deepEqual(
    await readdir(path.join(directory, "merges", "evidence")),
    evidence,
  );
  assert.deepEqual(
    await readdir(path.join(directory, "merges", "lineage")),
    lineage,
  );
});

test("same-length rewritten source history is not called up to date or automatically overwritten", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  const file = client.threads.get("B").path;
  const text = await readFile(file, "utf8");
  await writeFile(file, text.replace("B偏向克制表达", "B已经改变观点"));
  client.calls.length = 0;
  const inspected = await inspectMergeUpdates(client, merged.threadId);
  assert.equal(
    inspected.sources.find((source) => source.id === "B").state,
    "changed",
  );
  await assert.rejects(
    update(client, merged.threadId),
    /Source history was rolled back, replaced or compacted/,
  );
  assert.deepEqual(writes(client.calls), []);
  const tree = await contextTree(client, merged.threadId);
  assert.match(
    tree.rows.find((row) => row.current).name,
    /updates unavailable/,
  );
});

test("unavailable, archived and busy sources block automatic updates without hiding the tree", async (t) => {
  for (const state of ["deleted", "archived", "busy"])
    await t.test(state, async (t) => {
      const { client } = await fakeGraphClient(t);
      const merged = await firstMerge(client);
      if (state === "deleted") client.threads.delete("B");
      if (state === "archived") client.threads.get("B").archived = true;
      if (state === "busy") client.threads.get("B").status = { type: "active" };
      client.calls.length = 0;
      await assert.rejects(
        update(client, merged.threadId),
        /Cannot update all sources/,
      );
      const tree = await contextTree(client, merged.threadId);
      assert.equal(tree.rows.length, 4);
      assert.ok(tree.rows.find((row) => row.id === "B").disabledReason);
      assert.equal(
        tree.rows.find((row) => row.current).updates.unavailableSources,
        1,
      );
      assert.deepEqual(writes(client.calls), []);
    });
});

test("source compaction is a changed boundary requiring an explicit remerge", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await appendFile(
    client.threads.get("B").path,
    JSON.stringify({
      type: "compacted",
      payload: {
        replacement_history: [message("B的摘要", "b-summary", "assistant")],
      },
    }) + "\n",
  );
  client.calls.length = 0;
  await assert.rejects(update(client, merged.threadId), /compacted/);
  assert.deepEqual(writes(client.calls), []);
});

test("a compacted target keeps frozen ancestry and imports only new branch nodes", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  const original = await readEvidence(merged.evidence);
  await appendFile(
    client.threads.get(merged.threadId).path,
    JSON.stringify({
      type: "compacted",
      payload: {
        replacement_history: [
          message("当前主线的压缩状态", "m-summary", "assistant"),
        ],
      },
    }) + "\n",
  );
  await append(client, "B", message("B的后续事实", "b3"));
  assert.equal(
    (await inspectMergeUpdates(client, merged.threadId)).recovered,
    true,
  );
  // The fixture's basic fork copies response lines. Native fork replays the
  // retained checkpoint instead; emulate that boundary for this regression.
  client.onFork = async (fork, params) => {
    if (params.threadId !== merged.threadId) return;
    const snapshot = await loadSnapshot(client, params.threadId);
    await writeFile(
      fork.path,
      JSON.stringify({ type: "session_meta", payload: { id: fork.id } }) +
        "\n" +
        snapshot.records
          .map(
            (record) =>
              '{"type":"response_item","payload":' + record.json + "}",
          )
          .join("\n") +
        "\n",
    );
  };
  const updated = await update(client, merged.threadId, { mode: "reference" });
  assert.equal(updated.importedItems, 1);
  const archive = await readEvidence(updated.evidence);
  for (const node of original.graph.nodes)
    assert.deepEqual(
      archive.graph.nodes.find((current) => current.id === node.id),
      node,
    );
  assert.ok(
    archive.records.some(
      (record) => JSON.parse(record.json).id === "m-summary",
    ),
  );
  assert.equal((await update(client, updated.threadId)).noOp, true);
});

test("rollback of a merge followed by compaction cannot resurrect the removed merge", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await appendFile(
    client.threads.get(merged.threadId).path,
    [
      JSON.stringify({
        type: "event_msg",
        payload: { type: "thread_rolled_back", num_turns: 1 },
      }),
      JSON.stringify({
        type: "compacted",
        payload: {
          replacement_history: [
            message("回退后保留的历史", "rolled-back-summary", "assistant"),
          ],
        },
      }),
    ].join("\n") + "\n",
  );
  client.calls.length = 0;
  await assert.rejects(
    update(client, merged.threadId),
    /no longer in this chat's retained history/,
  );
  assert.deepEqual(writes(client.calls), []);
});

test("older graph merges can recover ordinary source update boundaries from immutable evidence", async (t) => {
  const snapshots = exampleSources();
  const { client, directory } = await fakeGraphClient(t, snapshots);
  const built = buildConversationGraph(snapshots);
  const evidence = await saveEvidence(
    built.archive,
    path.join(directory, "merges", "evidence"),
  );
  const { thread } = await client.request("thread/fork", { threadId: "A" });
  await append(
    client,
    thread.id,
    message(
      GRAPH_PREFIX +
        JSON.stringify({
          format: "codex-graph-merge-v1",
          mergeNodeId: built.graph.mergeNodeId,
          evidence,
          branches: built.imports.map((branch) => ({
            threadId: branch.threadId,
            primary: false,
          })),
        }),
    ),
  );
  await append(client, "B", message("旧会话的新内容", "b3"));
  const inspected = await inspectMergeUpdates(client, thread.id);
  assert.equal(
    inspected.sources.find((source) => source.id === "B").newItems,
    1,
  );
  await appendFile(
    client.threads.get(thread.id).path,
    JSON.stringify({
      type: "compacted",
      payload: {
        replacement_history: [
          message("旧合并会话的当前摘要", "old-summary", "assistant"),
        ],
      },
    }) + "\n",
  );
  assert.equal((await inspectMergeUpdates(client, thread.id)).recovered, true);
  client.onFork = async (fork, params) => {
    const snapshot = await loadSnapshot(client, params.threadId);
    await writeFile(
      fork.path,
      JSON.stringify({ type: "session_meta", payload: { id: fork.id } }) +
        "\n" +
        snapshot.records
          .map(
            (record) =>
              '{"type":"response_item","payload":' + record.json + "}",
          )
          .join("\n") +
        "\n",
    );
  };
  const updated = await update(client, thread.id, { mode: "reference" });
  assert.equal(updated.importedItems, 1);
  assert.equal((await update(client, updated.threadId)).noOp, true);
});

test("a source that grows during semantic update analysis is rejected before target creation", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await append(client, "B", message("B的新输入", "b3"));
  const runTurn = client.runTurn;
  client.runTurn = async (params) => {
    const result = await runTurn(params);
    await append(client, "B", message("分析期间又有新输入", "b4"));
    return result;
  };
  client.calls.length = 0;
  await assert.rejects(
    update(client, merged.threadId),
    /Source changed during update analysis/,
  );
  assert.ok(
    !client.calls.some(
      (call) =>
        call.method === "thread/fork" ||
        (call.method === "thread/start" && !call.params.ephemeral),
    ),
  );
});

test("missing evidence fails update checks without presenting a false no-op", async (t) => {
  const { client } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  await rm(merged.evidence.path);
  client.calls.length = 0;
  await assert.rejects(update(client, merged.threadId));
  const tree = await contextTree(client, merged.threadId);
  assert.match(
    tree.rows.find((row) => row.current).name,
    /updates unavailable/,
  );
  assert.deepEqual(writes(client.calls), []);
});

test("compacted recovery rejects a lineage index pointing at another valid merge archive", async (t) => {
  const { client, directory } = await fakeGraphClient(t);
  const merged = await firstMerge(client);
  const other = await mergeThreads(client, {
    threadIds: ["A", "B"],
    mode: "reference",
  });
  await appendFile(
    client.threads.get(merged.threadId).path,
    JSON.stringify({
      type: "compacted",
      payload: { replacement_history: [message("当前摘要", "summary")] },
    }) + "\n",
  );
  const file = path.join(
    directory,
    "merges",
    "lineage",
    merged.threadId + ".json",
  );
  const lineage = JSON.parse(await readFile(file, "utf8"));
  lineage.evidence = other.evidence;
  await writeFile(file, JSON.stringify(lineage) + "\n");
  client.calls.length = 0;
  await assert.rejects(
    update(client, merged.threadId),
    /does not match its saved lineage/,
  );
  assert.deepEqual(writes(client.calls), []);
});

test("an oversized update batch is rejected before inference or creation", async (t) => {
  const sources = Array.from({ length: 32 }, (_, i) =>
    source("source-" + i, [
      record(message("Original " + i, "item-" + i), "source-" + i),
    ]),
  );
  const { client } = await fakeGraphClient(t, sources);
  const merged = await mergeThreads(client, {
    threadIds: sources.map((source) => source.thread.id),
    mode: "reference",
  });
  for (let i = 0; i < sources.length; i++)
    await append(
      client,
      sources[i].thread.id,
      message("Update " + i, "update-" + i),
    );
  client.calls.length = 0;
  await assert.rejects(update(client, merged.threadId), /More than 31 sources/);
  assert.deepEqual(writes(client.calls), []);
});

test("update syntax accepts one merged chat and rejects explicit sources, legacy mode and ordinary chats", async (t) => {
  assert.equal(parseMergeArgs(["M", "--update"]).update, true);
  assert.equal(parseMergeArgs(["M", "--update", "--dry-run"]).dryRun, true);
  for (const args of [
    ["--update"],
    ["M", "B", "--update"],
    ["M", "--update", "--mode", "legacy"],
  ])
    assert.throws(() => parseMergeArgs(args));
  const { client } = await fakeGraphClient(t);
  await assert.rejects(update(client, "A"), /previously merged chat/);
  assert.deepEqual(writes(client.calls), []);
});
