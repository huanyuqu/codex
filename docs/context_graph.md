# Context graph

The context graph records a conversation as blocks: one user message, one
assistant message, or one tool call with its output. Forks of a conversation
are branches of the same graph. A shadow agent indexes every block with a short
card, and recall uses the cards to choose, for each request, which parts of the
history the model sees.

Everything here is under development and off by default.

## Turning it on

```toml
[features]
context_graph = true            # record conversations and forks as a graph
context_graph_indexing = true   # let a small model write the cards

[context_graph]
mode = "auto"                   # full | auto | recall
```

The graph lives in `context_graph_1.sqlite` next to Codex's other databases.
It is derived data: deleting it loses nothing that replaying the saved chats
cannot rebuild.

## Branches

With `context_graph` on, a `/fork` of a recorded chat becomes a branch of that
chat's graph. `/fork NAME` names the branch.

| Command | What it does |
| --- | --- |
| `/tree` | Lists the branches of the current chat as a tree, with what each is about, how many turns it added and its latest request. Enter switches to a branch. |
| `/tree NAME` | Switches to the branch with that name, branch ID or thread ID. |
| `/tree NAME --show` | Shows what folding the branch into the current chat would add, without switching. |
| `/branch` | Shows the current branch: where it was forked from and which branches it references. |
| `/branch NAME` | Renames the current branch. |
| `/fork NAME` | Forks the chat into a new branch and switches to it. |
| `/fork --at TURN [NAME]` | Forks the chat after turn `TURN`, leaving the later turns out of the new branch. |
| `/fork --bg [--worktree] [--name NAME] PROMPT` | Forks a branch that works on `PROMPT` in the background while this chat stays open. |
| `/merge` | Opens a picker of the other branches. |
| `/merge NAME…` | Folds each named branch into the current chat. |
| `/merge --reference NAME…` | References each named branch instead: recall chooses from its history. |
| `/merge --synthesize NAME…` | A model reads the branch and writes the merge note. |
| `/merge --inline NAME…` | Copies the branch's history into the chat as text, for short branches. |
| `/merge` in a side conversation | Folds the side conversation's questions and answers into its parent chat and returns there. |
| `/context` | Shows the context mode, the branch and the latest recalls. |
| `/context full\|auto\|recall` | Switches the context mode of the chat from its next turn. |

Editing an earlier prompt (Esc Esc, then pick the prompt) continues in a new
branch that leaves the edited turn and everything after it out; the original
conversation stays as it was. Set `edit_creates_branch = false` to rewind the
chat instead, as Codex does without the context graph.

The status line can show the current branch: add `context-branch` to
`tui.status_line`.

A fold adds a summary of what the branch did after it left the current chat:
each turn's request and result, the constraints, decisions and findings its
cards recorded, and its outcome. The next turn sees the summary; the branch
itself is unchanged, and `/tree` shows that it was merged.

A reference adds only a short note. From then on, the branch's own blocks are
part of what recall chooses from and what `context_search` and
`context_expand` read, so its details are there when a request needs them and
cost nothing when it does not. References need a `mode` other than `full`.

Writing `@NAME` in a message references the branch for that turn: in `auto`
and `recall` mode it becomes a reference and the history is recalled at once;
in `full` mode the branch is folded in instead.

The agent can branch on its own, too. `branch_create` forks the conversation
into a sub-agent that works on a task with the whole history so far, and
`branch_merge` waits for a branch to finish and merges it in any of the four
ways. Its branches show up in `/tree` like any other.

### Background branches and shared workspaces

`/fork --bg` starts the branch's first turn right away and reports here when
the turn ends; with `on_branch_complete = "fold"` its work is folded in
automatically. `--worktree` gives the branch a git worktree of its own (this
needs the `worktrees` feature), so it can change files while you keep working.

Branches of one chat that share a working directory take turns: a branch holds
the workspace for a whole turn, and another branch's turn waits until it ends.
No branch's files change in the middle of another branch's turn. This holds
across Codex processes as well, through a lock file under
`~/.codex/context_graph/leases`. Branches in their own worktrees run side by
side. When a background branch needs an approval, the chat you are in says so,
and `/tree` switches to the branch to answer it.

## Recall

The `mode` setting decides how a chat manages the history its model sees.

| Mode | Behavior |
| --- | --- |
| `full` | The whole history, compacted into a summary when the context window fills up. This is Codex's normal behavior. |
| `auto` | The whole history until it fills `soft_limit_percent` of the context window, then a recalled history instead of a summary. |
| `recall` | A recalled history before every turn. Meant for experiments. |

A recalled history keeps the most recent turns and the first request in full,
lists the user's constraints and the decisions still in force, has one line per
earlier turn, and adds the earlier blocks a screening model picked for the
current request, in full, as excerpts, or as summaries. `/compact` recalls
instead of summarizing when `mode` is not `full`. If recall fails, Codex falls
back to summary compaction.

The model also gets three tools: `context_search` finds blocks of the whole
history by words, `context_expand` reads a block or a whole turn in full, and
`context_recall` asks the screening model for the parts of the history a
question needs.

While a chat is idle, the shadow agent consolidates its settled turns: runs of
turns become episode cards, and the branch gets a card that says what it is
about. A long history's map of earlier turns sums up episodes in one line each,
and `/tree` shows each branch's card. Without an index model, the cards are made
from the turns themselves.

A block that read a file that has changed since is marked outdated: recall
shows its summary with a note instead of its stale contents.

```toml
[context_graph]
mode = "auto"
index_model = "gpt-6-luna"      # writes cards; defaults to the provider's memory model
screen_model = "gpt-6-luna"     # picks blocks; defaults to index_model
read_model = "none"             # reads blocks the cards cannot decide; defaults to screen_model
recall_budget_tokens = 40000
recent_turns = 2
soft_limit_percent = 50
on_branch_complete = "notify"   # notify | fold, for /fork --bg
recall_timeout_secs = 120       # give up on recall and fall back after this long
rebuild_overlap_percent = 50    # rebuild the recalled history when less of it still fits
edit_creates_branch = true      # editing an earlier prompt forks instead of rewinding
```

A recalled history is kept from turn to turn: later turns are appended to it,
and it is rebuilt only when the blocks it shows cover less than
`rebuild_overlap_percent` of what a new recall would pick, so the model's prompt
cache stays warm.

## App server

Clients use these experimental requests; the TUI uses them for `/tree` and
`/merge`, so they also work with a remote app server.

| Request | What it does |
| --- | --- |
| `contextGraph/read` | The branches of a thread's graph in tree order, and its merges. |
| `contextGraph/merge` | Merges a branch into a thread: fold, reference, synthesize or inline. |
| `contextGraph/branch/rename` | Renames a thread's branch, also before the thread first records. |
| `contextGraph/branch/preview` | What folding a branch into a thread would add. |
| `contextGraph/mode/set` | Switches a loaded thread's context mode. |
| `contextGraph/views/read` | The views recall built for a thread, most recent first. |

`contextGraph/updated` notifies clients when a merge or rename changes a graph.

## Saved conversations

`codex graph import` records the saved conversations that ran without the
context graph, so their forks show up as branches in `/tree`. Merges made with
the earlier `codex merge` tool are recorded as merges, and the graphs of the
conversations they merged are joined. `--dry-run` lists what would be imported;
thread IDs limit the import to those conversations, and `--archived` includes
archived ones. A conversation whose history is stored in pages is imported from
the latest history its model saw, so turns compacted away before are missing.

Deleting a conversation deletes what the graph derived from it.

## Evaluation

`codex-context-graph-eval` indexes saved rollouts, recalls from them at any
turn, and runs probe benchmarks that compare recall with the full history and
other selectors. Run it with `--help` for the commands.
