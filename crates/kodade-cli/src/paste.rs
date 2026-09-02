//! Paste sanitizing, bracketed-paste wrapping, and chunking.
//!
//! Mirrors the desktop app's `paste.ts`: pasted text can carry escape
//! sequences (an OSC 52 clipboard write, a CSI cursor move) and stray control
//! bytes. `sanitize` removes them so a malicious paste cannot drive the
//! terminal, `wrap` frames the bytes for the target program, and `chunks`
//! splits a large paste so the socket writer can pace it.

use std::{iter::Peekable, str::Chars};

/// Default paste chunk size; large pastes are paced one chunk at a time.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Strips escape sequences and C0 control bytes from pasted text.
///
/// Rules (in order):
/// - normalize `\r\n` -> `\n`;
/// - drop each `\x1b` escape sequence in full — a CSI (`ESC [ … final`), an
///   OSC (`ESC ] … BEL`/`ST`), or any other two-byte `ESC x` escape;
/// - drop remaining C0 control chars (`0x00`–`0x1f`) except `\n` and `\t`.
pub fn sanitize(text: &str) -> String {
    // Normalize CRLF first so the later CR strip cannot split a line.
    let normalized = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    let mut chars = normalized.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => strip_escape(&mut chars),
            '\n' | '\t' => out.push(c),
            // Other C0 control chars (includes a lone CR) are dropped.
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Consumes one escape sequence after its leading `ESC` has been read.
fn strip_escape(chars: &mut Peekable<Chars>) {
    match chars.peek() {
        // CSI: parameter/intermediate bytes until a final byte (0x40–0x7e).
        Some('[') => {
            chars.next();
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        // OSC: body until BEL (0x07) or ST (ESC \\).
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Any other escape is two bytes: drop the byte that follows ESC.
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

/// Frames text for the target program. Bracketed mode wraps it in the paste
/// markers so the program can tell a paste from typing; otherwise `\n` becomes
/// `\r`, the byte a terminal sends for Enter.
pub fn wrap(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

/// Splits bytes into `size`-byte chunks so a large paste can be paced.
pub fn chunks(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.chunks(size.max(1)).map(<[u8]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_osc52_attack_and_controls() {
        // An OSC 52 clipboard write embedded in otherwise plain text.
        let attack = "safe\x1b]52;c;ZXZpbA==\x07 text\x00\x07 end";
        assert_eq!(sanitize(attack), "safe text end");
    }

    #[test]
    fn sanitize_strips_csi_and_keeps_tabs_newlines() {
        // CSI cursor move dropped; tab and newline kept.
        let input = "a\x1b[2J\tb\r\nc\rd";
        assert_eq!(sanitize(input), "a\tb\ncd");
    }

    #[test]
    fn sanitize_normalizes_crlf() {
        assert_eq!(sanitize("one\r\ntwo\r\n"), "one\ntwo\n");
    }

    #[test]
    fn wrap_brackets_or_translates_newlines() {
        assert_eq!(wrap("hi\nthere", true), b"\x1b[200~hi\nthere\x1b[201~");
        assert_eq!(wrap("hi\nthere", false), b"hi\rthere".to_vec());
    }

    #[test]
    fn chunks_split_a_large_paste() {
        // A 200 KB paste splits into 64 KB chunks with the tail preserved.
        let bytes = vec![b'x'; 200 * 1024];
        let parts = chunks(&bytes, CHUNK_SIZE);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].len(), CHUNK_SIZE);
        assert_eq!(parts[3].len(), 200 * 1024 - 3 * CHUNK_SIZE);
        assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), bytes.len());
        assert!(chunks(&[], CHUNK_SIZE).is_empty());
    }

    #[test]
    fn large_paste_round_trips_through_sanitize() {
        // 200 KB of plain text passes sanitize untouched and wraps once bracketed.
        let text = "a".repeat(200 * 1024);
        let clean = sanitize(&text);
        assert_eq!(clean.len(), text.len());
        let wrapped = wrap(&clean, true);
        assert_eq!(chunks(&wrapped, CHUNK_SIZE).len(), 4);
    }
}
