use std::sync::Mutex;

use codex_context_graph::BlockId;
use codex_context_graph::BlockKind;
use codex_context_graph::BranchId;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::CARD_VERSION;
use codex_context_graph::history::HistoryBlock;
use codex_context_graph::prompt::ModelPrompt;
use codex_context_graph::view::LineRange;
use codex_context_graph::view::Reason;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::LocalContextGraphStore;

/// Answers screening and reading prompts with fixed JSON, or fails.
struct ScriptedModel {
    screen: Option<&'static str>,
    read: Option<&'static str>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedModel {
    fn new(screen: Option<&'static str>, read: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            screen,
            read,
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .map(|prompts| prompts.clone())
            .unwrap_or_default()
    }
}

impl ShadowModel for ScriptedModel {
    fn name(&self) -> String {
        "scripted".to_string()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            if let Ok(mut prompts) = self.prompts.lock() {
                prompts.push(prompt.schema_name.to_string());
            }
            let answer = match prompt.schema_name {
                "context_selection" => self.screen,
                "context_reading" => self.read,
                _ => None,
            };
            let Some(answer) = answer else {
                anyhow::bail!("no answer for {}", prompt.schema_name);
            };
            Ok(ModelCompletion {
                text: answer.to_string(),
                input_tokens: 1_000,
                cached_input_tokens: 0,
                output_tokens: 50,
            })
        })
    }
}

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn card(id: &str, kind: BlockKind, turn: &str) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored(id),
        kind,
        turn_id: Some(turn.to_string()),
        card_version: CARD_VERSION,
        indexer: "test".to_string(),
        title: format!("title {id}"),
        summary: format!("summary {id}"),
        entities: Default::default(),
        tools: Vec::new(),
        outcome: None,
        exit_code: None,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        file_stamps: Vec::new(),
        tokens: 0,
    }
}

/// Two turns: `u1` asks to fix the parser, `l1` is a long log, `a1` explains the fix; `u2`
/// asks a follow-up.
struct Session {
    cards: Vec<BlockCard>,
    items: Vec<Vec<ResponseItem>>,
}

impl Session {
    fn new() -> Self {
        let log = (1..=200)
            .map(|line| format!("parser log line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            cards: vec![
                card("u1", BlockKind::UserMessage, "t1"),
                card("l1", BlockKind::AssistantMessage, "t1"),
                card("a1", BlockKind::AssistantMessage, "t1"),
                card("u2", BlockKind::UserMessage, "t2"),
            ],
            items: vec![
                vec![message("user", "Fix the parser.")],
                vec![message("assistant", &log)],
                vec![message(
                    "assistant",
                    "The parser now skips comments.\nTests pass.",
                )],
                vec![message("user", "Does it handle nested comments?")],
            ],
        }
    }

    fn history(&self) -> History<'_> {
        History::new(
            self.cards
                .iter()
                .zip(&self.items)
                .map(|(card, items)| HistoryBlock {
                    card,
                    items,
                    complete: true,
                    origin: None,
                    changed: false,
                })
                .collect(),
        )
    }
}

fn config() -> RecallConfig {
    RecallConfig {
        view: ViewLimits {
            recent_turns: 1,
            ..ViewLimits::default()
        },
        read_over_tokens: 100,
        ..RecallConfig::default()
    }
}

fn shown(recall: &Recall) -> Vec<(&str, Presentation, Reason)> {
    recall
        .view
        .entries
        .iter()
        .map(|entry| {
            (
                entry.block_id.as_str(),
                entry.presentation.clone(),
                entry.reason,
            )
        })
        .collect()
}

const SCREEN: &str = r#"{"selected": [{"card": "C2", "detail": "full", "reason": "the log"}],
    "uncertain": ["C3"]}"#;

#[tokio::test]
async fn reading_decides_uncertain_and_large_picks() {
    let session = Session::new();
    let history = session.history();
    // Uncertain blocks are read first, then large picks: R1 is a1 and R2 is l1.
    let model = ScriptedModel::new(
        Some(SCREEN),
        Some(
            r#"{"blocks": [
                {"block": "R1", "decision": "excerpt", "lines": [{"from": 1, "to": 2}], "reason": "the fix"},
                {"block": "R2", "decision": "drop", "lines": [], "reason": "not needed"}
            ]}"#,
        ),
    );
    let recaller = Recaller::new(model.clone(), Some(model.clone()), config());
    let recall = recaller
        .recall(&history, "Does it handle nested comments?")
        .await
        .unwrap_or_else(|err| panic!("recall: {err}"));

    assert_eq!(
        shown(&recall),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            (
                "a1",
                Presentation::Excerpt {
                    lines: vec![LineRange { from: 1, to: 2 }],
                },
                Reason::Selected,
            ),
            ("u2", Presentation::Full, Reason::Recent),
        ]
    );
    assert_eq!(
        model.prompts(),
        vec![
            "context_selection".to_string(),
            "context_reading".to_string()
        ]
    );
    let stats = &recall.stats;
    assert_eq!(
        (stats.candidates, stats.picked, stats.uncertain, stats.read),
        (4, 1, 1, 2)
    );
    assert_eq!(stats.reads.len(), 1);
    assert_eq!(
        stats.screen.as_ref().map(|call| call.input_tokens),
        Some(1_000)
    );
}

#[tokio::test]
async fn without_a_reader_uncertain_blocks_are_summarized() {
    let session = Session::new();
    let history = session.history();
    let model = ScriptedModel::new(Some(SCREEN), None);
    let recall = Recaller::new(model.clone(), None, config())
        .recall(&history, "q")
        .await
        .unwrap_or_else(|err| panic!("recall: {err}"));
    assert_eq!(
        shown(&recall)[1..3],
        [
            ("l1", Presentation::Full, Reason::Selected),
            ("a1", Presentation::Summary, Reason::Selected),
        ]
    );
}

#[tokio::test]
async fn a_failed_reading_keeps_the_screened_picks() {
    let session = Session::new();
    let history = session.history();
    let screen = ScriptedModel::new(Some(SCREEN), None);
    let reader = ScriptedModel::new(None, None);
    let recall = Recaller::new(screen, Some(reader), config())
        .recall(&history, "q")
        .await
        .unwrap_or_else(|err| panic!("recall: {err}"));
    assert_eq!(
        shown(&recall)[1],
        ("l1", Presentation::Full, Reason::Selected)
    );
    assert_eq!(recall.view.entries.len(), 3);
    assert!(recall.stats.reads[0].error.is_some());
}

#[tokio::test]
async fn a_failed_screening_fails_the_recall() {
    let session = Session::new();
    let history = session.history();
    let model = ScriptedModel::new(None, None);
    assert!(
        Recaller::new(model, None, config())
            .recall(&history, "q")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn views_are_recorded_with_their_stats() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    let store = LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let session = Session::new();
    let history = session.history();
    let recall = Recaller::new(ScriptedModel::new(Some(SCREEN), None), None, config())
        .recall(&history, "q")
        .await
        .unwrap_or_else(|err| panic!("recall: {err}"));

    store
        .record_view(
            "graph-1",
            &BranchId::new("br_main"),
            "q",
            "scripted",
            &recall.view,
            &recall.stats,
        )
        .await
        .unwrap_or_else(|err| panic!("record: {err}"));
    let views = store
        .load_views("graph-1")
        .await
        .unwrap_or_else(|err| panic!("load: {err}"));
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].stats, recall.stats);
    let ids: Vec<&str> = views[0]
        .view
        .entries
        .iter()
        .map(|entry| entry.block_id.as_str())
        .collect();
    assert_eq!(ids, vec!["u1", "l1", "a1", "u2"]);
}

/// Answers each kind of prompt with fixed JSON, under its own name.
struct FixedModel {
    name: &'static str,
    answers: Vec<(&'static str, &'static str)>,
}

impl ShadowModel for FixedModel {
    fn name(&self) -> String {
        self.name.to_string()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            let Some((_, answer)) = self
                .answers
                .iter()
                .find(|(schema, _)| *schema == prompt.schema_name)
            else {
                anyhow::bail!("no answer for {}", prompt.schema_name);
            };
            Ok(ModelCompletion {
                text: answer.to_string(),
                ..ModelCompletion::default()
            })
        })
    }
}

/// Scores the cards in order with fixed scores.
struct FixedScorer(Vec<f32>);

impl RelevanceScorer for FixedScorer {
    fn name(&self) -> String {
        "fixed".to_string()
    }

    fn score<'a>(
        &'a self,
        _question: &'a str,
        documents: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<f32>>> {
        Box::pin(async move { Ok(self.0.iter().copied().take(documents.len()).collect()) })
    }
}

fn no_reading() -> RecallConfig {
    RecallConfig {
        read_over_tokens: u32::MAX,
        ..config()
    }
}

#[tokio::test]
async fn an_unsure_screening_is_redone_by_the_stronger_model() {
    let session = Session::new();
    let history = session.history();
    let first = Arc::new(FixedModel {
        name: "small",
        answers: vec![(
            "context_selection",
            r#"{"selected": [{"card": "C2", "detail": "summary", "reason": ""}],
                "uncertain": [], "confidence": "low"}"#,
        )],
    });
    let strong = Arc::new(FixedModel {
        name: "strong",
        answers: vec![(
            "context_selection",
            r#"{"selected": [{"card": "C3", "detail": "full", "reason": "the fix"}],
                "uncertain": [], "confidence": "high"}"#,
        )],
    });
    let recall = Recaller::new(first, None, no_reading())
        .with_escalation(Some(strong))
        .recall(&history, "nested comments?")
        .await
        .unwrap_or_else(|err| panic!("recall: {err:#}"));
    assert_eq!(
        recall
            .stats
            .escalation
            .as_ref()
            .map(|call| call.model.as_str()),
        Some("strong")
    );
    assert!(shown(&recall).contains(&("a1", Presentation::Full, Reason::Selected)));
    assert!(!shown(&recall).iter().any(|entry| entry.0 == "l1"));

    // A sure screening keeps its answer.
    let sure = Arc::new(FixedModel {
        name: "small",
        answers: vec![(
            "context_selection",
            r#"{"selected": [{"card": "C2", "detail": "summary", "reason": ""}],
                "uncertain": [], "confidence": "high"}"#,
        )],
    });
    let strong: Arc<dyn ShadowModel> = Arc::new(FixedModel {
        name: "strong",
        answers: Vec::new(),
    });
    let recall = Recaller::new(sure, None, no_reading())
        .with_escalation(Some(strong))
        .recall(&history, "nested comments?")
        .await
        .unwrap_or_else(|err| panic!("recall: {err:#}"));
    assert_eq!(recall.stats.escalation, None);
    assert!(shown(&recall).contains(&("l1", Presentation::Summary, Reason::Selected)));
}

#[tokio::test]
async fn pointwise_screening_picks_by_score_without_a_listwise_model() {
    let session = Session::new();
    let history = session.history();
    // The candidates are u1, l1 and a1; u2 is the recent turn.
    let unused: Arc<dyn ShadowModel> = Arc::new(FixedModel {
        name: "unused",
        answers: Vec::new(),
    });
    let recall = Recaller::new(
        unused,
        None,
        RecallConfig {
            strategy: ScreenStrategy::Pointwise,
            ..no_reading()
        },
    )
    .with_scorer(Some(Arc::new(FixedScorer(vec![0.1, 0.6, 0.9]))))
    .recall(&history, "nested comments?")
    .await
    .unwrap_or_else(|err| panic!("recall: {err:#}"));
    assert_eq!(recall.stats.scorer.as_deref(), Some("fixed"));
    assert_eq!(recall.stats.scored, 3);
    let shown = shown(&recall);
    assert!(shown.contains(&("a1", Presentation::Full, Reason::Selected)));
    assert!(shown.contains(&("l1", Presentation::Summary, Reason::Selected)));
}

#[tokio::test]
async fn a_cascade_screens_only_the_best_scored_cards() {
    let session = Session::new();
    let history = session.history();
    let model = Arc::new(FixedModel {
        name: "screen",
        answers: vec![(
            "context_selection",
            r#"{"selected": [{"card": "C1", "detail": "full", "reason": ""}],
                "uncertain": [], "confidence": "high"}"#,
        )],
    });
    let recall = Recaller::new(
        model,
        None,
        RecallConfig {
            strategy: ScreenStrategy::Cascade,
            cascade_candidates: 1,
            ..no_reading()
        },
    )
    .with_scorer(Some(Arc::new(FixedScorer(vec![0.1, 0.2, 0.9]))))
    .recall(&history, "nested comments?")
    .await
    .unwrap_or_else(|err| panic!("recall: {err:#}"));
    // C1 of the narrowed catalog is a1, the best scored card.
    assert_eq!(recall.stats.candidates, 1);
    assert!(shown(&recall).contains(&("a1", Presentation::Full, Reason::Selected)));
}

/// The session, with the long log's card naming the parser log.
fn session_with_log_title() -> Session {
    let mut session = Session::new();
    session.cards[1].title = "parser log of the failing run".to_string();
    session
}

#[tokio::test]
async fn lexical_matches_back_up_the_screening() {
    let session = session_with_log_title();
    let history = session.history();
    let model = ScriptedModel::new(
        Some(
            r#"{"selected": [{"card": "C3", "detail": "full", "reason": "the fix"}], "uncertain": []}"#,
        ),
        None,
    );
    let recall = Recaller::new(model.clone(), None, config())
        .recall(&history, "What did the parser log say?")
        .await
        .unwrap_or_else(|err| panic!("recall: {err}"));
    assert_eq!(
        shown(&recall),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            ("l1", Presentation::Summary, Reason::Lexical),
            ("a1", Presentation::Full, Reason::Selected),
            ("u2", Presentation::Full, Reason::Recent),
        ]
    );
    assert_eq!(recall.stats.lexical, 1);

    let without = Recaller::new(
        model,
        None,
        RecallConfig {
            max_matches: 0,
            ..config()
        },
    )
    .recall(&history, "What did the parser log say?")
    .await
    .unwrap_or_else(|err| panic!("recall: {err}"));
    assert!(!without.view.shows(1));
    assert_eq!(without.stats.lexical, 0);
}

#[tokio::test]
async fn blocks_a_reader_left_out_are_not_matched_back_in() {
    let session = session_with_log_title();
    let history = session.history();
    // R1 is the uncertain a1, R2 the large picked l1, which the reader drops.
    let model = ScriptedModel::new(
        Some(SCREEN),
        Some(
            r#"{"blocks": [
                {"block": "R1", "decision": "full", "lines": [], "reason": "the fix"},
                {"block": "R2", "decision": "drop", "lines": [], "reason": "noise"}
            ]}"#,
        ),
    );
    let selection = Recaller::new(model.clone(), Some(model), config())
        .select(&history, "What did the parser log say?")
        .await
        .unwrap_or_else(|err| panic!("select: {err}"));
    assert_eq!(selection.question, "What did the parser log say?");
    assert_eq!(
        selection
            .picks
            .iter()
            .map(|pick| pick.block_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a1"]
    );
    assert!(!selection.shows(&BlockId::from_stored("l1")));
}

#[test]
fn a_selection_plans_a_view_of_the_history_as_it_grew() {
    let selection = Selection {
        question: "q".to_string(),
        picks: vec![
            SelectedBlock {
                block_id: BlockId::from_stored("a1"),
                presentation: Presentation::Full,
                note: None,
            },
            SelectedBlock {
                block_id: BlockId::from_stored("gone"),
                presentation: Presentation::Full,
                note: None,
            },
        ],
        matches: vec![BlockId::from_stored("l1")],
        stats: RecallStats::default(),
    };
    // A turn was recorded after the selection was made.
    let mut session = Session::new();
    session.cards.push(card("u3", BlockKind::UserMessage, "t3"));
    session
        .items
        .push(vec![message("user", "And block comments?")]);
    let history = session.history();

    let recall = selection.plan(&history, config().view);
    assert_eq!(
        shown(&recall),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            ("l1", Presentation::Summary, Reason::Lexical),
            ("a1", Presentation::Full, Reason::Selected),
            ("u3", Presentation::Full, Reason::Recent),
        ]
    );
    assert_eq!(recall.stats.lexical, 1);
}
