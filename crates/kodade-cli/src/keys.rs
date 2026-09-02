//! One key-name table shared by the TUI's key handler and `pane send-keys`.
//!
//! The names follow tmux (`Enter`, `Escape`, `C-c`, `M-x`, `F5`) so muscle
//! memory carries over, and both call sites emit the same bytes for the same
//! key — a binding and a script can drive an agent identically.

use anyhow::{bail, Result};
use crossterm::event::KeyCode;

/// Named keys and the bytes a terminal sends for them. Aliases share a value.
const NAMED: &[(&str, &[u8])] = &[
    ("Enter", b"\r"),
    ("Return", b"\r"),
    ("C-m", b"\r"),
    ("Tab", b"\t"),
    ("BTab", b"\x1b[Z"),
    ("Space", b" "),
    ("Escape", b"\x1b"),
    ("Esc", b"\x1b"),
    ("BSpace", b"\x7f"),
    ("BackSpace", b"\x7f"),
    ("Up", b"\x1b[A"),
    ("Down", b"\x1b[B"),
    ("Right", b"\x1b[C"),
    ("Left", b"\x1b[D"),
    ("Home", b"\x1b[H"),
    ("End", b"\x1b[F"),
    ("PageUp", b"\x1b[5~"),
    ("PgUp", b"\x1b[5~"),
    ("PageDown", b"\x1b[6~"),
    ("PgDn", b"\x1b[6~"),
    ("Insert", b"\x1b[2~"),
    ("IC", b"\x1b[2~"),
    ("Delete", b"\x1b[3~"),
    ("DC", b"\x1b[3~"),
    ("F1", b"\x1bOP"),
    ("F2", b"\x1bOQ"),
    ("F3", b"\x1bOR"),
    ("F4", b"\x1bOS"),
    ("F5", b"\x1b[15~"),
    ("F6", b"\x1b[17~"),
    ("F7", b"\x1b[18~"),
    ("F8", b"\x1b[19~"),
    ("F9", b"\x1b[20~"),
    ("F10", b"\x1b[21~"),
    ("F11", b"\x1b[23~"),
    ("F12", b"\x1b[24~"),
];

/// Bytes for a named key, or `None` when the name is unknown.
pub fn named(name: &str) -> Option<&'static [u8]> {
    NAMED
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, bytes)| *bytes)
}

/// Bytes for a crossterm key code that is not a plain character.
pub fn from_code(code: KeyCode) -> Option<&'static [u8]> {
    let name = match code {
        KeyCode::Enter => "Enter",
        KeyCode::Backspace => "BSpace",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => "BTab",
        KeyCode::Esc => "Escape",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Right => "Right",
        KeyCode::Left => "Left",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Insert => "Insert",
        KeyCode::Delete => "Delete",
        KeyCode::F(n @ 1..=12) => return named(&format!("F{n}")),
        _ => return None,
    };
    named(name)
}

/// Bytes for one `pane send-keys` argument: a key name, a `C-x` / `M-x` chord,
/// or literal text. A token that looks like a key name (`C-…`, `M-…`, or a
/// capitalized word) but is not one is an error rather than silently typed.
pub fn parse(token: &str) -> Result<Vec<u8>> {
    if let Some(bytes) = named(token) {
        return Ok(bytes.to_vec());
    }
    // `M-x` is Escape then the key, so it composes with the rest of the table.
    if let Some(rest) = token.strip_prefix("M-") {
        let mut bytes = vec![0x1b];
        bytes.extend(parse(rest)?);
        return Ok(bytes);
    }
    if let Some(rest) = token.strip_prefix("C-") {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let upper = c.to_ascii_uppercase() as u8;
            // `@`..`_` covers ctrl-space through ctrl-underscore, letters included.
            if c.is_ascii() && (b'@'..=b'_').contains(&upper) {
                return Ok(vec![upper - 64]);
            }
        }
        bail!("unknown key '{token}' (control chords are one character: C-c)");
    }
    if looks_like_key_name(token) {
        bail!("unknown key '{token}' (use --literal to send it as text)");
    }
    Ok(token.as_bytes().to_vec())
}

/// A single capitalized word — the shape every name in the table has. Text with
/// spaces or lowercase starts is treated as literal input.
fn looks_like_key_name(token: &str) -> bool {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => return false,
    }
    token.len() > 1 && chars.all(|c| c.is_ascii_alphanumeric())
}

/// Every `send-keys` argument in order, concatenated into one write.
pub fn parse_all(tokens: &[String]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for token in tokens {
        bytes.extend(parse(token)?);
    }
    Ok(bytes)
}

/// `--literal`: every argument as-is, joined by spaces like the shell wrote it.
pub fn literal(tokens: &[String]) -> Vec<u8> {
    tokens.join(" ").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_cover_specials_and_chords() {
        assert_eq!(parse("Enter").unwrap(), b"\r");
        assert_eq!(parse("Escape").unwrap(), b"\x1b");
        assert_eq!(parse("F5").unwrap(), b"\x1b[15~");
        assert_eq!(parse("F12").unwrap(), b"\x1b[24~");
        assert_eq!(parse("Home").unwrap(), b"\x1b[H");
        assert_eq!(parse("End").unwrap(), b"\x1b[F");
        assert_eq!(parse("PageUp").unwrap(), b"\x1b[5~");
        assert_eq!(parse("PageDown").unwrap(), b"\x1b[6~");
        assert_eq!(parse("Delete").unwrap(), b"\x1b[3~");
        assert_eq!(parse("Insert").unwrap(), b"\x1b[2~");
        assert_eq!(parse("C-c").unwrap(), vec![3]);
        assert_eq!(parse("M-x").unwrap(), b"\x1bx");
        assert_eq!(parse("M-Enter").unwrap(), b"\x1b\r");
    }

    #[test]
    fn literal_text_passes_through_but_bad_key_names_error() {
        assert_eq!(parse("npm test").unwrap(), b"npm test");
        assert_eq!(parse("y").unwrap(), b"y");
        // Looks like a key name, is not one: better to fail than to type it.
        assert!(parse("Entr").is_err());
        assert!(parse("C-ctrl").is_err());
        assert!(parse("M-Nope").is_err());
        // The escape hatch for capitalized text.
        assert_eq!(literal(&["Hello".into(), "world".into()]), b"Hello world");
    }

    #[test]
    fn arguments_concatenate_in_order() {
        assert_eq!(
            parse_all(&["npm test".into(), "Enter".into()]).unwrap(),
            b"npm test\r"
        );
    }

    #[test]
    fn key_codes_and_names_agree() {
        // The TUI and `send-keys` must produce identical bytes for a key.
        assert_eq!(
            from_code(KeyCode::PageUp).unwrap(),
            parse("PageUp").unwrap()
        );
        assert_eq!(from_code(KeyCode::F(7)).unwrap(), parse("F7").unwrap());
        assert_eq!(
            from_code(KeyCode::Delete).unwrap(),
            parse("Delete").unwrap()
        );
        assert!(from_code(KeyCode::F(13)).is_none());
    }
}
