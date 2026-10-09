import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

export class RpcError extends Error {
  constructor(method, error) {
    super(`${method}: ${error.message}`);
    this.name = "RpcError";
    this.code = error.code;
    this.data = error.data;
  }
}

/** A private app-server connection. Inference is explicit through runTurn. */
export class AppServerClient {
  constructor(
    binaryPath,
    { env = process.env, config = [], timeoutMs = 30000 } = {},
  ) {
    this.env = env;
    this.nextId = 1;
    this.pending = new Map();
    this.notifications = new Set();
    this.timeoutMs = timeoutMs;
    this.failure = null;
    this.child = spawn(
      binaryPath,
      [
        "app-server",
        "--listen",
        "stdio://",
        ...config.flatMap((value) => ["-c", value]),
      ],
      {
        env,
        stdio: ["pipe", "pipe", "pipe"],
        windowsHide: true,
      },
    );
    // Drain logs without including potentially private diagnostics in errors.
    this.child.stderr.resume();
    this.lines = createInterface({
      input: this.child.stdout,
      crlfDelay: Infinity,
    });
    this.lines.on("line", (line) => this.receive(line));
    this.child.on("error", (error) =>
      this.fail(new Error(`Cannot start Codex app-server: ${error.message}`)),
    );
    this.child.stdin.on("error", (error) =>
      this.fail(new Error(`Codex app-server input closed: ${error.message}`)),
    );
    this.exited = new Promise((resolve) => {
      this.child.once("close", (code, signal) => {
        this.fail(new Error(`Codex app-server closed (${signal ?? code})`));
        resolve();
      });
    });
  }

  receive(line) {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      this.fail(new Error("Codex app-server returned invalid JSON"));
      return;
    }
    if (!message || typeof message !== "object" || Array.isArray(message)) {
      this.fail(new Error("Codex app-server returned an invalid response"));
      return;
    }
    if (message.method) {
      if (message.id !== undefined) {
        this.write({
          id: message.id,
          error: {
            code: -32601,
            message: "Context merge does not handle interactive requests",
          },
        });
      }
      for (const listener of [...this.notifications]) listener(message);
      return;
    }
    const pending = this.pending.get(message.id);
    if (!pending) return;
    clearTimeout(pending.timer);
    this.pending.delete(message.id);
    if (message.error)
      pending.reject(new RpcError(pending.method, message.error));
    else if (Object.prototype.hasOwnProperty.call(message, "result"))
      pending.resolve(message.result);
    else pending.reject(new Error(`Invalid response to ${pending.method}`));
  }

  fail(error) {
    this.failure ??= error;
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(this.failure);
    }
    this.pending.clear();
    for (const listener of [...this.notifications])
      listener(null, this.failure);
  }

  onNotification(listener) {
    this.notifications.add(listener);
    return () => this.notifications.delete(listener);
  }

  /** Subscribe before starting: completed items can precede the RPC reply. */
  runTurn(params, { timeoutMs = 600000, maxOutputBytes = 128 * 1024 } = {}) {
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0)
      return Promise.reject(new Error("Invalid inference timeout"));
    if (this.failure) return Promise.reject(this.failure);
    return new Promise((resolve, reject) => {
      let turnId;
      let done = false;
      let cancelRequested = false;
      const events = new Map();
      const finish = (error, result) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        unsubscribe();
        if (error) reject(error);
        else resolve(result);
      };
      const interrupt = () => {
        cancelRequested = true;
        if (turnId && !this.failure)
          this.request("turn/interrupt", {
            threadId: params.threadId,
            turnId,
          }).catch(() => {});
      };
      const complete = (id) => {
        const entry = events.get(id);
        if (!entry?.turn) return;
        if (entry.turn.status !== "completed") {
          finish(
            new Error(
              "Codex analysis " +
                entry.turn.status +
                ": " +
                (entry.turn.error?.message ?? "no successful result"),
            ),
          );
          return;
        }
        const candidates = [
          ...entry.items.values(),
          ...(entry.turn.items ?? []),
        ].filter(
          (item) =>
            item.type === "agentMessage" &&
            item.phase !== "commentary" &&
            typeof item.text === "string",
        );
        const text = candidates[candidates.length - 1]?.text;
        if (!text?.trim())
          finish(
            new Error("Codex analysis completed without a final response"),
          );
        else if (Buffer.byteLength(text, "utf8") > maxOutputBytes)
          finish(
            new Error("Codex analysis response exceeds its output budget"),
          );
        else finish(null, { text, turnId: id });
      };
      const unsubscribe = this.onNotification((message, error) => {
        if (error) {
          finish(error);
          return;
        }
        if (message.params?.threadId !== params.threadId) return;
        const id = message.params.turnId ?? message.params.turn?.id;
        if (!id || (turnId && id !== turnId)) return;
        if (!events.has(id)) events.set(id, { items: new Map() });
        const entry = events.get(id);
        if (
          message.method === "item/started" &&
          [
            "commandExecution",
            "fileChange",
            "mcpToolCall",
            "dynamicToolCall",
          ].includes(message.params.item?.type)
        ) {
          finish(
            new Error(
              "Semantic analysis attempted a tool execution; no merge was published",
            ),
          );
          interrupt();
          return;
        }
        if (message.method === "item/completed") {
          const item = message.params.item;
          if (item?.type === "agentMessage") entry.items.set(item.id, item);
        } else if (message.method === "turn/completed")
          entry.turn = message.params.turn;
        if (turnId) complete(turnId);
      });
      const timer = setTimeout(() => {
        finish(new Error("Timed out waiting for Codex semantic analysis"));
        interrupt();
      }, timeoutMs);
      this.request("turn/start", params).then(
        (result) => {
          turnId = result.turn?.id;
          if (done) {
            if (cancelRequested) interrupt();
            return;
          }
          if (!turnId) {
            finish(new Error("Codex returned no analysis turn ID"));
            return;
          }
          if (result.turn.status !== "inProgress") {
            if (!events.has(turnId)) events.set(turnId, { items: new Map() });
            events.get(turnId).turn = result.turn;
          }
          complete(turnId);
        },
        (error) => finish(error),
      );
    });
  }

  write(message) {
    if (this.failure) throw this.failure;
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  request(method, params) {
    if (this.failure) return Promise.reject(this.failure);
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Timed out waiting for ${method}`));
      }, this.timeoutMs);
      this.pending.set(id, { method, resolve, reject, timer });
      try {
        this.write({ id, method, params });
      } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
  }

  async initialize() {
    await this.request("initialize", {
      clientInfo: {
        name: "codex_context_merge",
        title: "Codex Context Merge",
        version: "1.0.0",
      },
      capabilities: { experimentalApi: true },
    });
    this.write({ method: "initialized", params: {} });
    return this;
  }

  async close() {
    this.child.stdin.end();
    const terminate = setTimeout(() => this.child.kill("SIGTERM"), 5000);
    const kill = setTimeout(() => this.child.kill("SIGKILL"), 6000);
    try {
      await this.exited;
    } finally {
      clearTimeout(terminate);
      clearTimeout(kill);
      this.lines.close();
    }
  }
}
