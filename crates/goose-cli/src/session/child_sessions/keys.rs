#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Open,
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Back,
    Close,
    Click { row: u16, col: u16 },
    WheelUp,
    WheelDown,
}

const ESC: u8 = 0x1b;

/// Turns raw terminal bytes into navigator keys. Escape sequences may arrive split across
/// reads, so incomplete input is kept until more bytes arrive or the input goes idle.
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
        lone_escape.then_some(Key::Back)
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
        0x7f | 0x08 => Key::Back,
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'q' => Key::Close,
        _ => return Parsed::Skip(1),
    };
    Parsed::Key(key, 1)
}

fn parse_escape(bytes: &[u8]) -> Parsed {
    match bytes.get(1) {
        None => Parsed::Incomplete,
        Some(b'[') => parse_csi(bytes),
        Some(b'O') => match bytes.get(2) {
            None => Parsed::Incomplete,
            Some(&final_byte) => match arrow(final_byte) {
                Some(key) => Parsed::Key(key, 3),
                None => Parsed::Skip(3),
            },
        },
        Some(_) => Parsed::Key(Key::Back, 1),
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
        (b"", final_byte) => arrow(final_byte),
        _ => None,
    };
    match key {
        Some(key) => Parsed::Key(key, used),
        None => Parsed::Skip(used),
    }
}

fn arrow(final_byte: u8) -> Option<Key> {
    match final_byte {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Enter),
        b'D' => Some(Key::Back),
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
            parse(b"\tjk\rq\x7fx"),
            [
                Key::Open,
                Key::Down,
                Key::Up,
                Key::Enter,
                Key::Close,
                Key::Back
            ]
        );
    }

    #[test]
    fn maps_arrows_and_pages() {
        assert_eq!(
            parse(b"\x1b[A\x1b[B\x1bOA\x1b[C\x1b[D\x1b[5~\x1b[6~"),
            [
                Key::Up,
                Key::Down,
                Key::Up,
                Key::Enter,
                Key::Back,
                Key::PageUp,
                Key::PageDown
            ]
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
        assert_eq!(parser.flush_idle(), Some(Key::Back));
        assert_eq!(parser.flush_idle(), None);
    }

    #[test]
    fn skips_unknown_sequences() {
        assert_eq!(parse(b"\x1b[1;5H\x1b[2~j"), [Key::Down]);
    }
}
