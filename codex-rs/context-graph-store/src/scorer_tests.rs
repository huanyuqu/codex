use codex_context_graph::prompt::ModelPrompt;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::*;
use crate::model::ModelCompletion;

/// Scores every card of a batch by its position: 0.1 for C1, 0.2 for C2, and so on.
struct CountingModel;

impl ShadowModel for CountingModel {
    fn name(&self) -> String {
        "counting".to_string()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            let cards = prompt
                .input
                .lines()
                .filter(|line| line.starts_with('C'))
                .count();
            let scores: Vec<_> = (1..=cards)
                .map(|card| json!({"card": format!("C{card}"), "needed": card as f32 / 10.0}))
                .collect();
            Ok(ModelCompletion {
                text: json!({ "scores": scores }).to_string(),
                ..ModelCompletion::default()
            })
        })
    }
}

#[tokio::test]
async fn a_model_scores_cards_in_batches() {
    let scorer = ModelScorer::new(Arc::new(CountingModel));
    let documents: Vec<String> = (0..MODEL_BATCH + 2).map(|n| format!("card {n}")).collect();
    let scores = scorer
        .score("q", &documents)
        .await
        .unwrap_or_else(|err| panic!("score: {err:#}"));
    assert_eq!(scores.len(), documents.len());
    // The second batch starts counting again.
    assert_eq!(scores[MODEL_BATCH], 0.1);
    assert_eq!(scores[1], 0.2);
}

#[tokio::test]
async fn a_rerank_endpoint_scores_cards_by_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .and(header("authorization", "Bearer secret"))
        .and(body_partial_json(json!({
            "model": "reranker",
            "query": "why did it fail",
            "documents": ["a", "b", "c"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {"index": 2, "relevance_score": 0.9},
                {"index": 0, "relevance_score": -2.0}
            ]
        })))
        .mount(&server)
        .await;
    let scorer = RerankScorer::new(
        format!("{}/rerank", server.uri()),
        Some("reranker".to_string()),
        Some("secret".to_string()),
    );
    let documents = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let scores = scorer
        .score("why did it fail", &documents)
        .await
        .unwrap_or_else(|err| panic!("score: {err:#}"));
    assert_eq!(scores[1], 0.0);
    assert_eq!(scores[2], 0.9);
    // Logits outside 0..1 are squashed.
    assert!((scores[0] - 0.119).abs() < 0.01, "{scores:?}");
    assert_eq!(scorer.name(), "reranker");
}
