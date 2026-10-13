use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::assistant;
use crate::test_support::custom_call;
use crate::test_support::custom_output;
use crate::test_support::developer;
use crate::test_support::exec;
use crate::test_support::output_text;
use crate::test_support::user;

fn tool_facts(items: &[ResponseItem]) -> BlockFacts {
    extract_facts(BlockKind::ToolExchange, items, Some("/repo"))
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn shell_reads_follow_cd_and_skip_flag_values() {
    let facts = tool_facts(&[
        exec(
            "fc_1",
            "call_1",
            r#"{"cmd":"cd core && sed -n '1,80p' src/lib.rs | head -n 20 && rg -n -C 8 install_live_recorder ../store/live.rs","workdir":"/repo"}"#,
        ),
        output_text(
            "fco_1",
            "call_1",
            "Chunk ID: 1\nProcess exited with code 0\nOutput:\nfn main() {}",
        ),
    ]);

    assert_eq!(
        facts.entities.files_read,
        strings(&["/repo/core/src/lib.rs", "/repo/store/live.rs"])
    );
    assert_eq!(facts.entities.symbols, strings(&["install_live_recorder"]));
    assert_eq!(facts.tools, strings(&["exec_command"]));
    assert_eq!(facts.exit_code, Some(0));
    assert_eq!(facts.outcome, Some(Outcome::Success));
    assert!(facts.entities.errors.is_empty());
}

#[test]
fn failing_commands_report_their_errors() {
    let facts = tool_facts(&[
        exec("fc_1", "call_1", r#"{"cmd":"cargo test -p codex-tui"}"#),
        output_text(
            "fco_1",
            "call_1",
            "Process exited with code 101\nOutput:\n   Compiling codex-tui\nerror[E0599]: no method named `reject_server_request` found\n  --> tui/src/app.rs:529:40\n",
        ),
    ]);

    assert_eq!(facts.exit_code, Some(101));
    assert_eq!(facts.outcome, Some(Outcome::Failure));
    assert_eq!(
        facts.entities.errors,
        strings(&["error[E0599]: no method named `reject_server_request` found"])
    );
    assert_eq!(
        facts.entities.commands,
        strings(&["cargo test -p codex-tui"])
    );
}

#[test]
fn successful_output_is_not_scanned_for_errors() {
    let facts = tool_facts(&[
        exec("fc_1", "call_1", r#"{"cmd":"cat notes.md"}"#),
        output_text(
            "fco_1",
            "call_1",
            "Process exited with code 0\nOutput:\nerror: this line is file content",
        ),
    ]);
    assert!(facts.entities.errors.is_empty());
}

#[test]
fn redirects_and_in_place_edits_are_writes() {
    let facts = tool_facts(&[exec(
        "fc_1",
        "call_1",
        r#"{"cmd":"echo hi > out/log.txt 2>/dev/null; sed -i '' 's/a/b/' src/a.rs; cp src/a.rs src/b.rs"}"#,
    )]);
    assert_eq!(
        facts.entities.files_written,
        strings(&["/repo/out/log.txt", "/repo/src/a.rs", "/repo/src/b.rs"])
    );
    assert_eq!(facts.entities.files_read, strings(&["/repo/src/a.rs"]));
}

#[test]
fn heredoc_bodies_are_not_parsed_as_commands() {
    let facts = tool_facts(&[exec(
        "fc_1",
        "call_1",
        r#"{"cmd":"cat > notes/plan.md <<'EOF'\nsee src/lib.rs | rm -rf build\nEOF"}"#,
    )]);
    assert_eq!(
        facts.entities.files_written,
        strings(&["/repo/notes/plan.md"])
    );
    assert!(facts.entities.files_read.is_empty());
}

#[test]
fn patches_write_files_and_name_definitions() {
    let patch = "*** Begin Patch\n*** Update File: src/graph.rs\n@@ impl Graph\n+    pub fn merge_base(&self) {}\n*** Add File: /repo/src/card.rs\n+struct BlockCard;\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n*** End Patch";
    let facts = tool_facts(&[
        custom_call("ctc_1", "call_1", "apply_patch", patch),
        custom_output(
            "ctco_1",
            "call_1",
            r#"{"output":"Success. Updated the following files:\nM src/graph.rs\n","metadata":{"exit_code":0}}"#,
        ),
    ]);
    assert_eq!(
        facts.entities.files_written,
        strings(&[
            "/repo/src/graph.rs",
            "/repo/src/card.rs",
            "/repo/src/old.rs",
            "/repo/src/new.rs",
        ])
    );
    assert_eq!(
        facts.entities.symbols,
        strings(&["merge_base", "BlockCard"])
    );
    assert_eq!(facts.outcome, Some(Outcome::Success));
}

#[test]
fn code_mode_scripts_pair_commands_with_their_workdir() {
    let script = r#"const results = await Promise.all([
  tools.exec_command({
    cmd: "sed -n '1,40p' paper.tex",
    workdir: "/papers/a",
  }),
  tools.exec_command({
    workdir: "/papers/b",
    cmd: "cat refs.bib",
  }),
]);
const patch = "*** Begin Patch\n*** Update File: /papers/a/paper.tex\n@@\n-old\n+new\n*** End Patch";
await tools.apply_patch(patch);"#;
    let facts = tool_facts(&[custom_call("ctc_1", "call_1", "exec", script)]);
    assert_eq!(
        facts.entities.files_read,
        strings(&["/papers/a/paper.tex", "/papers/b/refs.bib"])
    );
    assert_eq!(
        facts.entities.files_written,
        strings(&["/papers/a/paper.tex"])
    );
}

#[test]
fn messages_contribute_quoted_paths_symbols_and_urls() {
    let facts = extract_facts(
        BlockKind::UserMessage,
        &[user(
            "msg_1",
            "Keep `codex-rs/core/src/lib.rs:12` and `install_live_recorder()` as they are, and do not add `retries`. See https://example.com/spec.",
        )],
        Some("/repo"),
    );
    assert_eq!(
        facts.entities.files_mentioned,
        strings(&["/repo/codex-rs/core/src/lib.rs"])
    );
    assert_eq!(facts.entities.symbols, strings(&["install_live_recorder"]));
    assert_eq!(facts.entities.urls, strings(&["https://example.com/spec"]));
}

#[test]
fn user_messages_keep_pasted_errors_but_assistant_messages_do_not() {
    let pasted = "it fails with\nTypeError: cannot read properties of undefined";
    let from_user = extract_facts(BlockKind::UserMessage, &[user("msg_1", pasted)], None);
    let from_assistant = extract_facts(
        BlockKind::AssistantMessage,
        &[assistant("msg_2", pasted)],
        None,
    );
    assert_eq!(
        from_user.entities.errors,
        strings(&["TypeError: cannot read properties of undefined"])
    );
    assert!(from_assistant.entities.errors.is_empty());
}

#[test]
fn harness_context_only_gets_a_token_count() {
    let facts = extract_facts(
        BlockKind::ContextFragment,
        &[developer(
            "msg_1",
            "# AGENTS.md instructions for /repo\nRun `cargo test` in src/",
        )],
        None,
    );
    assert!(facts.entities.is_empty());
    assert!(facts.tokens > 0);
}
