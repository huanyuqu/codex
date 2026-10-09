# 在此分支使用上下文图 merge

当前功能在 `huanyuqu/codex` 的 `context-graph` 分支。`origin` 指向你的 fork，
`upstream` 指向 `openai/codex`。此目录现在包含完整的官方仓库和我们的修改。

最常用的命令：

```sh
# A、B、C 是 Codex 会话 UUID；第一个是主会话
./codex merge A_ID B_ID C_ID --dry-run
./codex merge A_ID B_ID C_ID --name "合并讨论" --resume
```

也可以在 `./codex` 的聊天界面中直接输入：

```text
/merge
```

菜单中输入文字搜索会话，空格选中 B、C，Enter 合并，Esc 取消。当前聊天
自动作为 A；成功后自动切换到合并后的新聊天，继续输入即可。A、B、C 的
原始上下文仍然保留。也支持直接指定分支 UUID：

```text
/merge B_ID C_ID
/merge B_ID C_ID --dry-run
/merge B_ID C_ID --semantic --goal "比较两种观点，保留分歧"
```

`/merge --semantic` 和 `/merge --mode reference` 会先打开同一个分支选择器，
再用这些选项合并。执行中的 Esc 或 Ctrl+C 取消合并；取消或失败时留在
当前聊天。当前会话需要空闲、可写，分支需要已保存；当前版本支持本地会话。
斜杠命令不需要 `--resume`，也不接受 `--cd`、`--config`、`--json`。

例如 A 为 `A1 → A2 → A3`，B 为 `A1 → B2`，C 为 `A1 → A2 → C3`。
合并后，存档仍记住 `B2` 的父节点是 `A1`、`C3` 的父节点是 `A2`，
并增加父节点为 `A3、B2、C3` 的 merge 节点。继续对话的新消息接在 merge
节点之后。

模型读取的是这个图的有限大小视图：短分支直接内嵌原始消息；长分支调用
Codex 分块生成摘要；预算连摘要分析也容纳不了时保留引用。原始消息、工具
调用/结果、附件和图结构另存为带校验和的完整存档。

主会话本身过长时，会建立新会话并放入主会话摘要或引用，避免把过长历史
继续复制进去。默认预算包括主历史、新增视图和给后续对话预留的空间；
实际采用的预算及估算方法可通过 `--json` 查看。

```sh
# 完全不调用模型，只加入图关系和原文引用
./codex merge A_ID B_ID C_ID --mode reference

# 在图视图之外，额外生成跨分支的通用叙述
./codex merge A_ID B_ID C_ID --semantic \
  --goal "比较两种方案，保留观点归属和未解决分歧"

# 指定整个上下文的规划上限和后续输入/输出预留
./codex merge A_ID B_ID C_ID --context-tokens 32768 --reserve-tokens 4096
```

`--semantic` 不再默认输出“已完成、待办、下一步”的代码任务模板。摘要和
融合结果采用叙述及原始节点引用。模型读取和引用检查不能保证每个细节都被
正确保留，重要细节应查原文。长分支的摘要为文本分析，不会理解存档图片；
短分支内嵌时图片仍作为原生图片输入。

存档默认位于 `~/.codex/merges/evidence/`，遵循 `CODEX_HOME`；实际路径由
merge 命令输出。可以查看图、分支原文或大消息的一个片段：

```sh
./codex merge graph /path/to/HASH.json --mermaid
./codex merge evidence /path/to/HASH.json --branch B_ID --limit 3 --json
./codex merge evidence /path/to/HASH.json r_NODE_ID \
  --start-byte 0 --length-bytes 512 --max-tokens 2048 --json
```

分页结果返回 `nextOffset`，片段返回 `nextByte`，可以据此继续读取。不要删除
存档；后续 merge 需要它恢复原始节点。重复合并未变化的分支会复用图节点；
有新增内容时只增加新节点及 merge 节点。

## 运行与开发

交互式 `/merge` 需要编译此分支的 Rust CLI。当前机器已具备 Rust 编译环境；
构建完成后，`./codex` 自动使用 `codex-rs/target/debug/codex` 和本地 Node
合并模块。全局安装的官方 `codex` 不会自动获得 `/merge`。
本地 debug 构建没有完整 daemon 安装包，因此 launcher 默认使用进程内
app-server，无需手动加 `--no-daemon`。

```sh
cd codex-rs
cargo build --locked -p codex-cli --bin codex
cd ..
./codex
```

可以用 `CODEX_CONTEXT_GRAPH_BINARY=/absolute/path/to/codex ./codex --no-daemon`
选择其它本地构建产物。Rust TUI 将合并模块的请求交给当前 app-server 连接，成功后
通过原生 resume 流程切换聊天；没有新增 `thread/merge` RPC 或 Desktop 按钮。

新 clone 需要 Node.js 22 或更新版本，以及与当前平台对应的原生 Codex 包。
仅使用外部 `./codex merge ...` 命令时，无需编译 Rust。例如 macOS Apple
Silicon 可安装已验证的版本：

```sh
npm install --prefix codex-cli --no-save --package-lock=false \
  '@openai/codex-darwin-arm64@npm:@openai/codex@0.162.0-darwin-arm64'
./codex --version
./codex merge --help
```

其它平台使用 launcher 中相应的平台包，或按官方构建流程将二进制放入
`codex-cli/vendor/<TARGET>/bin/`。`CODEX_MERGE_TEST_BINARY` 应指向原生可执行
文件，而非 JavaScript launcher。

```sh
CODEX_MERGE_TEST_BINARY=/usr/local/bin/codex \
  node --test --test-reporter=spec codex-cli/tests/*.test.js
```

同时运行原生 `/merge` 终端测试时，指定此分支的构建产物（Unix 及 Python 3）：

```sh
CODEX_MERGE_TEST_BINARY="$PWD/codex-rs/target/debug/codex" \
CODEX_TUI_MERGE_TEST_BINARY="$PWD/codex-rs/target/debug/codex" \
  node --test --test-reporter=spec codex-cli/tests/*.test.js

cd codex-rs
cargo test --locked -p codex-tui --lib slash_command
```

验证范围、真实模型案例及限制见 [VALIDATION.md](../VALIDATION.md)，详细
设计、预算和参数见 [context_merge.md](context_merge.md)。历史兼容模式为
`--mode legacy`；其旧任务状态格式为 `--mode legacy --semantic`。

## 跟踪 fork 与上游

```sh
# 保存并推送此功能分支
git add <修改的文件>
git commit -m "Describe the change"
git push -u origin context-graph

# 查看官方更新；fetch 不会改动当前工作区
git fetch upstream main
git log --oneline context-graph..upstream/main

# 工作区干净后，将官方更新合入本分支，重新运行测试，再推送
git merge upstream/main
git push origin context-graph
```

会话 merge 与这里的 Git 分支 merge 是两个独立功能。会话 merge 保留讨论
及证据，不会自动解决工作目录中的代码冲突。
