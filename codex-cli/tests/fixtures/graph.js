import assert from "node:assert/strict";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { hash, recordRef } from "../../bin/merge-history.js";

export const message = (text, id, role = "user") => ({
  type: "message",
  ...(id ? { id } : {}),
  role,
  content: [{ type: role === "user" ? "input_text" : "output_text", text }],
});
export const record = (item, source = "A", ordinal = 1) => {
  const json = typeof item === "string" ? item : JSON.stringify(item);
  const data = JSON.parse(json);
  return {
    key: data.id
      ? data.type + ":" + data.id + ":" + hash(json)
      : source + ":" + ordinal + ":" + hash(json),
    json,
    sourceThreadId: source,
    ordinal,
    fingerprint: hash(json),
    turnId: null,
  };
};
export const source = (id, records, parent) => ({
  thread: { id, cwd: process.cwd(), forkedFromId: parent ?? null },
  records,
});
export function exampleSources({
  stable = true,
  long = false,
  primaryLong = false,
} = {}) {
  const a1 = record(
    message("请比较两种写作风格，保留不同观点。", stable ? "a1" : undefined),
    "A",
    1,
  );
  const a2 = record(
    message(
      "主线讨论了读者和语气。" +
        (primaryLong ? "中文长主线。".repeat(5000) : ""),
      stable ? "a2" : undefined,
      "assistant",
    ),
    "A",
    2,
  );
  const a3 = record(
    message("主线希望保留两种候选。", stable ? "a3" : undefined),
    "A",
    3,
  );
  const b2 = record(
    message(
      "B偏向克制表达，但这是建议，尚未定稿。" +
        (long ? "长分支讨论，保留引语与限定条件。".repeat(6000) : ""),
      stable ? "b2" : undefined,
      "assistant",
    ),
    "B",
    2,
  );
  const c3 = record(
    message(
      "C偏向有力表达，仍需保留不确定性。",
      stable ? "c3" : undefined,
      "assistant",
    ),
    "C",
    3,
  );
  const copy = (r, id, ordinal) => (stable ? r : record(r.json, id, ordinal));
  return [
    source("A", [a1, a2, a3]),
    source("B", [copy(a1, "B", 1), b2], "A"),
    source("C", [copy(a1, "C", 1), copy(a2, "C", 2), c3], "A"),
  ];
}

export function fixtureReading(task) {
  let refs =
    task.fragments?.map((f) => f.nodeId) ??
    task.readings?.flatMap((r) => r.refs) ??
    [];
  if (!refs.length) {
    refs = [
      ...(task.primary?.records?.map((r) => r.ref) ?? []),
      ...(task.primary?.refs ?? []),
      ...(task.primary?.summary?.refs ?? []),
      ...task.branches.flatMap((b) => [
        ...(b.nodes?.filter((n) => n.json).map((n) => n.id) ?? []),
        ...(b.summary?.refs ?? []),
        ...(b.reading?.refs ?? []),
      ]),
    ];
  }
  return {
    text: "这些对话包含不同写作建议；保留观点的归属、限定条件与未解决分歧。",
    refs: [...new Set(refs)].slice(0, 2),
  };
}

export async function fakeGraphClient(
  t,
  sources = exampleSources(),
  { invalid = false, notices = false, config = {} } = {},
) {
  const directory = await mkdtemp(
    path.join(os.tmpdir(), "codex-graph-fixture-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  const threads = new Map();
  let next = 0;
  const calls = [];
  const create = async (id, items, parent, cwd = process.cwd()) => {
    const thread = {
      id,
      cwd,
      name: null,
      ephemeral: false,
      historyMode: "legacy",
      forkedFromId: parent ?? null,
      path: path.join(directory, id + ".jsonl"),
      status: { type: "idle" },
    };
    await writeFile(
      thread.path,
      [
        JSON.stringify({ type: "session_meta", payload: { id } }),
        ...items.map(
          (item) => '{"type":"response_item","payload":' + item + "}",
        ),
      ].join("\n") + "\n",
    );
    threads.set(id, thread);
    return thread;
  };
  for (const snapshot of sources)
    await create(
      snapshot.thread.id,
      snapshot.records.map((r) => r.json),
      snapshot.thread.forkedFromId,
    );
  const client = {
    env: { ...process.env, CODEX_HOME: directory },
    calls,
    threads,
    request: async (method, params) => {
      calls.push({ method, params });
      if (method === "config/read") return { config };
      if (method === "thread/read") {
        const thread = threads.get(params.threadId);
        assert.ok(thread, params.threadId);
        return { thread };
      }
      if (method === "thread/start") {
        const id = (params.ephemeral ? "analysis-" : "fresh-") + ++next;
        if (params.ephemeral)
          return {
            thread: { id, ephemeral: true },
            model: params.model ?? "fixture-reader",
            modelProvider: "fixture",
            reasoningEffort: "low",
          };
        return { thread: await create(id, [], null, params.cwd) };
      }
      if (method === "thread/fork") {
        const base = threads.get(params.threadId);
        const id = "fork-" + ++next;
        const text = await readFile(base.path, "utf8");
        const items = text
          .trim()
          .split("\n")
          .slice(1)
          .map((line) => JSON.parse(line).payload);
        const fork = await create(
          id,
          items.map(JSON.stringify),
          base.id,
          params.cwd ?? base.cwd,
        );
        if (notices)
          await appendFile(
            fork.path,
            JSON.stringify({
              type: "response_item",
              payload: {
                type: "message",
                role: "user",
                content: [
                  {
                    type: "input_text",
                    text: "<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>",
                  },
                ],
                internal_chat_message_metadata_passthrough: {
                  content_item_kinds: ["generic.turn_aborted"],
                },
              },
            }) + "\n",
          );
        if (client.onFork) await client.onFork(fork, params);
        return { thread: fork };
      }
      if (method === "thread/inject_items") {
        await appendFile(
          threads.get(params.threadId).path,
          params.items
            .map((payload) =>
              JSON.stringify({ type: "response_item", payload }),
            )
            .join("\n") + "\n",
        );
        return {};
      }
      if (method === "thread/name/set") {
        threads.get(params.threadId).name = params.name;
        return {};
      }
      if (["thread/unsubscribe", "thread/archive"].includes(method)) return {};
      throw new Error("Unsupported fixture RPC " + method);
    },
    runTurn: async (params) => {
      calls.push({ method: "model", params });
      const task = JSON.parse(params.input[0].text.split("\n").at(-1));
      const reading = fixtureReading(task);
      if (invalid) reading.refs = ["invented-node"];
      return { text: JSON.stringify(reading) };
    },
  };
  return { client, directory };
}
export { recordRef };
