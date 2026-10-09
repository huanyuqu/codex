# Validation

Environment: macOS arm64, Node.js 22.17.0, Rust 1.98.1. The earlier regression
used official native Codex CLI 0.162.0; the interactive feature also uses a native
CLI compiled from this branch (development version 0.0.0).
Git baseline: `huanyuqu/codex` main at
`2351d9e1b608e6f9d9a3699b71d7eb39ee41cfa4`, also the fetched `openai/codex` main
at validation time. Both merge interfaces use the existing app-server protocol.

## Automated regression

Run the suite with a genuine native executable:

```sh
CODEX_MERGE_TEST_BINARY="$PWD/codex-rs/target/debug/codex" \
CODEX_TUI_MERGE_TEST_BINARY="$PWD/codex-rs/target/debug/codex" \
  node --test --test-reporter=spec codex-cli/tests/*.test.js
```

129 tests passed with no failures or skips in the final regression run.

The final suite covers the earlier deterministic/task-state merge and the new
conversation graph compiler. Native integration tests use disposable isolated
Codex homes and controlled local Responses SSE providers. These tests require no
live model credentials and check protocol/persistence rather than model quality.

Graph checks cover:

- Exact `A1 → B2` and `A2 → C3` fork anchors, three merge parents, copied legacy
  identities, unknown ancestry, DAG validation, stable nodes, separate tool
  calls/results, and branch-local call correlations.
- Default short merges with one semantic synthesis call and inline originals,
  goals without an extra flag, and rejection of the removed flag. Explicit
  structural-library regressions still isolate projection/reuse without inference.
- Unchanged source-file hashes, repeated graph reuse, incremental branch updates,
  post-merge messages, and nested forks.
- A large single-message branch split into bounded UTF-8 analysis fragments while
  retaining one original node; hierarchical reduction, escaped text, cached
  readings, and bounded model inputs.
- Whole-context planning with native occupancy, window limits, future-dialogue
  reserves, forced-inline rejection, insufficient-analysis fallback to references,
  no-inference dry runs, and no-target failure on invalid model citations.
- Exact original evidence, large-node byte ranges, branch pagination, bounded
  metadata across 32 sources, graph inspection, and Mermaid output.
- Session-tree pagination, ancestor/sibling/descendant discovery, unrelated-root
  filtering, all direct merge parents, independent-root and nested merges, and
  reachability of fresh compact targets from each original source.
- Recovery of omitted native fork metadata from plain, reference-backed and zstd
  rollout headers; older graph merges and inherited capsules; deleted sources,
  archived branches, missing evidence, invalid lineage, and repeated cursors.
- Pending-source badges and `/merge --update`: exact source-prefix checks,
  preservation of subsequent mainline dialogue, unchanged-source tracking across
  repeated/nested updates, and preventing older imported graphs from regressing
  incorporated source boundaries. No-op updates and previews perform no inference,
  create no target and write no archives.
- Renames and settings events do not advance a source. Rewritten, rolled-back,
  compacted, deleted, archived or busy sources do not produce a false up-to-date
  result. Source changes during update analysis fail before target creation.
- Recovery of ordinary boundaries from older graph evidence, frozen ancestry
  after verified primary compaction, rejection of rolled-back merge recovery and
  mismatched lineage/evidence, and oversized update-batch rejection.

Four native graph tests exercise both legacy and paginated storage. Two create
staggered forks, compress a long branch, inline another branch's image, restart and
resume, reuse a graph, and merge new source messages. Two start with an oversized
primary, create a bounded fresh target, preserve all originals, restart, execute a
continuation turn, inspect the outgoing graph context, and merge again while
preserving all original parent links. They caught and verified the fix for native
`thread/start` creating its rollout lazily on first injection.
The first two also exercise default semantic synthesis through the external CLI
with a controlled provider, without an opt-in flag.

The earlier regression cases retain deterministic merge, task-state compatibility,
source races, titles, imports/images, restart/resume, rollback/steering,
compaction, bounded history references, zstd, numeric literals, archive integrity,
semantic reference/coverage validation, and analysis errors. Streaming tests
cover notifications before RPC replies, completion filtering, fallback items,
tool attempts, failures, deadlines, crashes, empty output, and listener cleanup.
Both native task-state cases also convert their saved results to a conversation
graph using the original branches, without further inference.

## Real-model generic writing case

A separate synthetic novel-writing discussion called the configured real model,
`gpt-6.1-sol`. B forked after A1, before the primary added A2's sealed-envelope
constraint. C forked after A2. B proposed restrained prose without knowing the
letter's contents. C withdrew a draft in which the character read the sealed
letter and replaced it with an explicitly uncertain guess. A3 asked to retain both
candidates without choosing a final style.

This earlier real-model validation used forced summary mode and the then-optional
semantic setting: two branch readings and one cross-branch narrative. Synthesis
is now the default; this historical case has not been rerun with live credentials.
The output retained the actual fork
positions, B's lack of later A2 context, the character's knowledge limits, C's
withdrawal/correction, the uncertain guess, and the absence of a final style
choice. It used a generic narrative, not a coding task-state template.

| Checked property                      | Result                                              |
| ------------------------------------- | --------------------------------------------------- |
| Real model calls                      | 3: two summaries and one cross-branch reading       |
| Imported original records             | 4                                                   |
| Persisted graph nodes                 | 11: ten originals and one merge                     |
| Added context                         | 6,439 bytes                                         |
| Context planning estimate             | 8,661, within the selected budget                   |
| Original synthetic rollouts unchanged | SHA-256 verified                                    |
| Staggered fork points                 | Matched exact saved ancestor records                |
| Restart/resume                        | Passed                                              |
| Repeated structural merge             | Reused the graph, with further inference prohibited |

The case used a disposable home linked to existing authentication/configuration
without printing credentials. It removed synthetic sessions afterward. Saved
thread IDs are validation identifiers, not durable user sessions. Original
evidence and model readings remain in ignored local artifacts:

- `.validation/live-graph-result.json`
- `.validation/live-graph-evidence/40c683310db8df4f1df5dfc7f4ac22efb17b0637ed6cb5cda56db98b119b0c63.json`

An earlier one-call synthetic cache example validated the compatibility task-state
merger, including an invalidation failure and an unsupported release claim. Its
report remains in `.validation/live-semantic-result.json`; it is not the new
compiler's generic validation case.

This is one real-model generic example, with short branch summaries. Long-input
chunking/reduction and native continuation use controlled providers. Citation,
coverage metadata, and checksum checks cannot prove semantic completeness or
correctness in other conversations. Summaries are text-only; archived image
contents are not inspected by their analysis. Planning estimates are conservative
byte/native-usage estimates, not an exact provider tokenizer or a guarantee about
all hidden target instructions and future input.

## Interactive CLI validation

`cargo build --locked -p codex-cli --bin codex` successfully builds the native CLI.
`cargo test --locked -p codex-tui --lib slash_command` passes 197 targeted tests,
including command availability, quoted arguments, unavailable/busy sessions,
multiple branch selection, tree search/current selection and picker cancellation. This is targeted TUI coverage,
not the full Rust workspace suite.

Eleven JavaScript tests exercise the JSONL bridge using a shared app-server
transport, early streaming notifications, default short synthesis and no-inference previews,
option-only picker requests, invalid arguments, cancellation before a turn reply,
host disconnects, read-only tree discovery, and update/no-op routing without a
picker. Nine additional tree tests cover the session relationships and
compatibility paths described above. Nineteen update regression cases, including
three unavailable-source subcases, cover the incremental update behavior.

The additional native terminal test uses a real Unix PTY, disposable Codex home,
and controlled Responses provider. It verifies bare `/merge` selecting B and C,
help and preview, a default merge with one analysis call, automatic switching, and a
continuation request containing both original branch deltas. A second run selects
explicit IDs and completes two summaries plus one semantic reading. A third holds
analysis open, presses Esc, and verifies cancellation while the current rollout
remains A. Both successful targets retain three merge parents; source conversation
records remain unchanged. It opens `/tree` before merging, searches B's UUID in
the tree picker after merging, switches B to C with `/tree C_ID`, then returns
to the saved merge using `/tree M_ID`; rollout identities verify every switch.
An additional terminal run continues B after its first merge, returns to M and
checks the pending-source tree indicator. It verifies an update preview without
inference, one default semantic update call, automatic switching to the new chat,
and a continuation request containing both B's new input and M's earlier dialogue.
Unchanged updates before and after that run make no model calls and keep the same
rollout identity. B changes only through the explicit continuation; updating its
merge preserves B's resulting conversation records. Native graph regressions in
both legacy and paginated storage also use automatic source discovery and check
the external CLI's no-op update response after restart.

Tree discovery and switching make no model calls. Ordinary native resume appends thread-settings events
to source logs, so this TUI check compares source context records rather than asserting
that the entire runtime log is byte-identical.

The checkout launcher selects the local debug build and uses its embedded
app-server because a debug binary has no daemon installation package. The TUI
passes merge RPCs through its current connection and opens the result with the
existing resume flow. Native terminal coverage currently requires Unix/Python 3;
the command/picker and bridge unit tests cover the platform-neutral code.

## Packaging and source checks

All fourteen runtime JavaScript files (the launcher and thirteen graph modules) are listed
in `codex-cli/package.json`. Targeted formatting, syntax checks, `npm pack
--dry-run`, and `git diff --check` validate the changed files and package contents.
The source patch is checked against the pinned Git baseline in an isolated
checkout, including new graph modules, tests, and documentation. Local source and
patch hashes are recorded in `.upstream/source-manifest.json`.

The TUI crate includes `/merge`, `/tree`, and their shared connection bridge. These checks do
not claim a full Rust workspace test run, a new native RPC endpoint, a Desktop
merge UI, or merging source Git modifications.
