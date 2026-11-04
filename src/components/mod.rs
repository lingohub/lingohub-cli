// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders};

use crate::action::Action;
use crate::colors::GREY;

pub mod api_key_input;
pub mod tree;
pub mod workspace;

/// Contract every TUI screen or panel must satisfy.
pub trait Component {
    fn draw(&mut self, frame: &mut Frame, area: Rect);

    fn update(&mut self, _action: &Action) -> Option<Action> {
        None
    }
}

pub fn draw_title(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let block = Block::default()
        .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREY));
    frame.render_widget(block, area);

    area.inner(Margin::new(1, 1))
}

// vim: foldmethod=marker foldmarker=<<<,>>>
