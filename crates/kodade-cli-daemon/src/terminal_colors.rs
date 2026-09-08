//! Read-only OSC terminal color queries backed by the active rendering client.

use std::io;

use kodade_cli_proto::{standard_xterm_rgb, TerminalColors};
use serde::{Deserialize, Serialize};

/// Enough for one complete 256-slot OSC 4 palette scan, while still bounding
/// state transferred during a live handoff.
const MAX_QUEUED_REPLY_BYTES: usize = 64 * 1024;

/// State that must follow a pane during a live daemon handoff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    colors: Option<TerminalColors>,
    #[serde(default)]
    replies: Vec<u8>,
}

impl Snapshot {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.replies.len() > MAX_QUEUED_REPLY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal color replies exceed handoff limit",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Tracker {
    snapshot: Snapshot,
}

impl Tracker {
    pub(crate) fn set(&mut self, colors: Option<TerminalColors>) {
        self.snapshot.colors = colors;
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub(crate) fn restore(snapshot: Snapshot) -> Self {
        Self { snapshot }
    }

    /// The VT parser owns OSC framing, including split sequences. This only
    /// interprets complete, otherwise-unhandled color queries.
    pub(crate) fn osc(&mut self, parts: &[&[u8]]) {
        let Some(colors) = self.snapshot.colors.clone() else {
            return;
        };
        let Some(code) = parts
            .first()
            .and_then(|value| std::str::from_utf8(value).ok())
        else {
            return;
        };
        match code {
            "10" if parts.get(1).is_some_and(|value| *value == b"?") => {
                self.reply_default(10, colors.foreground)
            }
            "11" if parts.get(1).is_some_and(|value| *value == b"?") => {
                self.reply_default(11, colors.background)
            }
            "12" if parts.get(1).is_some_and(|value| *value == b"?") => {
                self.reply_default(12, colors.cursor)
            }
            "4" => {
                for pair in parts[1..].chunks(2) {
                    let Some(index) = pair
                        .first()
                        .and_then(|value| std::str::from_utf8(value).ok())
                        .and_then(|value| value.parse::<u8>().ok())
                    else {
                        continue;
                    };
                    let rgb = if index < 16 {
                        Some(colors.palette[usize::from(index)])
                    } else {
                        standard_xterm_rgb(index)
                    };
                    if pair.get(1).is_some_and(|value| *value == b"?") {
                        if let Some(rgb) = rgb {
                            self.reply_palette(index, rgb);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.snapshot.replies)
    }

    fn append_reply(&mut self, reply: String) {
        if self.snapshot.replies.len() + reply.len() <= MAX_QUEUED_REPLY_BYTES {
            self.snapshot.replies.extend_from_slice(reply.as_bytes());
        }
    }

    fn reply_default(&mut self, code: u8, rgb: [u8; 3]) {
        self.append_reply(format!(
            "\x1b]{code};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
            u16::from(rgb[0]) * 257,
            u16::from(rgb[1]) * 257,
            u16::from(rgb[2]) * 257
        ));
    }

    fn reply_palette(&mut self, index: u8, rgb: [u8; 3]) {
        self.append_reply(format!(
            "\x1b]4;{index};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
            u16::from(rgb[0]) * 257,
            u16::from(rgb[1]) * 257,
            u16::from(rgb[2]) * 257
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> TerminalColors {
        TerminalColors {
            foreground: [1, 2, 3],
            background: [4, 5, 6],
            cursor: [10, 11, 12],
            palette: [[7, 8, 9]; 16],
        }
    }

    #[test]
    fn answers_known_read_only_queries_and_bounds_palette_pairs() {
        let mut tracker = Tracker::default();
        tracker.osc(&[b"10", b"?"]);
        assert!(tracker.take_replies().is_empty());
        tracker.set(Some(colors()));
        tracker.osc(&[b"10", b"?"]);
        tracker.osc(&[b"11", b"?"]);
        tracker.osc(&[b"4", b"3", b"?", b"17", b"?"]);
        tracker.osc(&[b"12", b"?"]);
        assert_eq!(tracker.take_replies(), b"\x1b]10;rgb:0101/0202/0303\x1b\\\x1b]11;rgb:0404/0505/0606\x1b\\\x1b]4;3;rgb:0707/0808/0909\x1b\\\x1b]4;17;rgb:0000/0000/5f5f\x1b\\\x1b]12;rgb:0a0a/0b0b/0c0c\x1b\\");
    }

    #[test]
    fn answers_a_complete_256_slot_palette_scan_in_one_osc() {
        let mut tracker = Tracker::default();
        tracker.set(Some(colors()));
        let mut values = Vec::with_capacity(513);
        values.push(b"4".to_vec());
        for index in 0_u16..=255 {
            values.push(index.to_string().into_bytes());
            values.push(b"?".to_vec());
        }
        let params = values.iter().map(Vec::as_slice).collect::<Vec<_>>();
        tracker.osc(&params);
        let replies = tracker.take_replies();
        assert_eq!(
            replies
                .windows(4)
                .filter(|part| *part == b"\x1b]4;")
                .count(),
            256
        );
        assert!(replies.len() <= MAX_QUEUED_REPLY_BYTES);
        assert!(replies
            .windows(27)
            .any(|part| part == b"\x1b]4;16;rgb:0000/0000/0000\x1b\\"));
        assert!(replies
            .windows(28)
            .any(|part| part == b"\x1b]4;255;rgb:eeee/eeee/eeee\x1b\\"));
    }

    #[test]
    fn bounds_queued_replies() {
        let mut tracker = Tracker::default();
        tracker.set(Some(colors()));
        for _ in 0..100 {
            tracker.osc(&[b"4", b"0", b"?"]);
        }
        assert!(tracker.snapshot().replies.len() <= MAX_QUEUED_REPLY_BYTES);
    }
}
