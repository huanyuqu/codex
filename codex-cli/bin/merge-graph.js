import { GRAPH_PREFIX, hash, recordRef } from "./merge-history.js";

export { GRAPH_PREFIX };
export const FUSION_PREFIX =
  "Codex conversation graph synthesis (model-generated reading):\n";
export const GRAPH_FORMAT = "codex-conversation-graph-v1";

export function parseGraphItem(item) {
  const text =
    item.type === "message" && item.role === "user" && item.content?.[0]?.text;
  if (typeof text !== "string" || !text.startsWith(GRAPH_PREFIX)) return null;
  let frame;
  try {
    frame = JSON.parse(text.slice(GRAPH_PREFIX.length));
  } catch {
    throw new Error("Invalid conversation graph provenance");
  }
  if (
    frame.format !== "codex-graph-merge-v1" ||
    typeof frame.mergeNodeId !== "string" ||
    typeof frame.evidence?.path !== "string" ||
    !/^[a-f0-9]{64}$/.test(frame.evidence?.sha256 ?? "")
  ) {
    throw new Error("Invalid conversation graph evidence reference");
  }
  return frame;
}

export function isFusionItem(item) {
  return (
    item.type === "message" &&
    item.role === "assistant" &&
    item.content?.[0]?.text?.startsWith(FUSION_PREFIX)
  );
}

export function ancestors(nodes, head) {
  const found = new Set();
  const pending = head ? [head] : [];
  while (pending.length) {
    const id = pending.pop();
    if (found.has(id)) continue;
    const node = nodes.get(id);
    if (!node) throw new Error("Missing graph ancestor: " + id);
    found.add(id);
    pending.push(...node.parents);
  }
  return found;
}

export function validateGraph(graph, catalog) {
  if (
    graph?.format !== GRAPH_FORMAT ||
    !Array.isArray(graph.nodes) ||
    !Array.isArray(graph.branches) ||
    !Array.isArray(graph.primaryPath) ||
    typeof graph.primaryThreadId !== "string"
  )
    throw new Error("Invalid conversation graph");
  const nodes = new Map();
  for (const node of graph.nodes) {
    if (
      !node ||
      typeof node.id !== "string" ||
      !node.id ||
      nodes.has(node.id) ||
      !Array.isArray(node.parents) ||
      new Set(node.parents).size !== node.parents.length ||
      node.parents.some((id) => typeof id !== "string")
    )
      throw new Error("Invalid or duplicate graph node");
    if (node.kind === "merge") {
      if (
        !/^m_[a-f0-9]{24}$/.test(node.id) ||
        node.recordRef !== undefined ||
        (node.primaryParent !== null &&
          !node.parents.includes(node.primaryParent))
      )
        throw new Error("Invalid merge node");
    } else if (
      !["message", "tool_call", "tool_result", "record"].includes(node.kind) ||
      node.recordRef !== node.id ||
      !catalog.has(node.recordRef)
    ) {
      throw new Error("Graph node has missing original evidence");
    }
    nodes.set(node.id, node);
  }
  const children = new Map();
  const remaining = new Map();
  for (const node of nodes.values()) {
    if (node.parents.some((id) => !nodes.has(id) || id === node.id))
      throw new Error("Missing or self-referencing graph parent");
    if (node.callNodeId && nodes.get(node.callNodeId)?.kind !== "tool_call")
      throw new Error("Invalid graph tool correlation");
    remaining.set(node.id, node.parents.length);
    for (const parent of node.parents) {
      if (!children.has(parent)) children.set(parent, []);
      children.get(parent).push(node.id);
    }
  }
  const ordered = [...remaining]
    .filter(([, count]) => !count)
    .map(([id]) => id);
  for (let i = 0; i < ordered.length; i++)
    for (const child of children.get(ordered[i]) ?? []) {
      remaining.set(child, remaining.get(child) - 1);
      if (!remaining.get(child)) ordered.push(child);
    }
  if (ordered.length !== nodes.size)
    throw new Error("Conversation graph contains a cycle");
  for (const node of nodes.values())
    if (node.callNodeId) {
      const call = catalog.get(node.callNodeId);
      const result = catalog.get(node.recordRef);
      if (
        node.kind !== "tool_result" ||
        !ancestors(nodes, node.id).has(node.callNodeId) ||
        JSON.parse(call.json).call_id !== JSON.parse(result.json).call_id
      )
        throw new Error("Invalid graph tool correlation");
    }
  const pathCheck = (values) => {
    if (
      !Array.isArray(values) ||
      new Set(values).size !== values.length ||
      values.some((id) => !nodes.has(id))
    )
      throw new Error("Invalid graph branch path");
    for (let i = 1; i < values.length; i++)
      if (!nodes.get(values[i]).parents.includes(values[i - 1]))
        throw new Error("Disconnected graph branch path");
  };
  pathCheck(graph.primaryPath);
  const ids = new Set();
  for (const branch of graph.branches) {
    if (typeof branch.threadId !== "string" || ids.has(branch.threadId))
      throw new Error("Invalid graph branch identity");
    ids.add(branch.threadId);
    pathCheck(branch.path);
    if (
      branch.head !== (branch.path.at(-1) ?? null) ||
      (branch.forkPoint !== null && !branch.path.includes(branch.forkPoint))
    )
      throw new Error("Invalid graph branch head or fork point");
  }
  if (
    !nodes.has(graph.mergeNodeId) ||
    nodes.get(graph.mergeNodeId).kind !== "merge"
  )
    throw new Error("Missing graph merge node");
  return { nodes, ordered };
}

function kind(item) {
  if (item.type === "message") return "message";
  if (
    [
      "function_call",
      "custom_tool_call",
      "local_shell_call",
      "web_search_call",
      "image_generation_call",
    ].includes(item.type)
  )
    return "tool_call";
  if (["function_call_output", "custom_tool_call_output"].includes(item.type))
    return "tool_result";
  return "record";
}

/** Canonicalize copied prefixes, retaining immutable identities across later merges. */
export function buildConversationGraph(snapshots) {
  const nodes = new Map();
  const catalog = new Map();
  const branches = new Map();
  // Flat legacy imports have no saved graph parents. Their provisional nodes
  // may be resolved against original source snapshots during this conversion;
  // nodes loaded from an already persisted graph are never reparented.
  const provisionalLegacyNodes = new Set();
  const addRecord = (record) => {
    const ref = record.ref ?? recordRef(record);
    const existing = catalog.get(ref);
    if (existing && existing.json !== record.json)
      throw new Error("Graph record identity collision");
    if (!existing)
      catalog.set(ref, { ...record, ref, fingerprint: hash(record.json) });
    return ref;
  };
  const addNode = (node) => {
    const existing = nodes.get(node.id);
    if (existing && JSON.stringify(existing) !== JSON.stringify(node))
      throw new Error("Graph node ancestry collision: " + node.id);
    if (!existing) nodes.set(node.id, node);
  };
  for (const snapshot of snapshots) {
    for (const record of snapshot.graphRecords?.values() ?? [])
      addRecord(record);
    for (const graph of snapshot.graphs ?? []) {
      for (const node of graph.nodes) addNode(node);
      for (const branch of graph.branches)
        branches.set(branch.threadId, branch);
    }
  }
  const sequences = [];
  const candidates = [...branches.values()];
  const inheritedHeads = new Map(
    [...branches.values()].map((branch) => [branch.threadId, branch.head]),
  );
  const mappings = [];
  for (const snapshot of snapshots) {
    const records = new Map(snapshot.records.map((r) => [recordRef(r), r]));
    const timeline = snapshot.timeline ?? snapshot.records.map(recordRef);
    let shared = 0;
    let parentSequence;
    const preferred = snapshot.thread.forkedFromId;
    for (const sequence of candidates) {
      let n = 0;
      while (n < timeline.length && n < sequence.path.length) {
        const a = timeline[n],
          b = sequence.path[n];
        if (
          a === b ||
          (records.get(a)?.json !== undefined &&
            records.get(a).json === catalog.get(b)?.json)
        )
          n++;
        else break;
      }
      if (
        n > shared ||
        (n === shared && n && sequence.threadId === preferred)
      ) {
        shared = n;
        parentSequence = sequence;
      }
    }
    const aliases = new Map();
    const path = [];
    let head = null;
    let turnGroup = null;
    const calls = new Map();
    const registerCall = (id) => {
      const record = catalog.get(id);
      if (!record) return;
      const item = JSON.parse(record.json);
      if (
        nodes.get(id)?.kind === "tool_call" &&
        typeof item.call_id === "string"
      )
        calls.set(item.call_id, id);
    };
    for (let i = 0; i < timeline.length; i++) {
      const originalId = timeline[i];
      let id;
      if (i < shared) {
        id = parentSequence.path[i];
        aliases.set(originalId, id);
        turnGroup = nodes.get(id)?.turnGroup ?? turnGroup;
      } else if (nodes.get(originalId)?.kind === "merge") {
        id = originalId;
        if (head && !nodes.get(id).parents.includes(head))
          throw new Error("Merge provenance does not match its main history");
      } else if (snapshot.legacyMerges?.has(originalId)) {
        const marker = snapshot.legacyMerges.get(originalId);
        const tips = [];
        for (const source of marker.branches) {
          let tip = null;
          for (const record of source) {
            const ref = addRecord(record);
            if (!nodes.has(ref)) {
              const item = JSON.parse(record.json);
              addNode({
                id: ref,
                kind: kind(item),
                recordRef: ref,
                parents: tip ? [tip] : [],
                sourceThreadId: record.sourceThreadId,
                turnId: record.turnId ?? null,
                turnGroup: record.turnId ?? ref,
                ...(item.role ? { role: item.role } : {}),
              });
              provisionalLegacyNodes.add(ref);
            }
            tip = ref;
          }
          if (tip) tips.push(tip);
        }
        id = originalId;
        addNode({
          id,
          kind: "merge",
          parents: [...new Set([head, ...tips].filter(Boolean))],
          primaryParent: head,
          ancestry: "legacy-import",
        });
      } else {
        const record = records.get(originalId);
        if (!record)
          throw new Error("Missing graph timeline record: " + originalId);
        id = addRecord(record);
        const item = JSON.parse(record.json);
        if (item.role === "user" || !turnGroup) turnGroup = record.turnId ?? id;
        const node = {
          id,
          kind: kind(item),
          recordRef: id,
          parents: head ? [head] : [],
          sourceThreadId: record.sourceThreadId,
          turnId: record.turnId ?? null,
          turnGroup,
          ...(item.role ? { role: item.role } : {}),
        };
        if (node.kind === "tool_result" && calls.has(item.call_id))
          node.callNodeId = calls.get(item.call_id);
        if (nodes.has(id)) {
          // Existing original evidence retains its original ancestry. The path
          // must agree rather than silently reparenting it after a merge.
          if (provisionalLegacyNodes.has(id)) {
            nodes.set(id, node);
            provisionalLegacyNodes.delete(id);
          } else if (head && !nodes.get(id).parents.includes(head))
            throw new Error(
              "Original graph node appears under a different parent",
            );
        } else addNode(node);
      }
      path.push(id);
      head = id;
      registerCall(id);
    }
    // Imported records may already be represented by inherited merge nodes.
    for (const record of snapshot.records)
      if (!aliases.has(recordRef(record))) addRecord(record);
    const forkPoint = shared ? path[shared - 1] : null;
    const branch = {
      threadId: snapshot.thread.id,
      name: snapshot.thread.name ?? null,
      path,
      head,
      forkedFromId: preferred ?? parentSequence?.threadId ?? null,
      forkPoint,
      ancestry: forkPoint
        ? preferred === parentSequence?.threadId
          ? "native-parent-and-prefix"
          : "shared-prefix"
        : "unknown",
    };
    const inherited = branches.get(branch.threadId);
    if (inherited && !forkPoint) {
      branch.forkPoint = inherited.forkPoint;
      branch.forkedFromId = preferred ?? inherited.forkedFromId;
      branch.ancestry = inherited.ancestry;
    }
    branches.set(branch.threadId, branch);
    sequences.push(branch);
    candidates.push(branch);
    mappings.push(aliases);
  }
  const primary = sequences[0];
  const visible = ancestors(nodes, primary.head);
  const imports = [];
  let skippedItems = 0;
  for (const branch of sequences.slice(1)) {
    const reachable = ancestors(nodes, branch.head);
    const introduced = [...nodes.keys()].filter(
      (id) => reachable.has(id) && !visible.has(id),
    );
    imports.push({ ...branch, nodeIds: introduced });
    skippedItems += [...reachable].filter(
      (id) => visible.has(id) && nodes.get(id).recordRef,
    ).length;
    for (const id of reachable) visible.add(id);
  }
  const parents = [...new Set(sequences.map((b) => b.head).filter(Boolean))];
  const introducedRecords = imports
    .flatMap((b) => b.nodeIds)
    .filter((id) => nodes.get(id).recordRef);
  const reusedGraph =
    introducedRecords.length === 0 &&
    nodes.get(primary.head)?.kind === "merge" &&
    sequences
      .slice(1)
      .every(
        (branch) =>
          inheritedHeads.get(branch.threadId) === branch.head &&
          ancestors(nodes, primary.head).has(branch.head),
      );
  const mergeNodeId = reusedGraph
    ? primary.head
    : "m_" +
      hash(
        JSON.stringify({
          format: GRAPH_FORMAT,
          parents,
          primaryParent: primary.head,
        }),
      ).slice(0, 24);
  if (!reusedGraph)
    addNode({
      id: mergeNodeId,
      kind: "merge",
      parents,
      primaryParent: primary.head,
      ancestry: "explicit-merge",
    });
  const graph = {
    format: GRAPH_FORMAT,
    nodes: [...nodes.values()],
    branches: [...branches.values()],
    primaryThreadId: primary.threadId,
    primaryPath: primary.path,
    mergeNodeId,
  };
  const validated = validateGraph(graph, catalog);
  // Keep a deterministic topological order in both the graph and archive.
  graph.nodes = validated.ordered.map((id) => nodes.get(id));
  const baseRefs = [
    ...new Set(
      snapshots[0].records.map(
        (record) => mappings[0].get(recordRef(record)) ?? recordRef(record),
      ),
    ),
  ];
  const importedRefs = [...new Set(introducedRecords)];
  const records = graph.nodes
    .filter((n) => n.recordRef)
    .map((n) => catalog.get(n.recordRef));
  const contexts = new Map();
  for (const snapshot of snapshots)
    for (const [id, context] of snapshot.contexts ?? []) {
      const previous = contexts.get(id);
      // An older imported graph must not move an already incorporated source
      // boundary backwards. Explicit sources below establish their CURRENT
      // boundary, including intentional rollback/replacement remerges.
      contexts.set(
        id,
        previous?.snapshot &&
          (!context.snapshot ||
            previous.snapshot.recordCount > context.snapshot.recordCount)
          ? { ...context, snapshot: previous.snapshot }
          : context,
      );
    }
  for (const snapshot of snapshots)
    contexts.set(snapshot.thread.id, {
      id: snapshot.thread.id,
      name: snapshot.thread.name ?? null,
      cwd: snapshot.thread.cwd,
      gitInfo: snapshot.thread.gitInfo ?? null,
      forkedFromId: snapshot.thread.forkedFromId ?? null,
      ...(snapshot.nativeVersion ? { snapshot: snapshot.nativeVersion } : {}),
    });
  const archive = {
    format: "codex-merge-evidence-v1",
    primarySourceThreadId: primary.threadId,
    baseRefs,
    primaryRefs: primary.path.filter((id) => nodes.get(id).recordRef),
    importedRefs,
    sources: [...contexts.values()],
    records,
    graph,
  };
  return {
    graph,
    archive,
    imports,
    primary,
    reusedGraph,
    summary: {
      baseThreadId: primary.threadId,
      sourceThreadIds: sequences.map((s) => s.threadId),
      importedItems: importedRefs.length,
      skippedItems,
      graphNodes: graph.nodes.length,
      mergeNodeId,
      reusedGraph,
    },
  };
}
