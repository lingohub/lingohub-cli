// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::action::{Action, map_key};
use crate::api::{ProjectData, StackData, WorkspaceData, fetch_projects, fetch_stacks, fetch_workspace};
use crate::colors::{BLUE_LIGHT, GREY};
use crate::components::Component;
use crate::components::api_key_input::KeyInput;
use crate::components::tree::TreeComponent;
use crate::components::workspace::WorkspaceComponent;
use crate::error::Error;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Margin};
use ratatui::style::palette::material::RED;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, TitlePosition};
use ratatui::{Frame, Terminal};
use std::io;
use tokio::sync::mpsc;

// ~~~~~~~~~~ App <<<

/// Top-level application state used for screen routing.
#[derive(Debug, Copy)]
pub enum AppState {
    ApiKeyInput,
    Workspace,
    Erred,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        *self
    }
}

/// Messages sent from background fetch tasks to the main loop.
pub enum BgMessage {
    Workspace(Result<WorkspaceData, Error>),
    Stacks(Result<Vec<StackData>, Error>),
    Projects(Result<Vec<ProjectData>, Error>),
}

pub struct App {
    pub state: AppState,
    pub key_input: KeyInput,
    pub workspace: WorkspaceComponent,
    pub tree: TreeComponent,
    pub error: Option<Error>,
    base_url: String,
    previous_state: Option<AppState>,
    tx: mpsc::UnboundedSender<BgMessage>,
    rx: mpsc::UnboundedReceiver<BgMessage>,
}

impl App {
    pub fn new(base_url: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            state: AppState::ApiKeyInput,
            key_input: KeyInput::new(),
            workspace: WorkspaceComponent::new(),
            tree: TreeComponent::new(),
            error: None,
            base_url,
            previous_state: None,
            tx,
            rx,
        }
    }

    /// Create an app with a pre-supplied API key and immediately begin fetching.
    /// Treats the key as externally provided, skipping the input screen.
    pub fn with_key(key: String, base_url: String) -> Self {
        let mut app = Self::new(base_url);
        app.key_input.value = key;
        app.key_input.from_env = true;
        app.workspace = WorkspaceComponent::new();
        app.submit();
        app
    }

    pub fn submit(&mut self) {
        let key = self.key_input.value.trim().to_owned();
        if key.is_empty() {
            return;
        }
        self.state = AppState::Workspace;
        self.workspace.set_loading();
        self.tree.set_loading();
        self.spawn_fetch_workspace(key.clone());
        self.spawn_fetch_stacks(key.clone());
        self.spawn_fetch_projects(key);
    }

    pub fn refresh(&mut self) {
        if matches!(self.state, AppState::Workspace) {
            let key = self.key_input.value.trim().to_owned();
            self.workspace.set_loading();
            self.tree.set_loading();
            self.spawn_fetch_workspace(key.clone());
            self.spawn_fetch_stacks(key.clone());
            self.spawn_fetch_projects(key);
        }
    }

    pub fn set_error(&mut self, error: Error) {
        self.previous_state = self.previous_state.or(Some(self.state));
        self.state = AppState::Erred;
        self.error = Some(error);
    }

    pub fn clear_error(&mut self) {
        self.state = self.previous_state.unwrap();
        self.error = None;
    }

    pub fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                BgMessage::Workspace(Ok(data)) => self.workspace.set_data(data),
                BgMessage::Workspace(Err(e)) => self.set_error(e),
                BgMessage::Stacks(Ok(stacks)) => self.tree.set_stacks(stacks),
                BgMessage::Stacks(Err(e)) => self.set_error(e),
                BgMessage::Projects(Ok(projects)) => self.tree.set_projects(projects),
                BgMessage::Projects(Err(e)) => self.set_error(e),
            }
        }
    }

    fn spawn_fetch_workspace(&self, key: String) {
        let tx = self.tx.clone();
        let base = self.base_url.clone();
        tokio::spawn(async move {
            let _ = tx.send(BgMessage::Workspace(fetch_workspace(&base, &key).await));
        });
    }

    fn spawn_fetch_stacks(&self, key: String) {
        let tx = self.tx.clone();
        let base = self.base_url.clone();
        tokio::spawn(async move {
            let result = fetch_stacks(&base, &key).await;
            let _ = tx.send(BgMessage::Stacks(result));
        });
    }

    fn spawn_fetch_projects(&self, key: String) {
        let tx = self.tx.clone();
        let base = self.base_url.clone();
        tokio::spawn(async move {
            let result = fetch_projects(&base, &key).await;
            let _ = tx.send(BgMessage::Projects(result));
        });
    }
}
// >>>

// ~~~~~~~~~~ Launch App <<<

pub fn launch_tui(api_key: Option<String>, base_url: String) -> io::Result<()> {
    let mut app = match api_key {
        Some(key) => App::with_key(key, base_url),
        None => App::new(base_url),
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = tui_run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
    )?;
    terminal.show_cursor()?;

    result
}
// >>>

// ~~~~~~~~~~ Run app <<<

fn tui_run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        app.poll();
        terminal.draw(|f| draw_app(f, app))?;

        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }

        let action = match event::read()? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(Action::Quit)
            }
            Event::Paste(text) if matches!(app.state, AppState::ApiKeyInput) => {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                Some(Action::AppendString(clean))
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => map_key(app, key.code),
            _ => None,
        };

        match action {
            Some(Action::Quit) => break,
            Some(Action::Submit) => app.submit(),
            Some(Action::Refresh) => app.refresh(),
            Some(Action::BackToInput) => {
                app.state = match app.key_input.from_env {
                    false => AppState::ApiKeyInput,
                    true => AppState::Workspace,
                }
            }
            Some(Action::DismissError) => app.clear_error(),
            Some(a) => {
                app.key_input.update(&a);
                app.tree.update(&a);
            }
            None => {}
        }
    }

    Ok(())
}

// >>>

// ~~~~~~~~~~ Draw app <<<

pub fn draw_app(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    frame.render_widget(
        Block::default()
            .title(Span::styled(
                " LingoHub TUI ",
                Style::default().fg(BLUE_LIGHT).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(GREY)),
        area,
    );

    let inner = area.inner(Margin::new(1, 1));

    frame.render_widget(
        Line::from(format!(
            "State: {:?}({:?}), Err: {:?}",
            app.state, app.previous_state, app.error
        )),
        inner,
    );

    match if matches!(app.state, AppState::Erred) {
        app.previous_state.unwrap()
    } else {
        app.state
    } {
        AppState::ApiKeyInput => app.key_input.draw(frame, inner),
        AppState::Workspace => {
            let [ws_area, tree_area] =
                Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(inner);
            app.workspace.draw(frame, ws_area);
            app.tree.draw(frame, tree_area);
        }
        AppState::Erred => { /* Rendered below */ }
    }

    // Render error on top of everything else if present
    if let Some(error) = &app.error {
        let [_, error_area, _] = Layout::default()
            .constraints([Constraint::Fill(1), Constraint::Min(10), Constraint::Fill(1)])
            .margin(3)
            .areas(inner);
        frame.render_widget(Clear, error_area);
        let block = Paragraph::new(error.to_string()).centered().block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(RED.c50))
                .title("Error")
                .title_position(TitlePosition::Top)
                .title_style(Style::default().fg(RED.c300).add_modifier(Modifier::BOLD)),
        );
        frame.render_widget(block, error_area);
    }
}
// >>>

// vim: foldmethod=marker foldmarker=<<<,>>>
