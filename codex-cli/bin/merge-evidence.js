import { mkdir, open, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  hash,
  isConversationItem,
  parseMergeItem,
  recordRef,
} from "./merge-history.js";
import {
  ancestors,
  isFusionItem,
  parseGraphItem,
  validateGraph,
} from "./merge-graph.js";

export const SEMANTIC_PREFIX =
  "Codex semantic context merge (model-generated historical state):\n";
export const IMAGE_PREFIX =
  "Codex merge images (quoted historical evidence):\n";
export const FORK_PREFIX = "Codex merge native fork annotations:\n";
export const MAX_EVIDENCE_BYTES = 64 * 1024 * 1024;
export { recordRef };

export function isNativeForkAnnotation(record) {
  const item = JSON.parse(record.json);
  const kinds =
    item.internal_chat_message_metadata_passthrough?.content_item_kinds;
  return (
    item.type === "message" &&
    item.role === "user" &&
    kinds?.length === 1 &&
    kinds[0] === "generic.turn_aborted" &&
    item.content?.length === 1 &&
    item.content[0].type === "input_text" &&
    /^<turn_aborted>\nThe user interrupted the previous turn on purpose\.[\s\S]*\n<\/turn_aborted>$/.test(
      item.content[0].text,
    )
  );
}

function parseForkItem(item) {
  const text =
    item.type === "message" &&
    item.role === "assistant" &&
    item.content?.[0]?.text;
  if (typeof text !== "string" || !text.startsWith(FORK_PREFIX)) return null;
  let note;
  try {
    note = JSON.parse(text.slice(FORK_PREFIX.length));
  } catch {
    throw new Error("Invalid native fork annotation provenance");
  }
  if (
    note.format !== "codex-merge-fork-annotations-v1" ||
    !Array.isArray(note.fingerprints) ||
    note.fingerprints.some((fingerprint) => !/^[a-f0-9]{64}$/.test(fingerprint))
  )
    throw new Error("Invalid native fork annotation provenance");
  return note;
}

export function forkAnnotationItem(records) {
  if (records.some((record) => !isNativeForkAnnotation(record)))
    throw new Error("Unexpected native fork history change");
  return {
    type: "message",
    role: "assistant",
    phase: "final_answer",
    content: [
      {
        type: "output_text",
        text:
          FORK_PREFIX +
          JSON.stringify({
            format: "codex-merge-fork-annotations-v1",
            fingerprints: records.map((r) => hash(r.json)),
            explanation:
              "These interruption notices were added by native fork to an idle, unchanged source snapshot. They are merge metadata, not a new user interruption or new task evidence. Retain the existing merged task state.",
          }),
      },
    ],
  };
}

export function parseSemanticItem(item) {
  const text =
    item.type === "message" &&
    ["user", "assistant"].includes(item.role) &&
    item.content?.[0]?.text;
  if (typeof text !== "string" || !text.startsWith(SEMANTIC_PREFIX))
    return null;
  let capsule;
  try {
    capsule = JSON.parse(text.slice(SEMANTIC_PREFIX.length));
  } catch {
    throw new Error("Invalid semantic merge provenance");
  }
  if (
    capsule.format !== "codex-semantic-merge-v1" ||
    !path.isAbsolute(capsule.evidence?.path ?? "") ||
    !/^[a-f0-9]{64}$/.test(capsule.evidence?.sha256 ?? "")
  )
    throw new Error("Invalid semantic merge evidence reference");
  return capsule;
}

export function parseImageItem(item) {
  const text =
    item.type === "message" && item.role === "user" && item.content?.[0]?.text;
  if (typeof text !== "string" || !text.startsWith(IMAGE_PREFIX)) return null;
  let bundle;
  try {
    bundle = JSON.parse(text.slice(IMAGE_PREFIX.length));
  } catch {
    throw new Error("Invalid merge image provenance");
  }
  if (
    bundle.format !== "codex-merge-images-v1" ||
    !path.isAbsolute(bundle.evidence?.path ?? "") ||
    !/^[a-f0-9]{64}$/.test(bundle.evidence?.sha256 ?? "") ||
    !Array.isArray(bundle.refs) ||
    !bundle.refs.length
  )
    throw new Error("Invalid merge image evidence");
  return bundle;
}

function checkRecord(record) {
  if (
    !record ||
    typeof record.key !== "string" ||
    !record.key ||
    typeof record.json !== "string" ||
    typeof record.sourceThreadId !== "string" ||
    !record.sourceThreadId
  )
    throw new Error("Invalid archived source record");
  let item;
  try {
    item = JSON.parse(record.json);
  } catch {
    throw new Error("Invalid archived source JSON");
  }
  if (!isConversationItem(item))
    throw new Error(
      "Evidence contains a non-conversation instruction or record",
    );
  if (
    record.ref !== recordRef(record) ||
    record.fingerprint !== hash(record.json)
  )
    throw new Error("Evidence record checksum mismatch");
}

export function validateEvidence(archive) {
  if (
    archive?.format !== "codex-merge-evidence-v1" ||
    !Array.isArray(archive.records) ||
    !Array.isArray(archive.sources) ||
    typeof archive.primarySourceThreadId !== "string"
  )
    throw new Error("Invalid merge evidence archive");
  const catalog = new Map();
  for (const record of archive.records) {
    checkRecord(record);
    if (catalog.has(record.ref))
      throw new Error("Duplicate evidence reference");
    catalog.set(record.ref, record);
  }
  for (const field of ["baseRefs", "primaryRefs", "importedRefs"]) {
    if (
      !Array.isArray(archive[field]) ||
      new Set(archive[field]).size !== archive[field].length ||
      archive[field].some((ref) => !catalog.has(ref))
    )
      throw new Error("Invalid evidence " + field);
  }
  if (archive.primaryRefs.some((ref) => !archive.baseRefs.includes(ref)))
    throw new Error("Primary evidence must belong to the base history");
  if (archive.graph) validateGraph(archive.graph, catalog);
  return catalog;
}

export function evidenceLocation(env = process.env) {
  return path.join(
    env.CODEX_HOME ?? path.join(os.homedir(), ".codex"),
    "merges",
    "evidence",
  );
}

/** Content-addressed, exclusive writes make every referenced snapshot immutable. */
export async function saveEvidence(archive, directory) {
  validateEvidence(archive);
  const text = JSON.stringify(archive) + "\n";
  if (Buffer.byteLength(text) > MAX_EVIDENCE_BYTES)
    throw new Error("Merge evidence exceeds its archive size limit");
  const sha256 = hash(text);
  const file = path.resolve(directory, sha256 + ".json");
  await mkdir(directory, { recursive: true, mode: 0o700 });
  try {
    await writeFile(file, text, { flag: "wx", mode: 0o600 });
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
    await readEvidence({ path: file, sha256 });
  }
  return { path: file, sha256 };
}

export async function readEvidence(evidence) {
  if (
    !path.isAbsolute(evidence.path) ||
    !/^[a-f0-9]{64}$/.test(evidence.sha256)
  )
    throw new Error("Invalid evidence location");
  let handle;
  try {
    handle = await open(evidence.path, "r");
    const before = await handle.stat();
    if (!before.isFile() || before.size > MAX_EVIDENCE_BYTES)
      throw new Error("Evidence archive exceeds its size limit");
    const text = await handle.readFile("utf8");
    const after = await handle.stat();
    if (
      before.size !== after.size ||
      before.mtimeMs !== after.mtimeMs ||
      hash(text) !== evidence.sha256
    )
      throw new Error("Merge evidence checksum mismatch");
    const archive = JSON.parse(text);
    validateEvidence(archive);
    return archive;
  } catch (error) {
    throw new Error(
      "Cannot read merge evidence " + evidence.path + ": " + error.message,
    );
  } finally {
    await handle?.close();
  }
}

/** Resolve capsules to original records, never summarize an older summary. */
export async function expandSnapshot(snapshot, cache = new Map()) {
  let records = [];
  let primaryKeys = new Set();
  let primarySourceThreadId = snapshot.thread.id;
  const capsules = [];
  const contexts = new Map();
  let timeline = [];
  const graphs = [];
  const graphRecords = new Map();
  const graphCapsules = [];
  const legacyMerges = new Map();
  const ignoredAnnotations = new Set(
    snapshot.records.flatMap(
      (record) => parseForkItem(JSON.parse(record.json))?.fingerprints ?? [],
    ),
  );
  const append = async (record, isPrimary, depth = 0) => {
    if (depth >= 64) throw new Error("Merge provenance is too deeply nested");
    const item = JSON.parse(record.json);
    const forkNote = parseForkItem(item);
    if (forkNote) {
      for (const fingerprint of forkNote.fingerprints)
        ignoredAnnotations.add(fingerprint);
      records = records.filter(
        (entry) =>
          !(
            ignoredAnnotations.has(hash(entry.json)) &&
            isNativeForkAnnotation(entry)
          ),
      );
      const kept = new Set(records.map(recordRef));
      timeline = timeline.filter((id) => !id.startsWith("r_") || kept.has(id));
      return;
    }
    if (
      ignoredAnnotations.has(hash(record.json)) &&
      isNativeForkAnnotation(record)
    )
      return;
    if (isFusionItem(item)) return;
    const raw = parseMergeItem(item);
    if (raw) {
      for (const branch of raw.branches) {
        contexts.set(branch.threadId, {
          id: branch.threadId,
          cwd: branch.cwd,
          name: branch.name,
        });
        for (const nested of branch.records)
          await append(nested, false, depth + 1);
      }
      if (isPrimary) {
        const id = "m_" + hash(record.json).slice(0, 24);
        legacyMerges.set(id, {
          branches: raw.branches.map((branch) => branch.records),
        });
        timeline.push(id);
      }
      return;
    }
    const graphCapsule = parseGraphItem(item);
    if (graphCapsule) {
      const key =
        graphCapsule.evidence.path + ":" + graphCapsule.evidence.sha256;
      if (!cache.has(key))
        cache.set(key, await readEvidence(graphCapsule.evidence));
      const archive = cache.get(key);
      if (
        !archive.graph ||
        archive.graph.mergeNodeId !== graphCapsule.mergeNodeId
      )
        throw new Error("Conversation graph does not match its saved frame");
      const catalog = validateEvidence(archive);
      const base = archive.baseRefs.map((ref) => catalog.get(ref));
      if (isPrimary) {
        const embedded =
          graphCapsule.primaryEmbedded === true && records.length === 0;
        if (
          !embedded &&
          (base.length !== records.length ||
            base.some((entry, i) => entry.json !== records[i].json))
        ) {
          throw new Error(
            "Conversation graph prefix changed; a canonical history checkpoint is required",
          );
        }
        records = [...base];
        primaryKeys = new Set(
          archive.primaryRefs.map((ref) => catalog.get(ref).key),
        );
        primarySourceThreadId = archive.primarySourceThreadId;
        timeline = [...archive.graph.primaryPath];
        if (timeline.at(-1) !== graphCapsule.mergeNodeId)
          timeline.push(graphCapsule.mergeNodeId);
      }
      for (const original of archive.records)
        graphRecords.set(original.ref, original);
      for (const ref of archive.importedRefs) {
        const original = catalog.get(ref);
        if (
          !records.some(
            (entry) =>
              entry.key === original.key && entry.json === original.json,
          )
        )
          records.push(original);
      }
      for (const context of archive.sources) contexts.set(context.id, context);
      const expectedImages = (graphCapsule.imageRefs ?? []).flatMap((ref) => {
        const original = catalog.get(ref);
        if (!original) throw new Error("Missing graph image reference");
        const data = JSON.parse(original.json);
        return [
          ...(data.content ?? []),
          ...(Array.isArray(data.output) ? data.output : []),
        ].filter((part) => part.type === "input_image");
      });
      if (
        JSON.stringify(expectedImages) !== JSON.stringify(item.content.slice(1))
      )
        throw new Error("Graph images do not match original evidence");
      graphs.push(archive.graph);
      graphCapsules.push(graphCapsule);
      return;
    }
    const capsule = parseSemanticItem(item);
    const imageBundle = parseImageItem(item);
    if (imageBundle) {
      const key = imageBundle.evidence.path + ":" + imageBundle.evidence.sha256;
      if (!cache.has(key))
        cache.set(key, await readEvidence(imageBundle.evidence));
      const catalog = validateEvidence(cache.get(key));
      const expected = imageBundle.refs.flatMap((ref) => {
        if (!catalog.has(ref))
          throw new Error("Missing archived image reference");
        const original = JSON.parse(catalog.get(ref).json);
        return [
          ...(original.content ?? []),
          ...(Array.isArray(original.output) ? original.output : []),
        ].filter((part) => part.type === "input_image");
      });
      if (JSON.stringify(expected) !== JSON.stringify(item.content.slice(1)))
        throw new Error("Merge image evidence does not match its archive");
      return;
    }
    if (capsule) {
      const key = capsule.evidence.path + ":" + capsule.evidence.sha256;
      if (!cache.has(key)) cache.set(key, await readEvidence(capsule.evidence));
      const archive = cache.get(key);
      const catalog = new Map(
        archive.records.map((entry) => [entry.ref, entry]),
      );
      const base = archive.baseRefs.map((ref) => catalog.get(ref));
      // Legacy native forks change fallback identities. Restore the archived
      // identities only when their complete prefix still matches byte for byte.
      if (
        isPrimary &&
        base.length === records.length &&
        base.every((entry, i) => entry.json === records[i].json)
      ) {
        const aliases = new Map(
          records.map((entry, i) => [recordRef(entry), base[i].ref]),
        );
        timeline = timeline.map((id) => aliases.get(id) ?? id);
        records = [...base];
        primaryKeys = new Set(
          archive.primaryRefs.map((ref) => catalog.get(ref).key),
        );
        primarySourceThreadId = archive.primarySourceThreadId;
      }
      for (const context of archive.sources) contexts.set(context.id, context);
      for (const ref of archive.importedRefs) {
        const original = catalog.get(ref);
        if (
          !records.some(
            (entry) =>
              entry.key === original.key && entry.json === original.json,
          )
        )
          await append(original, false, depth + 1);
      }
      capsules.push(capsule);
      if (isPrimary) {
        const id = "m_" + hash(record.json).slice(0, 24);
        legacyMerges.set(id, {
          branches: archive.importedRefs.length
            ? [archive.importedRefs.map((ref) => catalog.get(ref))]
            : [],
        });
        timeline.push(id);
      }
      return;
    }
    records.push(record);
    if (isPrimary) {
      primaryKeys.add(record.key);
      timeline.push(recordRef(record));
    }
  };
  for (const record of snapshot.records) await append(record, true);
  return {
    ...snapshot,
    records,
    primaryKeys,
    primarySourceThreadId,
    capsules,
    contexts,
    timeline,
    graphs,
    graphRecords,
    graphCapsules,
    legacyMerges,
  };
}

export function prepareEvidence(snapshots, plan) {
  const records = [];
  const byKey = new Map();
  const add = (record) => {
    const previous = byKey.get(record.key);
    if (previous) {
      if (previous.json !== record.json)
        throw new Error("Source record identity collision");
      return previous.ref;
    }
    const entry = {
      ...record,
      fingerprint: hash(record.json),
      ref: recordRef(record),
    };
    records.push(entry);
    byKey.set(entry.key, entry);
    return entry.ref;
  };
  const baseRefs = snapshots[0].records.map(add);
  const importedRefs = plan.bundle.branches.flatMap((branch) =>
    branch.records.map(add),
  );
  const primaryRefs = snapshots[0].records
    .filter((record) => snapshots[0].primaryKeys?.has(record.key) ?? true)
    .map(add);
  const contexts = new Map(
    snapshots.flatMap((s) => [...(s.contexts?.entries() ?? [])]),
  );
  for (const snapshot of snapshots) {
    contexts.set(snapshot.thread.id, {
      id: snapshot.thread.id,
      name: snapshot.thread.name ?? null,
      cwd: snapshot.thread.cwd,
      gitInfo: snapshot.thread.gitInfo ?? null,
      forkedFromId: snapshot.thread.forkedFromId ?? null,
    });
  }
  const archive = {
    format: "codex-merge-evidence-v1",
    primarySourceThreadId:
      snapshots[0].primarySourceThreadId ?? snapshots[0].thread.id,
    baseRefs,
    primaryRefs,
    importedRefs,
    sources: [...contexts.values()],
    records,
  };
  validateEvidence(archive);
  return archive;
}

function parseReadArgs(args, graphOnly = false) {
  const options = { positional: [], maxBytes: 64 * 1024, offset: 0, limit: 20 };
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === "--json") options.json = true;
    else if (arg === "--mermaid" && graphOnly) options.mermaid = true;
    else if (arg === (graphOnly ? "--node" : "--branch")) {
      const value = args[++i];
      if (!value || value.startsWith("--"))
        throw new Error(arg + " requires a value");
      options[arg.slice(2)] = value;
    } else if (
      (graphOnly
        ? ["--max-bytes", "--max-tokens"]
        : [
            "--max-bytes",
            "--max-tokens",
            "--offset",
            "--limit",
            "--start-byte",
            "--length-bytes",
          ]
      ).includes(arg)
    ) {
      const value = args[++i];
      if (
        !/^[0-9]+$/.test(value ?? "") ||
        !Number.isSafeInteger(Number(value)) ||
        (Number(value) === 0 && !["--offset", "--start-byte"].includes(arg))
      )
        throw new Error("Invalid " + arg);
      const key = {
        "--max-bytes": "maxBytes",
        "--max-tokens": "maxTokens",
        "--offset": "offset",
        "--limit": "limit",
        "--start-byte": "startByte",
        "--length-bytes": "lengthBytes",
      }[arg];
      options[key] = Number(value);
    } else if (arg.startsWith("-"))
      throw new Error("Unknown evidence option: " + arg);
    else options.positional.push(arg);
  }
  if (!options.positional.length)
    throw new Error(
      "Usage: codex merge " +
        (graphOnly ? "graph" : "evidence") +
        " <ARCHIVE_PATH> [REF...] [OPTIONS]",
    );
  if (
    graphOnly &&
    (options.positional.length !== 1 || (options.json && options.mermaid))
  )
    throw new Error(
      "Graph reads require one archive and either --json or --mermaid",
    );
  return options;
}

const outputFits = (text, options) =>
  Buffer.byteLength(text) <=
  Math.min(options.maxBytes, options.maxTokens ?? Infinity);
function printBounded(text, options) {
  if (!outputFits(text, options))
    throw new Error(
      "Evidence output exceeds --max-bytes or --max-tokens; request a page or a byte range",
    );
  process.stdout.write(text);
}

export async function runEvidenceCommand(args) {
  try {
    const options = parseReadArgs(args);
    const file = path.resolve(options.positional[0]);
    const archive = await readEvidence({
      path: file,
      sha256: path.basename(file, ".json"),
    });
    const catalog = validateEvidence(archive);
    const refs = options.positional.slice(1);
    if (options.branch && refs.length)
      throw new Error("Use either --branch or explicit node references");
    if (options.startByte !== undefined || options.lengthBytes !== undefined) {
      if (refs.length !== 1)
        throw new Error(
          "Byte ranges require exactly one original node reference",
        );
      const record = catalog.get(refs[0]);
      if (!record) throw new Error("Unknown evidence reference: " + refs[0]);
      const bytes = Buffer.from(record.json);
      const start = options.startByte ?? 0;
      let end = Math.min(bytes.length, start + (options.lengthBytes ?? 1024));
      if (start >= bytes.length || (bytes[start] & 0xc0) === 0x80)
        throw new Error(
          "Byte range starts outside the node or inside a UTF-8 character",
        );
      while (end < bytes.length && (bytes[end] & 0xc0) === 0x80) end--;
      printBounded(
        JSON.stringify({
          ref: record.ref,
          sourceThreadId: record.sourceThreadId,
          startByte: start,
          endByte: end,
          totalBytes: bytes.length,
          fragment: bytes.subarray(start, end).toString("utf8"),
          nextByte: end < bytes.length ? end : null,
        }) + "\n",
        options,
      );
      return 0;
    }
    if (refs.length) {
      const selected = refs.map((ref) => {
        if (!catalog.has(ref))
          throw new Error("Unknown evidence reference: " + ref);
        return catalog.get(ref);
      });
      const output = options.json
        ? JSON.stringify(selected) + "\n"
        : selected
            .map((r) => r.ref + " (" + r.sourceThreadId + ")\n" + r.json)
            .join("\n") + "\n";
      printBounded(output, options);
      return 0;
    }
    let records = archive.records;
    if (options.branch) {
      if (!archive.graph)
        throw new Error("Branch reads require a conversation graph archive");
      const { nodes } = validateGraph(archive.graph, catalog);
      const branch = archive.graph.branches.find(
        (b) => b.threadId === options.branch,
      );
      if (!branch) throw new Error("Unknown graph branch: " + options.branch);
      const selected = ancestors(nodes, branch.head);
      const common = ancestors(nodes, branch.forkPoint);
      records = records.filter(
        (r) => selected.has(r.ref) && !common.has(r.ref),
      );
    }
    const page = records.slice(options.offset, options.offset + options.limit);
    const serialize = () =>
      JSON.stringify({
        sources: archive.sources.filter((source) =>
          page.some((record) => record.sourceThreadId === source.id),
        ),
        branch: options.branch ?? null,
        records: options.branch
          ? page
          : page.map(({ ref, sourceThreadId, ordinal, json }) => ({
              ref,
              sourceThreadId,
              ordinal,
              bytes: Buffer.byteLength(json),
            })),
        totalRecords: records.length,
        offset: options.offset,
        nextOffset:
          options.offset + page.length < records.length
            ? options.offset + page.length
            : null,
      }) + "\n";
    while (page.length && !outputFits(serialize(), options)) page.pop();
    if (!page.length && records.length > options.offset)
      throw new Error(
        "A node or index entry exceeds the read budget; request its reference with --start-byte and --length-bytes",
      );
    printBounded(serialize(), options);
    return 0;
  } catch (error) {
    process.stderr.write("codex merge evidence: " + error.message + "\n");
    return 1;
  }
}

export async function runGraphCommand(args) {
  try {
    const options = parseReadArgs(args, true);
    const file = path.resolve(options.positional[0]);
    const archive = await readEvidence({
      path: file,
      sha256: path.basename(file, ".json"),
    });
    if (!archive.graph)
      throw new Error("This archive predates conversation graphs");
    const graph = archive.graph;
    const { nodes } = validateGraph(graph, validateEvidence(archive));
    let value = options.json
      ? graph
      : {
          format: graph.format,
          nodeCount: nodes.size,
          merge: nodes.get(graph.mergeNodeId),
          branches: graph.branches.map(({ path, ...branch }) => ({
            ...branch,
            pathLength: path.length,
          })),
        };
    if (options.node) {
      if (!nodes.has(options.node))
        throw new Error("Unknown graph node: " + options.node);
      value = {
        node: nodes.get(options.node),
        children: [...nodes.values()]
          .filter((n) => n.parents.includes(options.node))
          .map((n) => n.id),
      };
    }
    let output = JSON.stringify(value) + "\n";
    if (options.mermaid) {
      const selected = options.node
        ? ancestors(nodes, options.node)
        : new Set(nodes.keys());
      const lines = ["flowchart LR"];
      for (const node of nodes.values())
        if (selected.has(node.id)) {
          lines.push("  " + node.id + '["' + node.kind + " " + node.id + '"]');
          for (const parent of node.parents)
            if (selected.has(parent))
              lines.push("  " + parent + " --> " + node.id);
        }
      output = lines.join("\n") + "\n";
    }
    printBounded(output, options);
    return 0;
  } catch (error) {
    process.stderr.write("codex merge graph: " + error.message + "\n");
    return 1;
  }
}
