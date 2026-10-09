import assert from "node:assert/strict";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { AppServerClient } from "../bin/merge-rpc.js";

async function server(t) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "codex-merge-turn-"));
  const file = path.join(dir, "fake-server");
  await writeFile(
    file,
    [
      "#!/usr/bin/env node",
      "const readline = require('node:readline');",
      "const send = value => process.stdout.write(JSON.stringify(value)+'\\n');",
      "const lines = readline.createInterface({input:process.stdin});",
      "let interrupts=0;",
      "lines.on('line', line => {",
      " const request=JSON.parse(line); if(request.id===undefined) return;",
      " if(request.method==='turn/interrupt') interrupts++;",
      " if(request.method==='stats') return send({id:request.id,result:{interrupts}});",
      " if(request.method!=='turn/start') return send({id:request.id,result:{}});",
      " const threadId=request.params.threadId, kind=request.params.testCase;",
      " if(kind==='crash') return process.exit(2);",
      " const item={type:'agentMessage',id:'final',phase:'final_answer',text:'{\"goal\":\"ok\"}'};",
      " const notify=(method,params)=>send({method,params:{threadId,...params}});",
      " if(kind==='early') notify('item/completed',{turnId:'turn-1',item});",
      " if(kind==='early') notify('turn/completed',{turn:{id:'turn-1',status:'completed',items:[]}});",
      " send({id:request.id,result:{turn:{id:'turn-1',status:'inProgress',items:[]}}});",
      " if(kind==='early'||kind==='timeout') return;",
      " if(kind==='tool') return notify('item/started',{turnId:'turn-1',item:{type:'commandExecution',id:'shell'}});",
      " if(kind==='failed') return notify('turn/completed',{turn:{id:'turn-1',status:'failed',error:{message:'fixture failure'},items:[]}});",
      " notify('item/completed',{turnId:'turn-1',item:{type:'agentMessage',id:'commentary',phase:'commentary',text:'not the result'}});",
      " send({method:'item/completed',params:{threadId:'other',turnId:'turn-1',item:{...item,text:'wrong thread'}}});",
      " if(kind==='empty') return notify('turn/completed',{turn:{id:'turn-1',status:'completed',items:[]}});",
      " if(kind==='completed-items') return notify('turn/completed',{turn:{id:'turn-1',status:'completed',items:[item]}});",
      " notify('item/completed',{turnId:'turn-1',item});",
      " notify('turn/completed',{turn:{id:'turn-1',status:'completed',items:[]}});",
      "});",
      "lines.on('close',()=>process.exit(0));",
    ].join("\n"),
  );
  await chmod(file, 0o700);
  const client = new AppServerClient(file);
  t.after(async () => {
    await client.close();
    await rm(dir, { recursive: true, force: true });
  });
  await client.initialize();
  return client;
}

for (const testCase of ["early", "normal", "completed-items"]) {
  test(
    "model turn collects its final output and ignores other messages: " +
      testCase,
    async (t) => {
      const client = await server(t);
      const result = await client.runTurn({
        threadId: "analysis",
        testCase,
        input: [],
      });
      assert.equal(result.text, '{"goal":"ok"}');
      assert.equal(client.notifications.size, 0);
      assert.equal(
        (await client.request("stats", {})).interrupts,
        0,
        "successful early completion must not send an interrupt",
      );
    },
  );
}
for (const [testCase, pattern] of [
  ["failed", /fixture failure/],
  ["tool", /attempted a tool execution/],
  ["timeout", /Timed out/],
  ["crash", /closed \(2\)/],
  ["empty", /without a final response/],
]) {
  test(
    "model turn fails promptly and cleans its subscription: " + testCase,
    async (t) => {
      const client = await server(t);
      await assert.rejects(
        client.runTurn(
          { threadId: "analysis", testCase, input: [] },
          { timeoutMs: 100 },
        ),
        pattern,
      );
      assert.equal(client.notifications.size, 0);
    },
  );
}
