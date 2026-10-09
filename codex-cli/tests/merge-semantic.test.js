import assert from "node:assert/strict";
import {
  access,
  appendFile,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import {
  mergeThreads as mergeThreadsImpl,
  planMerge,
} from "../bin/merge-context.js";
const mergeThreads = (client, options) =>
  mergeThreadsImpl(client, { mode: "legacy", semantic: false, ...options });
import { parseMergeArgs } from "../bin/merge-cli.js";
import { hash } from "../bin/merge-history.js";
import {
  expandSnapshot,
  forkAnnotationItem,
  parseSemanticItem,
  prepareEvidence,
  readEvidence,
  recordRef,
  saveEvidence,
  SEMANTIC_PREFIX,
} from "../bin/merge-evidence.js";
import {
  prepareSemantic,
  semanticPlan,
  STATE_SCHEMA,
  validateState,
} from "../bin/merge-semantic.js";
import { message, fixtureState } from "./fixtures/semantic.js";
const record = (item, sourceThreadId, ordinal) => {
  const json = typeof item === "string" ? item : JSON.stringify(item);
  return {
    key: sourceThreadId + ":" + ordinal + ":" + hash(json),
    sourceThreadId,
    ordinal,
    turnId: null,
    json,
  };
};
const snapshot = (id, records) => ({
  thread: { id, cwd: "/work", name: id },
  records,
});

function fixture() {
  const common = record(
    message("Add caching; do not change the API", "user-base"),
    "a",
    1,
  );
  const sources = [
    snapshot("a", [common]),
    snapshot("b", [
      common,
      record(message("Check cache performance", "user-b"), "b", 2),
      record(
        {
          type: "function_call_output",
          call_id: "same-id",
          output: "Cache performance passed",
        },
        "b",
        3,
      ),
    ]),
    snapshot("c", [
      common,
      record(message("Check cache correctness", "user-c"), "c", 2),
      record(
        {
          type: "function_call_output",
          call_id: "same-id",
          output: "Cache invalidation FAILED: stale data",
        },
        "c",
        3,
      ),
    ]),
  ];
  const plan = planMerge(sources);
  return { sources, plan, archive: prepareEvidence(sources, plan) };
}

test("semantic extraction retains the common task, each branch delta, and original tool evidence", () => {
  const { sources, plan, archive } = fixture();
  const prepared = prepareSemantic(sources, plan);
  const task = JSON.parse(prepared.input[0].text.split("\n").at(-1));
  assert.equal(task.commonPrefixItems, 1);
  assert.deepEqual(
    task.deltas.map((d) => d.refs.length),
    [2, 2],
  );
  assert.equal(task.primaryRefs.length, 1);
  assert.equal(archive.records.length, 5);
  assert.match(prepared.input[0].text, /stale data/);
  assert.match(JSON.stringify(STATE_SCHEMA), /additionalProperties/);
  assert.equal(
    validateState(fixtureState(archive), archive).conflicts[0].status,
    "unresolved",
  );
});

test("unknown citations, missing user constraints, and missing failed tool results are rejected", () => {
  const { archive } = fixture();
  const invented = fixtureState(archive);
  invented.facts[0].refs = ["invented"];
  assert.throws(() => validateState(invented, archive), /unknown evidence/);
  const omittedUser = fixtureState(archive);
  omittedUser.constraints.pop();
  omittedUser.nextActions[0].refs.pop();
  assert.throws(
    () => validateState(omittedUser, archive),
    /omitted user instructions/,
  );
  const omittedFailure = fixtureState(archive);
  omittedFailure.facts.pop();
  omittedFailure.pending = [];
  omittedFailure.conflicts = [];
  assert.throws(
    () => validateState(omittedFailure, archive),
    /omitted user instructions or tool evidence/,
  );
});

test("assistant claims cannot become verified results without tool evidence", () => {
  const { archive } = fixture();
  const state = fixtureState(archive);
  state.completed = [
    {
      text: "Implementation is done and tested",
      status: "verified",
      scope: "/work",
      refs: [archive.records[0].ref],
    },
  ];
  assert.throws(
    () => validateState(state, archive),
    /must cite an original tool result/,
  );
});

test("model output cannot change the merge goal, lose conflict alternatives, or exceed its summary budget", () => {
  const { archive } = fixture();
  const state = fixtureState(archive);
  assert.throws(
    () => validateState(state, archive, { goal: "Preserve correctness" }),
    /changed the requested merge goal/,
  );
  state.conflicts[0].alternatives.pop();
  assert.throws(
    () => validateState(state, archive),
    /at least two alternatives/,
  );
  assert.throws(
    () => validateState(fixtureState(archive), archive, { summaryBytes: 1 }),
    /summary-bytes/,
  );
  state.conflicts = [];
  state.facts[0].scope = "";
  assert.throws(
    () => validateState(state, archive),
    /explicit historical scope/,
  );
});

test("evidence archives preserve original numeric literals and reject later tampering", async (t) => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-semantic-evidence-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const { sources } = fixture();
  sources[1].records.push(
    record(
      '{"type":"function_call_output","call_id":"large","output":900719925474099312345}',
      "b",
      4,
    ),
  );
  const archive = prepareEvidence(sources, planMerge(sources));
  const evidence = await saveEvidence(archive, dir);
  const saved = await readEvidence(evidence);
  assert.match(saved.records.at(-1).json, /stale data/);
  assert.ok(
    saved.records.some((r) => r.json.includes("900719925474099312345")),
  );
  assert.equal((await stat(evidence.path)).mode & 0o777, 0o600);
  assert.deepEqual(await saveEvidence(archive, dir), evidence);
  await appendFile(evidence.path, " ");
  await assert.rejects(readEvidence(evidence), /checksum mismatch/);
});

test("semantic input size errors never truncate evidence; archived images reach the model", () => {
  const { sources, plan } = fixture();
  assert.throws(
    () => prepareSemantic(sources, plan, { maxInputBytes: 1 }),
    /no evidence was truncated/,
  );
  const image = {
    type: "input_image",
    image_url: "data:image/png;base64,fixture",
  };
  sources[1].records.push(
    record(
      {
        ...message("image evidence", "image-b"),
        content: [{ type: "input_text", text: "image evidence" }, image],
      },
      "b",
      4,
    ),
  );
  const prepared = prepareSemantic(sources, planMerge(sources));
  assert.equal(prepared.input[1].url, image.image_url);
  assert.match(prepared.input[0].text, /imageNumber/);
});

test("semantic dry run calls no model and creates no archive", async () => {
  const { sources, plan } = fixture();
  const client = {
    request: () => assert.fail("dry-run must not start any thread"),
    runTurn: () => assert.fail("no inference"),
  };
  const result = await semanticPlan(client, sources, plan, { dryRun: true });
  assert.equal(result.summary.analysisExecuted, false);
  assert.equal(result.item, null);
  assert.equal(result.summary.contextBytes, 0);
});

test("semantic capsules restore legacy fork identities and preserve repeated-merge deduplication", async (t) => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-semantic-capsule-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const { sources, plan, archive } = fixture();
  const evidence = await saveEvidence(archive, dir);
  const capsule = {
    format: "codex-semantic-merge-v1",
    evidence,
    state: fixtureState(archive),
  };
  const item = message(SEMANTIC_PREFIX + JSON.stringify(capsule), "capsule");
  const copiedBase = record(
    JSON.parse(sources[0].records[0].json),
    "merged",
    1,
  );
  const expanded = await expandSnapshot(
    snapshot("merged", [copiedBase, record(item, "merged", 2)]),
  );
  assert.equal(expanded.records[0].key, sources[0].records[0].key);
  assert.equal(expanded.primarySourceThreadId, "a");
  assert.equal(expanded.primaryKeys.size, 1);
  assert.equal(
    planMerge([expanded, ...sources.slice(1)]).summary.importedItems,
    0,
  );
  await rm(evidence.path);
  await assert.rejects(
    expandSnapshot(snapshot("merged", [copiedBase, record(item, "merged", 2)])),
    /Cannot read merge evidence/,
  );
});

test("malformed semantic provenance fails instead of silently losing its original evidence", () => {
  assert.throws(
    () => parseSemanticItem(message(SEMANTIC_PREFIX + "{}", "bad")),
    /Invalid semantic merge evidence/,
  );
  assert.equal(parseSemanticItem(message("ordinary message", "normal")), null);
});

test("semantic CLI defaults and analysis limits", () => {
  const options = parseMergeArgs([
    "a",
    "b",
    "--goal",
    "preserve API",
    "--model",
    "fixture",
    "--timeout-seconds",
    "60",
  ]);
  assert.equal(options.analysisTimeoutMs, 60000);
  assert.equal(options.summaryBytes, 16384);
  assert.equal(options.semantic, true);
  assert.equal(parseMergeArgs(["a", "b", "--goal", "goal"]).goal, "goal");
  assert.equal(
    parseMergeArgs(["a", "b", "--mode", "reference"]).semantic,
    false,
  );
  assert.throws(
    () => parseMergeArgs(["a", "b", "--semantic"]),
    /Unknown merge option/,
  );
  assert.throws(
    () => parseMergeArgs(["a", "b", "--summary-bytes", "0"]),
    /positive/,
  );
  assert.throws(
    () => parseMergeArgs(["a", "b", "--timeout-seconds", "999999999"]),
    /too large/,
  );
});

async function fakeClient(t, transform) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-semantic-pipeline-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const { sources } = fixture();
  const threads = new Map();
  const calls = [];
  let next = 0;
  const persist = async (id, items) => {
    const file = path.join(dir, id + ".jsonl");
    const lines = [
      { type: "session_meta", payload: { id } },
      ...items.map((item) => ({ type: "response_item", payload: item })),
    ];
    await writeFile(
      file,
      lines.map((line) => JSON.stringify(line)).join("\n") + "\n",
    );
    threads.set(id, {
      id,
      path: file,
      cwd: "/work",
      ephemeral: false,
      status: { type: "notLoaded" },
    });
    return threads.get(id);
  };
  for (const source of sources)
    await persist(
      source.thread.id,
      source.records.map((r) => JSON.parse(r.json)),
    );
  const client = {
    env: { CODEX_HOME: dir },
    calls,
    async request(method, params) {
      calls.push({ method, params });
      if (method === "thread/read")
        return { thread: threads.get(params.threadId) };
      if (method === "config/read")
        return {
          config: {
            mcp_servers: {
              "mock.server": { command: "node", tool_timeout_sec: null },
            },
          },
        };
      if (method === "thread/start")
        return {
          thread: { id: "analysis-" + ++next, ephemeral: true },
          model: "fixture",
          modelProvider: "fixture",
          reasoningEffort: null,
        };
      if (method === "thread/fork") {
        const parent = await readFile(
          threads.get(params.threadId).path,
          "utf8",
        );
        const items = parent
          .trim()
          .split("\n")
          .slice(1)
          .map((line) => JSON.parse(line).payload);
        const fork = await persist("merged-" + ++next, items);
        if (client.onFork) await client.onFork(fork, params);
        return { thread: fork };
      }
      if (method === "thread/inject_items") {
        await appendFile(
          threads.get(params.threadId).path,
          params.items
            .map((item) =>
              JSON.stringify({ type: "response_item", payload: item }),
            )
            .join("\n") + "\n",
        );
      }
      return {};
    },
    async runTurn(params) {
      calls.push({ method: "model", params });
      const task = JSON.parse(params.input[0].text.split("\n").at(-1));
      const state = fixtureState(task, task.requestedGoal ?? undefined);
      return { text: transform ? transform(state) : JSON.stringify(state) };
    },
  };
  return { client, dir };
}

test("pipeline calls the model before a durable fork, preserves originals, and reuses unchanged analysis", async (t) => {
  const { client, dir } = await fakeClient(t);
  const before = await Promise.all(
    ["a", "b", "c"].map((id) =>
      readFile(path.join(dir, id + ".jsonl"), "utf8"),
    ),
  );
  const first = await mergeThreads(client, {
    threadIds: ["a", "b", "c"],
    semantic: true,
  });
  assert.equal(first.analysisExecuted, true);
  assert.ok(
    client.calls.findIndex((c) => c.method === "model") <
      client.calls.findIndex((c) => c.method === "thread/fork"),
  );
  const config = client.calls.find((c) => c.method === "thread/start").params
    .config;
  assert.equal(config.mcp_servers["mock.server"].enabled, false);
  for (const [i, id] of ["a", "b", "c"].entries())
    assert.equal(
      await readFile(path.join(dir, id + ".jsonl"), "utf8"),
      before[i],
    );
  const again = await mergeThreads(client, {
    threadIds: [first.threadId, "b", "c"],
    semantic: true,
  });
  assert.equal(again.importedItems, 0);
  assert.equal(again.reusedAnalysis, true);
  assert.equal(client.calls.filter((c) => c.method === "model").length, 1);
  assert.equal(
    client.calls.filter((c) => c.method === "thread/inject_items").length,
    1,
  );
  const changedGoal = await mergeThreads(client, {
    threadIds: [again.threadId, "b"],
    semantic: true,
    goal: "Prioritize cache correctness",
  });
  assert.equal(changedGoal.analysisExecuted, true);
  assert.equal(changedGoal.state.goal, "Prioritize cache correctness");
  assert.equal(client.calls.filter((c) => c.method === "model").length, 2);
  const restoredGoal = await mergeThreads(client, {
    threadIds: [changedGoal.threadId, "b"],
    semantic: true,
  });
  assert.equal(restoredGoal.analysisExecuted, true);
  assert.equal(restoredGoal.reusedAnalysis, false);
  assert.equal(restoredGoal.state.goal, first.state.goal);
  assert.equal(client.calls.filter((c) => c.method === "model").length, 3);
  const latestInjection = client.calls
    .filter((c) => c.method === "thread/inject_items")
    .at(-1);
  assert.equal(
    parseSemanticItem(latestInjection.params.items.at(-1)).state.goal,
    first.state.goal,
  );
});

test("invalid model output leaves no final target or injection", async (t) => {
  const { client } = await fakeClient(t, (state) => {
    state.facts[0].refs = ["hallucinated"];
    return JSON.stringify(state);
  });
  await assert.rejects(
    mergeThreads(client, { threadIds: ["a", "b", "c"], semantic: true }),
    /unknown evidence/,
  );
  assert.ok(
    !client.calls.some(
      (c) => c.method === "thread/fork" || c.method === "thread/inject_items",
    ),
  );
  assert.ok(client.calls.some((c) => c.method === "thread/unsubscribe"));
});

test("source-only preview performs no analysis, archive writes, or target creation", async (t) => {
  const { client, dir } = await fakeClient(t);
  const preview = await mergeThreads(client, {
    threadIds: ["a", "b", "c"],
    semantic: true,
    dryRun: true,
  });
  assert.equal(preview.analysisExecuted, false);
  assert.deepEqual(
    [...new Set(client.calls.map((c) => c.method))],
    ["thread/read"],
  );
  await assert.rejects(access(path.join(dir, "merges")));
});

const nativeInterruption = () => ({
  type: "message",
  id: "native-fork-interruption",
  role: "user",
  content: [
    {
      type: "input_text",
      text: "<turn_aborted>\nThe user interrupted the previous turn on purpose. Any tools may have partially executed.\n</turn_aborted>",
    },
  ],
  internal_chat_message_metadata_passthrough: {
    content_item_kinds: ["generic.turn_aborted"],
  },
});

test("native fork metadata is retained while genuine source interruptions remain evidence", async (t) => {
  const { client } = await fakeClient(t);
  client.onFork = async (fork) => {
    await appendFile(
      fork.path,
      JSON.stringify({ type: "response_item", payload: nativeInterruption() }) +
        "\n",
    );
  };
  const first = await mergeThreads(client, {
    threadIds: ["a", "b", "c"],
    semantic: true,
  });
  const next = await mergeThreads(client, {
    threadIds: [first.threadId, "b", "c"],
    semantic: true,
  });
  assert.equal(next.reusedAnalysis, true);
  assert.equal(client.calls.filter((c) => c.method === "model").length, 1);
  const interruption = record(
    nativeInterruption(),
    "real-user-interruption",
    1,
  );
  assert.equal(
    (await expandSnapshot(snapshot("real-user-interruption", [interruption])))
      .records.length,
    1,
  );
  const annotated = record(forkAnnotationItem([interruption]), "merge", 2);
  assert.equal(
    (await expandSnapshot(snapshot("merge", [interruption, annotated]))).records
      .length,
    0,
  );
});

test("a source change resembling a native interruption still rejects and archives the target", async (t) => {
  const { client, dir } = await fakeClient(t);
  client.onFork = async (fork, params) => {
    const line =
      JSON.stringify({ type: "response_item", payload: nativeInterruption() }) +
      "\n";
    await appendFile(fork.path, line);
    await appendFile(path.join(dir, params.threadId + ".jsonl"), line);
  };
  await assert.rejects(
    mergeThreads(client, { threadIds: ["a", "b", "c"], semantic: true }),
    /Base thread changed before fork/,
  );
  assert.ok(client.calls.some((c) => c.method === "thread/archive"));
  assert.ok(!client.calls.some((c) => c.method === "thread/inject_items"));
});
