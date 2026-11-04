// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::Component;
use crate::action::Action;
use crate::colors::{BLUE_DARK, BLUE_LIGHT, GREY};

/// API key input screen — collects the user's token before connecting.
pub struct KeyInput {
    pub value: String,
    /// True when the key was supplied externally (env var or CLI flag);
    /// hides the "Backspace to go back" hint on the error screen.
    pub from_env: bool,
}

impl KeyInput {
    pub fn new() -> Self {
        Self {
            value: String::new(),
            from_env: false,
        }
    }

    /// Returns a masked representation of the key safe for display.
    pub fn masked(&self) -> String {
        let s = &self.value;
        if s.is_empty() {
            return String::new();
        }
        if s.len() < 16 {
            s.to_owned()
        } else {
            format!(
                "{}...{}",
                &s[..s.len().min(8)],
                &s[s.len().saturating_sub(8).max(8)..]
            )
        }
    }
}

impl Component for KeyInput {
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let [_, title_area, input_area, hint_area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Welcome to the LingoHub TUI",
                    Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(Span::styled(
                    "Enter your API key, or set LINGOHUB_API_KEY and restart.",
                    Style::default().fg(Color::Gray),
                )),
            ])
            .alignment(Alignment::Center),
            title_area,
        );

        let (key_text, key_color) = if self.value.is_empty() {
            (String::from("lh-api_..."), GREY)
        } else {
            (self.masked(), Color::White)
        };

        frame.render_widget(
            Paragraph::new(Span::styled(key_text, Style::default().fg(key_color))).block(
                Block::default()
                    .title(" API Key ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BLUE_DARK)),
            ),
            input_area,
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "  Enter  Connect   •   Esc  Quit",
                    Style::default().fg(GREY),
                )),
                Line::from(Span::styled(
                    "  Paste with Ctrl+V or your terminal's paste shortcut",
                    Style::default().fg(GREY),
                )),
            ])
            .alignment(Alignment::Center),
            hint_area,
        );
    }

    fn update(&mut self, action: &Action) -> Option<Action> {
        match action {
            Action::AppendChar(c) => self.value.push(*c),
            Action::AppendString(s) => self.value.push_str(s),
            Action::DeleteChar => {
                self.value.pop();
            }
            _ => {}
        }
        None
    }
}
