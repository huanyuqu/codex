use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::retry_session;

#[test]
fn candidates_leave_out_the_recent_turns_and_harness_context() {
    let session = retry_session();
    let history = session.history();
    let candidates = score_candidates(&history, history.recent_start(1));
    assert_eq!(candidates.first(), Some(&session.index("u1")));
    assert!(!candidates.contains(&session.index("u4")));
    let documents = candidate_documents(&history, &candidates);
    assert_eq!(documents.len(), candidates.len());
    assert!(documents[0].starts_with("[user] add retries to the HTTP client"));

    let prompt = score_prompt("why did the test fail", &documents[..2]);
    assert!(prompt.input.starts_with("## Cards\nC1 [user] add retries"));
    assert!(
        prompt
            .input
            .ends_with("## Request\nwhy did the test fail\n")
    );
}

#[test]
fn scores_become_picks_and_blocks_to_read() {
    assert_eq!(
        parse_scores(
            r#"{"scores": [{"card": "C2", "needed": 0.9}, {"card": "C9", "needed": 1}, {"card": "c1", "needed": 7}]}"#,
            3
        ),
        Ok(vec![1.0, 0.9, 0.0])
    );
    let candidates = [10, 11, 12, 13];
    let (picks, uncertain) = picks_from_scores(&candidates, &[0.6, 0.95, 0.1, 0.4], 5);
    assert_eq!(
        picks
            .iter()
            .map(|pick| (pick.index, pick.presentation.clone()))
            .collect::<Vec<_>>(),
        vec![(11, Presentation::Full), (10, Presentation::Summary)]
    );
    assert_eq!(uncertain, vec![13]);
    assert_eq!(
        top_candidates(&candidates, &[0.6, 0.95, 0.1, 0.4], 2),
        vec![10, 11]
    );
}
