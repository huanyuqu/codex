import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import test from "node:test";
import * as zlib from "node:zlib";
import { parseMergeArgs } from "../bin/merge-cli.js";
import {
  mergeThreads as mergeThreadsImpl,
  planMerge,
} from "../bin/merge-context.js";
const mergeThreads = (client, options) =>
  mergeThreadsImpl(client, { mode: "legacy", semantic: false, ...options });
import {
  hash,
  loadSnapshot,
  MERGE_PREFIX,
  rawArrayItems,
  rawField,
} from "../bin/merge-history.js";

const message = (text, role = "user", id) => ({
  type: "message",
  ...(id ? { id } : {}),
  role,
  content: [
    { type: role === "assistant" ? "output_text" : "input_text", text },
  ],
});
function record(text, origin = "a", index = 1) {
  const json = JSON.stringify(message(text));
  return {
    key: `${origin}:${index}:${hash(json)}`,
    fingerprint: hash(json),
    sourceThreadId: origin,
    ordinal: index,
    turnId: null,
    json,
  };
}
const snapshot = (id, records) => ({
  thread: { id, cwd: `/work/${id}`, name: null },
  records,
});

test("three siblings preserve branch order and omit shared prefixes", () => {
  const shared = record("shared");
  const plan = planMerge([
    snapshot("a", [shared]),
    snapshot("b", [shared, record("B", "b")]),
    snapshot("c", [shared, record("C", "c")]),
  ]);
  assert.equal(plan.summary.importedItems, 2);
  assert.equal(plan.summary.skippedItems, 2);
  assert.deepEqual(
    plan.bundle.branches.map(
      (b) => JSON.parse(b.records[0].json).content[0].text,
    ),
    ["B", "C"],
  );
  assert.equal(plan.item.role, "user");
});

test("legacy siblings with distinct fallback keys deduplicate only the prefix", () => {
  const plan = planMerge([
    snapshot("a", [record("shared")]),
    snapshot("b", [record("shared", "b"), record("shared", "b", 2)]),
  ]);
  assert.equal(plan.summary.importedItems, 1);
  assert.equal(plan.summary.skippedItems, 1);
});

test("identical messages after divergence remain separate", () => {
  const plan = planMerge([
    snapshot("a", [record("A"), record("repeated", "a", 2)]),
    snapshot("b", [record("B", "b"), record("repeated", "b", 2)]),
  ]);
  assert.equal(plan.summary.importedItems, 2);
});

test("common histories between later sources are deduplicated", () => {
  const b = [record("B", "b"), record("b-only", "b", 2)];
  const plan = planMerge([
    snapshot("a", [record("A")]),
    snapshot("b", b),
    snapshot("c", [...b, record("C", "c", 3)]),
  ]);
  assert.equal(plan.summary.importedItems, 3);
  assert.equal(plan.summary.skippedItems, 2);
});

test("merging the same source again imports no duplicate records", () => {
  const a = snapshot("a", [record("A")]);
  const b = snapshot("b", [record("B", "b")]);
  const first = planMerge([a, b]);
  const marker = { ...record("", "merged"), json: JSON.stringify(first.item) };
  const next = planMerge([snapshot("merged", [...a.records, marker]), b]);
  assert.equal(next.summary.importedItems, 0);
});

test("merged sources flatten their prior branch records", () => {
  const first = planMerge([
    snapshot("b", [record("B", "b")]),
    snapshot("c", [record("C", "c")]),
  ]);
  const merged = snapshot("merged", [
    record("B", "b"),
    { ...record("", "merged"), json: JSON.stringify(first.item) },
  ]);
  const plan = planMerge([snapshot("a", [record("A")]), merged]);
  assert.equal(plan.summary.importedItems, 2);
  assert.equal(plan.bundle.branches[0].records[1].sourceThreadId, "c");
  assert.equal(JSON.stringify(plan.bundle).split(MERGE_PREFIX).length, 1);
});

test("conflicting conclusions and colliding tool call IDs are retained", () => {
  const make = (origin, output) => {
    const json = JSON.stringify({
      type: "function_call_output",
      call_id: "collision",
      output,
    });
    return { key: `${origin}:1:${hash(json)}`, sourceThreadId: origin, json };
  };
  const plan = planMerge([
    snapshot("a", [record("A")]),
    snapshot("b", [make("b", "42")]),
    snapshot("c", [make("c", "99")]),
  ]);
  assert.equal(plan.summary.importedItems, 2);
  assert.match(plan.bundle.branches[0].records[0].json, /42/);
  assert.match(plan.bundle.branches[1].records[0].json, /99/);
});

test("branch images are attached as native image inputs, and remain deduplicable", () => {
  const image = {
    type: "input_image",
    image_url: "data:image/png;base64,fixture",
  };
  const imageRecord = record("look at this", "b");
  imageRecord.json = JSON.stringify({
    ...message("look at this"),
    content: [message("look at this").content[0], image],
  });
  const a = snapshot("a", []);
  const b = snapshot("b", [imageRecord]);
  const plan = planMerge([a, b]);
  assert.deepEqual(plan.item.content[1], image);
  assert.equal(plan.summary.importedImages, 1);
  assert.equal(plan.bundle.imageSources[0].sourceThreadId, "b");
  const saved = snapshot("merged", [
    { ...record("", "merged"), json: JSON.stringify(plan.item) },
  ]);
  assert.equal(planMerge([saved, b]).summary.importedImages, 0);
  assert.throws(
    () => planMerge([a, b], { maxBytes: plan.summary.contextBytes - 1 }),
    /exceeding/,
  );
});

test("malformed provenance is preserved as an ordinary user message", () => {
  const malformed = record(
    `${MERGE_PREFIX}{"format":"codex-context-merge-v1","branches":[{}]}`,
    "b",
  );
  assert.equal(
    planMerge([snapshot("a", []), snapshot("b", [malformed])]).summary
      .importedItems,
    1,
  );
});

test("empty branches and deterministic output", () => {
  const sources = [snapshot("a", []), snapshot("b", [])];
  const first = planMerge(sources);
  assert.equal(first.summary.importedItems, 0);
  assert.deepEqual(planMerge(sources), first);
});

test("limits are measured in UTF-8 bytes without silent truncation", () => {
  const sources = [snapshot("a", []), snapshot("b", [record("中文内容", "b")])];
  const expected = planMerge(sources).summary.contextBytes;
  assert.doesNotThrow(() => planMerge(sources, { maxBytes: expected }));
  assert.throws(
    () => planMerge(sources, { maxBytes: expected - 1 }),
    /exceeding/,
  );
  for (const maxBytes of [0, -1, NaN, Infinity, 1.5])
    assert.throws(
      () => planMerge(sources, { maxBytes }),
      /positive safe integer/,
    );
});

test("source count and identity validation", () => {
  assert.throws(() => planMerge([snapshot("a", [])]), /between 2 and 32/);
  assert.throws(
    () => planMerge([snapshot("a", []), snapshot("a", [])]),
    /distinct/,
  );
});

test("raw JSON fields and array values preserve large numbers and escaped delimiters", () => {
  const json =
    '{"other":"payload\\\": inside", "payload": {"output":9007199254740993,"s":"x, } ] \\\""}}';
  assert.equal(
    rawField(json, "payload"),
    '{"output":9007199254740993,"s":"x, } ] \\\""}',
  );
  assert.equal(rawField(json, "missing"), undefined);
  assert.deepEqual(
    rawArrayItems('[{"a":9007199254740993}, "comma, ]", [1,2]]'),
    ['{"a":9007199254740993}', '"comma, ]"', "[1,2]"],
  );
  assert.deepEqual(rawArrayItems("[]"), []);
});

async function fixture(t, records, id = "a") {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-merge-unit-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const file = path.join(dir, `${id}.jsonl`);
  const lines = [{ type: "session_meta", payload: { id } }, ...records];
  await writeFile(
    file,
    lines
      .map((line, ordinal) => JSON.stringify({ ordinal, ...line }))
      .join("\n") + "\n",
  );
  const thread = {
    id,
    path: file,
    cwd: dir,
    ephemeral: false,
    status: { type: "notLoaded" },
  };
  const client = { request: async () => ({ thread }) };
  return { file, thread, client, dir };
}

test("read message/tool context while keeping source instructions out of imported data", async (t) => {
  const f = await fixture(t, [
    { type: "response_item", payload: message("system policy", "system") },
    {
      type: "response_item",
      payload: message("developer policy", "developer"),
    },
    {
      type: "response_item",
      payload: message(
        "<environment_context>old workspace</environment_context>",
      ),
    },
    { type: "response_item", payload: message("user request") },
    {
      type: "response_item",
      payload: {
        type: "function_call",
        call_id: "x",
        name: "shell",
        arguments: '{"cmd":"echo 42"}',
      },
    },
    {
      type: "response_item",
      payload: { type: "function_call_output", call_id: "x", output: "42" },
    },
    { type: "response_item", payload: message("answer", "assistant") },
  ]);
  const before = await readFile(f.file, "utf8");
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 4);
  assert.equal(await readFile(f.file, "utf8"), before);
});

test("referenced fork reads the frozen ancestor prefix, not later ancestor changes", async (t) => {
  const f = await fixture(
    t,
    [{ type: "response_item", payload: message("shared") }],
    "a",
  );
  const original = await readFile(f.file);
  await writeFile(
    f.file,
    Buffer.concat([
      original,
      Buffer.from(
        JSON.stringify({
          ordinal: 2,
          type: "response_item",
          payload: message("later parent change"),
        }) + "\n",
      ),
    ]),
  );
  const child = path.join(f.dir, "b.jsonl");
  await writeFile(
    child,
    JSON.stringify({
      ordinal: 0,
      type: "session_meta",
      payload: {
        id: "b",
        history_base: {
          thread_id: "a",
          end_byte_offset: original.length,
          end_ordinal_exclusive: 2,
        },
      },
    }) +
      "\n" +
      JSON.stringify({
        ordinal: 2,
        type: "response_item",
        payload: message("branch"),
      }) +
      "\n",
  );
  const client = {
    request: async (_method, { threadId }) => ({
      thread: {
        ...f.thread,
        id: threadId,
        path: threadId === "a" ? f.file : child,
      },
    }),
  };
  const s = await loadSnapshot(client, "b");
  assert.equal(s.records.length, 2);
  assert.equal(s.records[0].sourceThreadId, "a");
  assert.equal(s.records[1].sourceThreadId, "b");
  assert.doesNotMatch(JSON.stringify(s.records), /later parent change/);
});

test("compaction imports the replacement context and preserves exact numeric literals", async (t) => {
  const f = await fixture(t, [
    { type: "response_item", payload: message("discarded") },
  ]);
  await writeFile(
    f.file,
    (await readFile(f.file, "utf8")) +
      '{"ordinal":2,"type":"compacted","payload":{"replacement_history":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"policy"}]},{"type":"function_call_output","call_id":"x","output":9007199254740993}]}}\n',
  );
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 1);
  assert.match(s.records[0].json, /9007199254740993/);
  assert.doesNotMatch(s.records[0].json, /discarded|policy/);
});

test("legacy compaction keeps user text and its summary but drops old answers", async (t) => {
  const f = await fixture(t, [
    { type: "response_item", payload: message("user constraint") },
    {
      type: "response_item",
      payload: message("discarded answer", "assistant"),
    },
    { type: "compacted", payload: { message: "summary" } },
  ]);
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 2);
  assert.match(s.records[0].json, /user constraint/);
  assert.match(s.records[1].json, /summary/);
  assert.doesNotMatch(JSON.stringify(s.records), /discarded answer/);
});

test("rollback does not resurrect removed turns", async (t) => {
  const f = await fixture(t, [
    { type: "turn_context", payload: { turn_id: "turn-a" } },
    { type: "response_item", payload: message("keep") },
    { type: "turn_context", payload: { turn_id: "turn-b" } },
    { type: "response_item", payload: message("remove") },
    {
      type: "event_msg",
      payload: { type: "thread_rolled_back", num_turns: 1 },
    },
  ]);
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 1);
  assert.match(s.records[0].json, /keep/);
});

test("rollback counts steering input boundaries even within the same turn ID", async (t) => {
  const f = await fixture(t, [
    { type: "turn_context", payload: { turn_id: "same-turn" } },
    { type: "response_item", payload: message("initial question") },
    { type: "response_item", payload: message("steering prompt") },
    {
      type: "response_item",
      payload: message("discarded output", "assistant"),
    },
    {
      type: "event_msg",
      payload: { type: "thread_rolled_back", num_turns: 1 },
    },
  ]);
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 1);
  assert.match(s.records[0].json, /initial question/);
});

test("ambiguous rollback across a compaction checkpoint is rejected", async (t) => {
  const f = await fixture(t, [
    {
      type: "compacted",
      payload: { replacement_history: [message("checkpoint")] },
    },
    {
      type: "event_msg",
      payload: { type: "thread_rolled_back", num_turns: 1 },
    },
  ]);
  await assert.rejects(
    loadSnapshot(f.client, "a"),
    /canonical checkpoint replay/,
  );
});

test("inter-agent messages remain in branch context", async (t) => {
  const f = await fixture(t, [
    {
      type: "inter_agent_communication",
      payload: { sender: "worker", message: "independent finding" },
    },
  ]);
  const s = await loadSnapshot(f.client, "a");
  assert.equal(s.records.length, 1);
  assert.match(s.records[0].json, /independent finding/);
});

test("tool correlation IDs are scoped to their source even for identical calls", async (t) => {
  const call = {
    type: "response_item",
    payload: {
      type: "function_call",
      call_id: "collision",
      name: "exec_command",
      arguments: "{}",
    },
  };
  const a = await fixture(t, [call], "a");
  const b = await fixture(t, [call], "b");
  const left = await loadSnapshot(a.client, "a");
  const right = await loadSnapshot(b.client, "b");
  assert.equal(left.records[0].json, right.records[0].json);
  assert.notEqual(left.records[0].key, right.records[0].key);
});

test("unfinished turns and live active sources are rejected", async (t) => {
  const f = await fixture(t, [
    { type: "event_msg", payload: { type: "task_started", turn_id: "turn-a" } },
  ]);
  await assert.rejects(loadSnapshot(f.client, "a"), /unfinished turn/);
  f.thread.status.type = "active";
  await assert.rejects(loadSnapshot(f.client, "a"), /idle/);
});

test("an interrupted turn is a usable snapshot", async (t) => {
  const f = await fixture(t, [
    { type: "event_msg", payload: { type: "task_started" } },
    { type: "response_item", payload: message("partial work") },
    { type: "event_msg", payload: { type: "turn_aborted" } },
  ]);
  assert.equal((await loadSnapshot(f.client, "a")).records.length, 1);
});

test("incomplete records, wrong identities, ephemeral sources, and cyclic references fail closed", async (t) => {
  const f = await fixture(t, []);
  await writeFile(f.file, '{"type":"session_meta","payload":{"id":"a"}}');
  await assert.rejects(loadSnapshot(f.client, "a"), /Incomplete rollout/);
  await writeFile(f.file, '{"type":"session_meta","payload":{"id":"wrong"}}\n');
  await assert.rejects(loadSnapshot(f.client, "a"), /identity mismatch/);
  f.thread.ephemeral = true;
  await assert.rejects(loadSnapshot(f.client, "a"), /persisted rollout/);
  f.thread.ephemeral = false;
  await writeFile(
    f.file,
    '{"type":"session_meta","payload":{"id":"a","history_base":{"thread_id":"a","end_byte_offset":0,"end_ordinal_exclusive":1}}}\n',
  );
  await assert.rejects(loadSnapshot(f.client, "a"), /Cyclic/);
});

test(
  "compressed local rollouts are supported when Node exposes zstd",
  { skip: !zlib.zstdCompressSync },
  async (t) => {
    const f = await fixture(t, [
      { type: "response_item", payload: message("compressed") },
    ]);
    await writeFile(
      `${f.file}.zst`,
      zlib.zstdCompressSync(await readFile(f.file)),
    );
    await rm(f.file);
    assert.equal((await loadSnapshot(f.client, "a")).records.length, 1);
  },
);

test("dry run reads sources but never forks, injects, or names a thread", async (t) => {
  const a = await fixture(
    t,
    [{ type: "response_item", payload: message("A") }],
    "a",
  );
  const b = await fixture(
    t,
    [{ type: "response_item", payload: message("B") }],
    "b",
  );
  const methods = [];
  const client = {
    request: async (method, { threadId }) => {
      methods.push(method);
      return { thread: threadId === "a" ? a.thread : b.thread };
    },
  };
  const preview = await mergeThreads(client, {
    threadIds: ["a", "b"],
    dryRun: true,
  });
  assert.equal(preview.dryRun, true);
  assert.deepEqual(methods, ["thread/read", "thread/read"]);
});

test("base changes between preflight and fork archive the fresh fork", async (t) => {
  const a = await fixture(
    t,
    [{ type: "response_item", payload: message("A") }],
    "a",
  );
  const b = await fixture(
    t,
    [{ type: "response_item", payload: message("B") }],
    "b",
  );
  const target = await fixture(
    t,
    [{ type: "response_item", payload: message("changed A") }],
    "merged",
  );
  const methods = [];
  const client = {
    request: async (method, params) => {
      methods.push(method);
      if (method === "thread/fork") return { thread: target.thread };
      if (method === "thread/archive") return {};
      return {
        thread: { a: a.thread, b: b.thread, merged: target.thread }[
          params.threadId
        ],
      };
    },
  };
  await assert.rejects(
    mergeThreads(client, { threadIds: ["a", "b"] }),
    /Base thread changed.*archived/,
  );
  assert.equal(methods.at(-1), "thread/archive");
  assert.ok(!methods.includes("thread/inject_items"));
});

test("argument parsing forwards literal names, config, and directory without shell interpretation", () => {
  const options = parseMergeArgs([
    "a",
    "b",
    "c",
    "--name",
    "$(echo private) `literal`",
    "-c",
    'model="gpt-6"',
    "-C",
    ".",
    "--json",
  ]);
  assert.deepEqual(options.threadIds, ["a", "b", "c"]);
  assert.equal(options.name, "$(echo private) `literal`");
  assert.deepEqual(options.config, ['model="gpt-6"']);
  assert.equal(options.cwd, process.cwd());
  assert.equal(options.json, true);
});

test("invalid arguments fail before the app-server starts", () => {
  for (const args of [
    ["a"],
    ["a", "a"],
    ["a", "b", "--wat"],
    ["a", "b", "--name"],
    ["a", "b", "--max-bytes", "1.5"],
    ["a", "b", "--name", " "],
    ["a", "b", "--dry-run", "--resume"],
  ])
    assert.throws(() => parseMergeArgs(args));
  assert.equal(parseMergeArgs(["--help"]).help, true);
});
