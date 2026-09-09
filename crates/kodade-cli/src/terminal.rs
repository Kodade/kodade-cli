//! Own terminal modes for exactly the lifetime of an attached UI.

use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Once,
};

use anyhow::Result;
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

pub fn begin_synchronized_output(mut writer: impl Write) -> std::io::Result<()> {
    writer.write_all(SYNC_BEGIN)
}

pub fn end_synchronized_output(mut writer: impl Write) -> std::io::Result<()> {
    writer.write_all(SYNC_END)?;
    // Present this frame now; buffering END until the next BEGIN can leave
    // terminals such as Foot waiting indefinitely to display any content.
    writer.flush()
}

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
        if supports_keyboard_enhancement().unwrap_or(false) {
            KEYBOARD_ENHANCED.store(true, Ordering::Release);
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                        // Without alternate keys, Shift+A arrives as the base
                        // key `a` + SHIFT and typed text loses its shift.
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
                ),
            )?;
        }
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
    // A failed frame or a detached client must never leave a host terminal
    // waiting for a synchronized-output terminator.
    let _ = end_synchronized_output(&mut stdout);
    if KEYBOARD_ENHANCED.swap(false, Ordering::AcqRel) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout, DisableBracketedPaste);
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = execute!(stdout, Show);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronized_output_markers_are_balanced() {
        let mut bytes = Vec::new();
        begin_synchronized_output(&mut bytes).unwrap();
        bytes.extend_from_slice(b"frame");
        end_synchronized_output(&mut bytes).unwrap();
        assert_eq!(bytes, b"\x1b[?2026hframe\x1b[?2026l");
    }

    #[test]
    fn completed_frame_reaches_a_buffered_terminal_before_the_next_frame() {
        let mut writer = std::io::BufWriter::new(Vec::new());
        begin_synchronized_output(&mut writer).unwrap();
        writer.write_all(b"frame").unwrap();
        // Ratatui flushes its draw before we append the frame terminator.
        writer.flush().unwrap();
        end_synchronized_output(&mut writer).unwrap();
        assert_eq!(writer.get_ref(), b"\x1b[?2026hframe\x1b[?2026l");
    }
}
