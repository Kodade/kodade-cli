//! Small, bounded terminal negotiation state owned by each pane parser.

use kodade_cli_proto::KeyboardModes;

const SUPPORTED_KITTY_FLAGS: u8 = 0b11;

#[derive(Default)]
pub struct Modes {
    main_keyboard: KeyboardModes,
    alternate_keyboard: KeyboardModes,
    main_keyboard_stack: Vec<u8>,
    alternate_keyboard_stack: Vec<u8>,
    replies: Vec<u8>,
    capability_parser: vte::Parser,
    capability_events: CapabilityEvents,
    reset_escaped: bool,
}

impl Modes {
    pub fn keyboard(&self, alternate_screen: bool) -> KeyboardModes {
        if alternate_screen {
            self.alternate_keyboard
        } else {
            self.main_keyboard
        }
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn feed(&mut self, byte: u8) {
        if self.reset_escaped && byte == b'c' {
            *self = Self::default();
            return;
        }
        self.reset_escaped = byte == 27;
        self.capability_events.completed = None;
        self.capability_parser
            .advance(&mut self.capability_events, &[byte]);
        if let Some(query) = self.capability_events.completed.take() {
            self.xtgettcap(&query);
        }
    }

    fn xtgettcap(&mut self, query: &[u8]) {
        let mut reply = Vec::new();
        for encoded in query.split(|byte| *byte == b';') {
            let Ok(name) = decode_hex(encoded) else {
                self.replies.extend_from_slice(b"\x1bP0+r\x1b\\");
                return;
            };
            let value: &[u8] = match name.as_slice() {
                b"Tc" | b"Su" => b"1",
                b"RGB" => b"8",
                b"Ms" => b"\x1b]52;%p1%s;%p2%s\x07",
                _ => {
                    self.replies.extend_from_slice(b"\x1bP0+r\x1b\\");
                    return;
                }
            };
            if !reply.is_empty() {
                reply.push(b';');
            }
            reply.extend_from_slice(encoded);
            reply.push(b'=');
            hex_encode(value, &mut reply);
        }
        self.replies.extend_from_slice(b"\x1bP1+r");
        self.replies.extend_from_slice(&reply);
        self.replies.extend_from_slice(b"\x1b\\");
    }

    pub fn csi(
        &mut self,
        size: (u16, u16),
        cursor: (u16, u16),
        alternate_screen: bool,
        i1: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let (rows, cols) = size;
        let p = |n: usize| params.get(n).and_then(|x| x.first()).copied().unwrap_or(0);
        match (i1, c) {
            // Kitty keyboard protocol. The protocol requires independent main
            // and alternate-screen stacks; cap each stack to keep hostile PTY
            // output from accumulating state indefinitely.
            (Some(b'>'), 'u') => self.push_keyboard(alternate_screen, kitty_flags(p(0))),
            (Some(b'='), 'u') => {
                let flags = kitty_flags(p(0));
                let keyboard = self.keyboard_mut(alternate_screen);
                match p(1) {
                    0 | 1 => keyboard.kitty_flags = flags,
                    2 => keyboard.kitty_flags |= flags,
                    3 => keyboard.kitty_flags &= !flags,
                    _ => {}
                }
            }
            (Some(b'<'), 'u') => self.pop_keyboard(alternate_screen, p(0).max(1) as usize),
            (Some(b'?'), 'u') => self.replies.extend_from_slice(
                format!("\x1b[?{}u", self.keyboard(alternate_screen).kitty_flags).as_bytes(),
            ),
            // xterm modifyOtherKeys levels are 0, 1, and 2.
            (Some(b'>'), 'm') if p(0) == 4 => {
                self.keyboard_mut(alternate_screen).modify_other_keys = p(1).min(2) as u8
            }
            (Some(b'>'), 'n') if p(0) == 4 => {
                self.keyboard_mut(alternate_screen).modify_other_keys = 0
            }
            (Some(b'?'), 'm') if p(0) == 4 => self.replies.extend_from_slice(
                format!(
                    "\x1b[>4;{}m",
                    self.keyboard(alternate_screen).modify_other_keys
                )
                .as_bytes(),
            ),
            // DSR: terminal OK and current cursor position.
            (None, 'n') if p(0) == 5 => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, 'n') if p(0) == 6 => self
                .replies
                .extend_from_slice(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes()),
            (Some(b'?'), 'n') if p(0) == 6 => self
                .replies
                .extend_from_slice(format!("\x1b[?{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes()),
            // xterm text-area character size query. Pixel size is deliberately
            // unanswered because this terminal does not render pixel geometry.
            (None, 't') if p(0) == 18 => self
                .replies
                .extend_from_slice(format!("\x1b[8;{rows};{cols}t").as_bytes()),
            _ => {}
        }
    }

    fn push_keyboard(&mut self, alternate_screen: bool, flags: u8) {
        let (stack, keyboard) = if alternate_screen {
            (
                &mut self.alternate_keyboard_stack,
                &mut self.alternate_keyboard,
            )
        } else {
            (&mut self.main_keyboard_stack, &mut self.main_keyboard)
        };
        if stack.len() == 16 {
            stack.remove(0);
        }
        stack.push(keyboard.kitty_flags);
        keyboard.kitty_flags = flags;
    }

    fn pop_keyboard(&mut self, alternate_screen: bool, count: usize) {
        let (stack, keyboard) = if alternate_screen {
            (
                &mut self.alternate_keyboard_stack,
                &mut self.alternate_keyboard,
            )
        } else {
            (&mut self.main_keyboard_stack, &mut self.main_keyboard)
        };
        for _ in 0..count {
            keyboard.kitty_flags = stack.pop().unwrap_or(0);
        }
    }

    fn keyboard_mut(&mut self, alternate_screen: bool) -> &mut KeyboardModes {
        if alternate_screen {
            &mut self.alternate_keyboard
        } else {
            &mut self.main_keyboard
        }
    }
}

fn kitty_flags(flags: u16) -> u8 {
    (flags & u16::from(SUPPORTED_KITTY_FLAGS)) as u8
}

#[derive(Default)]
struct CapabilityEvents {
    collecting: bool,
    overflow: bool,
    bytes: Vec<u8>,
    completed: Option<Vec<u8>>,
}
impl vte::Perform for CapabilityEvents {
    fn hook(&mut self, _: &vte::Params, intermediate: &[u8], ignore: bool, action: char) {
        self.collecting = !ignore && intermediate == b"+" && action == 'q';
        self.overflow = false;
        self.bytes.clear();
    }
    fn put(&mut self, byte: u8) {
        if self.collecting {
            if self.bytes.len() < 1024 {
                self.bytes.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }
    fn unhook(&mut self) {
        if self.collecting {
            self.completed = Some(if self.overflow {
                vec![0]
            } else {
                std::mem::take(&mut self.bytes)
            });
        }
        self.collecting = false;
    }
}

fn decode_hex(value: &[u8]) -> Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    value
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16).ok_or(())?;
            let lo = (pair[1] as char).to_digit(16).ok_or(())?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

fn hex_encode(value: &[u8], out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in value {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 15) as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_are_bounded_and_reply() {
        let mut m = Modes::default();
        m.csi((24, 80), (2, 3), false, Some(b'>'), &[&[99]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 3);
        m.csi((24, 80), (2, 3), false, None, &[&[6]], 'n');
        assert_eq!(m.take_replies(), b"\x1b[3;4R");
        m.csi((24, 80), (2, 3), false, Some(b'<'), &[], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 0);
    }

    #[test]
    fn kitty_stacks_are_bounded_and_screen_local() {
        let mut m = Modes::default();
        m.csi((24, 80), (0, 0), false, Some(b'>'), &[&[1]], 'u');
        m.csi((24, 80), (0, 0), false, Some(b'>'), &[&[8]], 'u');
        m.csi((24, 80), (0, 0), false, Some(b'<'), &[&[1]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 1);
        m.csi((24, 80), (0, 0), true, Some(b'>'), &[&[2]], 'u');
        m.csi((24, 80), (0, 0), true, Some(b'<'), &[&[1]], 'u');
        assert_eq!(m.keyboard(true).kitty_flags, 0);
        m.csi((24, 80), (0, 0), false, Some(b'<'), &[&[1]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 0);
    }

    #[test]
    fn kitty_set_modes_apply_flags_without_promoting_unsupported_bits() {
        let mut m = Modes::default();
        m.csi((24, 80), (0, 0), false, Some(b'='), &[&[8]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 0);
        m.csi((24, 80), (0, 0), false, Some(b'='), &[&[1]], 'u');
        m.csi((24, 80), (0, 0), false, Some(b'='), &[&[2], &[2]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 3);
        m.csi((24, 80), (0, 0), false, Some(b'='), &[&[1], &[3]], 'u');
        assert_eq!(m.keyboard(false).kitty_flags, 2);
    }

    #[test]
    fn replies_only_claim_supported_modes() {
        let mut m = Modes::default();
        m.csi((24, 80), (2, 3), false, Some(b'?'), &[&[4]], 'm');
        m.csi((24, 80), (2, 3), false, Some(b'?'), &[&[6]], 'n');
        m.csi((24, 80), (2, 3), false, None, &[&[18]], 't');
        assert_eq!(m.take_replies(), b"\x1b[>4;0m\x1b[?3;4R\x1b[8;24;80t");
    }

    #[test]
    fn capability_queries_only_report_supported_features() {
        let mut m = Modes::default();
        for byte in b"\x1bP+q5463;524742;4d73;5375\x1b\\" {
            m.feed(*byte);
        }
        assert_eq!(
            m.take_replies(),
            b"\x1bP1+r5463=31;524742=38;4d73=1b5d35323b25703125733b257032257307;5375=31\x1b\\"
        );
    }
}
