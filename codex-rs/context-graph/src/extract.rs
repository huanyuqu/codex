//! Facts about a block that can be read off its items without a model: the files it read or
//! wrote, the commands it ran, how they ended, and the errors, symbols and URLs it mentions.

use std::collections::HashSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;

use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::BlockKind;
use crate::text::content_text;
use crate::text::item_text;
use crate::text::output_text;
use crate::text::truncate;

const MAX_FILES: usize = 30;
const MAX_SYMBOLS: usize = 20;
const MAX_COMMANDS: usize = 10;
const MAX_ERRORS: usize = 5;
const MAX_URLS: usize = 10;
const MAX_COMMAND_CHARS: usize = 200;
const MAX_ERROR_CHARS: usize = 160;
/// Lines scanned for errors at each end of a long output.
const ERROR_SCAN_LINES: usize = 300;

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    /// The command was still running when its output was recorded.
    Partial,
}

/// Names a block mentions, grouped by what they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEntities {
    /// Files the block read, so later changes to them make the block stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_read: Vec<String>,
    /// Files the block created, edited, moved or deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_written: Vec<String>,
    /// Other paths the block mentions, such as a script it ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_mentioned: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
}

impl BlockEntities {
    pub fn is_empty(&self) -> bool {
        self.files_read.is_empty()
            && self.files_written.is_empty()
            && self.files_mentioned.is_empty()
            && self.symbols.is_empty()
            && self.commands.is_empty()
            && self.errors.is_empty()
            && self.urls.is_empty()
    }

    /// Every file the block touches or mentions, without duplicates.
    pub fn files(&self) -> impl Iterator<Item = &String> {
        let mut seen = HashSet::new();
        self.files_read
            .iter()
            .chain(&self.files_written)
            .chain(&self.files_mentioned)
            .filter(move |file| seen.insert(file.as_str()))
    }
}

/// Everything [`extract_facts`] learns about a block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFacts {
    pub entities: BlockEntities,
    /// Tools the block called, qualified by namespace.
    pub tools: Vec<String>,
    pub outcome: Option<Outcome>,
    /// The first nonzero exit code, or zero when every command succeeded.
    pub exit_code: Option<i32>,
    /// Rough token count of the block's readable text.
    pub tokens: u32,
}

/// Extracts the facts of one sealed block from its items. Relative paths are resolved against
/// the tool call's working directory, or `cwd` when the call does not name one.
///
/// Harness context and compaction blocks only get a token count: their files and commands
/// describe the environment, not the work.
pub fn extract_facts(kind: BlockKind, items: &[ResponseItem], cwd: Option<&str>) -> BlockFacts {
    let chars: usize = items
        .iter()
        .filter_map(item_text)
        .map(|text| text.text.chars().count())
        .sum();
    let tokens = u32::try_from(chars.div_ceil(4)).unwrap_or(u32::MAX);
    if matches!(kind, BlockKind::ContextFragment | BlockKind::Compaction) {
        return BlockFacts {
            tokens,
            ..BlockFacts::default()
        };
    }

    let mut collector = Collector {
        cwd: cwd.map(str::to_string),
        ..Collector::default()
    };
    for item in items {
        collector.item(item);
    }
    collector.finish(tokens)
}

#[derive(Default)]
struct Collector {
    cwd: Option<String>,
    files_read: Ordered,
    files_written: Ordered,
    files_mentioned: Ordered,
    symbols: Ordered,
    commands: Ordered,
    errors: Ordered,
    urls: Ordered,
    tools: Ordered,
    exit_codes: Vec<i32>,
    running: bool,
    failed: bool,
    succeeded: bool,
}

impl Collector {
    fn item(&mut self, item: &ResponseItem) {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text = content_text(content);
                self.prose(&text);
                if role == "user" {
                    self.errors_in(&text);
                }
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                ..
            } => {
                self.tool(namespace.as_deref(), name);
                self.function_call(name, arguments);
            }
            ResponseItem::CustomToolCall {
                name,
                namespace,
                input,
                ..
            } => {
                self.tool(namespace.as_deref(), name);
                self.custom_tool_call(name, input);
            }
            ResponseItem::LocalShellCall {
                action: LocalShellAction::Exec(exec),
                ..
            } => {
                self.tool(None, "shell");
                let cwd = self.cwd.clone();
                let workdir = exec.working_directory.as_deref().or(cwd.as_deref());
                self.shell(&exec.command.join(" "), workdir);
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => self.output(output),
            ResponseItem::WebSearchCall { action, .. } => {
                self.tool(None, "web_search");
                self.web_search(action.as_ref());
            }
            ResponseItem::AgentMessage { .. } => {
                if let Some(text) = item_text(item) {
                    self.prose(&text.text);
                }
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::AdditionalTools { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger {}
            | ResponseItem::Other => {}
        }
    }

    fn tool(&mut self, namespace: Option<&str>, name: &str) {
        let name = match namespace {
            Some(namespace) if !namespace.is_empty() => format!("{namespace}.{name}"),
            _ => name.to_string(),
        };
        self.tools.push(name, usize::MAX);
    }

    fn function_call(&mut self, name: &str, arguments: &str) {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let cwd = self.cwd.clone();
        let workdir = args
            .get("workdir")
            .and_then(Value::as_str)
            .or(cwd.as_deref());
        match name {
            "exec_command" | "shell" | "shell_command" | "container.exec" | "local_shell" => {
                match args.get("cmd").or_else(|| args.get("command")) {
                    Some(Value::String(command)) => self.shell(command, workdir),
                    Some(Value::Array(parts)) => {
                        let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                        // `["bash", "-lc", "<script>"]` runs the script.
                        let script = match parts.as_slice() {
                            [_, flag, script] if flag.starts_with('-') && flag.contains('c') => {
                                (*script).to_string()
                            }
                            _ => parts.join(" "),
                        };
                        self.shell(&script, workdir);
                    }
                    _ => {}
                }
            }
            "view_image" => {
                if let Some(path) = args.get("path").and_then(Value::as_str) {
                    self.files_read
                        .push(normalize_path(path, workdir), MAX_FILES);
                }
            }
            "run" if args.get("search_query").is_some() || args.get("open").is_some() => {
                self.web_run(&args);
            }
            _ => self.generic_arguments(&args, workdir),
        }
    }

    fn custom_tool_call(&mut self, name: &str, input: &str) {
        if name == "exec" {
            self.code_mode_script(input);
        } else if name == "apply_patch" || input.contains("*** Begin Patch") {
            self.patch(input);
        }
    }

    /// Tool calls inside a code mode script, which passes arguments as JavaScript object
    /// literals such as `tools.exec_command({ cmd: "...", workdir: "..." })`.
    fn code_mode_script(&mut self, script: &str) {
        let workdirs: Vec<(usize, String)> = string_fields(&JS_WORKDIR, script);
        for (position, command) in string_fields(&JS_CMD, script) {
            // Each call's workdir sits next to its command in the same object literal.
            let workdir = workdirs
                .iter()
                .min_by_key(|(workdir_position, _)| workdir_position.abs_diff(position))
                .map(|(_, workdir)| workdir.clone())
                .or_else(|| self.cwd.clone());
            self.shell(&command, workdir.as_deref());
        }
        if script.contains("web__run") || script.contains("web.run") {
            for (_, query) in string_fields(&JS_QUERY, script) {
                self.commands.push(
                    truncate(&format!("search: {query}"), MAX_COMMAND_CHARS),
                    MAX_COMMANDS,
                );
            }
        }
        if script.contains("view_image") {
            for (_, path) in string_fields(&JS_PATH, script) {
                let path = normalize_path(&path, self.cwd.as_deref());
                self.files_read.push(path, MAX_FILES);
            }
        }
        if script.contains("*** Begin Patch") {
            // Patches are string literals, so their line breaks are escaped.
            let unescaped = script.replace("\\n", "\n").replace("\\\"", "\"");
            self.patch(&unescaped);
        }
    }

    fn web_run(&mut self, args: &Value) {
        if let Some(queries) = args.get("search_query").and_then(Value::as_array) {
            for query in queries {
                if let Some(query) = query.get("q").and_then(Value::as_str) {
                    self.commands.push(
                        truncate(&format!("search: {query}"), MAX_COMMAND_CHARS),
                        MAX_COMMANDS,
                    );
                }
            }
        }
        if let Some(pages) = args.get("open").and_then(Value::as_array) {
            for page in pages {
                if let Some(url) = page.get("ref_id").and_then(Value::as_str)
                    && url.starts_with("http")
                {
                    self.urls.push(url.to_string(), MAX_URLS);
                }
            }
        }
    }

    fn web_search(&mut self, action: Option<&WebSearchAction>) {
        match action {
            Some(WebSearchAction::Search { query, queries }) => {
                let queries = match queries {
                    Some(queries) if !queries.is_empty() => queries.clone(),
                    _ => query.iter().cloned().collect(),
                };
                for query in queries {
                    self.commands.push(
                        truncate(&format!("search: {query}"), MAX_COMMAND_CHARS),
                        MAX_COMMANDS,
                    );
                }
            }
            Some(WebSearchAction::OpenPage { url: Some(url) })
            | Some(WebSearchAction::FindInPage { url: Some(url), .. }) => {
                self.urls.push(url.clone(), MAX_URLS);
            }
            _ => {}
        }
    }

    /// Paths and URLs in the arguments of a tool this module does not know.
    fn generic_arguments(&mut self, args: &Value, workdir: Option<&str>) {
        let Some(object) = args.as_object() else {
            return;
        };
        for (key, value) in object {
            let values: Vec<&str> = match value {
                Value::String(value) => vec![value.as_str()],
                Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let key = key.to_ascii_lowercase();
            for value in values {
                if value.starts_with("http://") || value.starts_with("https://") {
                    self.urls.push(value.to_string(), MAX_URLS);
                } else if (key.contains("path") || key.contains("file")) && looks_like_path(value) {
                    self.files_mentioned
                        .push(normalize_path(value, workdir), MAX_FILES);
                }
            }
        }
    }

    fn patch(&mut self, patch: &str) {
        for captures in PATCH_FILE.captures_iter(patch) {
            if let Some(path) = captures.get(2) {
                let path = normalize_path(path.as_str().trim(), self.cwd.as_deref());
                self.files_written.push(path, MAX_FILES);
            }
        }
        for line in patch.lines() {
            if line.starts_with('+') || line.starts_with('-') || line.starts_with("@@") {
                self.definitions_in(line);
            }
        }
    }

    fn shell(&mut self, script: &str, workdir: Option<&str>) {
        let script = script.trim();
        if script.is_empty() {
            return;
        }
        self.commands
            .push(truncate(script, MAX_COMMAND_CHARS), MAX_COMMANDS);
        if script.contains("*** Begin Patch") {
            self.patch(script);
        }
        let mut cwd = workdir.map(str::to_string);
        for segment in split_shell(script) {
            // Only the command line of a heredoc names files; its body is data.
            let line = segment.lines().next().unwrap_or_default();
            let line = line.split("<<").next().unwrap_or_default();
            let words = shlex::split(line)
                .unwrap_or_else(|| line.split_whitespace().map(str::to_string).collect());
            self.shell_words(&words, &mut cwd);
        }
    }

    fn shell_words(&mut self, words: &[String], cwd: &mut Option<String>) {
        let mut words: Vec<&str> = words.iter().map(String::as_str).collect();
        // Redirections write their target whatever the command is.
        let mut index = 0;
        while index < words.len() {
            let word = words[index];
            let target = match word {
                ">" | ">>" | "1>" | "2>" | "&>" => {
                    let target = words.get(index + 1).copied();
                    words.drain(index..(index + 2).min(words.len()));
                    target
                }
                _ if (word.starts_with('>') || word.starts_with("1>"))
                    && !word.starts_with(">&") =>
                {
                    words.remove(index);
                    Some(word.trim_start_matches(['1', '>']))
                }
                _ => {
                    index += 1;
                    None
                }
            };
            if let Some(target) = target
                && looks_like_path(target)
                && target != "/dev/null"
            {
                self.files_written
                    .push(normalize_path(target, cwd.as_deref()), MAX_FILES);
            }
        }

        // Skip environment assignments and wrappers to find the program.
        while let Some(first) = words.first() {
            let wrapper = matches!(
                *first,
                "sudo" | "time" | "env" | "nohup" | "command" | "exec"
            );
            if wrapper || (first.contains('=') && !first.starts_with('-')) {
                words.remove(0);
            } else {
                break;
            }
        }
        let Some((program, args)) = words.split_first() else {
            return;
        };
        let program = program.rsplit('/').next().unwrap_or(program);
        let (operands, flag_values) = split_operands(program, args);
        let cwd_ref = cwd.as_deref();
        match program {
            "cd" => {
                if let Some(dir) = operands.first() {
                    *cwd = Some(normalize_path(dir, cwd_ref));
                }
            }
            "cat" | "head" | "tail" | "nl" | "less" | "more" | "bat" | "wc" | "file" | "stat"
            | "diff" | "cmp" | "jq" | "xxd" | "hexdump" | "strings" | "ls" | "tree" => {
                self.paths(&operands, cwd_ref, PathUse::Read);
            }
            "sed" | "perl" => {
                let in_place = args
                    .iter()
                    .any(|arg| arg.starts_with("-i") || arg.starts_with("-pi"));
                // The first operand is the script unless it came with -e.
                let files = if flag_values.is_empty() {
                    operands.get(1..).unwrap_or_default()
                } else {
                    &operands[..]
                };
                let use_ = if in_place {
                    PathUse::Write
                } else {
                    PathUse::Read
                };
                self.paths(files, cwd_ref, use_);
            }
            "rg" | "grep" | "egrep" | "ag" | "ack" => {
                let has_pattern_flag = flag_values
                    .iter()
                    .any(|(flag, _)| matches!(*flag, "-e" | "-f" | "--regexp" | "--file"));
                let (pattern, files) = match operands.split_first() {
                    Some((pattern, files)) if !has_pattern_flag => (Some(*pattern), files),
                    _ => (None, &operands[..]),
                };
                if let Some(pattern) = pattern
                    && IDENTIFIER.is_match(pattern)
                    && pattern.len() > 2
                {
                    self.symbols.push(pattern.to_string(), MAX_SYMBOLS);
                }
                self.paths(files, cwd_ref, PathUse::Read);
            }
            "tee" | "touch" | "rm" | "rmdir" | "mkdir" | "truncate" | "chmod" => {
                self.paths(&operands, cwd_ref, PathUse::Write);
            }
            "mv" | "cp" | "ln" | "install" => {
                if let Some((destination, sources)) = operands.split_last() {
                    let use_ = if program == "mv" {
                        PathUse::Write
                    } else {
                        PathUse::Read
                    };
                    self.paths(sources, cwd_ref, use_);
                    self.paths(&[*destination], cwd_ref, PathUse::Write);
                }
            }
            "git" => {
                let subcommand = operands.first().copied().unwrap_or_default();
                let files = operands.get(1..).unwrap_or_default();
                let use_ = match subcommand {
                    "show" | "diff" | "log" | "blame" | "status" => PathUse::Read,
                    "checkout" | "restore" | "rm" | "mv" | "apply" => PathUse::Write,
                    _ => PathUse::Mention,
                };
                self.paths(files, cwd_ref, use_);
            }
            _ => self.paths(&operands, cwd_ref, PathUse::Mention),
        }
    }

    fn paths(&mut self, words: &[&str], cwd: Option<&str>, use_: PathUse) {
        for word in words {
            if !looks_like_path(word) {
                continue;
            }
            let path = normalize_path(word, cwd);
            let list = match use_ {
                PathUse::Read => &mut self.files_read,
                PathUse::Write => &mut self.files_written,
                PathUse::Mention => &mut self.files_mentioned,
            };
            list.push(path, MAX_FILES);
        }
    }

    fn output(&mut self, output: &FunctionCallOutputPayload) {
        let text = output_text(output);
        let mut codes: Vec<i32> = EXIT_CODE
            .captures_iter(&text)
            .filter_map(|captures| {
                captures
                    .get(1)
                    .or_else(|| captures.get(2))
                    .or_else(|| captures.get(3))
            })
            .filter_map(|code| code.as_str().parse().ok())
            .collect();
        let running = text.contains("Process running with session ID");
        let first_line = text.lines().map(str::trim).find(|line| !line.is_empty());
        let patch_success = first_line.is_some_and(|line| line.starts_with("Success."))
            || text.starts_with("{\"output\":\"Success.");
        let explicit_failure = output.success == Some(false)
            || first_line.is_some_and(|line| {
                line.starts_with("apply_patch verification failed")
                    || line.starts_with("Failed to")
                    || line.starts_with("error:")
                    || line.starts_with("Error:")
            });

        if codes.iter().any(|code| *code != 0) || explicit_failure || codes.is_empty() {
            self.errors_in(&text);
        }
        self.running |= running;
        self.failed |= explicit_failure;
        self.succeeded |= patch_success || output.success == Some(true);
        self.exit_codes.append(&mut codes);
    }

    /// File paths and code identifiers quoted in backticks, and URLs.
    fn prose(&mut self, text: &str) {
        for url in URL.find_iter(text) {
            let url = url.as_str().trim_end_matches(['.', ',', ';', ':']);
            self.urls.push(url.to_string(), MAX_URLS);
        }
        for captures in BACKTICK.captures_iter(text) {
            let Some(span) = captures.get(1).map(|span| span.as_str().trim()) else {
                continue;
            };
            if span.contains(' ') {
                continue;
            }
            let span = LINE_SUFFIX.replace(span, "");
            if looks_like_path(&span) {
                let path = normalize_path(&span, self.cwd.as_deref());
                self.files_mentioned.push(path, MAX_FILES);
            } else if let Some(symbol) = code_identifier(&span) {
                self.symbols.push(symbol, MAX_SYMBOLS);
            }
        }
    }

    fn definitions_in(&mut self, line: &str) {
        for captures in DEFINITION.captures_iter(line) {
            if let Some(name) = captures.get(1) {
                self.symbols.push(name.as_str().to_string(), MAX_SYMBOLS);
            }
        }
    }

    fn errors_in(&mut self, text: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let scanned: Box<dyn Iterator<Item = &&str>> = if lines.len() > 2 * ERROR_SCAN_LINES {
            Box::new(
                lines[..ERROR_SCAN_LINES]
                    .iter()
                    .chain(&lines[lines.len() - ERROR_SCAN_LINES..]),
            )
        } else {
            Box::new(lines.iter())
        };
        for line in scanned {
            let line = line.trim();
            if ERROR_LINE.is_match(line) {
                self.errors
                    .push(truncate(line, MAX_ERROR_CHARS), MAX_ERRORS);
            }
        }
    }

    fn finish(self, tokens: u32) -> BlockFacts {
        let exit_code = self
            .exit_codes
            .iter()
            .copied()
            .find(|code| *code != 0)
            .or_else(|| self.exit_codes.first().copied());
        let outcome = if exit_code.is_some_and(|code| code != 0) || self.failed {
            Some(Outcome::Failure)
        } else if self.running {
            Some(Outcome::Partial)
        } else if exit_code == Some(0) || self.succeeded {
            Some(Outcome::Success)
        } else {
            None
        };
        BlockFacts {
            entities: BlockEntities {
                files_read: self.files_read.items,
                files_written: self.files_written.items,
                files_mentioned: self.files_mentioned.items,
                symbols: self.symbols.items,
                commands: self.commands.items,
                errors: self.errors.items,
                urls: self.urls.items,
            },
            tools: self.tools.items,
            outcome,
            exit_code,
            tokens,
        }
    }
}

#[derive(Clone, Copy)]
enum PathUse {
    Read,
    Write,
    Mention,
}

/// A list that keeps the first occurrence of each value, up to a limit.
#[derive(Default)]
struct Ordered {
    items: Vec<String>,
    seen: HashSet<String>,
}

impl Ordered {
    fn push(&mut self, value: String, limit: usize) {
        if self.items.len() < limit && !value.is_empty() && self.seen.insert(value.clone()) {
            self.items.push(value);
        }
    }
}

static PATCH_FILE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?m)^\*\*\* (Add File|Update File|Delete File|Move to): (.+)$"));
static JS_CMD: LazyLock<Regex> = LazyLock::new(|| js_string_field("cmd"));
static JS_WORKDIR: LazyLock<Regex> = LazyLock::new(|| js_string_field("workdir"));
static JS_QUERY: LazyLock<Regex> = LazyLock::new(|| js_string_field("q"));
static JS_PATH: LazyLock<Regex> = LazyLock::new(|| js_string_field("path"));
static EXIT_CODE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(?m)Process exited with code (-?\d+)|"exit_code"\s*:\s*(-?\d+)|^Exit code: (-?\d+)"#)
});
static URL: LazyLock<Regex> = LazyLock::new(|| regex(r#"https?://[^\s<>()\[\]"'`]+"#));
static BACKTICK: LazyLock<Regex> = LazyLock::new(|| regex(r"`([^`\n]{2,120})`"));
static LINE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| regex(r"(:\d+)+$"));
static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$"));
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"\b(?:fn|def|class|struct|enum|trait|interface|function|mod)\s+([A-Za-z_][A-Za-z0-9_]*)")
});
static ERROR_LINE: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"^(error(\[E\d+\])?:|[A-Za-z]*Error:|[A-Za-z]*Exception:|fatal:|FAILED\b|FAIL:)",
        r"|panicked at|test result: FAILED|No such file or directory|command not found",
        r"|Traceback \(most recent call last\)",
    ))
});

fn regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex {pattern}: {err}"),
    }
}

/// Matches `name: "value"` in a JavaScript object literal, with or without a quoted key.
fn js_string_field(name: &str) -> Regex {
    regex(&format!(
        r#"(?:^|[{{,\s])["']?{name}["']?\s*:\s*"((?:[^"\\]|\\.)*)""#
    ))
}

/// Every string value `field` matches in `script`, with the position where it starts.
fn string_fields(field: &Regex, script: &str) -> Vec<(usize, String)> {
    field
        .captures_iter(script)
        .filter_map(|captures| captures.get(1))
        .filter_map(|value| {
            let text: String = serde_json::from_str(&format!("\"{}\"", value.as_str())).ok()?;
            Some((value.start(), text))
        })
        .collect()
}

/// Splits a shell script into simple commands at `&&`, `||`, `|`, `;` and newlines outside
/// quotes. Heredoc bodies stay attached to the command that reads them.
fn split_shell(script: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(open), _) if ch == open => {
                quote = None;
                current.push(ch);
            }
            (Some(_), '\\') => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), _) => current.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                current.push(ch);
            }
            (None, '\\') => {
                if let Some(next) = chars.next()
                    && next != '\n'
                {
                    current.push(ch);
                    current.push(next);
                }
            }
            (None, '&' | '|') if chars.peek() == Some(&ch) => {
                chars.next();
                segments.push(std::mem::take(&mut current));
            }
            (None, '\n') if current.contains("<<") => {
                // The rest of the script is the heredoc body of this command.
                current.push(ch);
                current.extend(chars.by_ref());
            }
            (None, '|' | ';' | '\n') => segments.push(std::mem::take(&mut current)),
            (None, _) => current.push(ch),
        }
    }
    segments.push(current);
    segments
        .into_iter()
        .map(|segment| segment.trim().to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Separates a command's operands from its flags, and returns the values of flags that take one,
/// such as the context size in `rg -C 8`.
fn split_operands<'a>(program: &str, args: &[&'a str]) -> (Vec<&'a str>, Vec<(&'a str, &'a str)>) {
    let value_flags: &[&str] = match program {
        "rg" | "grep" | "egrep" | "ag" | "ack" => &[
            "-e",
            "-f",
            "-g",
            "-t",
            "-T",
            "-C",
            "-A",
            "-B",
            "-m",
            "-M",
            "--glob",
            "--iglob",
            "--type",
            "--type-not",
            "--context",
            "--after-context",
            "--before-context",
            "--max-count",
            "--max-columns",
            "--regexp",
            "--file",
        ],
        "sed" => &["-e", "-f"],
        "perl" => &["-e", "-E"],
        "head" | "tail" => &["-n", "-c"],
        "git" => &["-C", "-c"],
        _ => &[],
    };
    let mut operands = Vec::new();
    let mut flag_values = Vec::new();
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        if value_flags.contains(&arg)
            || (program == "perl"
                && arg.ends_with('e')
                && arg.starts_with('-')
                && !arg.starts_with("--"))
        {
            if let Some(value) = args.next() {
                flag_values.push((arg, value));
            }
        } else if !arg.starts_with('-') && !arg.is_empty() {
            operands.push(arg);
        }
    }
    (operands, flag_values)
}

/// Whether a shell word or quoted span names a file rather than a flag, pattern or number.
fn looks_like_path(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'']);
    if word.is_empty()
        || word.len() > 300
        || word.starts_with('-')
        || word.contains("://")
        || word.contains([
            '*', '?', '{', '}', '$', '<', '>', '|', '\n', '(', ')', '=', ',',
        ])
        || word.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    {
        return false;
    }
    if word.contains('/') {
        return word.chars().any(char::is_alphanumeric);
    }
    // A bare file name needs an extension that starts with a letter, like `main.rs`.
    match word.rsplit_once('.') {
        Some((stem, extension)) => {
            !stem.is_empty()
                && (1..=10).contains(&extension.len())
                && extension.starts_with(|ch: char| ch.is_ascii_alphabetic())
                && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// An identifier from a backtick span when it reads as code: it has `_`, `::`, `()` or an inner
/// capital, so plain words such as `retries` are not mistaken for symbols.
fn code_identifier(span: &str) -> Option<String> {
    let name = span.strip_suffix("()").unwrap_or(span);
    if !IDENTIFIER.is_match(name) || name.len() < 3 {
        return None;
    }
    let codey = name.contains('_')
        || name.contains("::")
        || span.ends_with("()")
        || name.chars().skip(1).any(|ch| ch.is_ascii_uppercase());
    codey.then(|| name.to_string())
}

/// Joins relative paths onto `cwd` and removes `.` and `..` components lexically.
fn normalize_path(path: &str, cwd: Option<&str>) -> String {
    let path = path.trim_matches(['"', '\'']);
    let joined = match cwd {
        Some(cwd) if !Path::new(path).is_absolute() && !path.starts_with('~') => {
            Path::new(cwd).join(path)
        }
        _ => PathBuf::from(path),
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other),
        }
    }
    normalized.to_string_lossy().into_owned()
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
