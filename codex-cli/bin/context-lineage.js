import { mkdir, open, readdir, rename, rm, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import os from "node:os";
import path from "node:path";
import { hash, snapshotVersion } from "./merge-history.js";
import { parseGraphItem } from "./merge-graph.js";

export const LINEAGE_FORMAT = "codex-session-lineage-v1";
const MAX_INDEX_BYTES = 128 * 1024;
const directory = (env = process.env) =>
  path.join(
    env.CODEX_HOME ?? path.join(os.homedir(), ".codex"),
    "merges",
    "lineage",
  );
const clean = (value) => String(value ?? "").replace(/[\x00-\x1f\x7f]/g, " ");
const validId = (id) =>
  typeof id === "string" && id.length > 0 && id.length <= 256;

export function validateMergeLineage(entry) {
  if (
    entry?.format !== LINEAGE_FORMAT ||
    !validId(entry.threadId) ||
    !Array.isArray(entry.sources) ||
    entry.sources.length < 2 ||
    entry.sources.length > 32 ||
    entry.sources.some(
      (source) => !validId(source.id) || source.id === entry.threadId,
    ) ||
    new Set(entry.sources.map((source) => source.id)).size !==
      entry.sources.length ||
    (entry.frameFingerprint !== undefined &&
      !/^[a-f0-9]{64}$/.test(entry.frameFingerprint))
  )
    throw new Error("Invalid session merge lineage");
  return entry;
}

// Evidence is written before the native target exists. Bind the frozen merge
// to that target, including compact fresh starts and later context compaction.
export async function saveMergeLineage(
  env,
  threadId,
  snapshots,
  summary,
  persisted,
) {
  const frame = persisted?.records.findLast(
    (record) =>
      parseGraphItem(JSON.parse(record.json))?.mergeNodeId ===
      summary.mergeNodeId,
  );
  const entry = validateMergeLineage({
    format: LINEAGE_FORMAT,
    threadId,
    sources: snapshots.map(({ thread, records }) => ({
      id: thread.id,
      name: thread.name ?? null,
      forkedFromId: thread.forkedFromId ?? null,
      snapshot: snapshotVersion(records),
    })),
    evidence: summary.evidence ?? null,
    ...(frame ? { frameFingerprint: hash(frame.json) } : {}),
  });
  const text = JSON.stringify(entry) + "\n";
  if (Buffer.byteLength(text) > MAX_INDEX_BYTES)
    throw new Error("Oversized lineage index");
  const root = directory(env);
  await mkdir(root, { recursive: true, mode: 0o700 });
  const file = path.join(root, encodeURIComponent(threadId) + ".json");
  const temporary = file + "." + randomUUID() + ".tmp";
  try {
    await writeFile(temporary, text, { flag: "wx", mode: 0o600 });
    await rename(temporary, file);
  } finally {
    await rm(temporary, { force: true });
  }
  return entry;
}

export async function readMergeLineage(env, threadId) {
  if (!validId(threadId)) throw new Error("Invalid conversation ID");
  let handle;
  try {
    handle = await open(
      path.join(directory(env), encodeURIComponent(threadId) + ".json"),
      "r",
    );
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size > MAX_INDEX_BYTES)
      throw new Error("Oversized lineage index");
    const entry = validateMergeLineage(
      JSON.parse(await handle.readFile("utf8")),
    );
    if (entry.threadId !== threadId)
      throw new Error("Lineage identity mismatch");
    return entry;
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  } finally {
    await handle?.close();
  }
}

export async function readMergeLineages(env, warnings) {
  let files;
  try {
    files = await readdir(directory(env));
  } catch (error) {
    if (error.code === "ENOENT") return new Map();
    throw error;
  }
  const entries = new Map();
  for (const name of files.filter((file) => file.endsWith(".json")).sort()) {
    try {
      const id = decodeURIComponent(name.slice(0, -5));
      if (name !== encodeURIComponent(id) + ".json")
        throw new Error("Lineage identity mismatch");
      const entry = await readMergeLineage(env, id);
      if (entry) entries.set(entry.threadId, entry);
    } catch (error) {
      warnings.push(
        `Could not read lineage ${clean(name)}: ${clean(error.message)}`,
      );
    }
  }
  return entries;
}
