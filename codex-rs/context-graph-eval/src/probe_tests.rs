use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::test_support::Session;

#[test]
fn generation_lists_blocks_by_turn_and_resolves_labels() {
    let session = Session::new();
    let history = session.history();
    let prompt = build_generation_prompt(&history, 3);
    assert_eq!(prompt.blocks, vec![0, 1, 2, 3, 4, 5]);
    assert!(prompt.prompt.input.starts_with(
        "## Session\n\n### Turn 1\n\n#### B1 [user_message]\n[user]\nAdd a cache to the resolver.\n"
    ));
    assert!(
        prompt
            .prompt
            .input
            .ends_with("## Task\nWrite 3 questions. The last turn is turn 3.\n")
    );

    let probes = parse_generated_probes(
        &prompt,
        &history,
        r#"{"probes": [
            {"category": "superseded", "question": "How large is the cache?", "answer": "512 entries",
             "key_facts": ["512 entries"], "support": ["B4", "B4", "B9"], "outdated": ["B2"]},
            {"category": "trivia", "question": "q", "answer": "a", "key_facts": [], "support": ["B1"], "outdated": []},
            {"category": "lexical", "question": "q", "answer": "a", "key_facts": [], "support": ["B9"], "outdated": []}
        ]}"#,
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(
        probes,
        vec![GeneratedProbeFields {
            category: "superseded".to_string(),
            question: "How large is the cache?".to_string(),
            answer: "512 entries".to_string(),
            key_facts: vec!["512 entries".to_string()],
            support: vec![BlockId::from_stored("a2")],
            outdated: vec![BlockId::from_stored("a1")],
        }]
    );
}

#[test]
fn json_lines_round_trip_and_a_missing_file_is_empty() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let path = dir.path().join("probes.jsonl");
    assert_eq!(
        read_jsonl::<Probe>(&path).unwrap_or_else(|err| panic!("read: {err}")),
        Vec::new()
    );
    let probe = Probe {
        id: "g#3#0".to_string(),
        rollout: PathBuf::from("rollout.jsonl"),
        graph_id: "g".to_string(),
        turn: 3,
        category: "local".to_string(),
        question: "q".to_string(),
        answer: "a".to_string(),
        key_facts: Vec::new(),
        support: vec![BlockId::from_stored("a3")],
        outdated: Vec::new(),
        generator: "model".to_string(),
    };
    append_jsonl(&path, &probe).unwrap_or_else(|err| panic!("append: {err}"));
    append_jsonl(&path, &probe).unwrap_or_else(|err| panic!("append: {err}"));
    assert_eq!(
        read_jsonl::<Probe>(&path).unwrap_or_else(|err| panic!("read: {err}")),
        vec![probe.clone(), probe]
    );
}

#[test]
fn probe_points_are_turns_at_shares_of_the_session() {
    assert_eq!(turn_at(/*last_turn*/ 10, 0.5), 5);
    assert_eq!(turn_at(/*last_turn*/ 9, 0.5), 5);
    assert_eq!(turn_at(/*last_turn*/ 10, 1.0), 10);
    // Out of range shares are clamped, and a session always has its first turn.
    assert_eq!(turn_at(/*last_turn*/ 10, 0.0), 1);
    assert_eq!(turn_at(/*last_turn*/ 10, 2.0), 10);
    assert_eq!(turn_at(/*last_turn*/ 0, 0.5), 1);
}
