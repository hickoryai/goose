use super::keys::Key;

pub fn is_capable() -> bool {
    false
}

pub struct EchoOff;

impl EchoOff {
    pub fn start() -> Option<Self> {
        None
    }
}

pub struct KeyListener;

impl KeyListener {
    pub fn start() -> std::io::Result<Self> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    pub async fn next(&mut self) -> Option<Key> {
        None
    }
}

pub struct Screen;

impl Screen {
    pub fn enter() -> Self {
        Self
    }

    pub fn size(&self) -> (usize, usize) {
        (0, 0)
    }

    pub fn draw(&self, _lines: &[String]) {}
}
