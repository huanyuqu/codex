use pretty_assertions::assert_eq;

use super::*;

fn exit(output: &str) -> BackgroundCommandExit {
    BackgroundCommandExit {
        process_id: 7,
        command: "cargo test".to_string(),
        exit_code: Some(0),
        failure: None,
        duration: Duration::from_millis(12_340),
        output: output.to_string(),
    }
}

#[test]
fn notices_say_how_the_command_ended_with_the_end_of_its_output() {
    assert_eq!(
        notice_text(&exit("test result: ok\n")),
        "<background_command_exit session_id=\"7\">\nThe command `cargo test` you left running \
         in the background exited with code 0 after 12.3s. Its session is closed. The end of its \
         output:\ntest result: ok\n</background_command_exit>"
    );
    let failed = BackgroundCommandExit {
        failure: Some("killed by the sandbox".to_string()),
        ..exit("")
    };
    assert!(notice_text(&failed).contains("background failed: killed by the sandbox"));
    assert!(notice_text(&failed).contains("output:\n(no output)\n"));

    let long = "x".repeat(NOTICE_OUTPUT_CHARS + 10);
    let text = notice_text(&exit(&long));
    assert!(text.contains("[... 10 earlier characters omitted ...]"));
}
