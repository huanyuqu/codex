//! Plain-text renderings of block items, for indexing prompts and close reading.

use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::models::plaintext_agent_message_content;

/// One item of a block as a label and its readable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemText {
    /// Who or what produced the text, such as `user`, `tool call exec_command` or `tool output`.
    pub label: String,
    pub text: String,
}

/// Returns the readable parts of `item`, or `None` when it has nothing readable, such as
/// encrypted reasoning or an opaque compaction.
pub fn item_text(item: &ResponseItem) -> Option<ItemText> {
    let (label, text) = match item {
        ResponseItem::Message { role, content, .. } => (role.clone(), content_text(content)),
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => (
            format!("message from {author} to {recipient}"),
            plaintext_agent_message_content(content).unwrap_or_default(),
        ),
        ResponseItem::Reasoning { summary, .. } => (
            "reasoning summary".to_string(),
            summary
                .iter()
                .map(|ReasoningItemReasoningSummary::SummaryText { text }| text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            ..
        } => (
            format!("tool call {}", qualified_name(namespace.as_deref(), name)),
            arguments.clone(),
        ),
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            ..
        } => (
            format!("tool call {}", qualified_name(namespace.as_deref(), name)),
            input.clone(),
        ),
        ResponseItem::LocalShellCall {
            action: LocalShellAction::Exec(exec),
            ..
        } => ("tool call shell".to_string(), exec.command.join(" ")),
        ResponseItem::ToolSearchCall { arguments, .. } => {
            ("tool search".to_string(), arguments.to_string())
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            ("tool output".to_string(), output_text(output))
        }
        ResponseItem::ToolSearchOutput { tools, .. } => (
            "tool search output".to_string(),
            format!("{} tools", tools.len()),
        ),
        ResponseItem::WebSearchCall { action, .. } => {
            ("web search".to_string(), web_search_text(action.as_ref()))
        }
        ResponseItem::ImageGenerationCall { revised_prompt, .. } => (
            "image generation".to_string(),
            revised_prompt.clone().unwrap_or_default(),
        ),
        ResponseItem::AdditionalTools { tools, .. } => {
            ("tool catalog".to_string(), format!("{} tools", tools.len()))
        }
        ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger {}
        | ResponseItem::Other => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(ItemText {
        label,
        text: text.to_string(),
    })
}

/// Text of a function or custom tool output, with images and other media as placeholders.
pub fn output_text(output: &FunctionCallOutputPayload) -> String {
    match output.text_content() {
        Some(text) => text.to_string(),
        None => output
            .content_items()
            .and_then(function_call_output_content_items_to_text)
            .unwrap_or_else(|| "<non-text output>".to_string()),
    }
}

/// Text parts of a message joined by newlines, with media as placeholders.
pub fn content_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => text.as_str(),
            ContentItem::InputImage { .. } => "<image>",
            ContentItem::InputAudio { .. } => "<audio>",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn qualified_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}.{name}"),
        _ => name.to_string(),
    }
}

fn web_search_text(action: Option<&WebSearchAction>) -> String {
    match action {
        Some(WebSearchAction::Search { query, queries }) => match queries {
            Some(queries) if !queries.is_empty() => queries.join(" | "),
            _ => query.clone().unwrap_or_default(),
        },
        Some(WebSearchAction::OpenPage { url }) => url.clone().unwrap_or_default(),
        Some(WebSearchAction::FindInPage { url, pattern }) => format!(
            "{} in {}",
            pattern.as_deref().unwrap_or_default(),
            url.as_deref().unwrap_or_default()
        ),
        Some(WebSearchAction::Other) | None => String::new(),
    }
}

/// Renders a block as labelled sections, keeping the head and tail of long items.
///
/// Each item keeps at most `max_item_chars` characters, and the whole rendering at most
/// `max_total_chars`; later items share whatever budget earlier ones left.
pub fn render_block(
    items: &[ResponseItem],
    max_item_chars: usize,
    max_total_chars: usize,
) -> String {
    let mut rendered = String::new();
    for ItemText { label, text } in items.iter().filter_map(item_text) {
        let remaining = max_total_chars.saturating_sub(rendered.chars().count());
        if remaining < 64 {
            rendered.push_str("[... more items omitted]\n");
            break;
        }
        let header = format!("[{label}]\n");
        let budget = max_item_chars
            .min(remaining)
            .saturating_sub(header.chars().count() + 1);
        rendered.push_str(&header);
        rendered.push_str(&head_and_tail(&text, budget));
        rendered.push('\n');
    }
    rendered
}

/// Characters of one item that close reading and excerpts see.
pub const READ_ITEM_CHARS: usize = 8_000;
/// Characters of one block that close reading and excerpts see.
pub const READ_BLOCK_CHARS: usize = 16_000;

/// The lines of a block's rendering for close reading. Excerpts name lines of this rendering,
/// counted from 1, so it must stay deterministic.
pub fn block_lines(items: &[ResponseItem]) -> Vec<String> {
    render_block(items, READ_ITEM_CHARS, READ_BLOCK_CHARS)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Keeps roughly the first 60% and last 40% of `text` within `max_chars` characters.
pub fn head_and_tail(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let marker_room = 40;
    let keep = max_chars.saturating_sub(marker_room);
    let head = keep * 3 / 5;
    let tail = keep - head;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!(
        "{head_text}\n[... {} chars omitted ...]\n{tail_text}",
        total - head - tail
    )
}

/// First line of `text`, cut to `max_chars` characters with an ellipsis.
pub fn first_line(text: &str, max_chars: usize) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    truncate(line.unwrap_or_default(), max_chars)
}

/// Rough token count of `text`: about four ASCII characters per token, and a little under one
/// token per character of other scripts, such as Chinese.
pub fn estimate_tokens(text: &str) -> u32 {
    let (ascii, other) = text.chars().fold((0usize, 0usize), |(ascii, other), ch| {
        if ch.is_ascii() {
            (ascii + 1, other)
        } else {
            (ascii, other + 1)
        }
    });
    u32::try_from(ascii.div_ceil(4) + (other * 4).div_ceil(5)).unwrap_or(u32::MAX)
}

/// `text` cut to `max_chars` characters with an ellipsis.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
