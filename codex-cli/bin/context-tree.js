import { open } from "node:fs/promises";
import path from "node:path";
import * as zlib from "node:zlib";
import { promisify } from "node:util";
import { readEvidence } from "./merge-evidence.js";
import { GRAPH_PREFIX, parseGraphItem } from "./merge-graph.js";
import {
  LINEAGE_FORMAT,
  readMergeLineages,
  validateMergeLineage,
} from "./context-lineage.js";
import {
  createUpdateCache,
  inspectMergeUpdates,
  loadUpdateSnapshot,
  summarizeUpdates,
} from "./merge-updates.js";

const clean = (value) => String(value ?? "").replace(/[\x00-\x1f\x7f]/g, " ");
const validId = (id) =>
  typeof id === "string" && id.length > 0 && id.length <= 256;

async function readForkParent(thread) {
  if (!thread.path || !path.isAbsolute(thread.path)) return null;
  let handle;
  let compressed = thread.path.endsWith(".zst");
  try {
    try {
      handle = await open(thread.path, "r");
    } catch (error) {
      if (error.code !== "ENOENT" || compressed) throw error;
      handle = await open(thread.path + ".zst", "r");
      compressed = true;
    }
    const maximum = 1024 * 1024;
    let bytes;
    if (compressed) {
      if (!zlib.zstdDecompress)
        throw new Error("Compressed metadata requires Node.js 22.15 or newer");
      const stat = await handle.stat();
      if (stat.size > 64 * 1024 * 1024)
        throw new Error("Compressed rollout exceeds size limit");
      bytes = await promisify(zlib.zstdDecompress)(await handle.readFile(), {
        maxOutputLength: 64 * 1024 * 1024,
      });
    } else {
      const chunks = [];
      let offset = 0;
      while (offset < maximum) {
        const chunk = Buffer.alloc(16384);
        const { bytesRead } = await handle.read(chunk, 0, chunk.length, offset);
        if (!bytesRead) break;
        const data = chunk.subarray(0, bytesRead);
        chunks.push(data);
        offset += bytesRead;
        if (data.includes(10)) break;
      }
      bytes = Buffer.concat(chunks);
    }
    const end = bytes.indexOf(10);
    if (end < 0 || end > maximum)
      throw new Error("Incomplete or oversized session metadata");
    const meta = JSON.parse(bytes.subarray(0, end).toString("utf8"));
    if (meta.type !== "session_meta" || meta.payload?.id !== thread.id)
      throw new Error("Session metadata identity mismatch");
    const parent =
      meta.payload.forked_from_id ?? meta.payload.history_base?.thread_id;
    return validId(parent) ? parent : null;
  } finally {
    await handle?.close();
  }
}

async function listThreads(client, warnings) {
  const threads = new Map();
  for (const archived of [false, true]) {
    let cursor = null;
    const seen = new Set();
    do {
      const result = await client.request("thread/list", {
        cursor,
        limit: 100,
        sortKey: "updated_at",
        modelProviders: [],
        sourceKinds: ["cli", "vscode", "exec", "appServer", "unknown"],
        archived,
      });
      if (!Array.isArray(result.data))
        throw new Error("Invalid conversation list");
      for (const thread of result.data)
        if (validId(thread.id) && !thread.ephemeral && !threads.has(thread.id))
          threads.set(thread.id, { ...thread, archived });
      cursor = result.nextCursor ?? null;
      if (cursor !== null && (typeof cursor !== "string" || seen.has(cursor)))
        throw new Error("Invalid or repeated conversation list cursor");
      seen.add(cursor);
    } while (cursor !== null);
  }
  // Native list summaries in some storage versions omit forkedFromId even
  // though thread/read and session_meta preserve it. Read only the small header.
  for (const thread of threads.values()) {
    if (thread.forkedFromId) continue;
    try {
      thread.forkedFromId = await readForkParent(thread);
    } catch (error) {
      warnings.push(
        `Could not read ancestry of ${clean(thread.id)}: ${clean(error.message)}`,
      );
    }
  }
  return threads;
}

function relationships(threads, lineages) {
  const parents = new Map();
  for (const thread of threads.values())
    if (validId(thread.forkedFromId) && thread.forkedFromId !== thread.id)
      parents.set(thread.id, [thread.forkedFromId]);
  for (const entry of lineages.values()) {
    for (const source of entry.sources)
      if (
        !parents.has(source.id) &&
        validId(source.forkedFromId) &&
        source.forkedFromId !== source.id
      )
        parents.set(source.id, [source.forkedFromId]);
    parents.set(
      entry.threadId,
      entry.sources.map((s) => s.id),
    );
  }
  return parents;
}

function familyOf(currentId, parents) {
  const adjacent = new Map();
  const link = (a, b) => {
    if (!adjacent.has(a)) adjacent.set(a, new Set());
    adjacent.get(a).add(b);
  };
  for (const [child, sources] of parents)
    for (const source of sources) {
      link(child, source);
      link(source, child);
    }
  const family = new Set([currentId]);
  const pending = [currentId];
  while (pending.length)
    for (const next of adjacent.get(pending.pop()) ?? [])
      if (!family.has(next)) {
        family.add(next);
        pending.push(next);
      }
  return family;
}

/** Read-only discovery. Ordinary fork navigation never needs model inference. */
export async function contextTree(client, currentId) {
  if (!validId(currentId)) throw new Error("Invalid current conversation ID");
  const warnings = [];
  const threads = await listThreads(client, warnings);
  if (!threads.has(currentId)) {
    const { thread } = await client.request("thread/read", {
      threadId: currentId,
      includeTurns: false,
    });
    if (thread.id !== currentId)
      throw new Error("Current conversation identity mismatch");
    threads.set(currentId, { ...thread, archived: false });
  }
  const lineages = await readMergeLineages(client.env, warnings);
  const updateCache = createUpdateCache();
  const frames = new Map();
  const inspected = new Set();
  const snapshotFrames = async (id) => {
    if (frames.has(id)) return frames.get(id);
    const snapshot = await loadUpdateSnapshot(client, id, updateCache);
    const frame =
      snapshot.records
        .map((r) => parseGraphItem(JSON.parse(r.json)))
        .filter(Boolean)
        .at(-1) ?? null;
    frames.set(id, frame);
    return frame;
  };
  // Compatibility with graph merges saved before the lineage index existed.
  // Native forks already connect the family. A compact fresh merge starts with
  // the graph capsule, so its preview also makes it discoverable from a source.
  while (true) {
    const family = familyOf(currentId, relationships(threads, lineages));
    const candidates = [...threads.values()].filter(
      (t) =>
        !inspected.has(t.id) &&
        !lineages.has(t.id) &&
        !t.archived &&
        t.path &&
        !["active", "systemError"].includes(t.status?.type) &&
        (family.has(t.id) || t.preview?.startsWith(GRAPH_PREFIX.slice(0, 32))),
    );
    if (!candidates.length) break;
    for (const thread of candidates) {
      inspected.add(thread.id);
      try {
        const frame = await snapshotFrames(thread.id);
        if (!frame) continue;
        const parent = thread.forkedFromId;
        if (
          parent &&
          threads.has(parent) &&
          (await snapshotFrames(parent))?.mergeNodeId === frame.mergeNodeId
        )
          continue; // An inherited capsule is a fork, not a new merge.
        const archive = await readEvidence(frame.evidence);
        const sourceIds = [
          ...new Set([
            archive.primarySourceThreadId,
            ...frame.branches.filter((b) => !b.primary).map((b) => b.threadId),
          ]),
        ];
        if (sourceIds.length < 2 || sourceIds.includes(thread.id)) continue;
        const sources = sourceIds.map(
          (id) => archive.sources.find((s) => s.id === id) ?? { id },
        );
        lineages.set(
          thread.id,
          validateMergeLineage({
            format: LINEAGE_FORMAT,
            threadId: thread.id,
            sources,
            evidence: frame.evidence,
          }),
        );
      } catch (error) {
        if (family.has(thread.id))
          warnings.push(
            `Could not inspect ${clean(thread.id)}: ${clean(error.message)}`,
          );
      }
    }
  }
  const parents = relationships(threads, lineages);
  const family = familyOf(currentId, parents);
  for (const id of family)
    if (!threads.has(id)) {
      const source = [...lineages.values()]
        .flatMap((e) => e.sources)
        .find((s) => s.id === id);
      threads.set(id, { ...source, id, missing: true });
    }
  const label = (id) =>
    clean(threads.get(id)?.name || threads.get(id)?.preview || id).slice(0, 96);
  const updates = new Map();
  for (const id of family) {
    const thread = threads.get(id);
    if (
      (!lineages.has(id) && id !== currentId) ||
      thread.missing ||
      thread.archived ||
      !thread.path ||
      ["active", "systemError"].includes(thread.status?.type)
    )
      continue;
    try {
      const inspection = await inspectMergeUpdates(client, id, {
        cache: updateCache,
        lineage: lineages.get(id),
        threads,
      });
      if (inspection.merged) updates.set(id, summarizeUpdates(inspection));
    } catch (error) {
      updates.set(id, {
        sources: [],
        pendingSources: 0,
        pendingItems: 0,
        unavailableSources: 1,
      });
      warnings.push(
        `Could not check updates for ${clean(id)}: ${clean(error.message)}`,
      );
    }
  }
  const currentUpdates = updates.get(currentId);
  const children = new Map();
  for (const id of family) {
    const primary = parents.get(id)?.[0];
    if (primary) {
      if (!children.has(primary)) children.set(primary, []);
      children.get(primary).push(id);
    }
  }
  const order = (a, b) =>
    (threads.get(a)?.createdAt ?? 0) - (threads.get(b)?.createdAt ?? 0) ||
    a.localeCompare(b);
  for (const ids of children.values()) ids.sort(order);
  const rows = [];
  const visited = new Set();
  const visit = (id, prefix, connector) => {
    if (visited.has(id)) return;
    visited.add(id);
    const thread = threads.get(id);
    const sources = parents.get(id) ?? [];
    const merge = lineages.has(id);
    const sourceUpdate = currentUpdates?.sources.find(
      (source) => source.id === id,
    );
    const ownUpdates = updates.get(id);
    const badge = ownUpdates?.pendingItems
      ? ` [${ownUpdates.pendingItems} pending items]`
      : ownUpdates?.unavailableSources
        ? " [updates unavailable]"
        : sourceUpdate?.state === "pending"
          ? ` [+${sourceUpdate.newItems} pending]`
          : "";
    const updateDescription = ownUpdates
      ? ownUpdates.sources
          .filter((source) => source.state !== "upToDate")
          .map((source) =>
            source.state === "pending"
              ? `${label(source.id)} +${source.newItems}`
              : `${label(source.id)}: ${clean(source.reason)}`,
          )
          .join("; ")
      : sourceUpdate?.state === "pending"
        ? `${sourceUpdate.newItems} items not yet merged into the current chat`
        : "";
    const relation = merge
      ? "merge ← " + sources.map((p) => `${label(p)} [${p}]`).join(" + ")
      : sources.length
        ? `fork ← ${label(sources[0])} [${sources[0]}]`
        : "root";
    const disabledReason = thread.missing
      ? "Source chat is no longer available"
      : thread.archived
        ? "Archived chat; use /resume to restore it"
        : !thread.path
          ? "Chat has no saved local history"
          : ["active", "systemError"].includes(thread.status?.type) &&
              id !== currentId
            ? "Chat is busy or unavailable"
            : null;
    rows.push({
      id,
      name: prefix + connector + label(id) + badge,
      current: id === currentId,
      description: `${id} · ${relation}${updateDescription ? " · updates: " + updateDescription : ""}`,
      disabledReason,
      search: [
        id,
        label(id),
        merge ? "merge" : sources.length ? "fork" : "root",
        badge ? "updates pending" : "",
      ].join(" "),
      parents: sources,
      kind: merge ? "merge" : sources.length ? "fork" : "root",
      ...(ownUpdates ? { updates: ownUpdates } : {}),
    });
    const descendants = children.get(id) ?? [];
    const nextPrefix =
      prefix + (connector ? (connector === "└─ " ? "   " : "│  ") : "");
    descendants.forEach((child, i) =>
      visit(child, nextPrefix, i === descendants.length - 1 ? "└─ " : "├─ "),
    );
  };
  [...family]
    .filter((id) => !parents.get(id)?.length)
    .sort(order)
    .forEach((id) => visit(id, "", ""));
  // Corrupt/cyclic native ancestry must not make a conversation disappear.
  [...family].sort(order).forEach((id) => visit(id, "", ""));
  const updateHint = currentUpdates?.pendingSources
    ? `${currentUpdates.pendingSources} source chats have ${currentUpdates.pendingItems} unmerged items. Run /merge --update to merge their updates.`
    : currentUpdates?.unavailableSources
      ? "Some source updates could not be checked. See the tree details before updating."
      : null;
  return {
    currentThreadId: currentId,
    rows,
    warnings: warnings.slice(0, 8),
    updateHint,
  };
}
