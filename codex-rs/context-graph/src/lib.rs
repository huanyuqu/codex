//! Context blocks and the context graph.
//!
//! A context block is the smallest unit of conversation history that is kept or dropped as a
//! whole: one user message, one assistant message with the reasoning before it, or one tool call
//! with its outputs. Blocks form an append-only DAG, and branches are named heads into that DAG.
//!
//! Block IDs are derived from the stable IDs of the response items they contain, so replaying the
//! same history always yields the same blocks, and history inherited by a fork yields blocks that
//! are shared with its source. This crate is pure logic and performs no IO.

mod block;
pub mod branches;
pub mod card;
pub mod card_prompt;
pub mod episode;
pub mod extract;
mod graph;
pub mod history;
mod ids;
mod index;
pub mod links;
pub mod lookup;
pub mod prompt;
pub mod read;
pub mod render;
mod replay;
pub mod screen;
pub mod search;
mod segmenter;
#[cfg(test)]
mod test_support;
pub mod text;
pub mod turn;
pub mod view;

pub use block::Block;
pub use block::BlockKind;
pub use block::MERGE_ITEM_PREFIX;
pub use graph::AppendOutcome;
pub use graph::BlockNode;
pub use graph::Branch;
pub use graph::BranchStatus;
pub use graph::ContextGraph;
pub use graph::GRAPH_FORMAT;
pub use graph::GraphError;
pub use graph::GraphExport;
pub use ids::BlockId;
pub use ids::BranchId;
pub use index::BlockIndexer;
pub use index::IndexUpdate;
pub use index::index_rollout;
pub use replay::GraphChange;
pub use replay::GraphRecorder;
pub use replay::ObservedBlock;
pub use replay::RecorderEvent;
pub use replay::ReplayStats;
pub use replay::replay_rollout;
pub use segmenter::DefaultClassifier;
pub use segmenter::MessageClass;
pub use segmenter::MessageClassifier;
pub use segmenter::SealedBlock;
pub use segmenter::Segmenter;
