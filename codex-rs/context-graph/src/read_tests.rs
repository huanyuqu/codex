use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::retry_session;

#[test]
fn blocks_are_numbered_by_line_and_split_by_size() {
    let session = retry_session();
    let history = session.history();
    let t3 = session.index("t3");
    let a3 = session.index("a3");
    let prompts = build_read_prompts(
        &history,
        "Why did the test fail?",
        &[t3, a3],
        ReadLimits::default(),
    );
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    assert_eq!(prompt.blocks, vec![t3, a3]);
    assert_eq!(prompt.line_counts, vec![9, 2]);
    assert!(prompt.prompt.input.contains(
        "### R1 [tool_exchange] turn 3: $ cargo test\n   1| [tool call exec_command]\n   2| \
         {\"cmd\":\"cargo test\"}\n   3| [tool output]\n   4| running 4 tests\n"
    ));
    assert!(
        prompt
            .prompt
            .input
            .ends_with("## Request\nWhy did the test fail?\n")
    );

    let split = build_read_prompts(
        &history,
        "q",
        &[t3, a3],
        ReadLimits {
            max_prompt_tokens: 10,
        },
    );
    let blocks: Vec<Vec<usize>> = split.into_iter().map(|prompt| prompt.blocks).collect();
    assert_eq!(blocks, vec![vec![t3], vec![a3]]);
}

#[test]
fn answers_clamp_and_merge_excerpt_lines() {
    let session = retry_session();
    let history = session.history();
    let (t3, a3, e2) = (
        session.index("t3"),
        session.index("a3"),
        session.index("e2"),
    );
    let prompts = build_read_prompts(&history, "q", &[t3, a3, e2], ReadLimits::default());
    let answer = parse_read_answer(
        &prompts[0],
        r#"{"blocks": [
            {"block": "R1", "decision": "excerpt", "lines": [{"from": 6, "to": 7}, {"from": 0, "to": 2}, {"from": 7, "to": 40}], "reason": "the failure"},
            {"block": "R2", "decision": "excerpt", "lines": [{"from": 9, "to": 3}], "reason": ""},
            {"block": "R1", "decision": "drop", "lines": [], "reason": "repeat"}
        ]}"#,
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));

    assert_eq!(
        answer.decisions,
        vec![
            ReadDecision {
                index: t3,
                presentation: Some(Presentation::Excerpt {
                    lines: vec![LineRange { from: 1, to: 2 }, LineRange { from: 6, to: 9 },],
                }),
                note: Some("the failure".to_string()),
            },
            ReadDecision {
                index: a3,
                presentation: Some(Presentation::Summary),
                note: None,
            },
        ]
    );
    assert_eq!(answer.missing, vec![e2]);
}
