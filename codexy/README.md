# codexy

codexy is a build of [Codex CLI](https://github.com/openai/codex) from the fork
[huanyuqu/codex](https://github.com/huanyuqu/codex/tree/context-graph). It adds:

- **A context graph.** A conversation and its forks are branches of one graph. `/tree` shows them,
  `/fork --at TURN` forks from an earlier turn, `/fork --bg` works on a task in the background, and
  `/merge` folds, references or synthesizes other branches into the chat. Recall chooses which
  parts of a long history the model sees instead of summarizing it.
- **Background commands.** The model can start a build, a test suite or a server in the
  background, and you can move a running command there with `Ctrl+B`. The model is told when it
  exits.
- **`codexy merge`**, which merges saved conversations from the command line.

## Install

```sh
npm install -g codexy
codexy
```

It runs on macOS (Apple silicon and Intel) and Linux (x64 and arm64) and needs Node.js 16 or
newer. To update, run `npm install -g codexy@latest`.

codexy installs next to the official `codex` and shares its home directory, `~/.codex`: the same
login, `config.toml` and saved chats. The context graph keeps its own database there,
`context_graph_1.sqlite`.

## Defaults

codexy starts Codex with the context graph and its indexing turned on. Indexing has a small model
write a short card for each block of the conversation, so it uses some extra model requests. To
turn either off, pass your own override, which wins:

```sh
codexy -c features.context_graph_indexing=false
codexy -c features.context_graph=false
```

codexy always runs its own app-server instead of the background server shared by `codex`
sessions, because that server runs the official release.

## Documentation

- [Context graph](https://github.com/huanyuqu/codex/blob/context-graph/docs/context_graph.md)
- [Background commands](https://github.com/huanyuqu/codex/blob/context-graph/docs/background_commands.md)
- [`codexy merge`](https://github.com/huanyuqu/codex/blob/context-graph/docs/context_merge.md)

Everything else works as in Codex; see its
[documentation](https://github.com/openai/codex#readme).

codexy is not made or supported by OpenAI. Codex is licensed under the Apache License 2.0.
