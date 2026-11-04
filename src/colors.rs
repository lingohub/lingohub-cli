// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::style::{Color as CrosstermColor, StyledContent};
use ratatui::style::Color;

/// Neutral grey for borders and hint text.  `#5D5D5D`  (contrast: 2.6:1 on dark bg)
pub const GREY_RGB: (u8, u8, u8) = (93, 93, 93);

/// Deep royal blue — icon body.  `#6490ED`  (contrast: 5.5:1 on dark bg)
pub const BLUE_DARK_RGB: (u8, u8, u8) = (100, 144, 237);

/// Cornflower blue — folded-corner highlight.  `#96C4FF`  (contrast: 9.4:1 on dark bg)
pub const BLUE_LIGHT_RGB: (u8, u8, u8) = (150, 196, 255);

/// Neutral grey as a ratatui `Color`.
pub const GREY: Color = Color::Rgb(GREY_RGB.0, GREY_RGB.1, GREY_RGB.2);

/// Deep royal blue as a ratatui `Color`.
pub const BLUE_DARK: Color = Color::Rgb(BLUE_DARK_RGB.0, BLUE_DARK_RGB.1, BLUE_DARK_RGB.2);

/// Cornflower blue as a ratatui `Color`.
pub const BLUE_LIGHT: Color = Color::Rgb(BLUE_LIGHT_RGB.0, BLUE_LIGHT_RGB.1, BLUE_LIGHT_RGB.2);

/// Crossterm color for `GREY`.
pub const C_GREY: CrosstermColor = CrosstermColor::Rgb {
    r: GREY_RGB.0,
    g: GREY_RGB.1,
    b: GREY_RGB.2,
};

/// Crossterm color for `BLUE_DARK`.
pub const C_BLUE_DARK: CrosstermColor = CrosstermColor::Rgb {
    r: BLUE_DARK_RGB.0,
    g: BLUE_DARK_RGB.1,
    b: BLUE_DARK_RGB.2,
};

/// Crossterm color for `BLUE_LIGHT`.
pub const C_BLUE_LIGHT: CrosstermColor = CrosstermColor::Rgb {
    r: BLUE_LIGHT_RGB.0,
    g: BLUE_LIGHT_RGB.1,
    b: BLUE_LIGHT_RGB.2,
};

// ~~~~~~~~~~ Color Switch ~~~~~~~~~~

static COLOR_ENABLED: AtomicBool = AtomicBool::new(true);

/// Initialize the global color switch from the `--color` flag value.
///
/// Must be called once, early in `main`, before any colored output is produced.
/// `Auto` enables color only when stdout is a terminal.
pub fn set_color_enabled(always: bool, never: bool) {
    let enabled = if never {
        false
    } else if always {
        true
    } else {
        std::io::stdout().is_terminal()
    };
    COLOR_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns `true` when color output is currently enabled.
pub fn is_colored() -> bool {
    COLOR_ENABLED.load(Ordering::Relaxed)
}

/// A styled value whose ANSI sequences are suppressed when color is disabled.
///
/// Obtain one via [`styled`]. Implements `Display` so it can be used anywhere
/// a plain string would be — including inside `crossterm::style::Print`.
pub struct Styled<T: fmt::Display>(StyledContent<T>);

impl<T: fmt::Display> fmt::Display for Styled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if is_colored() {
            fmt::Display::fmt(&self.0, f)
        } else {
            fmt::Display::fmt(self.0.content(), f)
        }
    }
}

/// Wrap a `StyledContent` so its ANSI codes are omitted when color is disabled.
pub fn styled<T: fmt::Display>(content: StyledContent<T>) -> Styled<T> {
    Styled(content)
}
