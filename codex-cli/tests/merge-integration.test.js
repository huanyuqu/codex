import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { promisify } from "node:util";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { AppServerClient } from "../bin/merge-rpc.js";
import { mergeThreads as mergeThreadsImpl } from "../bin/merge-context.js";
const mergeThreads = (client, options) =>
  mergeThreadsImpl(client, { mode: "legacy", ...options });
import { hash, loadSnapshot, parseMergeItem } from "../bin/merge-history.js";

const nativeBinary = process.env.CODEX_MERGE_TEST_BINARY;
const exec = promisify(execFile);
const message = (text, role = "user") => ({
  type: "message",
  role,
  content: [
    { type: role === "assistant" ? "output_text" : "input_text", text },
  ],
});
const fixtureImage = {
  type: "input_image",
  image_url:
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
};

for (const historyMode of ["legacy", "paginated"]) {
  test(
    `native Codex (${historyMode}): fork, merge, persist images, restart, resume, re-merge`,
    { skip: !nativeBinary, timeout: 60000 },
    async (t) => {
      const home = await mkdtemp(path.join(os.tmpdir(), "codex-merge-native-"));
      const env = { ...process.env, CODEX_HOME: home };
      let client = new AppServerClient(nativeBinary, { env });
      t.after(async () => {
        await client.close();
        await rm(home, { recursive: true, force: true });
      });
      await client.initialize();
      const { thread: base } = await client.request("thread/start", {
        cwd: process.cwd(),
        ephemeral: false,
        historyMode,
      });
      assert.equal(base.historyMode, historyMode);
      await client.request("thread/inject_items", {
        threadId: base.id,
        items: [
          message("shared question"),
          message("shared answer", "assistant"),
        ],
      });
      const branches = [];
      for (const label of ["B", "C"]) {
        const { thread } = await client.request("thread/fork", {
          threadId: base.id,
          excludeTurns: true,
        });
        branches.push(thread);
        const question = message(`branch ${label} question`);
        if (label === "B") question.content.push(fixtureImage);
        await client.request("thread/inject_items", {
          threadId: thread.id,
          items: [
            question,
            {
              type: "function_call",
              call_id: "same-tool-id",
              name: "exec_command",
              arguments: '{"cmd":"echo independent observation"}',
            },
            {
              type: "function_call_output",
              call_id: "same-tool-id",
              output: `observed ${label}`,
            },
            message(`branch ${label} conflicting conclusion`, "assistant"),
          ],
        });
      }
      const threadIds = [base.id, ...branches.map((branch) => branch.id)];
      const sourceBytes = await Promise.all(
        threadIds.map(async (id) => {
          const { thread } = await client.request("thread/read", {
            threadId: id,
          });
          return {
            id,
            path: thread.path,
            sha: hash(await readFile(thread.path)),
          };
        }),
      );
      const listing = async () =>
        (await client.request("thread/list", { limit: 100 })).data
          .map((thread) => thread.id)
          .sort();
      const beforePreview = await listing();
      const preview = await mergeThreads(client, { threadIds, dryRun: true });
      assert.equal(preview.importedItems, 8);
      assert.equal(preview.skippedItems, 4);
      assert.deepEqual(await listing(), beforePreview);
      const merged = await mergeThreads(client, {
        threadIds,
        name: "Three-way context merge",
      });
      assert.equal(merged.importedItems, 8);
      assert.equal(merged.importedImages, 1);
      for (const source of sourceBytes)
        assert.equal(
          hash(await readFile(source.path)),
          source.sha,
          `source ${source.id} changed`,
        );
      const saved = await loadSnapshot(client, merged.threadId);
      const bundle = saved.records
        .map((r) => parseMergeItem(JSON.parse(r.json)))
        .find(Boolean);
      assert.equal(bundle.branches.length, 2);
      assert.deepEqual(
        bundle.branches.map((branch) => branch.records.length),
        [4, 4],
      );
      assert.match(JSON.stringify(bundle), /observed B/);
      assert.match(JSON.stringify(bundle), /observed C/);
      assert.match(JSON.stringify(bundle), /conflicting conclusion/);
      const savedMarker = saved.records
        .map((r) => JSON.parse(r.json))
        .find(parseMergeItem);
      assert.deepEqual(savedMarker.content[1], fixtureImage);

      await client.close();
      client = new AppServerClient(nativeBinary, { env });
      await client.initialize();
      const resumed = await client.request("thread/resume", {
        threadId: merged.threadId,
        excludeTurns: true,
      });
      assert.equal(resumed.thread.id, merged.threadId);
      assert.equal(resumed.thread.name, "Three-way context merge");
      const restarted = await loadSnapshot(client, merged.threadId);
      assert.deepEqual(
        restarted.records.map((r) => r.json),
        saved.records.map((r) => r.json),
      );
      const repeated = await mergeThreads(client, {
        threadIds: [merged.threadId, ...threadIds.slice(1)],
      });
      assert.equal(repeated.importedItems, 0);
      assert.equal(repeated.skippedItems, 12);
      const repeatedSnapshot = await loadSnapshot(client, repeated.threadId);
      assert.equal(
        repeatedSnapshot.records.filter((r) =>
          parseMergeItem(JSON.parse(r.json)),
        ).length,
        2,
      );

      // The existing merge is a frozen snapshot; only new source items are added.
      await client.request("thread/resume", {
        threadId: branches[0].id,
        excludeTurns: true,
      });
      await client.request("thread/inject_items", {
        threadId: branches[0].id,
        items: [message("new B finding"), message("new B answer", "assistant")],
      });
      const incremental = await mergeThreads(client, {
        threadIds: [repeated.threadId, branches[0].id],
      });
      assert.equal(incremental.importedItems, 2);

      const launcher = fileURLToPath(
        new URL("../bin/codex.js", import.meta.url),
      );
      const cli = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          "--mode",
          "legacy",
          incremental.threadId,
          branches[0].id,
          "--dry-run",
          "--json",
        ],
        { env, timeout: 30000 },
      );
      assert.equal(JSON.parse(cli.stdout).importedItems, 0);
      const cliMerged = await exec(
        process.execPath,
        [
          launcher,
          "merge",
          "--mode",
          "legacy",
          incremental.threadId,
          branches[1].id,
          "--name",
          "CLI merge",
          "--json",
        ],
        { env, timeout: 30000 },
      );
      const cliResult = JSON.parse(cliMerged.stdout);
      assert.equal(cliResult.importedItems, 0);
      assert.ok(cliResult.threadId);
      const cliSaved = await loadSnapshot(client, cliResult.threadId);
      assert.equal(cliSaved.thread.name, "CLI merge");
      assert.ok(
        cliSaved.records.some((r) => parseMergeItem(JSON.parse(r.json))),
      );
      await assert.rejects(
        exec(
          process.execPath,
          [
            launcher,
            "merge",
            "--mode",
            "legacy",
            base.id,
            branches[0].id,
            "--max-bytes",
            "1",
          ],
          { env, timeout: 30000 },
        ),
        /exceeding/,
      );
    },
  );
}
