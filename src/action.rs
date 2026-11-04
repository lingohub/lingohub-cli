// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crossterm::event::KeyCode;

use crate::tui_app::{App, AppState};

#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    // Key input editing
    Submit,
    DeleteChar,
    AppendChar(char),
    AppendString(String),
    // Navigation
    BackToInput,
    DismissError,
    // Workspace screen
    Refresh,
    // Stack list navigation
    SelectNext,
    SelectPrev,
    ToggleExpand,
}

pub fn map_key(app: &App, code: KeyCode) -> Option<Action> {
    Some(match &app.state {
        AppState::ApiKeyInput => match code {
            KeyCode::Esc => Action::Quit,
            KeyCode::Enter => Action::Submit,
            KeyCode::Backspace => Action::DeleteChar,
            KeyCode::Char(c) => Action::AppendChar(c),
            _ => return None,
        },
        AppState::Workspace => match code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('j') | KeyCode::Down => Action::SelectNext,
            KeyCode::Char('k') | KeyCode::Up => Action::SelectPrev,
            KeyCode::Char('-') | KeyCode::Enter => Action::ToggleExpand,
            KeyCode::Backspace if !app.key_input.from_env => Action::BackToInput,
            _ => return None,
        },
        AppState::Erred => match code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace | KeyCode::Enter => Action::DismissError,
            _ => return None,
        },
    })
}
