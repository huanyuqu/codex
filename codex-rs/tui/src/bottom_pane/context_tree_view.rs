//! The `/tree` view: the branches of the conversation's context graph as a tree.
//!
//! Each branch expands to its turns and each turn to its blocks, with the titles of their cards.
//! From any row the user can switch to a branch, preview a turn or block in the pager, fork after
//! a turn, merge or reference a branch, archive it, or search the cards of every branch. Branch
//! turns and search results load through the app server; they reach the open view through
//! [`ContextTreeUpdate`].

use codex_app_server_protocol::ContextGraphBranch;
use codex_app_server_protocol::ContextGraphBranchStatus;
use codex_app_server_protocol::ContextGraphMergeMode;
use codex_app_server_protocol::ContextGraphReadResponse;
use codex_app_server_protocol::ContextGraphSearchHit;
use codex_app_server_protocol::ContextGraphTurn;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::scroll_state::ScrollState;

pub(crate) const CONTEXT_TREE_VIEW_ID: &str = "context-tree";

/// Rows the view shows at once.
const VISIBLE_ROWS: usize = 16;

/// Data that reaches the open view after it asked for it.
#[derive(Debug, Clone)]
pub(crate) enum ContextTreeUpdate {
    /// The turns of a branch, or why they could not be read.
    Turns {
        branch_id: String,
        result: Result<Vec<ContextGraphTurn>, String>,
    },
    /// The blocks matching a search, or why the search failed.
    Search {
        query: String,
        result: Result<Vec<ContextGraphSearchHit>, String>,
    },
}

struct TurnNode {
    turn: ContextGraphTurn,
    expanded: bool,
}

struct BranchNode {
    branch: ContextGraphBranch,
    /// The tree lines and the name.
    label: String,
    description: String,
    expanded: bool,
    turns: Option<Result<Vec<TurnNode>, String>>,
}

impl BranchNode {
    fn archived(&self) -> bool {
        self.branch.status == ContextGraphBranchStatus::Archived
    }

    fn thread_id(&self) -> Option<ThreadId> {
        if self.branch.status == ContextGraphBranchStatus::RolledBack {
            return None;
        }
        self.branch
            .thread_id
            .as_deref()
            .and_then(|id| ThreadId::from_string(id).ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Branch(usize),
    /// How many turns the branch inherited, which its parent shows.
    Inherited(usize, usize),
    Turn(usize, usize),
    Block(usize, usize, usize),
    Loading(usize),
    Failed(usize),
    Hit(usize),
}

/// The hits of a search, or why it failed.
type SearchResult = Result<Vec<ContextGraphSearchHit>, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    /// Typing a search.
    Query,
    /// Showing search results.
    Results,
}

pub(crate) struct ContextTreeView {
    thread_id: ThreadId,
    branches: Vec<BranchNode>,
    rows: Vec<Row>,
    state: ScrollState,
    mode: Mode,
    query: String,
    /// The latest search and its results; `None` while it runs.
    search: Option<(String, Option<SearchResult>)>,
    app_event_tx: AppEventSender,
    complete: bool,
}

impl ContextTreeView {
    /// A view of `graph` read for the chat `thread_id`, with `labels` the tree lines and names
    /// and `descriptions` one line about each branch.
    pub(crate) fn new(
        thread_id: ThreadId,
        graph: &ContextGraphReadResponse,
        labels: Vec<String>,
        descriptions: Vec<String>,
        app_event_tx: AppEventSender,
    ) -> Self {
        let branches = graph
            .branches
            .iter()
            .zip(labels)
            .zip(descriptions)
            .map(|((branch, label), description)| BranchNode {
                branch: branch.clone(),
                label,
                description,
                expanded: false,
                turns: None,
            })
            .collect::<Vec<_>>();
        let mut view = Self {
            thread_id,
            branches,
            rows: Vec::new(),
            state: ScrollState::new(),
            mode: Mode::Browse,
            query: String::new(),
            search: None,
            app_event_tx,
            complete: false,
        };
        view.rebuild_rows();
        let current = view.rows.iter().position(
            |row| matches!(row, Row::Branch(index) if view.branches[*index].branch.current),
        );
        view.state.selected_idx = current.or(Some(0));
        view.state.ensure_visible(view.rows.len(), VISIBLE_ROWS);
        view
    }

    fn rebuild_rows(&mut self) {
        let selected = self.selected_row();
        let mut rows = Vec::new();
        if self.mode == Mode::Results {
            if let Some((_, Some(Ok(hits)))) = &self.search {
                rows.extend((0..hits.len()).map(Row::Hit));
            }
        } else {
            for (index, node) in self.branches.iter().enumerate() {
                rows.push(Row::Branch(index));
                if !node.expanded {
                    continue;
                }
                match &node.turns {
                    None => rows.push(Row::Loading(index)),
                    Some(Err(_)) => rows.push(Row::Failed(index)),
                    Some(Ok(turns)) => {
                        let inherited = turns
                            .iter()
                            .filter(|turn| turn.turn.inherited && turn.turn.number > 0)
                            .count();
                        if inherited > 0 {
                            rows.push(Row::Inherited(index, inherited));
                        }
                        for (turn_index, turn) in turns.iter().enumerate() {
                            if turn.turn.inherited {
                                continue;
                            }
                            rows.push(Row::Turn(index, turn_index));
                            if turn.expanded {
                                rows.extend(
                                    (0..turn.turn.blocks.len())
                                        .map(|block| Row::Block(index, turn_index, block)),
                                );
                            }
                        }
                    }
                }
            }
        }
        self.rows = rows;
        let position =
            selected.and_then(|selected| self.rows.iter().position(|row| *row == selected));
        self.state.selected_idx = position.or((!self.rows.is_empty()).then_some(0));
        self.state.clamp_selection(self.rows.len());
        self.state.ensure_visible(self.rows.len(), VISIBLE_ROWS);
    }

    fn selected_row(&self) -> Option<Row> {
        self.state
            .selected_idx
            .and_then(|index| self.rows.get(index))
            .copied()
    }

    fn turn(&self, branch: usize, turn: usize) -> Option<&ContextGraphTurn> {
        match &self.branches.get(branch)?.turns {
            Some(Ok(turns)) => turns.get(turn).map(|node| &node.turn),
            _ => None,
        }
    }

    fn hits(&self) -> &[ContextGraphSearchHit] {
        match &self.search {
            Some((_, Some(Ok(hits)))) => hits,
            _ => &[],
        }
    }

    /// The branch a row belongs to.
    fn branch_of(&self, row: Row) -> Option<&BranchNode> {
        let index = match row {
            Row::Branch(index)
            | Row::Inherited(index, _)
            | Row::Turn(index, _)
            | Row::Block(index, _, _)
            | Row::Loading(index)
            | Row::Failed(index) => index,
            Row::Hit(hit) => {
                let branch_id = &self.hits().get(hit)?.branch_id;
                self.branches
                    .iter()
                    .position(|node| &node.branch.branch_id == branch_id)?
            }
        };
        self.branches.get(index)
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        match delta {
            -1 => self.state.move_up_wrap(len),
            1 => self.state.move_down_wrap(len),
            delta if delta < 0 => self.state.page_up_clamped(len, VISIBLE_ROWS),
            _ => self.state.page_down_clamped(len, VISIBLE_ROWS),
        }
        self.state.ensure_visible(len, VISIBLE_ROWS);
    }

    fn expand(&mut self) {
        match self.selected_row() {
            Some(Row::Branch(index)) => {
                let node = &mut self.branches[index];
                if node.expanded {
                    return;
                }
                node.expanded = true;
                if node.turns.is_none() {
                    self.app_event_tx.send(AppEvent::ContextTreeLoadBranch {
                        thread_id: self.thread_id,
                        branch_id: node.branch.branch_id.clone(),
                    });
                }
            }
            Some(Row::Turn(branch, turn)) => {
                if let Some(Some(Ok(turns))) =
                    self.branches.get_mut(branch).map(|node| &mut node.turns)
                    && let Some(turn) = turns.get_mut(turn)
                {
                    turn.expanded = true;
                }
            }
            _ => return,
        }
        self.rebuild_rows();
    }

    fn collapse(&mut self) {
        let target = match self.selected_row() {
            Some(Row::Branch(index)) => {
                self.branches[index].expanded = false;
                Row::Branch(index)
            }
            Some(Row::Turn(branch, turn)) => {
                let expanded = match &mut self.branches[branch].turns {
                    Some(Ok(turns)) => turns
                        .get_mut(turn)
                        .map(|turn| std::mem::replace(&mut turn.expanded, false))
                        .unwrap_or(false),
                    _ => false,
                };
                if expanded {
                    Row::Turn(branch, turn)
                } else {
                    Row::Branch(branch)
                }
            }
            Some(Row::Block(branch, turn, _)) => Row::Turn(branch, turn),
            Some(Row::Inherited(branch, _) | Row::Loading(branch) | Row::Failed(branch)) => {
                Row::Branch(branch)
            }
            Some(Row::Hit(_)) | None => return,
        };
        self.rebuild_rows();
        if let Some(position) = self.rows.iter().position(|row| *row == target) {
            self.state.selected_idx = Some(position);
            self.state.ensure_visible(self.rows.len(), VISIBLE_ROWS);
        }
    }

    fn close(&mut self) {
        self.complete = true;
    }

    fn switch(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(target_thread_id) = self.branch_of(row).and_then(BranchNode::thread_id) else {
            return;
        };
        self.app_event_tx.send(AppEvent::SwitchContextBranch {
            current_thread_id: self.thread_id,
            target_thread_id,
        });
        self.close();
    }

    /// The blocks a row stands for, with a title for the pager.
    fn preview_target(&self, row: Row) -> Option<(String, Vec<String>)> {
        match row {
            Row::Turn(branch, turn) => {
                let node = self.branches.get(branch)?;
                let turn = self.turn(branch, turn)?;
                Some((
                    format!("{} · turn {}", node.branch.name, turn.number),
                    turn.blocks
                        .iter()
                        .map(|block| block.block_id.clone())
                        .collect(),
                ))
            }
            Row::Block(branch, turn, block) => {
                let node = self.branches.get(branch)?;
                let turn = self.turn(branch, turn)?;
                let block = turn.blocks.get(block)?;
                Some((
                    format!(
                        "{} · turn {} · {}",
                        node.branch.name, turn.number, block.title
                    ),
                    vec![block.block_id.clone()],
                ))
            }
            Row::Hit(hit) => {
                let hit = self.hits().get(hit)?;
                Some((
                    format!(
                        "{} · turn {} · {}",
                        hit.branch_name, hit.turn, hit.block.title
                    ),
                    vec![hit.block.block_id.clone()],
                ))
            }
            Row::Branch(_) | Row::Inherited(..) | Row::Loading(_) | Row::Failed(_) => None,
        }
    }

    fn preview(&mut self) {
        let Some((title, block_ids)) = self.selected_row().and_then(|row| self.preview_target(row))
        else {
            return;
        };
        self.app_event_tx.send(AppEvent::ContextTreePreview {
            thread_id: self.thread_id,
            title,
            block_ids,
        });
    }

    /// Forks the selected turn's branch after that turn.
    fn fork(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let turn = match row {
            Row::Turn(branch, turn) | Row::Block(branch, turn, _) => {
                self.turn(branch, turn).map(|turn| turn.number)
            }
            Row::Hit(hit) => self.hits().get(hit).map(|hit| hit.turn),
            _ => None,
        };
        let Some(turn) = turn
            .filter(|turn| *turn > 0)
            .and_then(|turn| usize::try_from(turn).ok())
        else {
            return;
        };
        let Some(source_thread_id) = self.branch_of(row).and_then(BranchNode::thread_id) else {
            return;
        };
        self.app_event_tx.send(AppEvent::ContextTreeFork {
            source_thread_id,
            turn,
        });
        self.close();
    }

    fn merge(&mut self, mode: ContextGraphMergeMode) {
        let Some(node) = self.selected_row().and_then(|row| self.branch_of(row)) else {
            return;
        };
        if node.branch.current {
            return;
        }
        let mut args = vec![node.branch.name.clone()];
        if mode == ContextGraphMergeMode::Reference {
            args.push("--reference".to_string());
        }
        self.app_event_tx.send(AppEvent::MergeCurrentSession {
            primary_thread_id: self.thread_id,
            args,
        });
        self.close();
    }

    fn toggle_archived(&mut self) {
        let Some(node) = self.selected_row().and_then(|row| self.branch_of(row)) else {
            return;
        };
        if node.branch.current || node.branch.thread_id.is_none() {
            return;
        }
        self.app_event_tx.send(AppEvent::ContextBranchUpdate {
            thread_id: self.thread_id,
            branch: Some(node.branch.name.clone()),
            name: None,
            purpose: None,
            archived: Some(!node.archived()),
            reopen_tree: true,
        });
        self.close();
    }

    fn start_search(&mut self) {
        self.mode = Mode::Query;
        self.query.clear();
    }

    fn run_search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.mode = Mode::Browse;
            return;
        }
        self.search = Some((query.clone(), None));
        self.mode = Mode::Results;
        self.rebuild_rows();
        self.app_event_tx.send(AppEvent::ContextTreeSearch {
            thread_id: self.thread_id,
            query,
        });
    }

    fn back_to_browse(&mut self) {
        self.mode = Mode::Browse;
        self.rebuild_rows();
    }

    fn row_line(&self, row: Row, width: usize) -> Line<'static> {
        let indent = |branch: usize| " ".repeat(self.branches[branch].branch.depth as usize * 2);
        let mut spans: Vec<Span<'static>> = Vec::new();
        match row {
            Row::Branch(index) => {
                let node = &self.branches[index];
                let marker = if node.expanded { "▾ " } else { "▸ " };
                spans.push(marker.dim());
                let name = node.label.clone();
                let mut style = Style::default();
                if node.branch.current {
                    style = style.bold();
                }
                if node.archived() || node.thread_id().is_none() {
                    style = style.dim();
                }
                spans.push(Span::styled(name, style));
                if node.branch.current {
                    spans.push(" (current)".cyan());
                }
                spans.push(format!("  {}", node.description).dim());
            }
            Row::Inherited(index, count) => {
                let turns = if count == 1 { "turn" } else { "turns" };
                spans.push(
                    format!(
                        "{}    inherits {count} {turns} from where it forked",
                        indent(index)
                    )
                    .dim(),
                );
            }
            Row::Turn(index, turn) => {
                if let Some(turn) = self.turn(index, turn) {
                    let expanded = matches!(
                        &self.branches[index].turns,
                        Some(Ok(turns)) if turns.iter().any(|node| node.expanded && node.turn.number == turn.number)
                    );
                    spans.push(format!("{}    ", indent(index)).into());
                    spans.push(if expanded { "▾ ".dim() } else { "▸ ".dim() });
                    spans.push(format!("{}. ", turn.number).dim());
                    spans.push(turn.title.clone().into());
                    let tokens: u32 = turn.blocks.iter().map(|block| block.tokens).sum();
                    spans.push(
                        format!("  {} blocks, {}", turn.blocks.len(), tokens_label(tokens)).dim(),
                    );
                }
            }
            Row::Block(index, turn, block) => {
                if let Some(block) = self
                    .turn(index, turn)
                    .and_then(|turn| turn.blocks.get(block))
                {
                    spans.push(format!("{}        ", indent(index)).into());
                    spans.push(format!("{:<10}", short_kind(&block.kind)).magenta());
                    spans.push(block.title.clone().into());
                    spans.push(format!("  {}", tokens_label(block.tokens)).dim());
                }
            }
            Row::Loading(index) => spans.push(format!("{}    loading…", indent(index)).dim()),
            Row::Failed(index) => {
                let error = match &self.branches[index].turns {
                    Some(Err(error)) => error.clone(),
                    _ => String::new(),
                };
                spans.push(format!("{}    could not load: {error}", indent(index)).red());
            }
            Row::Hit(hit) => {
                if let Some(hit) = self.hits().get(hit) {
                    spans.push(format!("{} · turn {} ", hit.branch_name, hit.turn).cyan());
                    spans.push(format!("{:<10}", short_kind(&hit.block.kind)).magenta());
                    spans.push(hit.block.title.clone().into());
                    if !hit.summary.is_empty() && hit.summary != hit.block.title {
                        spans.push(format!(" — {}", hit.summary).dim());
                    }
                }
            }
        }
        truncate_line(Line::from(spans), width)
    }

    fn hint_lines(&self) -> Vec<Line<'static>> {
        let lines: &[&str] = match self.mode {
            Mode::Browse => &[
                "→ expand · ← collapse · Enter switch or read · p read · f fork after turn",
                "m merge · r reference · a archive · / search · Esc close",
            ],
            Mode::Query => &["Enter search the cards of every branch · Esc back"],
            Mode::Results => &["Enter or p read · f fork after its turn · / new search · Esc back"],
        };
        lines.iter().map(|line| Line::from(line.dim())).collect()
    }

    fn header_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("Conversation tree".bold())];
        match self.mode {
            Mode::Browse => lines.push(Line::from(
                "Forks are indented under the branch they left.".dim(),
            )),
            Mode::Query => lines.push(Line::from(vec![
                "Search: ".dim(),
                self.query.clone().into(),
                "▏".dim(),
            ])),
            Mode::Results => {
                let status = match &self.search {
                    Some((query, None)) => format!("Searching for \"{query}\"…"),
                    Some((query, Some(Ok(hits)))) => match hits.len() {
                        1 => format!("1 block matches \"{query}\""),
                        hits => format!("{hits} blocks match \"{query}\""),
                    },
                    Some((query, Some(Err(error)))) => {
                        format!("Searching for \"{query}\" failed: {error}")
                    }
                    None => String::new(),
                };
                lines.push(Line::from(status.dim()));
            }
        }
        lines
    }
}

fn short_kind(kind: &str) -> &str {
    match kind {
        "user_message" => "you",
        "assistant_message" => "answer",
        "tool_exchange" => "tool",
        "inter_agent_message" => "agent",
        "context_fragment" => "context",
        other => other,
    }
}

fn tokens_label(tokens: u32) -> String {
    if tokens >= 1000 {
        format!("{:.1}k tokens", f64::from(tokens) / 1000.0)
    } else {
        format!("{tokens} tokens")
    }
}

/// `line` cut to `width` columns.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let text: String = span
            .content
            .chars()
            .take_while(|ch| {
                let ch_width = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
                if used + ch_width > width {
                    return false;
                }
                used += ch_width;
                true
            })
            .collect();
        let cut = text.chars().count() < span.content.chars().count();
        spans.push(Span::styled(text, span.style));
        if cut {
            break;
        }
    }
    Line::from(spans)
}

impl BottomPaneView for ContextTreeView {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL)
            || key_event.modifiers.contains(KeyModifiers::ALT)
        {
            return;
        }
        if self.mode == Mode::Query {
            match key_event.code {
                KeyCode::Enter => self.run_search(),
                KeyCode::Esc => self.back_to_browse(),
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(ch) => self.query.push(ch),
                _ => {}
            }
            return;
        }
        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(VISIBLE_ROWS as isize)),
            KeyCode::PageDown => self.move_by(VISIBLE_ROWS as isize),
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('p') => self.preview(),
            KeyCode::Char('f') => self.fork(),
            KeyCode::Esc if self.mode == Mode::Results => self.back_to_browse(),
            KeyCode::Esc => self.close(),
            KeyCode::Enter if self.mode == Mode::Results => self.preview(),
            KeyCode::Enter => match self.selected_row() {
                Some(Row::Branch(_)) => self.switch(),
                Some(Row::Turn(..) | Row::Block(..)) => self.preview(),
                _ => {}
            },
            _ if self.mode == Mode::Results => {}
            KeyCode::Right | KeyCode::Char('l') => self.expand(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse(),
            KeyCode::Char('m') => self.merge(ContextGraphMergeMode::Fold),
            KeyCode::Char('r') => self.merge(ContextGraphMergeMode::Reference),
            KeyCode::Char('a') => self.toggle_archived(),
            _ => {}
        }
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.close();
        CancellationEvent::Handled
    }

    fn view_id(&self) -> Option<&'static str> {
        Some(CONTEXT_TREE_VIEW_ID)
    }

    fn update_context_tree(&mut self, update: &ContextTreeUpdate) -> bool {
        match update {
            ContextTreeUpdate::Turns { branch_id, result } => {
                let Some(node) = self
                    .branches
                    .iter_mut()
                    .find(|node| &node.branch.branch_id == branch_id)
                else {
                    return false;
                };
                node.turns = Some(result.clone().map(|turns| {
                    turns
                        .into_iter()
                        .map(|turn| TurnNode {
                            turn,
                            expanded: false,
                        })
                        .collect()
                }));
            }
            ContextTreeUpdate::Search { query, result } => {
                if !matches!(&self.search, Some((searched, _)) if searched == query) {
                    return false;
                }
                self.search = Some((query.clone(), Some(result.clone())));
            }
        }
        self.rebuild_rows();
        true
    }
}

impl Renderable for ContextTreeView {
    fn desired_height(&self, _width: u16) -> u16 {
        let rows = self.rows.len().clamp(1, VISIBLE_ROWS);
        // Header, a blank line, the rows, a blank line and the hints.
        u16::try_from(self.header_lines().len() + rows + 2 + self.hint_lines().len())
            .unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        Block::default()
            .style(user_message_style())
            .render(area, buf);
        let width = usize::from(area.width.saturating_sub(4));
        let mut lines = self.header_lines();
        lines.push(Line::default());
        if self.rows.is_empty() {
            let empty = match self.mode {
                Mode::Results => "No blocks match.",
                _ => "No branches.",
            };
            lines.push(Line::from(empty.dim()));
        }
        let selected = self.state.selected_idx;
        for (index, row) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.state.scroll_top)
            .take(VISIBLE_ROWS)
        {
            let mut line = self.row_line(*row, width.saturating_sub(2));
            let prefix = if Some(index) == selected {
                "› "
            } else {
                "  "
            };
            line.spans.insert(0, prefix.into());
            if Some(index) == selected {
                line = line.style(super::picker_style::selection_style());
            }
            lines.push(line);
        }
        lines.push(Line::default());
        lines.extend(self.hint_lines());
        let inner = Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: area.height,
        };
        Paragraph::new(lines).render(inner, buf);
    }
}

#[cfg(test)]
#[path = "context_tree_view_tests.rs"]
mod tests;
