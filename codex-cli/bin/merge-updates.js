import { loadSnapshot, snapshotVersion } from "./merge-history.js";
import { expandSnapshot, readEvidence } from "./merge-evidence.js";
import { readMergeLineage } from "./context-lineage.js";
import { parseGraphItem } from "./merge-graph.js";

export const createUpdateCache = () => ({
  snapshots: new Map(),
  expanded: new Map(),
  evidence: new Map(),
});

export function loadUpdateSnapshot(client, id, cache) {
  if (!cache.snapshots.has(id))
    cache.snapshots.set(id, loadSnapshot(client, id));
  return cache.snapshots.get(id);
}

function validVersion(version) {
  return (
    Number.isSafeInteger(version?.recordCount) &&
    version.recordCount >= 0 &&
    /^[a-f0-9]{64}$/.test(version.sha256 ?? "")
  );
}

function archivedVersion(archive, branch, lineage) {
  const version =
    archive.sources.find((source) => source.id === branch.threadId)?.snapshot ??
    lineage?.sources.find((source) => source.id === branch.threadId)?.snapshot;
  if (version !== undefined) {
    if (!validVersion(version))
      throw new Error("Invalid merged source snapshot");
    return version;
  }
  // Older graph archives did not save native snapshot digests. Exact original
  // paths can recover ordinary sources; a path containing a merge needs an
  // explicit remerge to establish its native boundary.
  const nodes = new Map(archive.graph.nodes.map((node) => [node.id, node]));
  const records = new Map(
    archive.records.map((record) => [record.ref, record]),
  );
  if (branch.path.some((id) => !nodes.get(id)?.recordRef))
    throw new Error(
      "This older merged source has no saved update boundary; merge it explicitly once",
    );
  return snapshotVersion(
    branch.path.map((id) => records.get(nodes.get(id).recordRef)),
  );
}

async function archivedSourceIds(client) {
  const archived = new Set();
  const cursors = new Set();
  let cursor = null;
  do {
    const result = await client.request("thread/list", {
      cursor,
      limit: 100,
      modelProviders: [],
      sourceKinds: [],
      archived: true,
    });
    if (!Array.isArray(result.data))
      throw new Error("Invalid archived conversation list");
    for (const thread of result.data) archived.add(thread.id);
    cursor = result.nextCursor ?? null;
    if (cursor !== null && (typeof cursor !== "string" || cursors.has(cursor)))
      throw new Error("Invalid or repeated archived conversation list cursor");
    cursors.add(cursor);
  } while (cursor !== null);
  return archived;
}

/** Compare frozen merged source prefixes, never timestamps or mutable names. */
export async function inspectMergeUpdates(
  client,
  currentId,
  { cache = createUpdateCache(), lineage, threads } = {},
) {
  const primarySnapshot = await loadUpdateSnapshot(client, currentId, cache);
  if (!cache.expanded.has(currentId))
    cache.expanded.set(
      currentId,
      expandSnapshot(primarySnapshot, cache.evidence),
    );
  const primary = await cache.expanded.get(currentId);
  const frame = primary.graphCapsules.at(-1);
  let archive;
  let recovered = false;
  if (frame) {
    const key = frame.evidence.path + ":" + frame.evidence.sha256;
    archive = cache.evidence.get(key) ?? (await readEvidence(frame.evidence));
  } else {
    lineage ??= await readMergeLineage(client.env, currentId);
    const compacted = lineage?.frameFingerprint
      ? primarySnapshot.compactedGraphRecords.find(
          (record) => record.fingerprint === lineage.frameFingerprint,
        )
      : primarySnapshot.compactedGraphRecords.at(-1);
    if (!compacted && lineage?.evidence)
      throw new Error(
        "The saved merge is no longer in this chat's retained history; use /merge with explicit source IDs",
      );
    if (!compacted) return { merged: false, sources: [], primarySnapshot };
    const retainedFrame = parseGraphItem(JSON.parse(compacted.json));
    if (
      lineage?.evidence &&
      JSON.stringify(retainedFrame?.evidence) !==
        JSON.stringify(lineage.evidence)
    )
      throw new Error(
        "Compacted merge evidence does not match its saved lineage",
      );
    archive = await readEvidence(retainedFrame.evidence);
    if (archive.graph?.mergeNodeId !== retainedFrame.mergeNodeId)
      throw new Error("Compacted merge evidence does not match its graph node");
    recovered = true;
  }
  if (!archive.graph) return { merged: false, sources: [], primarySnapshot };
  // thread/read does not expose the archived flag on native Thread objects.
  // Tree discovery already has this inventory; standalone updates query it.
  const archived = threads ? null : await archivedSourceIds(client);
  const sources = [];
  for (const branch of archive.graph.branches) {
    if (branch.threadId === currentId) continue;
    const source = {
      id: branch.threadId,
      name: branch.name ?? null,
      newItems: 0,
    };
    try {
      const listed = threads?.get(branch.threadId);
      if (threads && !listed)
        throw new Error("Source chat is no longer available");
      if (listed?.archived || archived?.has(branch.threadId))
        throw new Error("Source chat is archived");
      const baseline = archivedVersion(archive, branch, lineage);
      const snapshot = await loadUpdateSnapshot(client, branch.threadId, cache);
      if (snapshot.thread.archived) throw new Error("Source chat is archived");
      source.name = snapshot.thread.name ?? source.name;
      source.snapshot = snapshotVersion(snapshot.records);
      if (
        snapshot.records.length < baseline.recordCount ||
        snapshotVersion(snapshot.records.slice(0, baseline.recordCount))
          .sha256 !== baseline.sha256
      ) {
        source.state = "changed";
        source.reason =
          "Source history was rolled back, replaced or compacted; merge this source explicitly";
      } else {
        source.newItems = snapshot.records.length - baseline.recordCount;
        source.state = source.newItems ? "pending" : "upToDate";
      }
    } catch (error) {
      source.state = "unavailable";
      source.reason = error.message;
    }
    sources.push(source);
  }
  return { merged: true, sources, primarySnapshot, archive, recovered };
}

export function summarizeUpdates(inspection) {
  const sources = inspection.sources.map(
    ({ id, name, state, newItems, reason }) => ({
      id,
      name,
      state,
      newItems,
      ...(reason ? { reason } : {}),
    }),
  );
  return {
    sources,
    pendingSources: sources.filter((source) => source.state === "pending")
      .length,
    pendingItems: sources.reduce((sum, source) => sum + source.newItems, 0),
    unavailableSources: sources.filter((source) =>
      ["changed", "unavailable"].includes(source.state),
    ).length,
  };
}

// A proven native compaction can remove the visible capsule. Preserve its
// frozen graph as ancestry, while keeping the CURRENT compacted text as the
// primary model input. A rollback cannot reactivate this evidence.
export function restoreCompactedGraph(primary, inspection) {
  if (!inspection.recovered) return primary;
  const { archive } = inspection;
  return {
    ...primary,
    timeline: [archive.graph.mergeNodeId, ...primary.timeline],
    graphs: [...primary.graphs, archive.graph],
    graphRecords: new Map([
      ...archive.records.map((record) => [record.ref, record]),
      ...primary.graphRecords,
    ]),
    contexts: new Map([
      ...archive.sources.map((source) => [source.id, source]),
      ...primary.contexts,
    ]),
  };
}
