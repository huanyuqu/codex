# Validation

Environment: macOS arm64, Node.js 22.17.0, official native Codex CLI 0.162.0.
Git baseline: `huanyuqu/codex` main at
`2351d9e1b608e6f9d9a3699b71d7eb39ee41cfa4`, also the fetched `openai/codex` main
at validation time. The modified launcher uses the existing native app-server.

## Automated regression

Run the suite with a genuine native executable:

```sh
CODEX_MERGE_TEST_BINARY=/usr/local/bin/codex \
  node --test --test-reporter=spec codex-cli/tests/*.test.js
```

89 tests passed with no failures or skips in the final regression run.

The final suite covers the earlier deterministic/task-state merge and the new
conversation graph compiler. Native integration tests use disposable isolated
Codex homes and controlled local Responses SSE providers. These tests require no
live model credentials and check protocol/persistence rather than model quality.

Graph checks cover:

- Exact `A1 → B2` and `A2 → C3` fork anchors, three merge parents, copied legacy
  identities, unknown ancestry, DAG validation, stable nodes, separate tool
  calls/results, and branch-local call correlations.
- Short original views with no inference, unchanged source-file hashes, repeated
  graph reuse, incremental branch updates, post-merge messages, and nested forks.
- A large single-message branch split into bounded UTF-8 analysis fragments while
  retaining one original node; hierarchical reduction, escaped text, cached
  readings, and bounded model inputs.
- Whole-context planning with native occupancy, window limits, future-dialogue
  reserves, forced-inline rejection, insufficient-analysis fallback to references,
  no-inference dry runs, and no-target failure on invalid model citations.
- Exact original evidence, large-node byte ranges, branch pagination, bounded
  metadata across 32 sources, graph inspection, and Mermaid output.

Four native graph tests exercise both legacy and paginated storage. Two create
staggered forks, compress a long branch, inline another branch's image, restart and
resume, reuse a graph, and merge new source messages. Two start with an oversized
primary, create a bounded fresh target, preserve all originals, restart, execute a
continuation turn, inspect the outgoing graph context, and merge again while
preserving all original parent links. They caught and verified the fix for native
`thread/start` creating its rollout lazily on first injection.

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

Forced summary mode generated two branch readings; optional semantic mode then
generated one cross-branch narrative. The output retained the actual fork
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
| Repeated default merge                | Reused the graph, with further inference prohibited |

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

## Packaging and source checks

All ten runtime JavaScript files (the launcher and nine merge modules) are listed
in `codex-cli/package.json`. Targeted formatting, syntax checks, `npm pack
--dry-run`, and `git diff --check` validate the changed files and package contents.
The source patch is checked against the pinned Git baseline in an isolated
checkout, including new graph modules, tests, and documentation. Local source and
patch hashes are recorded in `.upstream/source-manifest.json`.

No Rust crates changed. These checks do not claim a Rust workspace build, a new
native RPC endpoint, a Desktop merge UI, or merging source Git modifications.
