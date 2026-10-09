import { spawn } from "node:child_process";
import path from "node:path";
import { AppServerClient } from "./merge-rpc.js";
import { DEFAULT_MAX_BYTES, mergeThreads } from "./merge-context.js";
import { runEvidenceCommand, runGraphCommand } from "./merge-evidence.js";
import {
  DEFAULT_MAX_INPUT_BYTES,
  DEFAULT_SUMMARY_BYTES,
} from "./merge-semantic.js";

const HELP = `Merge Codex conversation contexts into a new resumable thread.

Usage: codex merge <BASE_THREAD_ID> <BRANCH_THREAD_ID>... [OPTIONS]
       codex merge <MERGED_THREAD_ID> --update [OPTIONS]

Options:
  --update            Merge pending updates from this chat's recorded sources
  --mode <MODE>       auto (default), inline, summary, reference, or legacy
  --context-tokens <N> Whole-context planning budget (uses native window if known)
  --reserve-tokens <N> Reserve for instructions and future dialogue (default: 4096)
  --goal <TEXT>       Optional focus for the cross-branch reading
  --model <MODEL>     Analysis model (defaults to configured Codex model)
  --summary-bytes <N> Maximum bytes per conversation reading (default: 16384)
  --max-input-bytes <N> Limit analysis input bytes (default: 524288)
  --timeout-seconds <N> Analysis deadline (default: 600)
  --evidence-dir <DIR> Store original records outside the default Codex home
  --name <TEXT>       Name the merged conversation
  --dry-run           Validate sources and show counts without creating a thread
  --json              Print a machine-readable result
  --resume            Open the merged conversation after saving it
  --max-bytes <N>     Limit added context in UTF-8 bytes (default: ${DEFAULT_MAX_BYTES})
  -C, --cd <DIR>      Working directory for the new conversation
  -c, --config <K=V>  Forward a Codex configuration override (repeatable)
  -h, --help          Show this help

The saved graph retains message nodes, fork points, and merge parents. Auto mode
embeds short branches and reads long branches into bounded narrative summaries.
Codex synthesizes the branches by default, preserving attributed disagreements.
Reference mode never calls a model. An oversized primary gets a compact view in
a fresh thread. All original records remain in the immutable evidence archive.
Sources must be persisted and idle; branch instructions retain their scopes.
Dry runs never call a model or write archives. Evidence can be read with:
  codex merge evidence <ARCHIVE_PATH> [REF...] [--json] [--max-bytes N]
  codex merge graph <ARCHIVE_PATH> [--json | --mermaid]
`;

export function parseMergeArgs(args, { allowSinglePrimary = false } = {}) {
  const options = {
    threadIds: [],
    config: [],
    maxBytes: DEFAULT_MAX_BYTES,
    mode: "auto",
  };
  let positionalOnly = false;
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (positionalOnly) {
      options.threadIds.push(arg);
      continue;
    }
    if (arg === "--") {
      positionalOnly = true;
      continue;
    }
    if (arg === "-h" || arg === "--help") {
      options.help = true;
      continue;
    }
    if (["--dry-run", "--json", "--resume", "--update"].includes(arg)) {
      options[
        {
          "--dry-run": "dryRun",
          "--json": "json",
          "--resume": "resume",
          "--update": "update",
        }[arg]
      ] = true;
    } else if (
      [
        "--name",
        "--mode",
        "--context-tokens",
        "--reserve-tokens",
        "--max-bytes",
        "--goal",
        "--model",
        "--summary-bytes",
        "--max-input-bytes",
        "--timeout-seconds",
        "--evidence-dir",
        "-c",
        "--config",
        "-C",
        "--cd",
      ].includes(arg)
    ) {
      const value = args[++i];
      if (value === undefined || value.startsWith("--"))
        throw new Error(`${arg} requires a value`);
      if (arg === "--name") options.name = value;
      else if (arg === "--mode") options.mode = value;
      else if (arg === "--goal") options.goal = value;
      else if (arg === "--model") options.model = value;
      else if (arg === "--evidence-dir")
        options.evidenceDir = path.resolve(value);
      else if (
        [
          "--summary-bytes",
          "--max-input-bytes",
          "--timeout-seconds",
          "--context-tokens",
          "--reserve-tokens",
        ].includes(arg)
      ) {
        if (
          !/^[1-9][0-9]*$/.test(value) ||
          !Number.isSafeInteger(Number(value))
        )
          throw new Error(arg + " requires a positive safe integer");
        const field = {
          "--summary-bytes": "summaryBytes",
          "--max-input-bytes": "maxInputBytes",
          "--timeout-seconds": "analysisTimeoutMs",
          "--context-tokens": "contextTokens",
          "--reserve-tokens": "reserveTokens",
        }[arg];
        options[field] =
          Number(value) * (arg === "--timeout-seconds" ? 1000 : 1);
      } else if (arg === "--max-bytes") {
        if (
          !/^[1-9][0-9]*$/.test(value) ||
          !Number.isSafeInteger(Number(value))
        )
          throw new Error("--max-bytes requires a positive safe integer");
        options.maxBytes = Number(value);
      } else if (arg === "-c" || arg === "--config") options.config.push(value);
      else options.cwd = path.resolve(value);
    } else if (arg.startsWith("-"))
      throw new Error(`Unknown merge option: ${arg}`);
    else options.threadIds.push(arg);
  }
  if (options.help) return options;
  if (
    !["auto", "inline", "summary", "reference", "legacy"].includes(options.mode)
  )
    throw new Error("Unknown merge mode");
  options.semantic = options.mode !== "reference";
  if (!options.semantic && options.goal !== undefined)
    throw new Error("--goal requires a mode that reads conversation content");
  for (const field of ["goal", "model"])
    if (options[field] !== undefined && !options[field].trim())
      throw new Error("--" + field + " must be non-empty");
  if (options.analysisTimeoutMs > 2147483647)
    throw new Error("--timeout-seconds is too large");
  if (options.semantic) {
    options.summaryBytes ??= DEFAULT_SUMMARY_BYTES;
    options.maxInputBytes ??= DEFAULT_MAX_INPUT_BYTES;
  }
  if (
    options.threadIds.length < (allowSinglePrimary || options.update ? 1 : 2) ||
    options.threadIds.length > 32 ||
    new Set(options.threadIds).size !== options.threadIds.length
  )
    throw new Error(
      "Provide 2 to 32 distinct source thread IDs; see codex merge --help",
    );
  if (
    options.update &&
    (options.threadIds.length !== 1 || options.mode === "legacy")
  )
    throw new Error(
      "--update takes one merged thread ID and cannot be combined with explicit source IDs or legacy mode",
    );
  if (options.name !== undefined && !options.name.trim())
    throw new Error("--name must be non-empty");
  if (options.dryRun && options.resume)
    throw new Error("--dry-run and --resume cannot be combined");
  return options;
}

async function resume(binaryPath, env, result, options) {
  const args = [
    ...(env.CODEX_CONTEXT_MERGE_NO_DAEMON === "1" ? ["--no-daemon"] : []),
    ...options.config.flatMap((value) => ["-c", value]),
    ...(options.cwd ? ["-C", options.cwd] : []),
    "resume",
    result.threadId,
  ];
  const child = spawn(binaryPath, args, { env, stdio: "inherit" });
  const handlers = new Map(
    ["SIGINT", "SIGTERM", "SIGHUP"].map((signal) => [
      signal,
      () => child.kill(signal),
    ]),
  );
  for (const [signal, handler] of handlers) process.on(signal, handler);
  try {
    return await new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("exit", (code, signal) =>
        resolve(code ?? (signal === "SIGINT" ? 130 : 1)),
      );
    });
  } finally {
    for (const [signal, handler] of handlers) process.off(signal, handler);
  }
}

function printSemanticDetails(result) {
  if (result.dryRun) {
    process.stdout.write(
      "Semantic input: " +
        result.inputBytes +
        " bytes; no model call or archive write.\n",
    );
    return;
  }
  process.stdout.write(
    "Analysis: " +
      (result.reusedAnalysis ? "reused" : "generated by " + result.model) +
      "\nGoal: " +
      result.state.goal +
      "\n",
  );
  for (const conflict of result.state.conflicts.filter(
    (entry) => entry.status === "unresolved",
  )) {
    process.stdout.write("Unresolved: " + conflict.topic + "\n");
  }
  for (const action of result.state.nextActions)
    process.stdout.write("Next: " + action.text + "\n");
  process.stdout.write("Original evidence: " + result.evidence.path + "\n");
}

export async function runMergeCommand(binaryPath, env, args) {
  if (args[0] === "evidence") return runEvidenceCommand(args.slice(1));
  if (args[0] === "graph") return runGraphCommand(args.slice(1));
  let client;
  const handlers = new Map();
  try {
    const options = parseMergeArgs(args);
    if (options.help) {
      process.stdout.write(HELP);
      return 0;
    }
    client = new AppServerClient(binaryPath, { env, config: options.config });
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
      const handler = () => client.child.kill(signal);
      handlers.set(signal, handler);
      process.on(signal, handler);
    }
    await client.initialize();
    if (
      !options.dryRun &&
      (options.semantic || ["auto", "summary"].includes(options.mode))
    )
      process.stderr.write(
        "Preparing conversation merge; Codex may summarize long branches.\n",
      );
    const result = await mergeThreads(client, options);
    await client.close();
    client = null;
    for (const [signal, handler] of handlers) process.off(signal, handler);
    handlers.clear();
    if (options.json) process.stdout.write(`${JSON.stringify(result)}\n`);
    else if (result.noOp) process.stdout.write(result.message + "\n");
    else {
      process.stdout.write(
        `${result.dryRun ? "Merge preview" : `Merged thread: ${result.threadId}`}\nAdded ${result.importedItems} items; skipped ${result.skippedItems} shared items; ${result.contextBytes} context bytes.\n`,
      );
      if (!result.dryRun)
        process.stdout.write(
          `Continue with: codex resume ${result.threadId}\n`,
        );
    }
    if (!options.json && result.semantic) printSemanticDetails(result);
    if (!options.json && result.graph) {
      process.stdout.write(
        "Graph: " + result.mergeNodeId + "; " + result.graphNodes + " nodes.\n",
      );
      for (const view of result.presentations)
        process.stdout.write(
          view.threadId +
            ": " +
            view.presentation +
            " (" +
            view.nodeCount +
            " original records)\n",
        );
      if (result.evidence)
        process.stdout.write(
          "Original graph and evidence: " + result.evidence.path + "\n",
        );
      if (result.reading) process.stdout.write(result.reading.text + "\n");
    }
    return options.resume ? await resume(binaryPath, env, result, options) : 0;
  } catch (error) {
    process.stderr.write(`codex merge: ${error.message}\n`);
    return 1;
  } finally {
    if (client) await client.close();
    for (const [signal, handler] of handlers) process.off(signal, handler);
  }
}
