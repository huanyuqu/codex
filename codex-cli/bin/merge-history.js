import { createHash } from "node:crypto";
import { open } from "node:fs/promises";
import path from "node:path";
import * as zlib from "node:zlib";
import { promisify } from "node:util";

export const MERGE_PREFIX = "Codex context merge (historical branch data):\n";
const MAX_ROLLOUT_BYTES = 64 * 1024 * 1024;
export const hash = (value) => createHash("sha256").update(value).digest("hex");
export const recordRef = (record) =>
  "r_" + hash(record.key + "\n" + record.json).slice(0, 24);

// Preserve the original JSON representation of tool results, including large
// integer literals. JSON.parse/stringify would silently round those literals.
export function rawField(json, field) {
  let i = 0;
  const whitespace = () => {
    while (/\s/.test(json[i] ?? "") && i < json.length) i++;
  };
  const valueEnd = () => {
    const start = i;
    let depth = 0;
    let quoted = false;
    let escaped = false;
    for (; i < json.length; i++) {
      const c = json[i];
      if (quoted) {
        if (escaped) escaped = false;
        else if (c === "\\") escaped = true;
        else if (c === '"') quoted = false;
      } else if (c === '"') quoted = true;
      else if (c === "{" || c === "[") depth++;
      else if (c === "}" || c === "]") {
        if (depth === 0) break;
        depth--;
      } else if (c === "," && depth === 0) break;
    }
    return json.slice(start, i).trim();
  };
  whitespace();
  if (json[i++] !== "{") throw new Error("Expected a JSON object");
  while (i < json.length) {
    whitespace();
    if (json[i] === "}") break;
    const keyStart = i;
    if (json[i++] !== '"') throw new Error("Invalid JSON object key");
    for (; i < json.length; i++) {
      if (json[i] === "\\") i++;
      else if (json[i] === '"') {
        i++;
        break;
      }
    }
    const key = JSON.parse(json.slice(keyStart, i));
    whitespace();
    if (json[i++] !== ":") throw new Error("Invalid JSON field");
    whitespace();
    const value = valueEnd();
    if (key === field) return value;
    if (json[i] === ",") i++;
  }
  return undefined;
}

export function parseMergeItem(item) {
  if (item.type !== "message" || item.role !== "user" || !item.content?.length)
    return null;
  const text = item.content[0]?.text;
  if (typeof text !== "string" || !text.startsWith(MERGE_PREFIX)) return null;
  let bundle;
  try {
    bundle = JSON.parse(text.slice(MERGE_PREFIX.length));
  } catch {
    return null;
  }
  if (
    bundle.format !== "codex-context-merge-v1" ||
    !Array.isArray(bundle.branches)
  )
    return null;
  for (const branch of bundle.branches) {
    if (!Array.isArray(branch.records)) return null;
    for (const record of branch.records) {
      if (
        typeof record.key !== "string" ||
        typeof record.json !== "string" ||
        typeof record.sourceThreadId !== "string"
      )
        return null;
      try {
        JSON.parse(record.json);
      } catch {
        return null;
      }
    }
  }
  return bundle;
}

export function rawArrayItems(json) {
  if (!json.startsWith("[") || !json.endsWith("]"))
    throw new Error("Expected a JSON array");
  const items = [];
  let start = 1;
  let depth = 0;
  let quoted = false;
  let escaped = false;
  for (let i = 1; i < json.length - 1; i++) {
    const c = json[i];
    if (quoted) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') quoted = false;
    } else if (c === '"') quoted = true;
    else if (c === "{" || c === "[") depth++;
    else if (c === "}" || c === "]") depth--;
    else if (c === "," && depth === 0) {
      items.push(json.slice(start, i).trim());
      start = i + 1;
    }
  }
  const tail = json.slice(start, -1).trim();
  if (tail) items.push(tail);
  return items;
}

export function isConversationItem(item) {
  if (item.type === "message") {
    if (item.role !== "user" && item.role !== "assistant") return false;
    const kinds =
      item.internal_chat_message_metadata_passthrough?.content_item_kinds ?? [];
    if (
      kinds.length &&
      kinds.every(
        (kind) =>
          kind === "environments.environment_context" ||
          kind.startsWith("agents_md."),
      )
    )
      return false;
    // Legacy environment records predate content_item_kinds.
    if (
      !kinds.length &&
      item.role === "user" &&
      item.content?.length === 1 &&
      /^<environment_context>[\s\S]*<\/environment_context>\s*$/.test(
        item.content[0]?.text ?? "",
      )
    )
      return false;
    return true;
  }
  return [
    "function_call",
    "function_call_output",
    "custom_tool_call",
    "custom_tool_call_output",
    "web_search_call",
    "image_generation_call",
    "local_shell_call",
    "agent_message",
    "inter_agent_communication",
  ].includes(item.type);
}

async function readRollout(filePath, endByteOffset) {
  let handle;
  let compressed = false;
  try {
    handle = await open(filePath, "r");
  } catch (error) {
    if (error.code !== "ENOENT" || filePath.endsWith(".zst")) throw error;
    handle = await open(`${filePath}.zst`, "r");
    compressed = true;
  }
  compressed ||= filePath.endsWith(".zst");
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size > MAX_ROLLOUT_BYTES)
      throw new Error(
        `Rollout is not a file or exceeds ${MAX_ROLLOUT_BYTES} bytes: ${filePath}`,
      );
    let buffer;
    if (compressed) {
      if (!zlib.zstdDecompress)
        throw new Error(
          "Reading compressed Codex rollouts requires Node.js 22.15 or newer",
        );
      buffer = await promisify(zlib.zstdDecompress)(await handle.readFile(), {
        maxOutputLength: MAX_ROLLOUT_BYTES,
      });
      if (endByteOffset !== undefined) {
        if (
          !Number.isSafeInteger(endByteOffset) ||
          endByteOffset < 0 ||
          endByteOffset > buffer.length
        )
          throw new Error(
            `Invalid compressed history reference offset: ${filePath}`,
          );
        buffer = buffer.subarray(0, endByteOffset);
      }
    } else {
      const length = endByteOffset ?? before.size;
      if (!Number.isSafeInteger(length) || length < 0 || length > before.size)
        throw new Error(`Invalid history reference offset: ${filePath}`);
      buffer = Buffer.alloc(length);
      let offset = 0;
      while (offset < length) {
        const { bytesRead } = await handle.read(
          buffer,
          offset,
          length - offset,
          offset,
        );
        if (!bytesRead)
          throw new Error(`Rollout was truncated while reading: ${filePath}`);
        offset += bytesRead;
      }
    }
    const after = await handle.stat();
    if (
      before.ino !== after.ino ||
      before.size !== after.size ||
      before.mtimeMs !== after.mtimeMs
    )
      throw new Error(
        `Thread history changed during merge; retry when idle: ${filePath}`,
      );
    if (buffer.length && buffer[buffer.length - 1] !== 10)
      throw new Error(
        `Incomplete rollout record; retry when idle: ${filePath}`,
      );
    return buffer.toString("utf8");
  } finally {
    await handle.close();
  }
}

/** Resolve local reference-backed forks without acquiring source write locks. */
export async function loadSnapshot(
  client,
  threadId,
  { chain = [], endByteOffset, endOrdinalExclusive } = {},
) {
  if (chain.includes(threadId) || chain.length >= 64)
    throw new Error("Cyclic or excessively deep Codex history reference");
  const { thread } = await client.request("thread/read", {
    threadId,
    includeTurns: false,
  });
  if (thread.id !== threadId)
    throw new Error("Codex returned a different source thread");
  if (
    endByteOffset === undefined &&
    ["active", "systemError"].includes(thread.status?.type)
  )
    throw new Error(`Thread ${threadId} must be idle before merging`);
  if (!thread.path || !path.isAbsolute(thread.path) || thread.ephemeral)
    throw new Error(`Thread ${threadId} must have a local persisted rollout`);
  const text = await readRollout(thread.path, endByteOffset);
  const lines = text
    .split("\n")
    .filter(Boolean)
    .map((raw, index) => {
      try {
        return { raw, item: JSON.parse(raw), index };
      } catch {
        throw new Error(`Invalid rollout record ${index + 1} in ${threadId}`);
      }
    });
  const meta = lines[0]?.item;
  if (meta?.type !== "session_meta" || meta.payload?.id !== threadId)
    throw new Error(`Rollout identity mismatch for ${threadId}`);
  let records = [];
  let inProgress = false;
  let hasCompaction = false;
  let currentTurnId = null;
  let tokenUsage = null;
  const base = meta.payload.history_base;
  if (base) {
    if (
      typeof base.thread_id !== "string" ||
      !Number.isSafeInteger(base.end_byte_offset) ||
      !Number.isSafeInteger(base.end_ordinal_exclusive)
    )
      throw new Error("Unsupported Codex history reference");
    const parent = await loadSnapshot(client, base.thread_id, {
      chain: [...chain, threadId],
      endByteOffset: base.end_byte_offset,
      endOrdinalExclusive: base.end_ordinal_exclusive,
    });
    records = [...parent.records];
    inProgress = parent.inProgress;
    currentTurnId = parent.currentTurnId;
    hasCompaction = parent.hasCompaction;
    tokenUsage = parent.tokenUsage;
  }
  const append = (item, raw, ordinal) => {
    if (!isConversationItem(item)) return;
    const json = raw;
    const fingerprint = hash(json);
    // call_id only correlates a tool request/result within its source. Separate
    // branches can reuse it, so it is not a globally unique item identity.
    const identity = item.id;
    const key = identity
      ? `${item.type}:${identity}:${fingerprint}`
      : `${threadId}:${ordinal}:${fingerprint}`;
    const isUserBoundary =
      (item.type === "message" && item.role === "user") ||
      ["agent_message", "inter_agent_communication"].includes(item.type);
    records.push({
      key,
      fingerprint,
      sourceThreadId: threadId,
      ordinal,
      turnId:
        item.internal_chat_message_metadata_passthrough?.turn_id ??
        currentTurnId,
      isUserBoundary,
      json,
    });
  };
  for (const { raw, item: line, index } of lines.slice(1)) {
    if (
      endOrdinalExclusive !== undefined &&
      (!Number.isSafeInteger(line.ordinal) ||
        line.ordinal >= endOrdinalExclusive)
    )
      throw new Error("History reference does not match its ordinal boundary");
    const payload = line.payload;
    if (line.type === "turn_context")
      currentTurnId = payload.turn_id ?? currentTurnId;
    else if (line.type === "event_msg") {
      if (payload.type === "token_count" && payload.info) {
        const last = payload.info.last_token_usage;
        const window = payload.info.model_context_window;
        if (
          Number.isSafeInteger(last?.input_tokens) &&
          Number.isSafeInteger(last?.output_tokens) &&
          last.input_tokens >= 0 &&
          last.output_tokens >= 0
        ) {
          tokenUsage = {
            inputTokens: last.input_tokens,
            outputTokens: last.output_tokens,
            contextWindow:
              Number.isSafeInteger(window) && window > 0 ? window : null,
            atRecordCount: records.length,
          };
        }
      }
      if (["task_started", "turn_started"].includes(payload.type)) {
        inProgress = true;
        currentTurnId = payload.turn_id ?? currentTurnId;
      } else if (
        [
          "task_complete",
          "task_completed",
          "turn_complete",
          "turn_completed",
          "turn_aborted",
        ].includes(payload.type)
      )
        inProgress = false;
      else if (payload.type === "thread_rolled_back") {
        tokenUsage = null;
        const count = payload.num_turns;
        if (!Number.isSafeInteger(count) || count < 0)
          throw new Error("Unsupported Codex rollback marker");
        if (count && hasCompaction)
          throw new Error(
            "This rollout combines compaction and rollback; canonical checkpoint replay is required before merging",
          );
        // Native rollback counts instruction boundaries, including steering
        // messages within a single turn, rather than unique turn IDs.
        const positions = records.flatMap((r, index) =>
          r.isUserBoundary ? [index] : [],
        );
        if (count && positions.length)
          records = records.slice(
            0,
            positions[Math.max(0, positions.length - count)],
          );
      }
    } else if (line.type === "response_item")
      append(payload, rawField(raw, "payload"), line.ordinal ?? index);
    else if (line.type === "inter_agent_communication")
      append({ type: line.type }, raw, line.ordinal ?? index);
    else if (line.type === "compacted") {
      tokenUsage = null;
      // Import the compacted context rather than resurrecting discarded items.
      hasCompaction = true;
      const replacement = payload.replacement_history;
      if (Array.isArray(replacement)) {
        records = [];
        const rawReplacement = rawField(
          rawField(raw, "payload"),
          "replacement_history",
        );
        rawArrayItems(rawReplacement).forEach((entry, i) =>
          append(JSON.parse(entry), entry, `${line.ordinal ?? index}.${i}`),
        );
      } else if (typeof payload.message === "string") {
        // Native legacy replay keeps bounded user text alongside its summary.
        // Do not silently approximate its truncation or discarded media rules.
        const users = records.filter((record) => {
          const item = JSON.parse(record.json);
          return item.type === "message" && item.role === "user";
        });
        let userBytes = 0;
        for (const record of users) {
          const item = JSON.parse(record.json);
          if (item.content.some((part) => part.type !== "input_text"))
            throw new Error(
              "Legacy compaction with media requires a replacement-history checkpoint before merging",
            );
          userBytes += Buffer.byteLength(
            item.content.map((part) => part.text).join("\n"),
            "utf8",
          );
        }
        if (userBytes > 80000)
          throw new Error(
            "Legacy compaction exceeds its user-text budget; a replacement-history checkpoint is required before merging",
          );
        records = users;
        const summary = {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: payload.message }],
        };
        append(summary, JSON.stringify(summary), line.ordinal ?? index);
      } else throw new Error("Unsupported Codex compaction checkpoint");
    }
  }
  if (endByteOffset === undefined && inProgress)
    throw new Error(
      `Thread ${threadId} has an unfinished turn; finish or interrupt it before merging`,
    );
  return {
    thread,
    records,
    inProgress,
    currentTurnId,
    hasCompaction,
    tokenUsage,
  };
}
