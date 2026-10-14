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
| `/tree` | Opens the conversation tree: every branch with what it is for, what it is about, how many turns it added and its merges. See below. |
| `/tree NAME`, `/switch NAME` | Switches to the branch with that name, branch ID or thread ID. `/switch` alone opens the tree. |
| `/tree NAME --show` | Shows what folding the branch into the current chat would add, without switching. |
| `/branch` | Shows the current branch: what it is for, where it was forked from and which branches it references. |
| `/branch rename NAME` | Renames the current branch (`/branch NAME` does too). |
| `/branch purpose TEXT` | Says what the current branch is for. Its model is told, and `/tree` shows it. `/branch purpose` alone shows it. |
| `/branch archive NAME`, `/branch unarchive NAME` | Archives a branch, or restores it. An archived branch keeps its history but leaves the merge picker. |
| `/fork NAME` | Forks the chat into a new branch and switches to it. |
| `/fork --at TURN [NAME]` | Forks the chat after turn `TURN`, leaving the later turns out of the new branch. |
| `/fork --bg [--worktree] [--name NAME] PROMPT` | Forks a branch that works on `PROMPT` in the background while this chat stays open. |
| `/fork … --purpose TEXT` | Any fork can say what the new branch is for; quote TEXT when it has spaces. |
| `/merge` | Opens a picker of the other branches and of other saved chats. |
| `/merge NAME…` | Folds the named branches into the current chat, as one note. A saved chat's thread ID works too: the chat joins the conversation as a branch first. |
| `/merge --reference NAME…` | References the branches instead: recall chooses from their histories. |
| `/merge --synthesize NAME…` | A model reads the branches and writes one merge note that keeps each result attributed to its branch and points out where they disagree. |
| `/merge --inline NAME…` | Copies the branches' histories into the chat as text, for short branches. |
| `/merge … --goal TEXT` | Says what the merge is for; a synthesized note focuses on it. |
| `/merge … --code` | Also merges the code of branches that work in their own git worktree. |
| `/merge` in a side conversation | Folds the side conversation's questions and answers into its parent chat and returns there. |
| `/context` | Shows the context mode, the branch, the latest recalls, what the latest one chose and why, and how often the model had to read blocks recall left out. |
| `/context full\|auto\|recall` | Switches the context mode of the chat from its next turn. |
| `/recall` | Rebuilds the chat's history by recall before the next turn, whatever the mode. |
| `/recall QUESTION` | Shows what recall would choose for QUESTION, without changing anything. |

### The conversation tree

`/tree` lists the branches with forks indented under the branch they left.

| Key | What it does |
| --- | --- |
| ↑ ↓, `j` `k` | Move. |
| → or `l` | Expands a branch to its turns, then a turn to its blocks, each with its card's title and size. |
| ← or `h` | Collapses, or moves to the parent row. |
| Enter | On a branch, switches to it; on a turn or block, reads it. |
| `p` | Reads the turn or block in the pager, from whichever branch recorded it. |
| `f` | Forks the branch after the selected turn and switches to the fork. |
| `m`, `r` | Merges the selected branch into the current chat: folds it, or references it. |
| `a` | Archives the selected branch, or restores it. |
| `/` | Searches the cards of every branch, without calling a model. Enter on a hit reads it, `f` forks after its turn. |
| Esc | Leaves the search, then closes the tree. |

Editing an earlier prompt (Esc Esc, then pick the prompt) continues in a new
branch that leaves the edited turn and everything after it out; the original
conversation stays as it was. Set `edit_creates_branch = false` to rewind the
chat instead, as Codex does without the context graph.

The status line can show the current branch: add `context-branch` to
`tui.status_line`. It also counts the background branches still working.

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

A synthesized note cites the blocks each statement rests on by block ID, which
`context_expand` reads. Branches too long for one prompt are condensed part by
part first. A merge with `--code` commits a branch's uncommitted work in its
worktree, then runs `git merge --no-ff` in the current chat's worktree; when
the merge stops at conflicts, it is left for you or the model to resolve, and
the merge note lists the conflicting files. Merging a branch that shares the
current workspace with `--code` is refused, since its changes are already
there.

The agent can branch on its own, too. `branch_create` forks the conversation
into a sub-agent that works on a task with the whole history so far, and
`branch_merge` waits for a branch to finish and merges it in any of the four
ways, optionally toward a `goal`. Its branches show up in `/tree` like any
other. Sub-agents that `spawn_agent` starts without the history join the
conversation as branches too, named after the agent.

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
| `auto` | The default. The whole history until it fills `soft_limit_percent` of the context window, then a recalled history instead of a summary. |
| `recall` | A recalled history before every turn. Meant for experiments. |

A recalled history keeps the most recent turns and the first request in full,
lists the user's constraints and the decisions still in force, has one line per
earlier turn, and adds the earlier blocks a screening model picked for the
current request, in full, as excerpts, or as summaries. Blocks whose cards
match the request's words, files or symbols but that the screening did not pick
are added as summaries, within `lexical_share_percent` of the budget, so a
block the screening missed is still in view; they are the first to go when the
history is over budget. `/compact` recalls instead of summarizing when `mode`
is not `full`. If recall fails, Codex falls back to summary compaction.

A turn does not wait long for recall:

- When a turn ends and the next one would recall, recall runs in the
  background for it, reading the turn's request and answer.
- When the next request comes, that recall is used as it is if it already shows
  the blocks lexical search ranks highest for the request; a request with
  nothing to match, such as "go on", is served by it too.
- Otherwise the turn waits up to `recall_wait_ms` for the recall of its
  request. If that takes longer, the turn goes on with the earlier recall, or
  with summaries of the blocks lexical search ranks highest, and the late
  recall adds what it picked before a later model request of the turn.

When the history has to shrink at once, because the context window is full or
you ran `/compact`, recall is waited for.

The model also gets three tools: `context_search` finds blocks of the whole
history by words, `context_expand` reads a block or a whole turn in full, and
`context_recall` asks the screening model for the parts of the history a
question needs.

When the screening is unsure, because the request refers to something the
cards do not show, chains several earlier steps, or leaves many blocks
undecided, `escalate_model` screens again with the same prompt. A history too
long for one catalog is first narrowed to the episodes the request needs, then
screened block by block within them.

`screen` changes how cards are judged. `listwise`, the default, has the
screening model read every card and choose. `pointwise` scores each card on its
own and picks the best; `cascade` scores every card, then lets the screening
model choose among the best ones. Scores come from `rerank_url`, a rerank
endpoint in the common `/rerank` format, or from the screening model when no
endpoint is set. A rerank endpoint receives the text of the cards.

Every recall is recorded in the chat's rollout as a `context_view_recalled`
event, with the blocks it showed, why, and how much of each, how it reached the
context window and how long the turn waited for it, so what the model saw can
be audited later. `/context` lists the latest recalls the same way. Each `context_expand` of a block is counted, and so is
each block it reads that the recalled history had left out; `/context` shows
both.

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
recall_wait_ms = 3000           # longest a turn waits for the recall of its request
speculative_recall = true       # recall for the next turn when a turn ends
lexical_share_percent = 15      # budget share for unpicked lexical matches; 0 adds none
rebuild_overlap_percent = 50    # rebuild the recalled history when less of it still fits
edit_creates_branch = true      # editing an earlier prompt forks instead of rewinding
escalate_model = "gpt-6.1-sol"  # screens again when the first screening is unsure
screen = "listwise"             # listwise | pointwise | cascade
rerank_url = "http://localhost:8080/rerank"   # scores cards for pointwise and cascade
rerank_model = "qwen3-reranker"
rerank_api_key_env = "RERANK_API_KEY"
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
| `contextGraph/read` | The branches of a thread's graph in tree order, with their purposes and statuses, and its merges. |
| `contextGraph/branch/read` | A branch's turns, inherited and its own, each with its blocks. |
| `contextGraph/block/read` | A block as text, read from whichever branch recorded it. |
| `contextGraph/search` | Blocks of every branch whose cards match words, files or symbols. |
| `contextGraph/merge` | Merges branches or saved chats into a thread as one note: fold, reference, synthesize or inline, with an optional goal and code merge. |
| `contextGraph/branch/update` | Renames a branch, sets what it is for, or archives or restores it; also before the thread first records. |
| `contextGraph/branch/preview` | What folding a branch into a thread would add. |
| `contextGraph/mode/set` | Switches a loaded thread's context mode. |
| `contextGraph/recall` | Shows the view recall would build for a question, or asks for a rebuild before the next turn. |
| `contextGraph/views/read` | The views recall built for a thread, most recent first, with the blocks each showed and why, and the counts of expanded and missed blocks. |

`contextGraph/updated` notifies clients when a merge or a branch change alters
a graph, and `contextGraph/cardsUpdated` when the shadow agent wrote cards.
Forks are made with `thread/fork`; a fork of a recorded chat joins its graph.

## Saved conversations

`/tree` and `/merge` work on the context graph; without the `context_graph`
feature they say how to turn it on. The earlier `codex merge` command of the npm
package, which merges saved conversations into a new one, still works on its
own.

`codex graph import` records the saved conversations that ran without the
context graph, so their forks show up as branches in `/tree`. Merges made with
the earlier `codex merge` tool are recorded as merges, and the graphs of the
conversations they merged are joined. `--dry-run` lists what would be imported;
thread IDs limit the import to those conversations, and `--archived` includes
archived ones. A conversation whose history is stored in pages is imported from
the latest history its model saw, so turns compacted away before are missing.

`codex graph export THREAD_ID` writes the graph of that conversation:
`--format tree` (the default) as an indented list of branches, `--format
mermaid` as a flowchart of branches, forks and merges, and `--format json` as
the whole graph with its cards and merges. `-o FILE` writes to a file.

Deleting a conversation deletes what the graph derived from it.

## Evaluation

`codex-context-graph-eval` indexes saved rollouts, recalls from them at any
turn, and runs probe benchmarks that compare recall with the full history and
other selectors. Run it with `--help` for the commands.

`probe-run --conditions` takes `full`, `window`, `bm25`, `oracle` and
`recall:SCREEN[+READ|+none]`. A recall condition can end in `@pointwise` or
`@cascade` to screen that way, and in `@lexN` to give lexical matches N percent
of the budget, so one run can compare, for example,
`recall:luna+none,recall:luna+none@lex0,recall:luna+none@cascade`.
`probe-report` shows, for each condition, the score, the share of support
blocks shown (the selection's recall), and the share of the earlier blocks
shown that are support (its precision). `--valid-only` counts only probes that
the oracle context answers and the recent turns alone do not.

`probe-gen --at 0.5,1` asks probes halfway through each session and at its
end, so that answers about earlier work are not overtaken by later turns.
`speculate` replays sessions turn by turn: as each turn ends it recalls ahead
of the next request, then recalls for that request, and records whether the
recall made ahead would have served it and how much of the request's picks it
shows. `speculate-report` summarizes how often turns start without screening,
how long they would wait with `--wait-ms`, and what the screening costs.
