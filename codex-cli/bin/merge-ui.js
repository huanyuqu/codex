import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import { JsonRpcClient } from "./merge-rpc.js";
import { parseMergeArgs } from "./merge-cli.js";
import { mergeThreads } from "./merge-context.js";
import { contextTree } from "./context-tree.js";

// The TUI owns the native app-server connection. Only requests and results
// cross this pipe; the existing engine performs the same graph merge as CLI.
const emit = (message) => process.stdout.write(JSON.stringify(message) + "\n");
const abort = new AbortController();
const client = new JsonRpcClient({
  write: (message) => emit({ type: "rpc", message }),
  signal: abort.signal,
  idPrefix: "context-merge-" + randomUUID() + "-",
});
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
let started = false;
let finished = false;

process.stdout.on("error", (error) => {
  finished = true;
  abort.abort();
  client.fail(error);
  lines.close();
  process.stdin.destroy();
  process.exitCode = 1;
});

async function run(message) {
  try {
    if (message.operation === "tree") {
      emit({
        type: "result",
        result: await contextTree(client, message.primaryThreadId),
      });
      return;
    }
    const options = parseMergeArgs([message.primaryThreadId, ...message.args], {
      allowSinglePrimary: true,
    });
    if (options.help) {
      emit({
        type: "result",
        result: {
          help: "Usage: /merge [BRANCH_ID...] [--mode auto|inline|summary|reference] [--goal TEXT] [--dry-run]\nThe current chat is the primary. Without branch IDs, /merge opens a multi-select picker. Codex synthesizes the branches by default; reference mode does not call a model. Successful merges open the new chat. Use /tree to browse related chats.",
        },
      });
      return;
    }
    if (options.config.length || options.cwd || options.resume || options.json)
      throw new Error(
        "/merge uses the current CLI connection; omit --config, --cd, --resume and --json",
      );
    if (options.threadIds.length === 1) {
      emit({
        type: "result",
        result: { selectBranches: true, args: message.args },
      });
      return;
    }
    options.model ??= message.model;
    emit({
      type: "progress",
      text: "Merging conversation context; long branches may need summaries. Esc cancels.",
    });
    const result = await mergeThreads(client, options);
    emit({ type: "result", result });
  } catch (error) {
    emit({ type: "error", message: error.message });
    process.exitCode = 1;
  } finally {
    finished = true;
    lines.close();
    process.stdin.destroy();
    client.fail(new Error("Conversation merge helper finished"));
  }
}

lines.on("line", (line) => {
  try {
    const message = JSON.parse(line);
    if (message.type === "start" && !started) {
      started = true;
      if (
        typeof message.primaryThreadId !== "string" ||
        (message.operation !== undefined &&
          !["merge", "tree"].includes(message.operation)) ||
        !Array.isArray(message.args) ||
        message.args.some((arg) => typeof arg !== "string")
      )
        throw new Error("Invalid conversation merge request");
      void run(message);
    } else if (message.type === "rpc" && started)
      client.receive(JSON.stringify(message.message));
    else if (message.type === "cancel") abort.abort();
    else throw new Error("Invalid conversation merge bridge message");
  } catch (error) {
    client.fail(error);
    emit({ type: "error", message: error.message });
    lines.close();
    process.stdin.destroy();
    process.exitCode = 1;
  }
});
lines.on("close", () => {
  if (!finished) {
    abort.abort();
    client.fail(new Error("Conversation merge bridge closed"));
    process.exitCode = 1;
  }
});
