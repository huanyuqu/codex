import assert from "node:assert/strict";
import { appendFile, readFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { mergeThreads } from "../bin/merge-context.js";
import {
  ancestors,
  buildConversationGraph,
  parseGraphItem,
  validateGraph,
} from "../bin/merge-graph.js";
import {
  expandSnapshot,
  readEvidence,
  validateEvidence,
} from "../bin/merge-evidence.js";
import { contextBudget } from "../bin/merge-views.js";
import { hash, loadSnapshot } from "../bin/merge-history.js";
import { parseMergeArgs } from "../bin/merge-cli.js";
import {
  exampleSources,
  fakeGraphClient,
  message,
  record,
  recordRef,
  source,
} from "./fixtures/graph.js";

const exec = promisify(execFile);
const launcher = fileURLToPath(new URL("../bin/codex.js", import.meta.url));

for (const stable of [true, false])
  test(
    "explicit A1/A2 fork anchors and three merge parents (stable IDs: " +
      stable +
      ")",
    () => {
      const snapshots = exampleSources({ stable });
      const built = buildConversationGraph(snapshots);
      const byText = (text) =>
        built.archive.records.find((r) => r.json.includes(text)).ref;
      const a1 = byText("比较两种"),
        a2 = byText("读者和语气"),
        a3 = byText("主线希望"),
        b2 = byText("B偏向"),
        c3 = byText("C偏向");
      const nodes = new Map(built.graph.nodes.map((n) => [n.id, n]));
      assert.deepEqual(nodes.get(b2).parents, [a1]);
      assert.deepEqual(nodes.get(c3).parents, [a2]);
      assert.deepEqual(nodes.get(built.graph.mergeNodeId).parents, [
        a3,
        b2,
        c3,
      ]);
      assert.equal(built.archive.records.length, 5);
      assert.equal(built.summary.importedItems, 2);
      assert.deepEqual(
        built.graph.branches.slice(1).map((b) => b.forkPoint),
        [a1, a2],
      );
    },
  );

test("tool requests and results are separate nodes with branch-local correlations and turn groups", () => {
  const a = record(message("比较资料", "root"));
  const b = [
    a,
    record(message("查找来源", "b-question"), "B", 2),
    record(
      {
        type: "function_call",
        name: "search",
        call_id: "same",
        arguments: "{}",
      },
      "B",
      3,
    ),
    record(
      { type: "function_call_output", call_id: "same", output: "B资料" },
      "B",
      4,
    ),
    record(message("B结论", "b-answer", "assistant"), "B", 5),
  ];
  const c = [
    a,
    record(
      {
        type: "function_call",
        name: "search",
        call_id: "same",
        arguments: "{}",
      },
      "C",
      2,
    ),
    record(
      { type: "function_call_output", call_id: "same", output: "C资料" },
      "C",
      3,
    ),
  ];
  const built = buildConversationGraph([
    source("A", [a]),
    source("B", b, "A"),
    source("C", c, "A"),
  ]);
  const nodes = new Map(built.graph.nodes.map((n) => [n.id, n]));
  assert.equal(nodes.get(recordRef(b[3])).callNodeId, recordRef(b[2]));
  assert.equal(nodes.get(recordRef(c[2])).callNodeId, recordRef(c[1]));
  assert.equal(nodes.get(recordRef(b[4])).turnGroup, recordRef(b[1]));
});

test("graph validation rejects missing parents, cycles, duplicate nodes, and broken branch paths", () => {
  const built = buildConversationGraph(exampleSources());
  const catalog = validateEvidence(built.archive);
  const missing = structuredClone(built.graph);
  missing.nodes[1].parents = ["missing"];
  assert.throws(() => validateGraph(missing, catalog), /Missing/);
  const cyclic = structuredClone(built.graph);
  cyclic.nodes[0].parents = [cyclic.mergeNodeId];
  assert.throws(() => validateGraph(cyclic, catalog), /cycle/);
  const duplicate = structuredClone(built.graph);
  duplicate.nodes.push(duplicate.nodes[0]);
  assert.throws(() => validateGraph(duplicate, catalog), /duplicate/);
  const path = structuredClone(built.graph);
  path.branches[0].path.reverse();
  assert.throws(() => validateGraph(path, catalog), /Disconnected|head/);
  const unsafe = structuredClone(built.graph);
  unsafe.nodes.find((n) => n.kind === "merge").id = 'unquoted["label"]';
  assert.throws(() => validateGraph(unsafe, catalog), /Invalid merge/);
  const wrongFork = structuredClone(built.graph);
  wrongFork.branches[1].forkPoint = built.graph.branches[2].head;
  assert.throws(() => validateGraph(wrongFork, catalog), /fork point/);
});

test("unrelated conversations retain unknown ancestry rather than invented fork points", () => {
  const built = buildConversationGraph([
    source("A", [record(message("读论文", "a"))]),
    source("B", [record(message("写小说", "b"), "B")]),
  ]);
  assert.equal(built.graph.branches[1].forkPoint, null);
  assert.equal(built.graph.branches[1].ancestry, "unknown");
});

test("inline merge needs no inference and keeps source bytes, speakers, graph, and exact evidence", async (t) => {
  const { client } = await fakeGraphClient(t);
  const before = await Promise.all(
    [...client.threads.values()].map(async (thread) => [
      thread.path,
      hash(await readFile(thread.path)),
    ]),
  );
  const result = await mergeThreads(client, { threadIds: ["A", "B", "C"] });
  assert.equal(result.modelCalls, 0);
  assert.equal(result.primaryEmbedded, false);
  assert.deepEqual(
    result.presentations.map((v) => v.presentation),
    ["inline", "inline"],
  );
  const saved = await loadSnapshot(client, result.threadId);
  const frame = saved.records
    .map((r) => parseGraphItem(JSON.parse(r.json)))
    .find(Boolean);
  assert.ok(frame.branches[0].nodes[0].json.includes('"role":"assistant"'));
  const archive = await readEvidence(result.evidence);
  assert.equal(archive.graph.nodes.length, 6);
  for (const [file, checksum] of before)
    assert.equal(hash(await readFile(file)), checksum);
  const shown = await exec(
    process.execPath,
    [launcher, "merge", "graph", result.evidence.path, "--json"],
    { env: client.env },
  );
  assert.deepEqual(JSON.parse(shown.stdout), archive.graph);
});

test("repeated and incremental merges preserve graph ancestry and stable original IDs", async (t) => {
  const { client } = await fakeGraphClient(
    t,
    exampleSources({ stable: false }),
    { notices: true },
  );
  const first = await mergeThreads(client, { threadIds: ["A", "B", "C"] });
  const repeated = await mergeThreads(client, {
    threadIds: [first.threadId, "B", "C"],
  });
  assert.equal(repeated.importedItems, 0);
  assert.equal(repeated.reusedGraph, true);
  assert.equal(repeated.mergeNodeId, first.mergeNodeId);
  const b4 = message("B后续讨论保留一个例外。", "b-new", "assistant");
  await client.request("thread/inject_items", { threadId: "B", items: [b4] });
  const incremental = await mergeThreads(client, {
    threadIds: [repeated.threadId, "B"],
  });
  assert.equal(incremental.importedItems, 1);
  const archive = await readEvidence(incremental.evidence);
  const old = await readEvidence(first.evidence);
  for (const node of old.graph.nodes)
    assert.deepEqual(
      archive.graph.nodes.find((n) => n.id === node.id),
      node,
    );
  const newNode = archive.records.find((r) => r.json.includes("B后续"));
  const bOld = old.graph.branches.find((b) => b.threadId === "B").head;
  assert.deepEqual(
    archive.graph.nodes.find((n) => n.id === newNode.ref).parents,
    [bOld],
  );
  assert.deepEqual(
    archive.graph.nodes.find((n) => n.id === incremental.mergeNodeId).parents,
    [first.mergeNodeId, newNode.ref],
  );
  const expanded = await expandSnapshot(
    await loadSnapshot(client, incremental.threadId),
  );
  assert.equal(expanded.timeline.at(-1), incremental.mergeNodeId);
});

test("messages written after merge parent the merge node, including forks of merged sessions", async (t) => {
  const { client } = await fakeGraphClient(t);
  const first = await mergeThreads(client, { threadIds: ["A", "B", "C"] });
  await client.request("thread/inject_items", {
    threadId: first.threadId,
    items: [message("继续讨论折中表达", "after-merge")],
  });
  const { thread: fork } = await client.request("thread/fork", {
    threadId: first.threadId,
  });
  await client.request("thread/inject_items", {
    threadId: fork.id,
    items: [message("新分支提出第三种风格", "nested-branch")],
  });
  const next = await mergeThreads(client, {
    threadIds: [first.threadId, fork.id],
  });
  const archive = await readEvidence(next.evidence);
  const after = archive.records.find((r) => r.json.includes("继续讨论折中"));
  const branch = archive.records.find((r) => r.json.includes("第三种风格"));
  const nodes = new Map(archive.graph.nodes.map((n) => [n.id, n]));
  assert.deepEqual(nodes.get(after.ref).parents, [first.mergeNodeId]);
  assert.deepEqual(nodes.get(branch.ref).parents, [after.ref]);
});

test("flat legacy merges convert to a graph using the supplied original branch snapshots", async (t) => {
  const { client } = await fakeGraphClient(
    t,
    exampleSources({ stable: false }),
  );
  const old = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "legacy",
  });
  const result = await mergeThreads(client, {
    threadIds: [old.threadId, "B", "C"],
  });
  assert.equal(result.importedItems, 0);
  const archive = await readEvidence(result.evidence);
  const nodeByText = (text) =>
    archive.graph.nodes.find((node) =>
      archive.records.find((r) => r.ref === node.id)?.json.includes(text),
    );
  assert.deepEqual(nodeByText("B偏向").parents, [nodeByText("比较两种").id]);
  assert.deepEqual(nodeByText("C偏向").parents, [nodeByText("读者和语气").id]);
  const repeated = await mergeThreads(client, {
    threadIds: [result.threadId, "B", "C"],
  });
  assert.equal(repeated.reusedGraph, true);
});

test("long single-message branches are chunked without splitting their persistent node identity", async (t) => {
  const { client } = await fakeGraphClient(t, exampleSources({ long: true }));
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    contextTokens: 16384,
    reserveTokens: 2048,
    maxInputBytes: 8000,
  });
  assert.equal(result.presentations[0].presentation, "summary");
  assert.equal(result.presentations[1].presentation, "inline");
  assert.ok(result.modelCalls > 1);
  assert.ok(result.estimatedContextTokens <= result.budget.inputLimit);
  const archive = await readEvidence(result.evidence);
  assert.equal(archive.records.length, 5);
  const large = archive.records.find((r) => r.json.includes("长分支讨论"));
  assert.ok(Buffer.byteLength(large.json) > 100000);
  const calls = client.calls.filter((c) => c.method === "model");
  assert.ok(
    calls.every(
      (c) => Buffer.byteLength(JSON.stringify(c.params.input)) <= 8000,
    ),
  );
  const fragments = calls.flatMap(
    (c) =>
      JSON.parse(c.params.input[0].text.split("\n").at(-1)).fragments ?? [],
  );
  assert.ok(fragments.length > 1);
  assert.deepEqual([...new Set(fragments.map((f) => f.nodeId))], [large.ref]);
  assert.equal(fragments[0].startByte, 0);
  assert.equal(fragments.at(-1).endByte, Buffer.byteLength(large.json));
});

test("compact primary views retain the full graph while creating a bounded fresh conversation", async (t) => {
  const { client } = await fakeGraphClient(
    t,
    exampleSources({ primaryLong: true }),
  );
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    contextTokens: 16384,
    reserveTokens: 2048,
  });
  assert.equal(result.primaryEmbedded, true);
  assert.ok(result.threadId.startsWith("fresh-"));
  assert.equal(result.presentations[0].primary, true);
  assert.ok(result.estimatedContextTokens <= result.budget.inputLimit);
  const expanded = await expandSnapshot(
    await loadSnapshot(client, result.threadId),
  );
  assert.equal(expanded.records.length, 5);
  const repeated = await mergeThreads(client, {
    threadIds: [result.threadId, "B", "C"],
  });
  assert.equal(repeated.reusedGraph, true);
  assert.equal(repeated.modelCalls, 0);
});

test("reference mode and dry-run do not call models, and previews write no archive", async (t) => {
  const { client, directory } = await fakeGraphClient(
    t,
    exampleSources({ long: true }),
  );
  const dry = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    dryRun: true,
    contextTokens: 16384,
  });
  assert.equal(dry.evidence, null);
  assert.equal(dry.analysisPlanned, true);
  assert.ok(
    !client.calls.some(
      (c) => c.method === "model" || c.method === "thread/fork",
    ),
  );
  await assert.rejects(readFile(directory + "/merges/evidence/anything"));
  const refs = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "reference",
  });
  assert.equal(refs.modelCalls, 0);
  assert.ok(refs.presentations.every((v) => v.presentation === "reference"));
});

test("auto falls back to references when compression cannot fit its analysis budget", async (t) => {
  const { client } = await fakeGraphClient(t, exampleSources({ long: true }));
  for (const options of [
    { contextTokens: 5500, reserveTokens: 1000 },
    { maxInputBytes: 1000 },
    { summaryBytes: 200 },
  ]) {
    const result = await mergeThreads(client, {
      threadIds: ["A", "B", "C"],
      ...options,
    });
    assert.equal(result.presentations[0].presentation, "reference");
    assert.equal(result.modelCalls, 0);
    assert.ok(result.estimatedContextTokens <= result.budget.inputLimit);
  }
  await assert.rejects(
    mergeThreads(client, {
      threadIds: ["A", "B"],
      mode: "summary",
      maxInputBytes: 1000,
    }),
    /Insufficient budget/,
  );
});

test("minimum summary inputs still support bounded reduction of heavily escaped messages", async (t) => {
  const root = record(message("讨论语言", "escape-root"));
  const large = record(
    message("引语、反斜杠\\和换行\n".repeat(700), "escape-large", "assistant"),
    "B",
    2,
  );
  const { client } = await fakeGraphClient(t, [
    source("A", [root]),
    source("B", [root, large], "A"),
  ]);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B"],
    mode: "summary",
    maxInputBytes: 2560,
  });
  assert.ok(result.modelCalls > 1);
  assert.ok(
    client.calls
      .filter((c) => c.method === "model")
      .every((c) => Buffer.byteLength(JSON.stringify(c.params.input)) <= 2560),
  );
});

test("summary caches reuse original chunk readings across equivalent merges", async (t) => {
  const { client } = await fakeGraphClient(t);
  const first = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "summary",
  });
  assert.equal(first.modelCalls, 2);
  const second = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "summary",
  });
  assert.equal(second.modelCalls, 0);
  assert.equal(second.reusedSummaries, 2);
});

test("optional semantic reading is a generic narrative with valid original citations", async (t) => {
  const { client } = await fakeGraphClient(t);
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    semantic: true,
    goal: "比较观点，保留分歧",
  });
  assert.equal(result.modelCalls, 1);
  assert.match(result.reading.text, /写作/);
  assert.equal(result.state, undefined);
  assert.equal(result.primaryEmbedded, false);
});

test("invalid summaries create no final target", async (t) => {
  const { client } = await fakeGraphClient(t, exampleSources(), {
    invalid: true,
  });
  await assert.rejects(
    mergeThreads(client, { threadIds: ["A", "B", "C"], mode: "summary" }),
    /unknown original/,
  );
  assert.ok(
    !client.calls.some(
      (c) =>
        c.method === "thread/fork" ||
        (c.method === "thread/start" && !c.params.ephemeral),
    ),
  );
});

test("byte-ranged and branch-paged reads preserve large originals and bound output", async (t) => {
  const { client } = await fakeGraphClient(t, exampleSources({ long: true }));
  const result = await mergeThreads(client, {
    threadIds: ["A", "B", "C"],
    mode: "reference",
  });
  const archive = await readEvidence(result.evidence);
  const large = archive.records.find((r) => r.json.includes("长分支讨论"));
  const part = await exec(
    process.execPath,
    [
      launcher,
      "merge",
      "evidence",
      result.evidence.path,
      large.ref,
      "--start-byte",
      "0",
      "--length-bytes",
      "500",
      "--max-tokens",
      "2048",
      "--json",
    ],
    { env: client.env },
  );
  assert.ok(Buffer.byteLength(part.stdout) < 2048);
  const value = JSON.parse(part.stdout);
  assert.equal(
    value.fragment,
    Buffer.from(large.json).subarray(0, value.endByte).toString("utf8"),
  );
  assert.ok(value.nextByte < value.totalBytes);
  const page = await exec(
    process.execPath,
    [
      launcher,
      "merge",
      "evidence",
      result.evidence.path,
      "--branch",
      "C",
      "--limit",
      "1",
      "--json",
    ],
    { env: client.env },
  );
  assert.equal(JSON.parse(page.stdout).records.length, 1);
  assert.match(JSON.parse(page.stdout).records[0].json, /C偏向/);
});

test("bounded evidence pages only carry metadata for their selected sources", async (t) => {
  const root = record(message("共同讨论", "many-root"));
  const sources = [source("A", [root])];
  for (let i = 0; i < 31; i++)
    sources.push(
      source(
        "branch-" + i,
        [root, record(message("观点" + i, "many-" + i), "branch-" + i, 2)],
        "A",
      ),
    );
  const { client } = await fakeGraphClient(t, sources);
  const result = await mergeThreads(client, {
    threadIds: sources.map((s) => s.thread.id),
    mode: "reference",
  });
  const page = await exec(
    process.execPath,
    [
      launcher,
      "merge",
      "evidence",
      result.evidence.path,
      "--branch",
      "branch-30",
      "--max-tokens",
      "2048",
      "--json",
    ],
    { env: client.env },
  );
  const value = JSON.parse(page.stdout);
  assert.ok(Buffer.byteLength(page.stdout) <= 2048);
  assert.deepEqual(
    value.sources.map((s) => s.id),
    ["branch-30"],
  );
  assert.equal(value.totalRecords, 1);
});

test("whole-context budget uses native occupancy and reserves future dialogue", () => {
  const snapshot = {
    records: [record(message("hello", "one"))],
    tokenUsage: {
      inputTokens: 9000,
      outputTokens: 200,
      atRecordCount: 1,
      contextWindow: 12000,
    },
  };
  const budget = contextBudget(
    snapshot,
    {},
    { contextTokens: 20000, reserveTokens: 1000 },
  );
  assert.equal(budget.window, 12000);
  assert.equal(budget.primaryEstimate, 9200);
  assert.equal(budget.inputLimit, 11000);
  assert.throws(
    () => contextBudget(snapshot, {}, { reserveTokens: 12000 }),
    /smaller/,
  );
});

test("explicit inline mode fails before inference or a target when the whole context cannot fit", async (t) => {
  const { client } = await fakeGraphClient(t, exampleSources({ long: true }));
  await assert.rejects(
    mergeThreads(client, {
      threadIds: ["A", "B", "C"],
      mode: "inline",
      contextTokens: 16000,
    }),
    /exceeding/,
  );
  assert.ok(
    !client.calls.some(
      (c) => c.method === "model" || c.method === "thread/fork",
    ),
  );
});

test("CLI accepts graph view and budget options, rejecting unknown modes and incompatible readings", () => {
  const options = parseMergeArgs([
    "A",
    "B",
    "--mode",
    "summary",
    "--context-tokens",
    "16000",
    "--reserve-tokens",
    "2000",
    "--model",
    "reader",
  ]);
  assert.equal(options.mode, "summary");
  assert.equal(options.reserveTokens, 2000);
  assert.equal(options.model, "reader");
  assert.throws(
    () => parseMergeArgs(["A", "B", "--mode", "wrong"]),
    /Unknown merge mode/,
  );
  assert.throws(
    () => parseMergeArgs(["A", "B", "--mode", "reference", "--semantic"]),
    /requires/,
  );
});
