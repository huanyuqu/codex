//! The models the shadow agent calls to index and recall.

use codex_context_graph::prompt::ModelPrompt;
use futures::future::BoxFuture;

/// A model's answer, with its token usage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCompletion {
    pub text: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

/// A model that answers [`ModelPrompt`]s with JSON that follows their schema.
pub trait ShadowModel: Send + Sync {
    /// The name recorded with the cards and views it produces, such as its model slug.
    fn name(&self) -> String;

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>>;
}
