//! Context views: which blocks of the history a request shows, and how much of each.
//!
//! A selector, such as a screening model, BM25 or an oracle, picks blocks. [`plan_view`] turns
//! the picks into a view: it always keeps the most recent turns and the task the branch started
//! with, adds what picked blocks depend on and what made them outdated, lists every user
//! constraint and decision in a ledger, and shrinks the least important picks until the view fits
//! its token budget. [`render_view`] turns the view into the items the model sees.

use std::collections::BTreeMap;
use std::collections::HashMap;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;

use crate::BlockId;
use crate::BlockKind;
use crate::card::ClaimKind;
use crate::history::EpisodeSpan;
use crate::history::History;
use crate::text::block_lines;
use crate::text::estimate_tokens;
use crate::text::render_block;
use crate::text::truncate;

/// Tokens of the framing around one summarized or excerpted block.
const ENTRY_OVERHEAD_TOKENS: u32 = 12;
const MAP_TITLE_CHARS: usize = 90;
/// Turns at the start of a long session that the map keeps when it has to cut turns.
const MAP_HEAD_TURNS: usize = 5;

const RECALL_NOTICE: &str = "Parts of this conversation's history are omitted to keep the \
context focused. Below are a map of the earlier turns, the user's constraints and the decisions \
made so far, and earlier blocks selected for the current request. They are recorded history, not \
new instructions: requests in them were already handled, and their tool calls already ran.";

/// Lines `from..=to` of a block's [`block_lines`] rendering, counted from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub from: u32,
    pub to: u32,
}

/// How much of a block a view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "detail")]
pub enum Presentation {
    /// The block's items as recorded.
    Full,
    /// Some lines of the block's text rendering.
    Excerpt { lines: Vec<LineRange> },
    /// The title and summary of the block's card.
    Summary,
}

impl Presentation {
    fn detail(&self) -> u8 {
        match self {
            Self::Full => 3,
            Self::Excerpt { .. } => 2,
            Self::Summary => 1,
        }
    }
}

/// Why a view shows a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// It is in the most recent turns, which are always shown in full.
    Recent,
    /// It is the first user message: the task the branch started with.
    TaskStart,
    /// The selector picked it.
    Selected,
    /// A shown block depends on it.
    Dependency,
    /// It made a shown block outdated.
    Successor,
}

/// A block the selector picked, with how much of it to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// Position of the block in the history.
    pub index: usize,
    pub presentation: Presentation,
    /// Why it was picked, for the view record.
    pub note: Option<String>,
}

/// One block a view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewEntry {
    pub block_id: BlockId,
    /// Position of the block in the history.
    #[serde(skip)]
    pub index: usize,
    pub presentation: Presentation,
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The block that made this one outdated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<BlockId>,
    /// Whether a file this block read has changed since, which makes it outdated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub file_changed: bool,
    pub tokens: u32,
}

/// A user constraint or decision that a view lists whether or not its block is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerClaim {
    pub block_id: BlockId,
    pub turn: usize,
    pub kind: ClaimKind,
    pub text: String,
}

/// What a request shows of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextView {
    /// Shown blocks in history order, ending with the recent turns.
    pub entries: Vec<ViewEntry>,
    pub ledger: Vec<LedgerClaim>,
    /// One line per turn before the recent ones.
    pub map: Vec<String>,
    /// Position of the first block of the recent turns.
    #[serde(skip)]
    pub recent_start: usize,
    /// Estimated tokens of the rendered view.
    pub tokens: u32,
    pub budget_tokens: u32,
    /// Picks shown with less detail than selected, to fit the budget.
    pub shrunk: u32,
    /// Picks left out to fit the budget.
    pub dropped: u32,
}

impl ContextView {
    /// Whether the view shows the block at `index`.
    pub fn shows(&self, index: usize) -> bool {
        self.entries.iter().any(|entry| entry.index == index)
    }
}

/// Limits of a context view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewLimits {
    pub budget_tokens: u32,
    /// Most recent turns shown in full.
    pub recent_turns: usize,
    /// Largest first user message that is shown in full; a larger one is summarized.
    pub task_start_max_tokens: u32,
    pub max_ledger_tokens: u32,
    pub max_map_turns: usize,
}

impl Default for ViewLimits {
    fn default() -> Self {
        Self {
            budget_tokens: 40_000,
            recent_turns: 2,
            task_start_max_tokens: 4_000,
            max_ledger_tokens: 4_000,
            max_map_turns: 60,
        }
    }
}

/// Builds the view of `history` that shows `picks`, most important first.
pub fn plan_view(history: &History<'_>, picks: &[Pick], limits: ViewLimits) -> ContextView {
    let recent_start = history.recent_start(limits.recent_turns);
    let mut entries: BTreeMap<usize, ViewEntry> = BTreeMap::new();
    // Picks and the blocks they pull in, most important first, for shrinking to the budget.
    let mut priority: Vec<usize> = Vec::new();
    let entry = |index: usize, presentation: Presentation, reason: Reason| ViewEntry {
        block_id: history.card(index).block_id.clone(),
        index,
        presentation,
        reason,
        note: None,
        superseded_by: None,
        file_changed: false,
        tokens: 0,
    };

    for index in (recent_start..history.len())
        .filter(|index| history.recallable(*index) && history.is_own(*index))
    {
        entries.insert(index, entry(index, Presentation::Full, Reason::Recent));
    }
    if let Some(task) = history.task_start().filter(|task| *task < recent_start) {
        let presentation = if history.tokens(task) <= limits.task_start_max_tokens {
            Presentation::Full
        } else {
            Presentation::Summary
        };
        entries.insert(task, entry(task, presentation, Reason::TaskStart));
    }
    for pick in picks {
        if history.is_recent(pick.index, recent_start) || !history.recallable(pick.index) {
            continue;
        }
        match entries.get_mut(&pick.index) {
            Some(existing) => {
                if pick.presentation.detail() > existing.presentation.detail() {
                    existing.presentation = pick.presentation.clone();
                }
            }
            None => {
                let mut picked = entry(pick.index, pick.presentation.clone(), Reason::Selected);
                picked.note = pick.note.clone();
                entries.insert(pick.index, picked);
                priority.push(pick.index);
            }
        }
    }

    // What the picks depend on, one level deep, as summaries.
    let picked = priority.clone();
    for index in picked {
        for dependency in &history.card(index).depends_on {
            let Some(dependency) = history.position(dependency) else {
                continue;
            };
            if !history.is_recent(dependency, recent_start)
                && history.recallable(dependency)
                && !entries.contains_key(&dependency)
            {
                entries.insert(
                    dependency,
                    entry(dependency, Presentation::Summary, Reason::Dependency),
                );
                priority.push(dependency);
            }
        }
    }

    // Outdated blocks are summarized and point to what replaced them, which is shown too.
    let superseded_by = superseded_by(history);
    let outdated: Vec<(usize, usize)> = entries
        .keys()
        .filter(|index| !history.is_recent(**index, recent_start))
        .filter_map(|index| Some((*index, *superseded_by.get(index)?)))
        .collect();
    for (index, successor) in outdated {
        if let Some(outdated) = entries.get_mut(&index) {
            outdated.superseded_by = Some(history.card(successor).block_id.clone());
            outdated.presentation = Presentation::Summary;
        }
        if history.recallable(successor) && !entries.contains_key(&successor) {
            entries.insert(
                successor,
                entry(successor, Presentation::Summary, Reason::Successor),
            );
            priority.push(successor);
        }
    }

    // A block whose files changed since it read them is summarized, like an outdated one.
    for (index, entry) in &mut entries {
        if !history.is_recent(*index, recent_start) && history.changed(*index) {
            entry.file_changed = true;
            entry.presentation = Presentation::Summary;
        }
    }

    let ledger = ledger(
        history,
        recent_start,
        &superseded_by,
        limits.max_ledger_tokens,
    );
    let mut view = ContextView {
        entries: Vec::new(),
        ledger,
        map: Vec::new(),
        recent_start,
        tokens: 0,
        budget_tokens: limits.budget_tokens,
        shrunk: 0,
        dropped: 0,
    };
    for entry in entries.values_mut() {
        entry.tokens = entry_tokens(history, entry);
    }
    view.entries = entries.into_values().collect();
    view.map = turn_map(history, &view, limits.max_map_turns);
    view.tokens = view_tokens(&view);
    fit_budget(history, &mut view, &priority);
    if view.dropped > 0 {
        view.map = turn_map(history, &view, limits.max_map_turns);
        view.tokens = view_tokens(&view);
    }
    view
}

/// For each block that a later block made outdated, the latest such block.
fn superseded_by(history: &History<'_>) -> HashMap<usize, usize> {
    let mut superseded_by = HashMap::new();
    for (index, block) in history.blocks().iter().enumerate() {
        for outdated in &block.card.supersedes {
            if let Some(outdated) = history.position(outdated)
                && outdated < index
            {
                superseded_by.insert(outdated, index);
            }
        }
    }
    superseded_by
}

/// User constraints and the decisions still in force from blocks before the recent turns. When
/// they exceed `max_tokens`, the oldest decisions go first; constraints always stay.
fn ledger(
    history: &History<'_>,
    recent_start: usize,
    superseded_by: &HashMap<usize, usize>,
    max_tokens: u32,
) -> Vec<LedgerClaim> {
    let mut claims: Vec<LedgerClaim> = Vec::new();
    for index in
        (0..recent_start).filter(|index| history.recallable(*index) && history.is_own(*index))
    {
        for claim in &history.card(index).claims {
            let keep = match claim.kind {
                ClaimKind::UserConstraint => true,
                ClaimKind::Decision => !superseded_by.contains_key(&index),
                ClaimKind::Finding | ClaimKind::Todo | ClaimKind::OpenQuestion => false,
            };
            if keep {
                claims.push(LedgerClaim {
                    block_id: history.card(index).block_id.clone(),
                    turn: history.turn(index),
                    kind: claim.kind,
                    text: claim.text.clone(),
                });
            }
        }
    }
    let mut total: u32 = claims.iter().map(ledger_claim_tokens).sum();
    while total > max_tokens {
        let Some(oldest_decision) = claims
            .iter()
            .position(|claim| claim.kind == ClaimKind::Decision)
        else {
            break;
        };
        total -= ledger_claim_tokens(&claims.remove(oldest_decision));
    }
    claims
}

fn ledger_claim_tokens(claim: &LedgerClaim) -> u32 {
    estimate_tokens(&claim.text) + 6
}

fn entry_tokens(history: &History<'_>, entry: &ViewEntry) -> u32 {
    match &entry.presentation {
        Presentation::Full => history.tokens(entry.index),
        Presentation::Excerpt { lines } => {
            estimate_tokens(&excerpt(history.block(entry.index).items, lines))
                + ENTRY_OVERHEAD_TOKENS
        }
        Presentation::Summary => {
            let card = history.card(entry.index);
            estimate_tokens(&card.title) + estimate_tokens(&card.summary) + ENTRY_OVERHEAD_TOKENS
        }
    }
}

fn view_tokens(view: &ContextView) -> u32 {
    let entries: u32 = view.entries.iter().map(|entry| entry.tokens).sum();
    let ledger: u32 = view.ledger.iter().map(ledger_claim_tokens).sum();
    let map: u32 = view.map.iter().map(|line| estimate_tokens(line) + 1).sum();
    entries + ledger + map + estimate_tokens(RECALL_NOTICE)
}

/// Summarizes, then leaves out, the least important of `priority` until the view fits.
fn fit_budget(history: &History<'_>, view: &mut ContextView, priority: &[usize]) {
    for shrink in [true, false] {
        for index in priority.iter().rev() {
            if view.tokens <= view.budget_tokens {
                return;
            }
            let Some(position) = view.entries.iter().position(|entry| entry.index == *index) else {
                continue;
            };
            let entry = &mut view.entries[position];
            if shrink {
                if entry.presentation == Presentation::Summary {
                    continue;
                }
                entry.presentation = Presentation::Summary;
                let tokens = entry_tokens(history, entry);
                view.tokens = view.tokens - entry.tokens + tokens;
                entry.tokens = tokens;
                view.shrunk += 1;
            } else {
                view.tokens -= entry.tokens;
                view.entries.remove(position);
                view.dropped += 1;
            }
        }
    }
}

/// One line per turn before the recent turns: what was asked, what came of it, and how many of
/// its blocks the view shows.
fn turn_map(history: &History<'_>, view: &ContextView, max_turns: usize) -> Vec<String> {
    let mut turns: BTreeMap<usize, (Option<usize>, Option<usize>, usize)> = BTreeMap::new();
    for index in
        (0..view.recent_start).filter(|index| history.recallable(*index) && history.is_own(*index))
    {
        let turn = turns.entry(history.turn(index)).or_default();
        match history.card(index).kind {
            BlockKind::UserMessage if turn.0.is_none() => turn.0 = Some(index),
            BlockKind::AssistantMessage => turn.1 = Some(index),
            _ => {}
        }
    }
    for entry in &view.entries {
        if entry.index < view.recent_start
            && history.is_own(entry.index)
            && let Some(turn) = turns.get_mut(&history.turn(entry.index))
        {
            turn.2 += 1;
        }
    }
    let title = |index: Option<usize>| {
        index.map_or_else(
            || "-".to_string(),
            |index| truncate(&history.card(index).title, MAP_TITLE_CHARS),
        )
    };
    let rows: Vec<(usize, usize, String)> = turns
        .into_iter()
        .map(|(turn, (request, result, shown))| {
            let line = if shown > 0 {
                format!(
                    "turn {turn}: {} → {} [{shown} shown below]",
                    title(request),
                    title(result)
                )
            } else {
                format!("turn {turn}: {} → {}", title(request), title(result))
            };
            (turn, shown, line)
        })
        .collect();
    if rows.len() <= max_turns {
        return rows.into_iter().map(|(_, _, line)| line).collect();
    }
    let lines = with_episodes(history, &rows);
    // A cut map has a line for the turns it leaves out, so summed-up episodes may take it.
    if lines.len() <= max_turns + 1 && lines.len() < rows.len() {
        return lines.into_iter().map(|(_, line)| line).collect();
    }
    let head = MAP_HEAD_TURNS.min(max_turns);
    let tail = max_turns - head;
    let left_out: usize = lines[head..lines.len() - tail]
        .iter()
        .map(|(turns, _)| turns)
        .sum();
    let mut cut: Vec<String> = lines[..head].iter().map(|(_, line)| line.clone()).collect();
    cut.push(format!("… {left_out} turns not listed …"));
    cut.extend(
        lines[lines.len() - tail..]
            .iter()
            .map(|(_, line)| line.clone()),
    );
    cut
}

/// The map lines of `rows` (turn, blocks shown, line) after the first few, with the turns of
/// each episode whose blocks the view does not show summed up in one line, and how many turns
/// each line covers.
fn with_episodes(history: &History<'_>, rows: &[(usize, usize, String)]) -> Vec<(usize, String)> {
    let head = MAP_HEAD_TURNS.min(rows.len());
    let shown: BTreeMap<usize, usize> = rows
        .iter()
        .map(|(turn, shown, _)| (*turn, *shown))
        .collect();
    let first_turn = rows.get(head).map_or(usize::MAX, |(turn, _, _)| *turn);
    let episodes: Vec<&EpisodeSpan> = history
        .episodes()
        .iter()
        .filter(|episode| episode.first_turn >= first_turn)
        .filter(|episode| {
            shown
                .range(episode.first_turn..=episode.last_turn)
                .all(|(_, shown)| *shown == 0)
        })
        .collect();
    let mut lines: Vec<(usize, String)> = rows[..head]
        .iter()
        .map(|(_, _, line)| (1, line.clone()))
        .collect();
    let mut row = head;
    while row < rows.len() {
        let turn = rows[row].0;
        if let Some(episode) = episodes.iter().find(|episode| episode.first_turn == turn) {
            let start = row;
            while row < rows.len() && rows[row].0 <= episode.last_turn {
                row += 1;
            }
            lines.push((
                row - start,
                format!(
                    "turns {}–{}: {} — {}",
                    episode.first_turn,
                    episode.last_turn,
                    truncate(&episode.title, MAP_TITLE_CHARS),
                    truncate(&episode.summary, 2 * MAP_TITLE_CHARS)
                ),
            ));
            continue;
        }
        lines.push((1, rows[row].2.clone()));
        row += 1;
    }
    lines
}

/// The lines of `items` that `ranges` name, with a marker where lines are skipped.
pub fn excerpt(items: &[ResponseItem], ranges: &[LineRange]) -> String {
    let lines = block_lines(items);
    let mut parts = Vec::new();
    for range in ranges {
        let from = usize::try_from(range.from.max(1)).unwrap_or(usize::MAX) - 1;
        let to = usize::try_from(range.to)
            .unwrap_or(usize::MAX)
            .min(lines.len());
        if from < to {
            parts.push(lines[from..to].join("\n"));
        }
    }
    parts.join("\n[...]\n")
}

const TOOLS_HINT: &str = "To read omitted history, find blocks with `context_search` and read \
a block or a whole turn with `context_expand`.";

/// Options for rendering a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderOptions {
    /// Keep reasoning items in the recent turns, for a model that can read its own reasoning.
    pub keep_recent_reasoning: bool,
    /// Tell the model it has tools to read omitted history.
    pub tools_hint: bool,
}

/// The items a model sees for `view`: a developer message with the notice, the turn map and the
/// ledger; the earlier blocks the view shows, in order; then the recent turns as recorded.
pub fn render_view(
    history: &History<'_>,
    view: &ContextView,
    options: RenderOptions,
) -> Vec<ResponseItem> {
    let mut preamble = preamble(view);
    if options.tools_hint {
        preamble.push_str("\n\n");
        preamble.push_str(TOOLS_HINT);
    }
    let mut items = vec![developer_message(&preamble)];

    let mut notes = String::new();
    for entry in &view.entries {
        let block = history.block(entry.index);
        let recent = history.is_recent(entry.index, view.recent_start);
        // Referenced blocks belong to another thread's history, so they are shown as text.
        if entry.presentation == Presentation::Full && block.complete && history.is_own(entry.index)
        {
            flush_notes(&mut notes, &mut items);
            let keep_reasoning = recent && options.keep_recent_reasoning;
            items.extend(
                block
                    .items
                    .iter()
                    .filter(|item| {
                        keep_reasoning || !matches!(item, ResponseItem::Reasoning { .. })
                    })
                    .cloned(),
            );
            continue;
        }
        if notes.is_empty() {
            notes.push_str("Recalled from earlier turns:\n");
        }
        notes.push('\n');
        notes.push_str(&entry_text(history, entry));
        notes.push('\n');
    }
    flush_notes(&mut notes, &mut items);
    items
}

/// The notice, the turn map and the ledger.
fn preamble(view: &ContextView) -> String {
    let mut preamble = String::from(RECALL_NOTICE);
    if !view.map.is_empty() {
        preamble.push_str("\n\n## Earlier turns\n");
        preamble.push_str(&view.map.join("\n"));
    }
    if !view.ledger.is_empty() {
        preamble.push_str("\n\n## Constraints and decisions\n");
        for claim in &view.ledger {
            let kind = match claim.kind {
                ClaimKind::UserConstraint => "user constraint",
                _ => "decision",
            };
            preamble.push_str(&format!("- ({kind}, turn {}) {}\n", claim.turn, claim.text));
        }
    }
    preamble.trim_end().to_string()
}

fn flush_notes(notes: &mut String, items: &mut Vec<ResponseItem>) {
    if !notes.is_empty() {
        items.push(developer_message(notes.trim_end()));
        notes.clear();
    }
}

/// The view as one text transcript: the notice, map and ledger, then every shown block as text,
/// recent turns included. For models that cannot replay recorded items, such as in offline
/// evaluation with another provider's model.
pub fn render_transcript(history: &History<'_>, view: &ContextView) -> String {
    let mut transcript = preamble(view);
    for entry in &view.entries {
        transcript.push_str("\n\n");
        transcript.push_str(&entry_text(history, entry));
    }
    transcript
}

/// A shown block as text: its card's title, then its summary, excerpt or full rendering.
pub fn entry_text(history: &History<'_>, entry: &ViewEntry) -> String {
    let block = history.block(entry.index);
    let card = block.card;
    let mut text = match history.origin(entry.index) {
        Some(branch) => format!(
            "[branch {branch} · {}] {}",
            kind_label(card.kind),
            card.title
        ),
        None => format!(
            "[turn {} · {}] {}",
            history.turn(entry.index),
            kind_label(card.kind),
            card.title
        ),
    };
    if let Some(successor) = entry
        .superseded_by
        .as_ref()
        .and_then(|id| history.position(id))
    {
        text.push_str(&format!(
            " (outdated: replaced in turn {} by \"{}\")",
            history.turn(successor),
            truncate(&history.card(successor).title, MAP_TITLE_CHARS)
        ));
    }
    if entry.file_changed {
        text.push_str(" (outdated: a file it read has changed since)");
    }
    match &entry.presentation {
        Presentation::Summary => {
            if !card.summary.is_empty() {
                text.push('\n');
                text.push_str(&card.summary);
            }
        }
        Presentation::Excerpt { lines } => {
            text.push_str(" (excerpt)\n");
            text.push_str(&excerpt(block.items, lines));
        }
        Presentation::Full => {
            text.push('\n');
            text.push_str(&render_block(block.items, usize::MAX, usize::MAX));
        }
    }
    text
}

fn kind_label(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::UserMessage => "user",
        BlockKind::AssistantMessage => "assistant",
        BlockKind::ToolExchange => "tool call",
        BlockKind::InterAgentMessage => "agent message",
        BlockKind::Reasoning => "reasoning",
        BlockKind::ContextFragment => "context",
        BlockKind::Compaction => "compaction",
        BlockKind::Merge => "merge",
    }
}

fn developer_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
