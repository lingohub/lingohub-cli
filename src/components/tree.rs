// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::Component;
use crate::action::Action;
use crate::api::{ProjectData, StackData};
use crate::colors::{BLUE_DARK, BLUE_LIGHT, GREY};

// ~~~~~~~~~~ Types <<<

/// Number of detail lines rendered beneath an expanded stack row (before its projects).
const STACK_DETAIL_ROWS: usize = 7;

/// Number of detail lines rendered beneath an expanded project row.
const PROJECT_DETAIL_ROWS: usize = 5;

/// A navigable row in the flattened tree.
#[derive(Clone, Copy)]
enum Row {
    /// Top-level stack entry; carries the stack's index.
    Stack(usize),
    /// Project nested under a stack; carries (stack_idx, local_project_idx within that stack).
    Project(usize, usize),
}

/// Loaded tree state, available once both stacks and projects have arrived.
struct Inner {
    stacks: Vec<StackData>,
    projects: Vec<ProjectData>,
    /// `groups[ti]` holds indices into `projects` belonging to stack `ti`.
    groups: Vec<Vec<usize>>,
    expanded_stacks: Vec<bool>,
    /// `expanded_projects[ti][pi]` — whether the project's detail rows are shown.
    expanded_projects: Vec<Vec<bool>>,
}

enum State {
    Loading,
    Ready(Inner),
}

// >>>

// ~~~~~~~~~~ Component <<<

/// TUI component that renders the workspace's stacks and their projects as a
/// two-level navigable tree.
///
/// Both datasets arrive independently via [`set_stacks`] / [`set_projects`];
/// the component waits until both are available before switching to the
/// ready state.
pub struct TreeComponent {
    state: State,
    pending_stacks: Option<Vec<StackData>>,
    pending_projects: Option<Vec<ProjectData>>,
    cursor: usize,
    scroll_offset: usize,
    last_height: usize,
}

impl TreeComponent {
    pub fn new() -> Self {
        Self {
            state: State::Loading,
            pending_stacks: None,
            pending_projects: None,
            cursor: 0,
            scroll_offset: 0,
            last_height: 0,
        }
    }

    pub fn set_loading(&mut self) {
        self.pending_stacks = None;
        self.pending_projects = None;
        self.state = State::Loading;
        self.cursor = 0;
        self.scroll_offset = 0;
    }

    pub fn set_stacks(&mut self, stacks: Vec<StackData>) {
        self.pending_stacks = Some(stacks);
        self.try_combine();
    }

    pub fn set_projects(&mut self, projects: Vec<ProjectData>) {
        self.pending_projects = Some(projects);
        self.try_combine();
    }

    /// Transition to `Ready` once both pending datasets are available.
    fn try_combine(&mut self) {
        if self.pending_stacks.is_none() || self.pending_projects.is_none() {
            return;
        }
        let stacks = self.pending_stacks.take().unwrap();
        let projects = self.pending_projects.take().unwrap();
        let groups = compute_groups(&stacks, &projects);
        let expanded_projects = groups.iter().map(|g| vec![false; g.len()]).collect();
        let expanded_stacks = vec![false; stacks.len()];
        self.cursor = 0;
        self.scroll_offset = 0;
        self.state = State::Ready(Inner {
            stacks,
            projects,
            groups,
            expanded_stacks,
            expanded_projects,
        });
    }
}

impl Component for TreeComponent {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(Span::styled(
                " Stacks & Projects ",
                Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(Alignment::Left)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(GREY));

        let inner_area = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        frame.render_widget(block, area);

        let [content_area, hint_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner_area);

        self.last_height = content_area.height as usize;

        match &self.state {
            State::Loading => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "Fetching...",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
                    ))
                    .alignment(Alignment::Center),
                    content_area,
                );
            }
            State::Ready(inner) => {
                let rows = compute_visible_rows(&inner.groups, &inner.expanded_stacks);
                let lines = render_lines(
                    &inner.stacks,
                    &inner.projects,
                    &inner.groups,
                    &inner.expanded_stacks,
                    &inner.expanded_projects,
                    &rows,
                    self.cursor,
                );
                frame.render_widget(
                    Paragraph::new(lines).scroll((self.scroll_offset as u16, 0)),
                    content_area,
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  j/k  Navigate   \u{2022}   Enter  Expand",
                        Style::default().fg(GREY),
                    ))
                    .alignment(Alignment::Center),
                    hint_area,
                );
            }
        }
    }

    fn update(&mut self, action: &Action) -> Option<Action> {
        let rows = match &self.state {
            State::Ready(inner) => compute_visible_rows(&inner.groups, &inner.expanded_stacks),
            _ => return None,
        };

        if rows.is_empty() {
            return None;
        }

        match action {
            Action::SelectNext => {
                if self.cursor + 1 < rows.len() {
                    self.cursor += 1;
                }
                let line = self.visual_line_of_cursor(&rows);
                self.adjust_scroll(line);
            }
            Action::SelectPrev => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                let line = self.visual_line_of_cursor(&rows);
                self.adjust_scroll(line);
            }
            Action::ToggleExpand => {
                // Copy the target row before releasing the borrow on `self.state`.
                let row = rows[self.cursor];
                drop(rows);
                if let State::Ready(inner) = &mut self.state {
                    match row {
                        Row::Stack(ti) => {
                            if let Some(e) = inner.expanded_stacks.get_mut(ti) {
                                *e = !*e;
                            }
                        }
                        Row::Project(ti, pi) => {
                            if let Some(e) = inner.expanded_projects.get_mut(ti).and_then(|v| v.get_mut(pi)) {
                                *e = !*e;
                            }
                        }
                    }
                    // Recompute after toggling so ensure_visible uses the updated layout.
                    let new_rows = compute_visible_rows(&inner.groups, &inner.expanded_stacks);
                    let line = visual_line_of(
                        self.cursor,
                        &new_rows,
                        &inner.expanded_stacks,
                        &inner.expanded_projects,
                    );
                    // Inline scroll adjustment — field borrows let us access self.* alongside inner.
                    if !new_rows.is_empty() && self.last_height > 0 {
                        if line < self.scroll_offset {
                            self.scroll_offset = line;
                        } else if line + 1 > self.scroll_offset + self.last_height {
                            self.scroll_offset = (line + 1).saturating_sub(self.last_height);
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }
}

impl TreeComponent {
    /// Visual line index of the current cursor row (counts expanded project detail lines).
    fn visual_line_of_cursor(&self, rows: &[Row]) -> usize {
        match &self.state {
            State::Ready(inner) => visual_line_of(
                self.cursor,
                rows,
                &inner.expanded_stacks,
                &inner.expanded_projects,
            ),
            _ => 0,
        }
    }

    /// Adjust `scroll_offset` so that `line` is within the visible window.
    fn adjust_scroll(&mut self, line: usize) {
        if self.last_height == 0 {
            return;
        }
        if line < self.scroll_offset {
            self.scroll_offset = line;
        } else if line + 1 > self.scroll_offset + self.last_height {
            self.scroll_offset = (line + 1).saturating_sub(self.last_height);
        }
    }
}

// >>>

// ~~~~~~~~~~ Pure helpers <<<

/// Group project indices by stack. Projects whose stack is not loaded are omitted.
fn compute_groups(stacks: &[StackData], projects: &[ProjectData]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); stacks.len()];
    for (pi, project) in projects.iter().enumerate() {
        if let Some(ti) = stacks.iter().position(|t| t.id == project.stack_id) {
            groups[ti].push(pi);
        }
    }
    groups
}

/// Flat list of navigable rows given the current expansion state.
fn compute_visible_rows(groups: &[Vec<usize>], expanded_stacks: &[bool]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (ti, group) in groups.iter().enumerate() {
        rows.push(Row::Stack(ti));
        if expanded_stacks.get(ti).copied().unwrap_or(false) {
            for pi in 0..group.len() {
                rows.push(Row::Project(ti, pi));
            }
        }
    }
    rows
}

/// Number of visual lines that precede `row_idx`, accounting for expanded
/// stack detail rows and project detail rows.
fn visual_line_of(
    row_idx: usize,
    rows: &[Row],
    expanded_stacks: &[bool],
    expanded_projects: &[Vec<bool>],
) -> usize {
    let mut line = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if i == row_idx {
            break;
        }
        line += 1;
        match row {
            Row::Stack(ti) => {
                if expanded_stacks.get(*ti).copied().unwrap_or(false) {
                    line += STACK_DETAIL_ROWS;
                }
            }
            Row::Project(ti, pi) => {
                if expanded_projects
                    .get(*ti)
                    .and_then(|v| v.get(*pi))
                    .copied()
                    .unwrap_or(false)
                {
                    line += PROJECT_DETAIL_ROWS;
                }
            }
        }
    }
    line
}

/// Produce the complete list of `Line`s to be rendered for the current tree state.
fn render_lines(
    stacks: &[StackData],
    projects: &[ProjectData],
    groups: &[Vec<usize>],
    expanded_stacks: &[bool],
    expanded_projects: &[Vec<bool>],
    rows: &[Row],
    cursor: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for (row_idx, row) in rows.iter().enumerate() {
        let selected = row_idx == cursor;

        match row {
            Row::Stack(ti) => {
                let stack = &stacks[*ti];
                let expanded = expanded_stacks.get(*ti).copied().unwrap_or(false);
                let n = groups[*ti].len();
                let arrow = if expanded { "\u{25bc}" } else { "\u{25b6}" };
                let suffix = format!(
                    "{}  ({} project{})",
                    stack.stack_type,
                    n,
                    if n == 1 { "" } else { "s" }
                );

                let (arrow_sty, name_sty, meta_sty) = if selected {
                    (
                        Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
                        Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
                        Style::default().fg(BLUE_DARK).add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        Style::default().fg(BLUE_DARK),
                        Style::default().fg(Color::White),
                        Style::default().fg(GREY),
                    )
                };

                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", arrow), arrow_sty),
                    Span::styled(format!("{:<32}", stack.name.clone()), name_sty),
                    Span::styled(suffix, meta_sty),
                ]));

                if expanded {
                    let w = 14usize;
                    let char_str = stack
                        .character_count
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "\u{2014}".to_owned());
                    let word_str = stack
                        .word_count
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "\u{2014}".to_owned());
                    let details: [(&str, String); STACK_DETAIL_ROWS] = [
                        ("ID", stack.id.clone()),
                        ("Color", stack.color.clone()),
                        ("Characters", char_str),
                        ("Words", word_str),
                        ("Counts Updated", stack.counts_updated_at.clone()),
                        ("Created", stack.created_at.clone()),
                        ("Updated", stack.updated_at.clone()),
                    ];
                    for (label, value) in &details {
                        lines.push(Line::from(vec![
                            Span::styled(format!("   {:<w$}  ", label, w = w), Style::default().fg(GREY)),
                            Span::styled(value.clone(), Style::default().fg(Color::White)),
                        ]));
                    }
                }
            }

            Row::Project(ti, pi) => {
                let proj = &projects[groups[*ti][*pi]];
                let expanded = expanded_projects
                    .get(*ti)
                    .and_then(|v| v.get(*pi))
                    .copied()
                    .unwrap_or(false);
                let arrow = if expanded { "\u{25bc}" } else { "\u{25b6}" };

                let (arrow_sty, title_sty, type_sty) = if selected {
                    (
                        Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
                        Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
                        Style::default().fg(BLUE_DARK).add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        Style::default().fg(BLUE_DARK),
                        Style::default().fg(Color::White),
                        Style::default().fg(GREY),
                    )
                };

                lines.push(Line::from(vec![
                    Span::styled(format!("     {} ", arrow), arrow_sty),
                    Span::styled(format!("{:<28}", proj.name.clone()), title_sty),
                    Span::styled(proj.project_type.clone(), type_sty),
                ]));

                if expanded {
                    let w = 12usize;
                    let details: [(&str, String); PROJECT_DETAIL_ROWS] = [
                        ("ID", proj.id.clone()),
                        (
                            "Website",
                            proj.website.as_deref().unwrap_or("\u{2014}").to_owned(),
                        ),
                        (
                            "AI Context",
                            proj.ai_context.as_deref().unwrap_or("\u{2014}").to_owned(),
                        ),
                        ("Created", proj.created_at.clone()),
                        ("Updated", proj.updated_at.clone()),
                    ];
                    for (label, value) in &details {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("       {:<w$}  ", label, w = w),
                                Style::default().fg(GREY),
                            ),
                            Span::styled(value.clone(), Style::default().fg(Color::White)),
                        ]));
                    }
                }
            }
        }
    }

    lines
}

// >>>

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    #[test]
    fn dashboard_renders_current_stack_and_project_wire_models() {
        let stack: StackData = serde_json::from_value(json!({"id":"stack_a","name":"Current stack","color":"#123456","type":"PUBLIC","character_count":null,"word_count":42,"counts_updated_at":"2026-09-29T00:00:00.123Z","created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-29T00:00:00.123Z"})).unwrap();
        let project: ProjectData = serde_json::from_value(json!({"id":"pr_a","stack_id":"stack_a","name":"Current project","type":"STANDARD_JSON","website":null,"ai_context":null,"created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-29T00:00:00.123Z"})).unwrap();
        let mut tree = TreeComponent::new();
        tree.set_stacks(vec![stack]);
        tree.set_projects(vec![project]);
        tree.update(&Action::ToggleExpand);
        let mut terminal = Terminal::new(TestBackend::new(100, 35)).unwrap();
        terminal.draw(|frame| tree.draw(frame, frame.area())).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Stacks & Projects"));
        assert!(rendered.contains("Current stack"));
        assert!(rendered.contains("Current project"));
        assert!(rendered.contains("STANDARD_JSON"));
    }

    fn render(tree: &mut TreeComponent, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
        terminal.draw(|frame| tree.draw(frame, frame.area())).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn expanded_project_shows_five_details_and_keeps_the_next_stack_reachable() {
        let stack = |id: &str, name: &str| -> StackData {
            serde_json::from_value(json!({"id":id,"name":name,"color":"#123456","type":"PUBLIC","character_count":null,"word_count":42,"counts_updated_at":"2026-09-29T00:00:00.123Z","created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-29T00:00:00.123Z"})).unwrap()
        };
        let project: ProjectData = serde_json::from_value(json!({"id":"pr_a","stack_id":"stack_a","name":"Current project","type":"STANDARD_JSON","website":"example.com","ai_context":"Checkout","created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"})).unwrap();
        let mut tree = TreeComponent::new();
        tree.set_stacks(vec![
            stack("stack_a", "First stack"),
            stack("stack_b", "Second stack"),
        ]);
        tree.set_projects(vec![project]);

        // Content height 11: the first stack (1 + 7 detail lines), the project (1 + 5) and the second stack
        // need 15 lines, so reaching the second stack scrolls by the exact number of project detail rows.
        render(&mut tree, 14);
        tree.update(&Action::ToggleExpand);
        tree.update(&Action::SelectNext);
        tree.update(&Action::ToggleExpand);
        let expanded = render(&mut tree, 30);
        for detail in ["pr_a", "example.com", "Checkout", "2026-09-30T00:00:00.123Z"] {
            assert!(expanded.contains(detail), "{detail}");
        }
        assert!(!expanded.contains("Open Source"));

        render(&mut tree, 14);
        tree.update(&Action::SelectNext);
        assert!(render(&mut tree, 14).contains("Second stack"));
    }
}

// vim: foldmethod=marker foldmarker=<<<,>>>
