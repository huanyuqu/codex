use pretty_assertions::assert_eq;

use super::*;
use crate::Block;
use crate::BlockKind;
use crate::extract::extract_facts;
use crate::test_support::exec;
use crate::test_support::output_text;
use crate::test_support::user;

fn card(id: &str, kind: BlockKind, items: &[ResponseItem]) -> BlockCard {
    let block = Block {
        id: BlockId::from_stored(id),
        kind,
        item_keys: vec![id.to_string()],
        turn_id: None,
        first_ordinal: 0,
        last_ordinal: 0,
        approx_bytes: 0,
        complete: true,
    };
    BlockCard::extracted(&block, extract_facts(kind, items, Some("/repo")), items)
}

struct Fixture {
    earlier: BlockCard,
    request_items: Vec<ResponseItem>,
    request: BlockCard,
    test_items: Vec<ResponseItem>,
    test_run: BlockCard,
}

fn fixture() -> Fixture {
    let earlier = card(
        "blk_0",
        BlockKind::UserMessage,
        &[user("msg_0", "use sqlite")],
    );
    let request_items = vec![user("msg_1", "add retries to the client")];
    let request = card("blk_1", BlockKind::UserMessage, &request_items);
    let test_items = vec![
        exec("fc_1", "call_1", r#"{"cmd":"cargo test -p client"}"#),
        output_text("fco_1", "call_1", "Process exited with code 0\n12 passed"),
    ];
    let test_run = card("blk_2", BlockKind::ToolExchange, &test_items);
    Fixture {
        earlier,
        request_items,
        request,
        test_items,
        test_run,
    }
}

fn prompt(fixture: &Fixture) -> CardPrompt {
    build_card_prompt(
        &[
            (&fixture.request, &fixture.request_items),
            (&fixture.test_run, &fixture.test_items),
        ],
        &[&fixture.earlier],
        PromptLimits::default(),
    )
}

#[test]
fn prompts_list_the_catalog_then_the_batch() {
    let fixture = fixture();
    let prompt = prompt(&fixture);
    assert_eq!(
        prompt.prompt.input,
        "## Earlier blocks\nE1 [user_message] use sqlite\n\n## Blocks to index\n\n### B1 [user_message]\n[user]\nadd retries to the client\n\n### B2 [tool_exchange]\nfacts: exit code 0\n[tool call exec_command]\n{\"cmd\":\"cargo test -p client\"}\n[tool output]\nProcess exited with code 0\n12 passed\n"
    );
    assert_eq!(
        prompt.batch,
        vec![BlockId::from_stored("blk_1"), BlockId::from_stored("blk_2")]
    );
    assert_eq!(prompt.linkable.len(), 3);
    assert_eq!(prompt.labels, vec!["E1", "B1", "B2"]);
    assert!(prompt.prompt.instructions.contains("user_constraint"));
    assert_eq!(
        prompt.prompt.schema["properties"]["cards"]["items"]["required"],
        json!([
            "block_id",
            "title",
            "summary",
            "claims",
            "depends_on",
            "supersedes"
        ])
    );
}

#[test]
fn answers_are_validated_against_the_batch() {
    let fixture = fixture();
    let prompt = prompt(&fixture);
    let answer = r#"```json
{"cards": [
  {"block_id": "B2", "title": "Run client tests", "summary": "All 12 tests pass.",
   "claims": [{"kind": "finding", "text": "client tests pass with retries"},
              {"kind": "rumor", "text": "dropped"}],
   "depends_on": ["B1", "E9", "blk_1"], "supersedes": []},
  {"block_id": " B1 ", "title": "Add retries", "summary": "User asks for retries.",
   "claims": [{"kind": "user_constraint", "text": " retries in the client "}],
   "depends_on": ["B2", "E1"], "supersedes": ["blk_0"]}
]}
```"#;
    let answer = parse_card_answer(&prompt, answer).unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(
        answer,
        CardAnswer {
            cards: vec![
                ModelCard {
                    block_id: BlockId::from_stored("blk_1"),
                    title: "Add retries".to_string(),
                    summary: "User asks for retries.".to_string(),
                    claims: vec![Claim {
                        kind: ClaimKind::UserConstraint,
                        text: "retries in the client".to_string(),
                    }],
                    // B2 comes later in the batch, so it cannot be a dependency.
                    depends_on: vec![BlockId::from_stored("blk_0")],
                    supersedes: vec![BlockId::from_stored("blk_0")],
                },
                ModelCard {
                    block_id: BlockId::from_stored("blk_2"),
                    title: "Run client tests".to_string(),
                    summary: "All 12 tests pass.".to_string(),
                    claims: vec![Claim {
                        kind: ClaimKind::Finding,
                        text: "client tests pass with retries".to_string(),
                    }],
                    depends_on: vec![BlockId::from_stored("blk_1")],
                    supersedes: Vec::new(),
                },
            ],
            missing: Vec::new(),
        }
    );
}

#[test]
fn answers_must_be_json_and_may_miss_blocks() {
    let fixture = fixture();
    let prompt = prompt(&fixture);
    assert!(matches!(
        parse_card_answer(&prompt, "not json"),
        Err(CardParseError::Json { .. })
    ));
    assert_eq!(
        parse_card_answer(&prompt, r#"{"cards": []}"#),
        Err(CardParseError::Empty)
    );
    let partial = parse_card_answer(
        &prompt,
        r#"{"cards": [{"block_id": "B1", "title": "t", "summary": "s", "claims": [], "depends_on": [], "supersedes": []}]}"#,
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(partial.cards.len(), 1);
    assert_eq!(partial.missing, vec![BlockId::from_stored("blk_2")]);
}

#[test]
fn model_cards_keep_extracted_facts_and_links() {
    let fixture = fixture();
    let mut card = fixture.test_run;
    card.link(Relation::Supersedes, [BlockId::from_stored("blk_old")]);
    ModelCard {
        block_id: card.block_id.clone(),
        title: "Run client tests".to_string(),
        summary: String::new(),
        claims: Vec::new(),
        depends_on: vec![BlockId::from_stored("blk_1")],
        supersedes: vec![BlockId::from_stored("blk_0")],
    }
    .apply_to(&mut card, "gpt-test");

    assert_eq!(card.title, "Run client tests");
    assert_eq!(card.summary, "exit 0");
    assert_eq!(card.indexer, "gpt-test");
    assert_eq!(card.exit_code, Some(0));
    assert_eq!(
        card.supersedes,
        vec![
            BlockId::from_stored("blk_old"),
            BlockId::from_stored("blk_0")
        ]
    );
    assert_eq!(card.depends_on, vec![BlockId::from_stored("blk_1")]);
}
