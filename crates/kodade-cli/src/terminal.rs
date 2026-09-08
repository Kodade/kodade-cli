//! Own terminal modes for exactly the lifetime of an attached UI.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Once,
};

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

pub struct TerminalModes;

impl TerminalModes {
    pub fn enter(mouse: bool) -> Result<Self> {
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::Release);
        // Establish ownership before any further fallible setup.
        let modes = Self;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        if mouse {
            execute!(stdout, EnableMouseCapture)?;
        }
        Ok(modes)
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    // Try every operation even if a disconnected output fails an earlier one.
    let _ = execute!(stdout, DisableBracketedPaste);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = execute!(stdout, Show);
}
