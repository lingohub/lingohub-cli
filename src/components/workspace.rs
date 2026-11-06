// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::Component;
use crate::api::WorkspaceData;
use crate::colors::GREY;
use crate::components::draw_title;

// ~~~~~~~~~~ Internal Data <<<

enum State {
    Loading,
    Loaded(WorkspaceData),
}

pub struct WorkspaceComponent {
    state: State,
}

impl WorkspaceComponent {
    pub fn new() -> Self {
        Self {
            state: State::Loading,
        }
    }

    pub fn set_data(&mut self, data: WorkspaceData) {
        self.state = State::Loaded(data);
    }

    pub fn set_loading(&mut self) {
        self.state = State::Loading;
    }
}

impl Component for WorkspaceComponent {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let inner_area = draw_title(frame, area, "Workspace");
        match &self.state {
            State::Loading => draw_loading(frame, inner_area),
            State::Loaded(data) => draw_loaded(frame, inner_area, data),
        }
    }
}
// >>>

// ~~~~~~~~~~ Draw Functions <<<

fn draw_loading(f: &mut Frame, area: Rect) {
    let [_, center, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)]).areas(area);

    f.render_widget(
        Paragraph::new(Span::styled(
            "Fetching workspace...",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        ))
        .alignment(Alignment::Center),
        center,
    );
}

fn draw_loaded(f: &mut Frame, area: Rect, data: &WorkspaceData) {
    let [content_area, hint_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let rows = [
        ("ID", data.id.as_str()),
        ("Name", data.name.as_str()),
        ("Created", data.created_at.as_str()),
        ("Updated", data.updated_at.as_str()),
    ];
    let label_width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    let lines: Vec<Line> = rows
        .iter()
        .map(|&(label, value)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<width$}  ", label, width = label_width),
                    Style::default().fg(GREY),
                ),
                Span::styled(value, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content_area);

    f.render_widget(
        Paragraph::new(Span::styled(
            "  r  Refresh   •   q  Quit",
            Style::default().fg(GREY),
        ))
        .alignment(Alignment::Center),
        hint_area,
    );
}
// >>>

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    #[test]
    fn workspace_panel_shows_retained_fields_without_last_activity() {
        let data: WorkspaceData = serde_json::from_value(json!({"id":"space_a","name":"Example workspace","created_at":"2026-09-29T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"})).unwrap();
        let mut workspace = WorkspaceComponent::new();
        workspace.set_data(data);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| workspace.draw(frame, frame.area()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for value in [
            "space_a",
            "Example workspace",
            "2026-09-29T00:00:00.123Z",
            "2026-09-30T00:00:00.123Z",
        ] {
            assert!(rendered.contains(value), "{value}");
        }
        assert!(!rendered.contains("Last Active"));
    }
}
// vim: foldmethod=marker foldmarker=<<<,>>>
