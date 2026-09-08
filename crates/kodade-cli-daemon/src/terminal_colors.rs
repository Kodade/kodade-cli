//! Read-only OSC terminal color queries backed by the active rendering client.

use kodade_cli_proto::TerminalColors;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot(Option<TerminalColors>);

#[derive(Default)]
pub(crate) struct Tracker {
    snapshot: Snapshot,
    parser: vte::Parser,
    events: Events,
    replies: Vec<u8>,
}

#[derive(Default)]
struct Events(Option<Vec<Vec<u8>>>);
impl vte::Perform for Events {
    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        self.0 = Some(params.iter().map(|part| part.to_vec()).collect());
    }
}

impl Tracker {
    pub(crate) fn set(&mut self, colors: Option<TerminalColors>) {
        self.snapshot = Snapshot(colors);
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }
    pub(crate) fn restore(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            ..Self::default()
        }
    }
    pub(crate) fn feed(&mut self, byte: u8) {
        self.events.0 = None;
        self.parser.advance(&mut self.events, &[byte]);
        let Some(parts) = self.events.0.take() else {
            return;
        };
        let Some(colors) = self.snapshot.0.clone() else {
            return;
        };
        let Some(code) = parts
            .first()
            .and_then(|value| std::str::from_utf8(value).ok())
        else {
            return;
        };
        match code {
            "10" if parts.get(1).is_some_and(|value| value == b"?") => {
                self.reply_default(10, colors.foreground)
            }
            "11" if parts.get(1).is_some_and(|value| value == b"?") => {
                self.reply_default(11, colors.background)
            }
            "4" => {
                for pair in parts[1..].chunks(2).take(16) {
                    let Some(index) = pair
                        .first()
                        .and_then(|value| std::str::from_utf8(value).ok())
                        .and_then(|value| value.parse::<usize>().ok())
                        .filter(|index| *index < 16)
                    else {
                        continue;
                    };
                    if pair.get(1).is_some_and(|value| value == b"?") {
                        self.reply_palette(index, colors.palette[index]);
                    }
                }
            }
            _ => {}
        }
    }
    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }
    fn reply_default(&mut self, code: u8, rgb: [u8; 3]) {
        self.replies.extend_from_slice(
            format!(
                "\x1b]{code};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                u16::from(rgb[0]) * 257,
                u16::from(rgb[1]) * 257,
                u16::from(rgb[2]) * 257
            )
            .as_bytes(),
        );
    }
    fn reply_palette(&mut self, index: usize, rgb: [u8; 3]) {
        self.replies.extend_from_slice(
            format!(
                "\x1b]4;{index};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                u16::from(rgb[0]) * 257,
                u16::from(rgb[1]) * 257,
                u16::from(rgb[2]) * 257
            )
            .as_bytes(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn colors() -> TerminalColors {
        TerminalColors {
            foreground: [1, 2, 3],
            background: [4, 5, 6],
            palette: [[7, 8, 9]; 16],
        }
    }
    #[test]
    fn answers_known_read_only_queries_and_bounds_palette_pairs() {
        let mut tracker = Tracker::default();
        for byte in b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b]4;3;?;17;?\x1b\\" {
            tracker.feed(*byte);
        }
        assert!(tracker.take_replies().is_empty());
        tracker.set(Some(colors()));
        for byte in b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b]4;3;?;17;?\x1b\\\x1b]12;?\x1b\\" {
            tracker.feed(*byte);
        }
        assert_eq!(tracker.take_replies(), b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]4;3;rgb:0707/0808/0909\x1b\\");
    }
}
