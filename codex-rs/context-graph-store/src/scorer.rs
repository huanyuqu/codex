//! Relevance scorers for pointwise screening: given a request and the text of some cards, each
//! scorer returns, for every card, how likely the request needs its block, from 0 to 1.
//!
//! [`ModelScorer`] asks a model in batches of cards. [`RerankScorer`] calls a rerank endpoint in
//! the common `/rerank` format (`{"model", "query", "documents"}` answered with
//! `{"results": [{"index", "relevance_score"}]}`), which local rerankers and hosted rerank APIs
//! serve; it sends the cards' text to that endpoint.

use std::sync::Arc;

use codex_context_graph::score::parse_scores;
use codex_context_graph::score::score_prompt;
use futures::future::BoxFuture;
use futures::future::join_all;
use serde::Deserialize;
use serde_json::json;

use crate::model::ShadowModel;

/// Cards a model judges per request.
const MODEL_BATCH: usize = 25;

/// Scores how likely a request needs each block, from the text of its card.
pub trait RelevanceScorer: Send + Sync {
    /// The name recorded with the views it helped build.
    fn name(&self) -> &str;

    fn score<'a>(
        &'a self,
        question: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<f32>>>;
}

/// Scores cards with a model, a batch per request, the batches in parallel.
pub struct ModelScorer {
    model: Arc<dyn ShadowModel>,
}

impl ModelScorer {
    pub fn new(model: Arc<dyn ShadowModel>) -> Self {
        Self { model }
    }
}

impl RelevanceScorer for ModelScorer {
    fn name(&self) -> &str {
        self.model.name()
    }

    fn score<'a>(
        &'a self,
        question: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<f32>>> {
        Box::pin(async move {
            let prompts: Vec<_> = documents
                .chunks(MODEL_BATCH)
                .map(|batch| (batch.len(), score_prompt(question, batch)))
                .collect();
            let answers = join_all(
                prompts
                    .iter()
                    .map(|(_, prompt)| self.model.complete(prompt)),
            )
            .await;
            let mut scores = Vec::with_capacity(documents.len());
            for ((len, _), answer) in prompts.iter().zip(answers) {
                // A failed batch scores its cards 0 rather than failing the whole screening.
                match answer.and_then(|answer| {
                    parse_scores(&answer.text, *len).map_err(anyhow::Error::from)
                }) {
                    Ok(batch) => scores.extend(batch),
                    Err(err) => {
                        tracing::warn!("a scoring batch failed: {err:#}");
                        scores.extend(std::iter::repeat_n(0.0, *len));
                    }
                }
            }
            Ok(scores)
        })
    }
}

/// Scores cards with a rerank endpoint.
pub struct RerankScorer {
    client: reqwest::Client,
    url: String,
    model: Option<String>,
    api_key: Option<String>,
    name: String,
}

impl RerankScorer {
    /// A scorer that posts to `url`, naming `model` when it is set and authenticating with
    /// `api_key` as a bearer token when it is set.
    pub fn new(url: String, model: Option<String>, api_key: Option<String>) -> Self {
        let name = model.clone().unwrap_or_else(|| format!("rerank {url}"));
        Self {
            client: reqwest::Client::new(),
            url,
            model,
            api_key,
            name,
        }
    }
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResult>,
}

#[derive(Deserialize)]
struct RerankResult {
    index: usize,
    #[serde(alias = "score")]
    relevance_score: f32,
}

impl RelevanceScorer for RerankScorer {
    fn name(&self) -> &str {
        &self.name
    }

    fn score<'a>(
        &'a self,
        question: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<f32>>> {
        Box::pin(async move {
            let mut body = json!({
                "query": question,
                "documents": documents,
                "top_n": documents.len(),
                "return_documents": false,
            });
            if let Some(model) = &self.model {
                body["model"] = json!(model);
            }
            let mut request = self.client.post(&self.url).json(&body);
            if let Some(api_key) = &self.api_key {
                request = request.bearer_auth(api_key);
            }
            let response = request.send().await?.error_for_status()?;
            let response: RerankResponse = response.json().await?;
            let mut scores = vec![0.0; documents.len()];
            for result in response.results {
                if let Some(score) = scores.get_mut(result.index) {
                    // Rerankers score on their own scales; a logistic squashes logits into 0..1.
                    *score = if (0.0..=1.0).contains(&result.relevance_score) {
                        result.relevance_score
                    } else {
                        1.0 / (1.0 + (-result.relevance_score).exp())
                    };
                }
            }
            Ok(scores)
        })
    }
}

#[cfg(test)]
#[path = "scorer_tests.rs"]
mod tests;
