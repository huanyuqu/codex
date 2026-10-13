use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::exec;
use crate::test_support::output_text;
use crate::test_support::reasoning;
use crate::test_support::user;

#[test]
fn short_text_is_kept_whole() {
    assert_eq!(head_and_tail("short", 100), "short");
}

#[test]
fn long_text_keeps_its_head_and_tail() {
    let text: String = (0..1000)
        .map(|n| char::from(b'a' + (n % 26) as u8))
        .collect();
    let kept = head_and_tail(&text, 200);
    assert!(kept.chars().count() <= 200);
    assert!(kept.starts_with(&text[..96]));
    assert!(kept.ends_with(&text[1000 - 64..]));
    assert!(kept.contains("chars omitted"));
}

#[test]
fn blocks_render_as_labelled_items() {
    let rendered = render_block(
        &[
            reasoning("rs_1"),
            exec("fc_1", "call_1", r#"{"cmd":"ls"}"#),
            output_text("fco_1", "call_1", "a.rs\nb.rs"),
        ],
        1_000,
        10_000,
    );
    assert_eq!(
        rendered,
        "[tool call exec_command]\n{\"cmd\":\"ls\"}\n[tool output]\na.rs\nb.rs\n"
    );
}

#[test]
fn rendering_stops_at_the_total_budget() {
    let long = "x".repeat(500);
    let items = vec![
        user("msg_1", &long),
        user("msg_2", &long),
        user("msg_3", &long),
    ];
    let rendered = render_block(&items, 1_000, 700);
    assert!(rendered.chars().count() <= 760);
    assert!(rendered.ends_with("[... more items omitted]\n"));
}

#[test]
fn truncation_marks_cut_text() {
    assert_eq!(truncate("abcdef", 4), "abc…");
    assert_eq!(first_line("\n  title line \nbody", 20), "title line");
}
