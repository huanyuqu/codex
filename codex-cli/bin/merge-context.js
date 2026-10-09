import {
  hash,
  loadSnapshot,
  MERGE_PREFIX,
  parseMergeItem,
  snapshotVersion,
} from "./merge-history.js";
import {
  expandSnapshot,
  forkAnnotationItem,
  isNativeForkAnnotation,
  parseSemanticItem,
} from "./merge-evidence.js";
import { semanticPlan } from "./merge-semantic.js";
import { parseGraphItem } from "./merge-graph.js";
import { graphMergePlan } from "./merge-views.js";
import { saveMergeLineage } from "./context-lineage.js";
import {
  inspectMergeUpdates,
  restoreCompactedGraph,
  summarizeUpdates,
} from "./merge-updates.js";

export const DEFAULT_MAX_BYTES = 512 * 1024;

function expand(records) {
  const result = [];
  for (const record of records) {
    const bundle = parseMergeItem(JSON.parse(record.json));
    if (bundle) {
      for (const branch of bundle.branches) result.push(...branch.records);
    } else result.push(record);
  }
  return result;
}

/** Keep branch order explicit; conflicting conclusions remain separate. */
export function planMerge(snapshots, { maxBytes = DEFAULT_MAX_BYTES } = {}) {
  if (
    !Array.isArray(snapshots) ||
    snapshots.length < 2 ||
    snapshots.length > 32
  )
    throw new Error("Merge requires between 2 and 32 source threads");
  if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0)
    throw new Error("maxBytes must be a positive safe integer");
  const ids = snapshots.map((s) => s.thread.id);
  if (
    ids.some((id) => typeof id !== "string" || !id.length) ||
    new Set(ids).size !== ids.length
  )
    throw new Error("Merge requires distinct non-empty thread IDs");
  const seen = new Set(
    expand(snapshots[0].records).map((record) => record.key),
  );
  const previous = [snapshots[0].records];
  const branches = [];
  const images = [];
  const imageSources = [];
  let importedItems = 0;
  let skippedItems = 0;
  for (const snapshot of snapshots.slice(1)) {
    // Legacy forks without stable item IDs still share an exact prefix. Only
    // deduplicate that prefix, not identical messages repeated later on.
    let shared = 0;
    for (const history of previous) {
      let n = 0;
      while (
        n < history.length &&
        n < snapshot.records.length &&
        history[n].json === snapshot.records[n].json
      )
        n++;
      shared = Math.max(shared, n);
    }
    const prefix = expand(snapshot.records.slice(0, shared));
    skippedItems += prefix.length;
    const records = [];
    for (const record of expand(snapshot.records.slice(shared))) {
      if (seen.has(record.key)) {
        skippedItems++;
        continue;
      }
      seen.add(record.key);
      records.push(record);
    }
    importedItems += records.length;
    for (const record of records) {
      const item = JSON.parse(record.json);
      const content = [
        ...(Array.isArray(item.content) ? item.content : []),
        ...(Array.isArray(item.output) ? item.output : []),
      ];
      for (const image of content.filter(
        (part) => part.type === "input_image",
      )) {
        images.push(image);
        imageSources.push({
          sourceThreadId: record.sourceThreadId,
          sourceItemKey: record.key,
          contentIndex: images.length,
        });
      }
    }
    branches.push({
      threadId: snapshot.thread.id,
      name: snapshot.thread.name ?? null,
      cwd: snapshot.thread.cwd,
      forkedFromId: snapshot.thread.forkedFromId ?? null,
      records,
    });
    previous.push(snapshot.records);
  }
  const bundle = {
    format: "codex-context-merge-v1",
    baseThreadId: ids[0],
    sourceThreadIds: ids,
    policy:
      "The first source provides the live conversation. Branch records are quoted historical context in source order. Preserve conflicting findings with their sources; verify them against the current workspace. Historical tool calls describe past executions. Working tree changes are managed separately.",
    branches,
    ...(images.length ? { imageSources } : {}),
  };
  const text = MERGE_PREFIX + JSON.stringify(bundle);
  const bytes =
    Buffer.byteLength(text, "utf8") +
    images.reduce(
      (sum, image) => sum + Buffer.byteLength(JSON.stringify(image), "utf8"),
      0,
    );
  if (bytes > maxBytes)
    throw new Error(
      `Merge context is ${bytes} bytes, exceeding --max-bytes ${maxBytes}; compact the source threads or raise the limit`,
    );
  return {
    bundle,
    item: {
      type: "message",
      role: "user",
      content: [{ type: "input_text", text }, ...images],
    },
    summary: {
      baseThreadId: ids[0],
      sourceThreadIds: ids,
      importedItems,
      skippedItems,
      importedImages: images.length,
      contextBytes: bytes,
      contextSha256: hash(JSON.stringify([text, ...images])),
    },
  };
}

/** Merge into a fresh, durable native fork. All source threads remain readable. */
export async function mergeThreads(client, options = {}) {
  let { threadIds } = options;
  const { maxBytes, name, dryRun = false, cwd } = options;
  const mode = options.mode ?? "auto";
  const semantic = options.semantic ?? mode !== "reference";
  options = { ...options, mode, semantic };
  if (
    !Array.isArray(threadIds) ||
    threadIds.length < (options.update ? 1 : 2) ||
    threadIds.length > 32 ||
    threadIds.some((id) => typeof id !== "string" || !id.length) ||
    new Set(threadIds).size !== threadIds.length
  )
    throw new Error("Merge requires 2 to 32 distinct non-empty thread IDs");
  if (name !== undefined && (typeof name !== "string" || !name.trim()))
    throw new Error("Merge name must be non-empty");
  for (const field of [
    "maxBytes",
    "summaryBytes",
    "maxInputBytes",
    "analysisTimeoutMs",
    "contextTokens",
    "reserveTokens",
  ]) {
    if (
      options[field] !== undefined &&
      (!Number.isSafeInteger(options[field]) || options[field] <= 0)
    )
      throw new Error(field + " must be a positive safe integer");
  }
  if (options.analysisTimeoutMs > 2147483647)
    throw new Error("Analysis timeout is too large");
  if (!["auto", "inline", "summary", "reference", "legacy"].includes(mode))
    throw new Error("Unknown merge mode");
  if (!semantic && options.goal !== undefined)
    throw new Error("A merge goal requires semantic synthesis");
  if (semantic && mode === "reference")
    throw new Error(
      "Semantic synthesis requires a mode that supplies conversation content",
    );
  if (
    mode === "legacy" &&
    !semantic &&
    [
      "model",
      "summaryBytes",
      "maxInputBytes",
      "analysisTimeoutMs",
      "evidenceDir",
    ].some((key) => options[key] !== undefined)
  )
    throw new Error(
      "Analysis options require semantic synthesis in legacy mode",
    );
  for (const field of ["goal", "model", "evidenceDir"]) {
    if (
      options[field] !== undefined &&
      (typeof options[field] !== "string" || !options[field].trim())
    )
      throw new Error(field + " must be non-empty");
  }
  let updateInspection;
  if (options.update) {
    if (threadIds.length !== 1 || mode === "legacy")
      throw new Error(
        "--update takes only the current/base thread ID and requires a graph merge mode",
      );
    updateInspection = await inspectMergeUpdates(client, threadIds[0]);
    if (!updateInspection.merged)
      throw new Error(
        "--update needs a previously merged chat; use /merge with source IDs first",
      );
    const blocked = updateInspection.sources.filter((source) =>
      ["changed", "unavailable"].includes(source.state),
    );
    if (blocked.length)
      throw new Error(
        "Cannot update all sources: " +
          blocked.map((source) => `${source.id}: ${source.reason}`).join("; "),
      );
    const pending = updateInspection.sources.filter(
      (source) => source.state === "pending",
    );
    if (!pending.length)
      return {
        dryRun,
        noOp: true,
        threadId: threadIds[0],
        message: "All merged sources are up to date. No new chat was created.",
        importedItems: 0,
        skippedItems: 0,
        contextBytes: 0,
        modelCalls: 0,
        updates: summarizeUpdates(updateInspection),
      };
    threadIds = [threadIds[0], ...pending.map((source) => source.id)];
    if (threadIds.length > 32)
      throw new Error(
        "More than 31 sources have updates; merge them explicitly in batches",
      );
    options = { ...options, threadIds };
  }
  const nativeSnapshots = [];
  for (const id of threadIds)
    nativeSnapshots.push(await loadSnapshot(client, id));
  if (updateInspection) {
    const expected = new Map([
      [threadIds[0], snapshotVersion(updateInspection.primarySnapshot.records)],
      ...updateInspection.sources.map((source) => [source.id, source.snapshot]),
    ]);
    for (const snapshot of nativeSnapshots)
      if (
        JSON.stringify(snapshotVersion(snapshot.records)) !==
        JSON.stringify(expected.get(snapshot.thread.id))
      )
        throw new Error(
          "Source changed while checking updates; retry when all sources are idle",
        );
  }
  const snapshots = [];
  const evidenceCache = new Map();
  for (const snapshot of nativeSnapshots) {
    let expanded = await expandSnapshot(snapshot, evidenceCache);
    if (!snapshots.length && updateInspection)
      expanded = restoreCompactedGraph(expanded, updateInspection);
    snapshots.push({
      ...expanded,
      nativeVersion: snapshotVersion(snapshot.records),
    });
  }
  let plan;
  if (mode === "legacy") {
    plan = planMerge(snapshots, {
      maxBytes: semantic ? Number.MAX_SAFE_INTEGER : maxBytes,
    });
    if (semantic) plan = await semanticPlan(client, snapshots, plan, options);
  } else
    plan = await graphMergePlan(client, snapshots, nativeSnapshots, options);
  if (updateInspection)
    plan.summary.updates = summarizeUpdates(updateInspection);
  if (dryRun) return { dryRun: true, ...plan.summary };
  if (updateInspection)
    for (const snapshot of nativeSnapshots) {
      const current = await loadSnapshot(client, snapshot.thread.id);
      if (
        JSON.stringify(snapshotVersion(current.records)) !==
        JSON.stringify(snapshotVersion(snapshot.records))
      )
        throw new Error(
          "Source changed during update analysis; retry when all sources are idle",
        );
    }
  let target;
  try {
    if (plan.primaryEmbedded) {
      ({ thread: target } = await client.request("thread/start", {
        ephemeral: false,
        cwd: cwd ?? nativeSnapshots[0].thread.cwd,
        historyMode: nativeSnapshots[0].thread.historyMode,
      }));
    } else
      ({ thread: target } = await client.request("thread/fork", {
        threadId: threadIds[0],
        ephemeral: false,
        excludeTurns: true,
        ...(cwd ? { cwd } : {}),
      }));
    if (!target?.id || threadIds.includes(target.id))
      throw new Error("Codex did not create a fresh merge target");
    if (plan.primaryEmbedded && (target.ephemeral || target.turns?.length))
      throw new Error("Codex did not create an empty persistent merge target");
    // Re-read the fork's exact persisted history: the base might have changed
    // between preflight and fork, including a same-length rollback/compaction.
    // Native thread/start delays writing an empty rollout until the first
    // injected item. Only a newly started empty target can skip this disk read;
    // sources and native forks must already have readable persisted history.
    const actual = plan.primaryEmbedded
      ? { records: [] }
      : await loadSnapshot(client, target.id);
    const baseExpected = nativeSnapshots[0].records.map((r) => r.json);
    const expected = plan.primaryEmbedded ? [] : baseExpected;
    const currentBase = await loadSnapshot(client, threadIds[0]);
    const annotations = actual.records.slice(expected.length);
    if (
      JSON.stringify(currentBase.records.map((r) => r.json)) !==
        JSON.stringify(baseExpected) ||
      JSON.stringify(
        actual.records.slice(0, expected.length).map((r) => r.json),
      ) !== JSON.stringify(expected) ||
      annotations.some((r) => !isNativeForkAnnotation(r))
    )
      throw new Error(
        "Base thread changed before fork; retry the merge when all sources are idle",
      );
    if (annotations.length) {
      const note = forkAnnotationItem(annotations);
      plan.items = [...(plan.items ?? (plan.item ? [plan.item] : [])), note];
      plan.summary.contextBytes += Buffer.byteLength(
        JSON.stringify(note),
        "utf8",
      );
      plan.summary.contextSha256 = hash(JSON.stringify(plan.items));
      if (plan.summary.contextBytes > (maxBytes ?? DEFAULT_MAX_BYTES))
        throw new Error("Native fork annotations exceed --max-bytes");
      if (
        plan.summary.budget &&
        (plan.primaryEmbedded ? 0 : plan.summary.budget.primaryEstimate) +
          plan.summary.contextBytes +
          32 >
          plan.summary.budget.inputLimit
      )
        throw new Error("Native fork annotations exceed the context budget");
    }
    if (name)
      await client.request("thread/name/set", {
        threadId: target.id,
        name: name.trim(),
      });
    if (plan.item || plan.items?.length)
      await client.request("thread/inject_items", {
        threadId: target.id,
        items: plan.items ?? [plan.item],
      });
    // An acknowledged injection must also be on disk before returning success.
    const persisted = await loadSnapshot(client, target.id);
    if (
      !persisted.records.some((r) => {
        const item = JSON.parse(r.json);
        if (!plan.item)
          return plan.existingGraphCapsule
            ? parseGraphItem(item)?.mergeNodeId ===
                plan.existingGraphCapsule.mergeNodeId
            : parseSemanticItem(item)?.analysisKey ===
                plan.existingCapsule?.analysisKey;
        return (
          item.type === "message" &&
          item.role === plan.item.role &&
          item.content?.[0]?.text === plan.item.content[0].text
        );
      })
    )
      throw new Error(
        "Codex acknowledged merge injection but the context was not persisted",
      );
    for (const injected of plan.items ?? []) {
      if (
        !persisted.records.some((r) => {
          const saved = JSON.parse(r.json);
          return (
            saved.role === injected.role &&
            saved.content?.[0]?.text === injected.content[0].text
          );
        })
      )
        throw new Error("A merge context item was not persisted");
    }
    await saveMergeLineage(
      client.env,
      target.id,
      nativeSnapshots,
      plan.summary,
      persisted,
    );
    await client.request("thread/unsubscribe", { threadId: target.id });
    return { dryRun: false, threadId: target.id, ...plan.summary };
  } catch (error) {
    // Keep failed forks recoverable, without leaving a misleading active merge.
    if (target?.id && !threadIds.includes(target.id)) {
      try {
        await client.request("thread/archive", { threadId: target.id });
        error.message += ` (incomplete merge ${target.id} archived)`;
      } catch {
        error.message += ` (incomplete merge ${target.id}; archive it before retrying)`;
      }
    }
    throw error;
  }
}
