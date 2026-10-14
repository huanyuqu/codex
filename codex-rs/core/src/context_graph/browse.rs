//! Reading the conversation's graph for clients: the text of a block of any branch.

use anyhow::Context as _;
use codex_context_graph::BlockId;
use codex_context_graph::BlockKind;
use codex_context_graph::text::render_block;

use super::ContextGraphService;

/// Characters of a block's text a client gets; longer items keep their head and tail.
const BLOCK_TEXT_CHARS: usize = 200_000;
const ITEM_TEXT_CHARS: usize = 50_000;

/// A block as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlockText {
    /// The branch that recorded the block.
    pub branch_name: String,
    pub kind: BlockKind,
    pub text: String,
}

impl ContextGraphService {
    /// The text of block `block_id`, read from this thread's history or from the rollout of the
    /// branch that recorded it.
    pub(crate) async fn read_block(&self, block_id: &str) -> anyhow::Result<ContextBlockText> {
        let id = BlockId::from_stored(block_id.trim());
        let tree = self.branch_tree().await?;
        let owner = tree
            .owner(&id)
            .with_context(|| format!("no block {block_id} in this conversation"))?
            .clone();
        let own = self
            .live
            .observed_history()
            .into_iter()
            .find(|block| block.block.id == id);
        let block = match own {
            Some(block) => block,
            None => {
                let thread_id = owner
                    .thread_id
                    .as_deref()
                    .context("the branch that recorded the block has no chat to read")?;
                self.live
                    .referenced_blocks(self.threads.as_ref(), thread_id, owner.head.as_ref())
                    .await?
                    .iter()
                    .find(|block| block.block.id == id)
                    .cloned()
                    .with_context(|| format!("block {block_id} is no longer in its chat"))?
            }
        };
        Ok(ContextBlockText {
            branch_name: owner.name,
            kind: block.block.kind,
            text: render_block(&block.items, ITEM_TEXT_CHARS, BLOCK_TEXT_CHARS),
        })
    }
}
