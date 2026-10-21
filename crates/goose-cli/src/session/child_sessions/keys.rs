/// One key as the terminal reported it. The navigator, the dock and the turn's input line
/// each decide what a key means to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Tab.
    Open,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    /// Ctrl+Enter, or Alt+Enter where the terminal cannot tell Ctrl+Enter from Enter.
    SendNow,
    Backspace,
    /// Ctrl-U.
    KillLine,
    Escape,
    /// Ctrl-C, when the terminal reports it as a key (kitty keyboard protocol) instead of
    /// raising SIGINT.
    Interrupt,
    Char(char),
    Click {
        row: u16,
        col: u16,
    },
    WheelUp,
    WheelDown,
}

const ESC: u8 = 0x1b;
const CTRL: u16 = 4;
const ALT: u16 = 2;

/// Turns raw terminal bytes into keys. Escape sequences may arrive split across reads, so
/// incomplete input is kept until more bytes arrive or the input goes idle.
#[derive(Default)]
pub struct KeyParser {
    pending: Vec<u8>,
}

impl KeyParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            match parse_one(&self.pending[i..]) {
                Parsed::Key(key, used) => {
                    keys.push(key);
                    i += used;
                }
                Parsed::Skip(used) => i += used,
                Parsed::Incomplete => break,
            }
        }
        self.pending.drain(..i);
        keys
    }

    /// A lone ESC is only distinguishable from the start of a sequence once input stops.
    pub fn flush_idle(&mut self) -> Option<Key> {
        let lone_escape = self.pending == [ESC];
        self.pending.clear();
        lone_escape.then_some(Key::Escape)
    }
}

enum Parsed {
    Key(Key, usize),
    Skip(usize),
    Incomplete,
}

fn parse_one(bytes: &[u8]) -> Parsed {
    let key = match bytes[0] {
        ESC => return parse_escape(bytes),
        b'\t' => Key::Open,
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x01 => Key::Home,
        0x05 => Key::End,
        0x15 => Key::KillLine,
        0x00..=0x1f => return Parsed::Skip(1),
        _ => return parse_char(bytes),
    };
    Parsed::Key(key, 1)
}

/// One UTF-8 character; a truncated sequence waits for its remaining bytes.
fn parse_char(bytes: &[u8]) -> Parsed {
    let len = match bytes[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Parsed::Skip(1),
    };
    if bytes.len() < len {
        return Parsed::Incomplete;
    }
    match std::str::from_utf8(&bytes[..len])
        .ok()
        .and_then(|text| text.chars().next())
    {
        Some(c) => Parsed::Key(Key::Char(c), len),
        None => Parsed::Skip(1),
    }
}

fn parse_escape(bytes: &[u8]) -> Parsed {
    match bytes.get(1) {
        None => Parsed::Incomplete,
        Some(b'[') => parse_csi(bytes),
        Some(b'O') => match bytes.get(2) {
            None => Parsed::Incomplete,
            Some(&final_byte) => match cursor_key(final_byte) {
                Some(key) => Parsed::Key(key, 3),
                None => Parsed::Skip(3),
            },
        },
        // Alt+Enter; the terminal's input translation may turn its CR into LF.
        Some(b'\r' | b'\n') => Parsed::Key(Key::SendNow, 2),
        Some(_) => Parsed::Key(Key::Escape, 1),
    }
}

fn parse_csi(bytes: &[u8]) -> Parsed {
    let Some(end) = bytes[2..]
        .iter()
        .position(|b| (0x40..=0x7e).contains(b))
        .map(|offset| offset + 2)
    else {
        return Parsed::Incomplete;
    };
    let params = &bytes[2..end];
    let used = end + 1;
    let key = match (params, bytes[end]) {
        ([b'<', mouse @ ..], final_byte @ (b'M' | b'm')) => sgr_mouse(mouse, final_byte == b'M'),
        (b"5", b'~') => Some(Key::PageUp),
        (b"6", b'~') => Some(Key::PageDown),
        (b"1" | b"7", b'~') => Some(Key::Home),
        (b"4" | b"8", b'~') => Some(Key::End),
        // kitty keyboard protocol: `code;modifiers u`.
        (params, b'u') => csi_u(params),
        // xterm modifyOtherKeys: `27;modifiers;code ~`.
        ([b'2', b'7', b';', rest @ ..], b'~') => std::str::from_utf8(rest)
            .ok()
            .and_then(|text| text.split_once(';'))
            .and_then(|(mods, code)| {
                modified(
                    code.parse().ok()?,
                    mods.parse::<u16>().ok()?.saturating_sub(1),
                )
            }),
        (b"", final_byte) => cursor_key(final_byte),
        _ => None,
    };
    match key {
        Some(key) => Parsed::Key(key, used),
        None => Parsed::Skip(used),
    }
}

fn csi_u(params: &[u8]) -> Option<Key> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    let code: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let mods = fields
        .next()
        .and_then(|m| m.split(':').next()?.parse::<u16>().ok())
        .map_or(0, |m| m.saturating_sub(1));
    modified(code, mods)
}

/// A key with modifiers (bit 1 Alt, bit 2 Ctrl), from either extended keyboard report.
fn modified(code: u32, mods: u16) -> Option<Key> {
    let ctrl = mods & CTRL != 0;
    match code {
        13 if ctrl || mods & ALT != 0 => Some(Key::SendNow),
        13 => Some(Key::Enter),
        27 => Some(Key::Escape),
        9 => Some(Key::Open),
        127 | 8 => Some(Key::Backspace),
        99 if ctrl => Some(Key::Interrupt),
        117 if ctrl => Some(Key::KillLine),
        97 if ctrl => Some(Key::Home),
        101 if ctrl => Some(Key::End),
        _ if ctrl => None,
        code => char::from_u32(code)
            .filter(|c| !c.is_control())
            .map(Key::Char),
    }
}

fn cursor_key(final_byte: u8) -> Option<Key> {
    match final_byte {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        _ => None,
    }
}

/// SGR 1006 mouse report: `button;col;row`, 1-based coordinates.
fn sgr_mouse(params: &[u8], pressed: bool) -> Option<Key> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';').map(|field| field.parse::<u16>().ok());
    let (button, col, row) = (fields.next()??, fields.next()??, fields.next()??);
    match button {
        0 if pressed => Some(Key::Click {
            row: row.saturating_sub(1),
            col: col.saturating_sub(1),
        }),
        64 => Some(Key::WheelUp),
        65 => Some(Key::WheelDown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Key> {
        KeyParser::default().feed(bytes)
    }

    #[test]
    fn maps_plain_keys() {
        assert_eq!(
            parse(b"\tjk\rq\x7f\x15"),
            [
                Key::Open,
                Key::Char('j'),
                Key::Char('k'),
                Key::Enter,
                Key::Char('q'),
                Key::Backspace,
                Key::KillLine,
            ]
        );
    }

    #[test]
    fn maps_cursor_keys_and_pages() {
        assert_eq!(
            parse(b"\x1b[A\x1b[B\x1bOA\x1b[C\x1b[D\x1b[H\x1b[4~\x1b[5~\x1b[6~"),
            [
                Key::Up,
                Key::Down,
                Key::Up,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End,
                Key::PageUp,
                Key::PageDown
            ]
        );
    }

    #[test]
    fn reads_multibyte_characters_across_reads() {
        let mut parser = KeyParser::default();
        let bytes = "é♪".as_bytes();
        assert_eq!(parser.feed(&bytes[..1]), []);
        assert_eq!(parser.feed(&bytes[1..]), [Key::Char('é'), Key::Char('♪')]);
    }

    #[test]
    fn ctrl_and_alt_enter_send_now() {
        assert_eq!(
            parse(b"\x1b[13;5u\x1b[27;5;13~\x1b\r\x1b\n\x1b[13u"),
            [
                Key::SendNow,
                Key::SendNow,
                Key::SendNow,
                Key::SendNow,
                Key::Enter
            ]
        );
    }

    #[test]
    fn kitty_reports_of_control_keys_keep_their_meaning() {
        assert_eq!(
            parse(b"\x1b[99;5u\x1b[27u\x1b[117;5u\x1b[65;2u"),
            [Key::Interrupt, Key::Escape, Key::KillLine, Key::Char('A')]
        );
    }

    #[test]
    fn maps_sgr_mouse_events() {
        assert_eq!(
            parse(b"\x1b[<0;5;3M\x1b[<0;5;3m\x1b[<64;1;1M\x1b[<65;1;1M\x1b[<2;1;1M"),
            [Key::Click { row: 2, col: 4 }, Key::WheelUp, Key::WheelDown]
        );
    }

    #[test]
    fn keeps_split_sequences_until_complete() {
        let mut parser = KeyParser::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert!(parser.feed(b"[<0;2").is_empty());
        assert_eq!(parser.feed(b";4M"), [Key::Click { row: 3, col: 1 }]);
        assert_eq!(parser.flush_idle(), None);
    }

    #[test]
    fn lone_escape_resolves_when_idle() {
        let mut parser = KeyParser::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(parser.flush_idle(), Some(Key::Escape));
        assert_eq!(parser.flush_idle(), None);
    }

    #[test]
    fn skips_unknown_sequences() {
        assert_eq!(parse(b"\x1b[1;5H\x1b[2~j"), [Key::Char('j')]);
    }
}
