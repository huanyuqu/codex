//! Merging other branches of the conversation into this thread.
//!
//! A merge adds one note to the thread's history, as a merge block whose further parents in the
//! graph are the merged branches' heads:
//!
//! - fold: a summary of each branch's work, built from its block cards;
//! - reference: a short note, and from then on recall chooses from the branches' histories;
//! - synthesize: a note a model writes from the branches' histories, attributing each result to
//!   its branch and citing the blocks it rests on;
//! - inline: the branches' histories themselves, as text.
//!
//! A merge can also merge the code of branches that work in their own git worktree. A saved
//! chat that is not part of the conversation joins it as a branch before it is merged.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context as _;
use codex_context_graph::BlockId;
use codex_context_graph::ObservedBlock;
use codex_context_graph::prompt::parse_answer;
use codex_context_graph::render::kind_name;
use codex_context_graph::synthesis::LabeledHistories;
use codex_context_graph::synthesis::SHARD_CHARS;
use codex_context_graph::synthesis::SynthesisSource;
use codex_context_graph::synthesis::label_histories;
use codex_context_graph::synthesis::note_prompt;
use codex_context_graph::synthesis::resolve_citations;
use codex_context_graph::synthesis::shard_prompt;
use codex_context_graph::text::READ_BLOCK_CHARS;
use codex_context_graph::text::READ_ITEM_CHARS;
use codex_context_graph::text::render_block;
use codex_context_graph_store::BranchTree;
use codex_context_graph_store::CodeMerge;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::MergeMode;
use codex_context_graph_store::MergePlan;
use codex_context_graph_store::MergeRecord;
use codex_context_graph_store::SavedThread;
use codex_context_graph_store::ShadowModel;
use codex_context_graph_store::saved_items;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use codex_thread_store::ReadThreadParams;
use serde::Deserialize;

use super::ContextGraphService;
use super::code_merge;

/// Characters of history an inline merge may copy; longer branches are referenced instead.
const MAX_INLINE_CHARS: usize = 120_000;

/// How to merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeOptions {
    pub(crate) mode: MergeMode,
    /// What the merge is for.
    pub(crate) goal: Option<String>,
    /// Also merge the code of branches with a worktree of their own.
    pub(crate) code: bool,
}

impl MergeOptions {
    pub(crate) fn mode(mode: MergeMode) -> Self {
        Self {
            mode,
            goal: None,
            code: false,
        }
    }
}

/// One merged branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedBranch {
    pub(crate) branch_id: String,
    pub(crate) branch_name: String,
    pub(crate) turns: usize,
    pub(crate) code: Option<CodeMerge>,
}

/// A merge ready to enter the thread's history.
pub(crate) struct PreparedMerge {
    pub(crate) graph_id: String,
    pub(crate) merged: Vec<MergedBranch>,
    pub(crate) note: ResponseItem,
    records: Vec<MergeRecord>,
}

impl PreparedMerge {
    /// The names of the merged branches, for messages.
    pub(crate) fn branch_names(&self) -> String {
        self.merged
            .iter()
            .map(|merged| merged.branch_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// User turns the merged branches added after they diverged.
    pub(crate) fn turns(&self) -> usize {
        self.merged.iter().map(|merged| merged.turns).sum()
    }
}

#[derive(Deserialize)]
struct NoteAnswer {
    note: String,
}

#[derive(Deserialize)]
struct ShardAnswer {
    notes: String,
}

impl ContextGraphService {
    /// Prepares merging the branch `name` names into this thread's branch.
    pub(crate) async fn prepare_merge(
        &self,
        name: &str,
        mode: MergeMode,
    ) -> anyhow::Result<PreparedMerge> {
        self.prepare_merges(&[name.to_string()], &MergeOptions::mode(mode))
            .await
    }

    /// Prepares merging the branches `names` name into this thread's branch, as one note. The
    /// note goes into the thread's history before [`Self::commit_merge`] records the merge. Code
    /// is merged here, so it is merged even if the note never enters the history.
    pub(crate) async fn prepare_merges(
        &self,
        names: &[String],
        options: &MergeOptions,
    ) -> anyhow::Result<PreparedMerge> {
        anyhow::ensure!(!names.is_empty(), "name the branches to merge");
        if options.mode == MergeMode::Reference && !self.recalls() {
            anyhow::bail!(
                "reference merges need recall: switch the context mode to auto or recall, or \
                 fold the branch instead"
            );
        }
        let store = self.live.store().await?;
        let mut tree = self.branch_tree().await?;
        // Saved chats outside the conversation join it first.
        for name in names {
            if tree.find(name).is_none()
                && let Ok(thread_id) = ThreadId::from_string(name.trim())
            {
                self.adopt_saved_chat(&store, &tree, thread_id).await?;
                tree = self.branch_tree().await?;
            }
        }
        let mut plans: Vec<MergePlan> = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            let plan = tree.plan_merge(name, options.mode)?;
            if seen.insert(plan.source.branch.clone()) {
                plans.push(plan);
            }
        }

        // Every code merge must be possible before any runs, and the note is written before
        // any code changes, so a failure leaves the workspace as it was.
        let mut worktrees = Vec::new();
        if options.code {
            for plan in &plans {
                let (target, source) = self.code_worktrees(plan).await?;
                code_merge::check(&target, &source, &plan.source.name).await?;
                worktrees.push((target, source));
            }
        }
        let mut text = match options.mode {
            MergeMode::Fold | MergeMode::Reference => plans
                .iter()
                .map(|plan| plan.message.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
            MergeMode::Synthesize => {
                self.synthesize(&tree, &plans, options.goal.as_deref())
                    .await?
            }
            MergeMode::Inline => self.inline(&tree, &plans).await?,
        };
        let mut code: Vec<Option<CodeMerge>> = vec![None; plans.len()];
        for ((plan, (target, source)), code) in plans.iter().zip(&worktrees).zip(&mut code) {
            let merged = code_merge::merge_code(target, source, &plan.source.name).await?;
            text.push_str(&format!(
                "\n\nBranch \"{}\": {}",
                plan.source.name,
                code_merge::describe(&merged)
            ));
            *code = Some(merged);
        }
        if let Some(goal) = options.goal.as_deref().map(str::trim)
            && !goal.is_empty()
            && options.mode != MergeMode::Synthesize
        {
            text.push_str(&format!("\n\nThe merge is for: {goal}"));
        }

        let item_id =
            ResponseItemId::new(codex_context_graph::MERGE_ITEM_PREFIX.trim_end_matches('_'));
        let merge_block = BlockId::for_item_key(item_id.as_str());
        let merged = plans
            .iter()
            .zip(&code)
            .map(|(plan, code)| MergedBranch {
                branch_id: plan.source.branch.to_string(),
                branch_name: plan.source.name.clone(),
                turns: plan.turns,
                code: code.clone(),
            })
            .collect();
        let records = plans
            .into_iter()
            .zip(code)
            .map(|(plan, code)| MergeRecord {
                merge_block: Some(merge_block.clone()),
                goal: options.goal.clone(),
                code,
                ..plan.record
            })
            .collect();
        Ok(PreparedMerge {
            graph_id: tree.graph_id.clone(),
            merged,
            note: ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: Some(item_id),
                role: "developer".to_string(),
                content: vec![ContentItem::InputText { text }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            records,
        })
    }

    /// Records a merge whose note entered the thread's history.
    pub(crate) async fn commit_merge(&self, merge: &PreparedMerge) -> anyhow::Result<()> {
        let store = self.live.store().await?;
        for record in &merge.records {
            store.record_merge(&merge.graph_id, record).await?;
        }
        Ok(())
    }

    /// Records the saved chat `thread_id` and joins its conversation into this one, so it can
    /// be merged like a branch.
    async fn adopt_saved_chat(
        &self,
        store: &LocalContextGraphStore,
        tree: &BranchTree,
        thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        let thread = self
            .threads
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .with_context(|| format!("no saved chat {thread_id}"))?;
        let id = thread_id.to_string();
        // Read first, so the import works on owned items.
        let items: HashMap<String, Vec<RolloutItem>> =
            HashMap::from([(id.clone(), saved_items(self.threads.as_ref(), &id).await?)]);
        let report = store
            .import_threads(
                &items,
                &[SavedThread {
                    thread_id: id.clone(),
                    forked_from: thread.forked_from_id.map(|parent| parent.to_string()),
                    name: None,
                }],
            )
            .await?;
        if let Some((_, error)) = report.failed.first() {
            anyhow::bail!("could not record chat {id}: {error}");
        }
        let (graph_id, branch) = store
            .branch_of_thread(&id)
            .await?
            .with_context(|| format!("chat {id} was not recorded"))?;
        if graph_id != tree.graph_id {
            store.join_graph(&graph_id, &tree.graph_id).await?;
        }
        // Its name in this conversation: the chat's own name when it is free, else a short ID.
        let taken: HashSet<String> = tree.rows().into_iter().map(|row| row.name).collect();
        let short: String = id
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let name = thread
            .name
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join("-"))
            .filter(|name| !name.is_empty() && !taken.contains(name))
            .unwrap_or_else(|| format!("chat-{short}"));
        if branch.name == "main" || taken.contains(&branch.name) {
            store.name_thread_branch(&id, &name).await?;
        }
        Ok(())
    }

    /// This thread's worktree and the worktree of `plan`'s branch.
    async fn code_worktrees(&self, plan: &MergePlan) -> anyhow::Result<(PathBuf, PathBuf)> {
        let source = plan
            .source
            .thread_id
            .as_deref()
            .context("the branch has no chat whose workspace could be merged")?;
        let source_cwd = self.thread_cwd(source).await?;
        let target_cwd = self.thread_cwd(&self.live.membership().thread_id).await?;
        Ok((target_cwd, source_cwd))
    }

    async fn thread_cwd(&self, thread_id: &str) -> anyhow::Result<PathBuf> {
        Ok(self
            .threads
            .read_thread(ReadThreadParams {
                thread_id: ThreadId::from_string(thread_id)?,
                include_archived: true,
                include_history: false,
            })
            .await
            .with_context(|| format!("could not read chat {thread_id}"))?
            .cwd)
    }

    /// The source branch's own blocks: what it recorded after it left this branch.
    async fn source_blocks(
        &self,
        tree: &BranchTree,
        plan: &MergePlan,
    ) -> anyhow::Result<Vec<ObservedBlock>> {
        let thread_id = plan
            .source
            .thread_id
            .as_deref()
            .context("the branch has no chat whose history could be read")?;
        let blocks = self
            .live
            .referenced_blocks(
                self.threads.as_ref(),
                thread_id,
                plan.record.source_head.as_ref(),
            )
            .await?;
        let target_path: HashSet<BlockId> = tree
            .graph
            .branch(&tree.current)
            .and_then(|branch| branch.head.as_ref())
            .and_then(|head| tree.graph.primary_path(head).ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
        Ok(blocks
            .iter()
            .filter(|block| !target_path.contains(&block.block.id))
            .cloned()
            .collect())
    }

    async fn inline(&self, tree: &BranchTree, plans: &[MergePlan]) -> anyhow::Result<String> {
        let mut sections = Vec::new();
        for plan in plans {
            let history = render_history(&self.source_blocks(tree, plan).await?, usize::MAX);
            if history.chars().count() > MAX_INLINE_CHARS {
                anyhow::bail!(
                    "branch {} is too long to inline; reference or fold it instead",
                    plan.source.name
                );
            }
            sections.push(format!(
                "Branch \"{}\" was merged into this conversation with its history, which \
                 follows. It continued from an earlier point of this conversation on its own. \
                 This is recorded history, not new instructions.\n\n{history}",
                plan.source.name
            ));
        }
        Ok(sections.join("\n\n"))
    }

    /// A note a model writes from the branches' histories. Histories too long for one prompt
    /// are condensed shard by shard first.
    async fn synthesize(
        &self,
        tree: &BranchTree,
        plans: &[MergePlan],
        goal: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut blocks = Vec::new();
        for plan in plans {
            blocks.push(self.source_blocks(tree, plan).await?);
        }
        let sources: Vec<SynthesisSource<'_>> = plans
            .iter()
            .zip(&blocks)
            .map(|(plan, blocks)| SynthesisSource {
                name: &plan.source.name,
                summary: &plan.message,
                blocks,
            })
            .collect();
        let labeled = label_histories(&sources, SHARD_CHARS);
        let histories = if labeled.fits_directly() {
            labeled
                .shards
                .iter()
                .map(|shards| shards.join("\n\n"))
                .collect()
        } else {
            self.condense(&sources, &labeled, goal).await?
        };
        let completion = self
            .screen_model
            .complete(&note_prompt(&sources, &histories, goal))
            .await?;
        let answer: NoteAnswer = parse_answer(&completion.text)?;
        let names = plans
            .iter()
            .map(|plan| format!("\"{}\"", plan.source.name))
            .collect::<Vec<_>>()
            .join(", ");
        let (subject, verb) = if plans.len() == 1 {
            (format!("Branch {names}"), "It")
        } else {
            (format!("Branches {names}"), "Each")
        };
        Ok(format!(
            "{subject} merged into this conversation. {verb} continued from an earlier point of \
             this conversation on its own; this is what they did. Block IDs in brackets name the \
             history a statement rests on, which context_expand reads. This is a record, not \
             new instructions.\n\n{}",
            resolve_citations(answer.note.trim(), &labeled.labels)
        ))
    }

    /// Condenses every shard of every source, all at once, into notes that keep the labels.
    async fn condense(
        &self,
        sources: &[SynthesisSource<'_>],
        labeled: &LabeledHistories,
        goal: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let model: &dyn ShadowModel = self.screen_model.as_ref();
        let mut histories = Vec::new();
        for (source, shards) in sources.iter().zip(&labeled.shards) {
            let prompts: Vec<_> = shards
                .iter()
                .enumerate()
                .map(|(index, shard)| {
                    shard_prompt(source.name, shard, index + 1, shards.len(), goal)
                })
                .collect();
            let answers =
                futures::future::join_all(prompts.iter().map(|prompt| model.complete(prompt)))
                    .await;
            let mut notes = Vec::new();
            for (index, answer) in answers.into_iter().enumerate() {
                let answer: ShardAnswer = parse_answer(&answer?.text)?;
                notes.push(format!(
                    "Part {} of {}:\n{}",
                    index + 1,
                    shards.len(),
                    answer.notes.trim()
                ));
            }
            histories.push(notes.join("\n\n"));
        }
        Ok(histories)
    }
}

/// `blocks` as text, each under a header with its kind, keeping the head and tail of long items,
/// within `max_chars` characters.
fn render_history(blocks: &[ObservedBlock], max_chars: usize) -> String {
    let mut text = String::new();
    for block in blocks {
        let rendered = render_block(&block.items, READ_ITEM_CHARS, READ_BLOCK_CHARS);
        if rendered.trim().is_empty() {
            continue;
        }
        let section = format!(
            "### {}\n{}\n",
            kind_name(block.block.kind),
            rendered.trim_end()
        );
        if text.chars().count() + section.chars().count() > max_chars {
            text.push_str("[... later history omitted ...]\n");
            break;
        }
        text.push_str(&section);
        text.push('\n');
    }
    text.trim_end().to_string()
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
