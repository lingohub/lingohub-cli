// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

//! Interactive inline prompts.
//!
//! Prompts are only used when both stdin and stdout are connected to a terminal
//! (see [`is_interactive`]).  Callers must check this before invoking any
//! prompt function; all output goes to **stdout** so the terminal experience is
//! seamless and nothing leaks into piped output.
//!
//! Text input uses **rustyline** for full readline editing.  Yes/no confirmation
//! and scrollable list selection are driven by **crossterm** raw mode on stdout.
//!
//! Colors use the shared brand palette from [`crate::colors`].

use std::io::{self, IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Print, Stylize};
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute, queue};
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::{Config, Editor};
use rustyline_derive::{Completer, Helper, Highlighter, Hinter, Validator};

use crate::colors::{C_BLUE_DARK, C_BLUE_LIGHT, C_GREY, styled};

/// Returns `true` when both stdin and stdout are connected to a terminal.
///
/// Use this to decide whether to offer interactive prompts.  When `false`,
/// callers should require all inputs to be supplied via flags or `--file`.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Outcome returned by [`select_from_list`].
pub enum SelectResult {
    /// The user selected the item at this 0-based index.
    Selected(usize),
    /// The user requested the next page of items.
    LoadMore,
}

// ~~~~~~~~~~ RawGuard <<<

/// Enables raw mode and hides the terminal cursor on construction;
/// restores both on drop regardless of how the enclosing scope exits.
struct RawGuard;

impl RawGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show);
        let _ = disable_raw_mode();
    }
}

// >>>

// ~~~~~~~~~~ prompt <<<

#[derive(Helper, Completer, Hinter, Highlighter, Validator)]
struct PromptHelper;

/// Display a text-input prompt and return the entered string.
///
/// Uses rustyline for full readline editing: history navigation, Ctrl+A/E/W,
/// left/right arrow keys, and the usual line-editing shortcuts all work out of
/// the box.
///
/// - **Enter** confirms (may return an empty string).
/// - **Ctrl+C** / **Ctrl+D** return an `Interrupted` error.
pub fn prompt(label: &str) -> io::Result<String> {
    let helper = PromptHelper {};
    let mut rl = Editor::with_config(Config::builder().build()).map_err(io::Error::other)?;
    rl.set_color_mode(rustyline::ColorMode::Forced);
    rl.set_helper(Some(helper));

    let prompt = format!("{}: ", label);
    let styled_prompt = styled(prompt.clone().with(C_BLUE_LIGHT).bold()).to_string();
    let prompt_tuple = (prompt, styled_prompt);
    match rl.readline(&prompt_tuple) {
        Ok(line) => Ok(line),
        Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
            Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"))
        }
        Err(e) => Err(io::Error::other(e)),
    }
}

// >>>

// ~~~~~~~~~~ confirm <<<

/// Queue the visual content of a yes/no prompt into `stdout`.
///
/// When `answer` is `None` the default `N` is highlighted (pre-confirmation).
/// Once answered the chosen option is highlighted instead.
fn queue_confirm_content(question: &str, answer: Option<bool>) -> io::Result<()> {
    let (y_style, n_style) = match answer {
        Some(true) => (styled("y".with(C_BLUE_LIGHT).bold()), styled("N".with(C_GREY))),
        _ => (styled("y".with(C_GREY)), styled("N".with(C_BLUE_LIGHT).bold())),
    };
    queue!(
        io::stdout(),
        Print("  "),
        Print(question),
        Print(styled("  [".with(C_GREY))),
        Print(y_style),
        Print(styled("/".with(C_GREY))),
        Print(n_style),
        Print(styled("]".with(C_GREY))),
    )
}

/// Display an inline yes/no prompt and return the user's choice.
///
/// The prompt is printed on the current line.  When the user presses a key the
/// chosen option is highlighted in place before the line is frozen.
///
/// - **y** / **Y** confirms.
/// - **n** / **N** or **Enter** declines (default is No).
/// - **Ctrl+C** returns an `Interrupted` error.
pub fn confirm(question: &str) -> io::Result<bool> {
    let _guard = RawGuard::new()?;
    let mut stdout = io::stdout();

    queue_confirm_content(question, None)?;
    stdout.flush()?;

    let result = loop {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match (k.modifiers, k.code) {
                (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
                }
                (_, KeyCode::Char('y') | KeyCode::Char('Y')) => break true,
                (_, KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('N')) => break false,
                _ => {}
            },
            _ => {}
        }
    };

    // Freeze with the chosen answer highlighted, then advance.
    queue!(stdout, cursor::MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    queue_confirm_content(question, Some(result))?;
    queue!(
        stdout,
        Print(if result { " Yes" } else { " No" }),
        cursor::MoveToNextLine(1)
    )?;
    execute!(stdout, Print("\r\n"))?;
    Ok(result)
}

// >>>

// ~~~~~~~~~~ select_from_list <<<

/// Maximum number of list rows displayed at once before the list scrolls.
const MAX_VISIBLE: usize = 8;

/// Queue one list item's visual content into `stdout` (no cursor positioning or newline).
fn queue_item_content(index: usize, primary: &str, secondary: &str, selected: bool) -> io::Result<()> {
    let num = format!("{:>3}", index + 1);
    let title = format!("{:<38}", primary);

    queue!(io::stdout(), Print("  "))?;
    if selected {
        queue!(
            io::stdout(),
            Print(styled("▶ ".with(C_BLUE_LIGHT).bold())),
            Print(styled(num.with(C_BLUE_LIGHT).bold())),
            Print("  "),
            Print(styled(title.with(C_BLUE_LIGHT).bold())),
        )?;
    } else {
        queue!(
            io::stdout(),
            Print("  "),
            Print(styled(num.with(C_BLUE_DARK))),
            Print("  "),
            Print(title),
        )?;
    }
    queue!(io::stdout(), Print("  "), Print(styled(secondary.with(C_GREY))))
}

/// Queue the hint line's visual content into `stdout` (no cursor positioning or newline).
fn queue_hint_content(has_more: bool) -> io::Result<()> {
    queue!(
        io::stdout(),
        Print("  "),
        Print(styled("↑↓".with(C_BLUE_LIGHT).bold())),
        Print(styled("  navigate".with(C_GREY))),
        Print(styled("   ·   ".with(C_GREY))),
        Print(styled("Enter".with(C_BLUE_LIGHT).bold())),
        Print(styled("  select".with(C_GREY))),
    )?;
    if has_more {
        queue!(
            io::stdout(),
            Print(styled("   ·   ".with(C_GREY))),
            Print(styled("→".with(C_BLUE_LIGHT).bold())),
            Print(styled("  load more".with(C_GREY))),
        )?;
    }
    Ok(())
}

/// Render the visible window of `items` plus the hint line.
///
/// `first` controls the render mode:
/// - `true`  — initial render: content is printed straight down from the
///   current cursor position without clearing, preserving everything
///   above.
/// - `false` — update render: the cursor is moved back up by the total row
///   count, then each line is cleared and redrawn in place.
///
/// All writes are buffered and flushed in a single syscall to prevent tearing.
fn draw_list(
    items: &[(String, String)],
    cursor_idx: usize,
    scroll_off: usize,
    visible: usize,
    has_more: bool,
    first: bool,
) -> io::Result<()> {
    let num_rows = (visible + 1) as u16; // item rows + hint row
    if !first {
        queue!(io::stdout(), cursor::MoveUp(num_rows))?;
    }

    for (i, (primary, secondary)) in items.iter().enumerate().skip(scroll_off).take(visible) {
        if !first {
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine)
            )?;
        }
        queue_item_content(i, primary, secondary, i == cursor_idx)?;
        queue!(io::stdout(), cursor::MoveToNextLine(1))?;
    }

    if !first {
        queue!(
            io::stdout(),
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue_hint_content(has_more)?;
    execute!(io::stdout(), cursor::MoveToNextLine(1))?;
    Ok(())
}

/// Display an inline scrollable list and return the user's selection.
///
/// The list is printed below all previous output without clearing anything.
/// Arrow-key navigation redraws only the list's own rows.  When the user
/// confirms, the highlighted state is left visible and the cursor sits on the
/// line immediately below the list.
///
/// - **↑** / **↓** navigate; the visible window scrolls at the edges.
/// - **Enter** selects the highlighted item.
/// - **→** or **↓** past the last item triggers `LoadMore` (when `has_more`).
/// - **Ctrl+C** returns an `Interrupted` error.
///
/// `display_from` sets the initial cursor position — pass the index of the
/// first newly-loaded item so focus lands there automatically.
pub fn select_from_list(
    items: &[(String, String)],
    display_from: usize,
    has_more: bool,
) -> io::Result<SelectResult> {
    if items.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "No items to select from.",
        ));
    }

    let _guard = RawGuard::new()?;
    let visible = items.len().min(MAX_VISIBLE);
    let mut cursor_idx = display_from.min(items.len() - 1);
    let mut scroll_off = cursor_idx.saturating_sub(visible - 1);

    // Reserve vertical space by scrolling the terminal with real newlines, then
    // moving back up.  MoveToNextLine alone does not scroll when the cursor is
    // near the bottom of the viewport, which would cause all items to pile up
    // on the last terminal line and corrupt the position tracking.
    let num_rows = (visible + 1) as u16;
    for _ in 0..num_rows {
        queue!(io::stdout(), Print("\r\n"))?;
    }
    queue!(io::stdout(), cursor::MoveUp(num_rows))?;
    io::stdout().flush()?;

    // Initial render: draw into the reserved space without clearing.
    draw_list(items, cursor_idx, scroll_off, visible, has_more, true)?;

    loop {
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match (k.modifiers, k.code) {
                (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"));
                }
                (_, KeyCode::Enter) => return Ok(SelectResult::Selected(cursor_idx)),
                (_, KeyCode::Up) => {
                    if cursor_idx > 0 {
                        cursor_idx -= 1;
                        if cursor_idx < scroll_off {
                            scroll_off = cursor_idx;
                        }
                        draw_list(items, cursor_idx, scroll_off, visible, has_more, false)?;
                    }
                }
                (_, KeyCode::Down) => {
                    if cursor_idx + 1 < items.len() {
                        cursor_idx += 1;
                        if cursor_idx >= scroll_off + visible {
                            scroll_off = cursor_idx + 1 - visible;
                        }
                        draw_list(items, cursor_idx, scroll_off, visible, has_more, false)?;
                    } else if has_more {
                        return Ok(SelectResult::LoadMore);
                    }
                }
                (_, KeyCode::Right) if has_more => return Ok(SelectResult::LoadMore),
                _ => {}
            },
            _ => {}
        }
    }
}

// >>>

// vim: foldmethod=marker foldmarker=<<<,>>>
