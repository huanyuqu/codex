# Background commands

A long command, such as a build, a test suite or a dev server, does not have to hold up the
conversation. It can run in the background while the model and you go on, and the model is told
when it exits instead of polling it.

## How a command gets to the background

- **The model starts it there.** `exec_command` takes `background: true`. The call returns after
  about a second with the command's session ID, and the command runs on.
- **You move it there.** While the turn is waiting on a command, press `Ctrl+B` (the status line
  says so). The waiting call returns at once with the output so far, the model learns that you
  moved the command to the background, and the turn goes on. At other times `Ctrl+B` moves the
  cursor left as usual. The key is the `chat.background_commands` action in `tui.keymap`.

Commands started the ordinary way behave as before: when one is still running after the call's
wait, the model gets its session ID and decides whether to poll it with `write_stdin`.

## When it exits

The model gets a message with the command, how it ended (its exit code, or why it failed), how
long it ran and the end of its output:

```text
<background_command_exit session_id="3">
The command `cargo test` you left running in the background exited with code 0 after 84.2s.
Its session is closed. The end of its output:
test result: ok. 412 passed; 0 failed
</background_command_exit>
```

- If a turn is running, the message joins it at its next model request.
- If the session is idle, a turn starts so the model can react at once. Set
  `background_terminal_wake = false` in `config.toml` to have the message wait for your next
  turn instead.

No message is sent when the model saw the command exit itself, for example by polling it, or when
the command was stopped with `/stop` or ended with the session.

`/ps` lists the commands running in the background, and `/stop` stops them.

## Not in `codex exec` or subagents

`codex exec` ends with its turn and stops the commands still running, so `exec_command` does not
offer `background` there. Subagents and internal sessions, such as Guardian reviews, answer
within their turn and do not get it either.

## App server

`thread/backgroundTerminals/detach` moves the commands a thread's turn is waiting on to the
background, as `Ctrl+B` does. `thread/backgroundTerminals/list`, `clean` and `terminate` list and
stop them.
