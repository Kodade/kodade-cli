//! Bounded ANSI export of a live terminal for daemon handoff.
//!
//! The exporter only uses cloned parsers.  Capturing a handoff must never move
//! an attached client's scrollback viewport or alter its alternate buffer.

use anyhow::{bail, Result};

use super::{PtyCallbacks, PtyParser};

const HISTORY_LIMIT: usize = 64 * 1024;
const SCREEN_LIMIT: usize = 128 * 1024;

pub(super) fn terminal_handoff(parser: &PtyParser) -> Result<(Vec<u8>, String)> {
    let (rows, cols) = parser.screen().size();
    let alternate = parser.screen().alternate_screen();
    let mut copy = PtyParser::new_with_callbacks(rows, cols, 10_000, PtyCallbacks::default());
    *copy.screen_mut() = parser.screen().clone();

    copy.process(b"\x1b[?47l");
    let history = history_ansi(&mut copy);
    copy.screen_mut().set_scrollback(0);
    let mut screen = Vec::new();
    for switch in [b"\x1b[?47l", b"\x1b[?47h"] {
        copy.process(switch);
        screen.extend_from_slice(switch);
        // Format grids with absolute addressing before restoring their modes.
        screen.extend_from_slice(b"\x1b[?6l\x1b[r");
        screen.extend(copy.screen().state_formatted());
        let (top, bottom, origin) = grid_modes(copy.screen());
        screen.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
        // DEC save/restore retains each grid's cursor and origin-mode state.
        let current = copy.screen().clone();
        copy.process(b"\x1b8");
        let saved_origin = grid_modes(copy.screen()).2;
        append_cursor(&mut screen, copy.screen(), top, saved_origin);
        screen.extend_from_slice(b"\x1b7");
        append_cursor(&mut screen, &current, top, origin);
        *copy.screen_mut() = current;
    }
    if !alternate {
        screen.extend_from_slice(b"\x1b[?47l");
    }
    if screen.len() > SCREEN_LIMIT {
        bail!("pane screens exceed handoff limit of 128 KiB");
    }
    Ok((screen, history))
}

// vt100's formatter preserves delayed wrapping by redrawing the last cell.
// Its CUP coordinates are absolute; DEC origin mode makes them margin-relative.
fn append_cursor(output: &mut Vec<u8>, screen: &vt100::Screen, top: u16, origin: bool) {
    output.extend_from_slice(if origin { b"\x1b[?6h" } else { b"\x1b[?6l" });
    output.extend_from_slice(b"\x1b[m");
    let cursor = screen.cursor_state_formatted();
    if origin {
        let mut remaining = cursor.as_slice();
        while let Some(start) = remaining.windows(2).position(|bytes| bytes == b"\x1b[") {
            output.extend_from_slice(&remaining[..start]);
            remaining = &remaining[start + 2..];
            let end = remaining
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .unwrap();
            let params = std::str::from_utf8(&remaining[..end]).unwrap();
            output.extend_from_slice(b"\x1b[");
            if remaining[end] == b'H' {
                let (row, col) = params.split_once(';').unwrap_or(("1", "1"));
                let row: u16 = row.parse().unwrap();
                output.extend_from_slice(
                    format!("{};{}", row.saturating_sub(top).max(1), col).as_bytes(),
                );
            } else {
                output.extend_from_slice(&remaining[..end]);
            }
            output.push(remaining[end]);
            remaining = &remaining[end + 1..];
        }
        output.extend_from_slice(remaining);
    } else {
        output.extend(cursor);
    }
    output.extend(screen.attributes_formatted());
}

/// vt100 intentionally keeps margins and origin mode private. Probe a clone
/// with cursor addressing, whose documented behavior exposes both settings.
fn grid_modes(screen: &vt100::Screen) -> (u16, u16, bool) {
    let (rows, cols) = screen.size();
    let mut probe = vt100::Parser::new(rows, cols, 0);
    *probe.screen_mut() = screen.clone();
    probe.process(b"\x1b[?6h\x1b[1;1H");
    let top = probe.screen().cursor_position().0;
    probe.process(b"\x1b[999;1H");
    let bottom = probe.screen().cursor_position().0;

    let mut origin_probe = vt100::Parser::new(rows.saturating_add(3), cols, 0);
    *origin_probe.screen_mut() = screen.clone();
    origin_probe
        .screen_mut()
        .set_size(rows.saturating_add(3), cols);
    origin_probe.process(b"\x1b[2;3r\x1b[1;1H");
    let origin = origin_probe.screen().cursor_position().0 == 1;
    (top, bottom, origin)
}

fn history_ansi(parser: &mut PtyParser) -> String {
    let (rows, cols) = parser.screen().size();
    let rows = rows as usize;
    parser.screen_mut().set_scrollback(usize::MAX);
    let max = parser.screen().scrollback();
    let budget = HISTORY_LIMIT.saturating_sub(2 * rows + 2);
    let mut kept = Vec::new();
    let mut logical = Vec::new();
    let mut bytes = 0;
    let mut pending_bytes = 0;
    let mut previous_wrapped = false;
    // Walk backwards and stop at the byte budget. Large scrollback must not
    // allocate an unbounded ANSI copy just to throw most of it away.
    for index in (0..max).rev() {
        parser.screen_mut().set_scrollback(max - index);
        let wrapped = parser.screen().row_wrapped(0);
        if index == max - 1 {
            previous_wrapped = wrapped;
        }
        let mut row = parser.screen().rows_formatted(0, cols).next().unwrap();
        let continuation = if index > 0 {
            parser.screen_mut().set_scrollback(max - index + 1);
            parser.screen().row_wrapped(0)
        } else {
            false
        };
        if continuation {
            // Commit delayed wrapping before a formatter that addresses the
            // row from column zero; erase the temporary default cell.
            let mut wrapped_row = b" \x08\x1b[X".to_vec();
            wrapped_row.extend(row);
            row = wrapped_row;
        }
        if !wrapped {
            row.extend_from_slice(b"\r\n");
        }
        pending_bytes += row.len();
        if bytes + pending_bytes > budget {
            break;
        }
        logical.push(row);
        if !continuation {
            logical.reverse();
            kept.push(logical.concat());
            logical.clear();
            bytes += pending_bytes;
            pending_bytes = 0;
        }
    }
    kept.reverse();
    let mut history = kept.concat();
    if !history.is_empty() {
        // A history row wrapping into the live grid needs a printable byte to
        // commit that wrap. The temporary cell is erased by screen replay.
        if previous_wrapped {
            history.extend_from_slice(b" \r");
        }
        // Move every captured row into scrollback before repainting the live
        // grid; clearing that grid must not erase the newest history rows.
        for _ in 1..rows {
            history.extend_from_slice(b"\r\n");
        }
    }
    String::from_utf8_lossy(&history).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(rows: u16, cols: u16, bytes: &[u8]) -> PtyParser {
        let mut parser = PtyParser::new_with_callbacks(rows, cols, 64, PtyCallbacks::default());
        parser.process(bytes);
        parser
    }

    fn assert_grid_eq(left: &vt100::Screen, right: &vt100::Screen) {
        assert_eq!(left.size(), right.size());
        for row in 0..left.size().0 {
            assert_eq!(
                left.row_wrapped(row),
                right.row_wrapped(row),
                "wrap row {row}"
            );
            for col in 0..left.size().1 {
                assert_eq!(
                    left.cell(row, col),
                    right.cell(row, col),
                    "cell {row},{col}"
                );
            }
        }
        assert_eq!(left.cursor_position(), right.cursor_position());
    }

    #[test]
    fn replay_preserves_styled_wide_wrapped_scrollback_and_live_grid() {
        let mut source = parser(
            3,
            6,
            b"\x1b[31;1mred\x1b[0m \xe7\x95\x8c\r\n123456WRAP\r\nline3\r\nline4\r\nline5\r\nline6",
        );
        let (screen, history) = terminal_handoff(&source).unwrap();
        assert!(history.contains("\x1b["));
        let mut replay = PtyParser::new_with_callbacks(3, 6, 64, PtyCallbacks::default());
        replay.process(history.as_bytes());
        replay.process(&screen);
        assert_grid_eq(source.screen(), replay.screen());
        source.screen_mut().set_scrollback(usize::MAX);
        replay.screen_mut().set_scrollback(usize::MAX);
        assert!(source.screen().scrollback() > 0);
        assert_eq!(source.screen().scrollback(), replay.screen().scrollback());
        let count = source.screen().scrollback();
        for offset in 0..=count {
            source.screen_mut().set_scrollback(offset);
            replay.screen_mut().set_scrollback(offset);
            assert_grid_eq(source.screen(), replay.screen());
        }
    }

    #[test]
    fn replay_preserves_history_across_viewports_and_bounds_large_exports() {
        for rows in [2, 5] {
            let mut source = PtyParser::new_with_callbacks(rows, 6, 64, PtyCallbacks::default());
            for line in 0..30 {
                source.process(
                    format!("\x1b[3{}m{:03}abcdefghi\x1b[m\r\n", line % 8, line).as_bytes(),
                );
            }
            let (screen, history) = terminal_handoff(&source).unwrap();
            let mut replay = PtyParser::new_with_callbacks(rows, 6, 64, PtyCallbacks::default());
            replay.process(history.as_bytes());
            replay.process(&screen);
            source.screen_mut().set_scrollback(usize::MAX);
            let count = source.screen().scrollback();
            replay.screen_mut().set_scrollback(usize::MAX);
            assert_eq!(count, replay.screen().scrollback());
            for offset in 0..=count {
                source.screen_mut().set_scrollback(offset);
                replay.screen_mut().set_scrollback(offset);
                assert_grid_eq(source.screen(), replay.screen());
            }
        }
        let mut large = PtyParser::new_with_callbacks(50, 100, 10_000, PtyCallbacks::default());
        for _ in 0..10_000 {
            large.process(b"\x1b[31mA\x1b[32mB\x1b[33mC\x1b[34mD\x1b[35mE\x1b[m\r\n");
        }
        let (_, history) = terminal_handoff(&large).unwrap();
        assert!(!history.is_empty());
        assert!(history.len() <= HISTORY_LIMIT);
    }

    #[test]
    fn replay_preserves_both_buffers_saved_cursors_and_margin_origin_modes() {
        let mut source = parser(
            6,
            10,
            b"primary\x1b7\x1b[2;5r\x1b[?6h\x1b[?47halt\x1b7\x1b[3;4r\x1b[?6h",
        );
        let (screen, history) = terminal_handoff(&source).unwrap();
        let mut replay = PtyParser::new_with_callbacks(6, 10, 64, PtyCallbacks::default());
        replay.process(history.as_bytes());
        replay.process(&screen);
        assert!(replay.screen().alternate_screen());
        assert_grid_eq(source.screen(), replay.screen());
        source.process(b"\x1b[?47l");
        replay.process(b"\x1b[?47l");
        assert_grid_eq(source.screen(), replay.screen());
        for bytes in [
            b"\x1b8".as_slice(),
            b"X\x1b[999;1H\nZ",
            b"\x1b[?47h\x1b8Y\x1b[999;1H\nQ",
        ] {
            source.process(bytes);
            replay.process(bytes);
            assert_grid_eq(source.screen(), replay.screen());
            assert_eq!(grid_modes(source.screen()), grid_modes(replay.screen()));
        }
    }

    #[test]
    fn replay_preserves_delayed_wrap_and_saved_cursor_origin() {
        for bytes in [
            b"123456".as_slice(),
            b"\x1b[?6h",
            b"\x1b[2;4r\x1b[?6h123456",
            b"\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b7\x1b[?6l",
            b"\x1b[2;4r\x1b[3;4H\x1b7\x1b[?6h",
        ] {
            let mut source = parser(5, 6, bytes);
            let (screen, history) = terminal_handoff(&source).unwrap();
            let mut replay = parser(5, 6, history.as_bytes());
            replay.process(&screen);
            assert_grid_eq(source.screen(), replay.screen());
            for next in [b"Z".as_slice(), b"\x1b8Q", b"\x1b[999;1H\nMORE"] {
                source.process(next);
                replay.process(next);
                assert_grid_eq(source.screen(), replay.screen());
                assert_eq!(grid_modes(source.screen()), grid_modes(replay.screen()));
            }
        }
    }
}
