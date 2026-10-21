use super::keys::{Key, KeyParser};
use std::io::{IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use tokio::sync::mpsc;

const POLL_MS: i32 = 50;
const ENTER_SCREEN: &str = "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h";
const LEAVE_SCREEN: &str = "\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l";

pub fn is_capable() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// Reads keys from stdin without line buffering or echo for as long as it lives.
/// ISIG stays on so Ctrl-C still reaches the session's cancel handler, and OPOST stays on so
/// parent output printed meanwhile keeps its newline translation.
pub struct KeyListener {
    keys: mpsc::UnboundedReceiver<Key>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    original: libc::termios,
}

impl KeyListener {
    pub fn start() -> std::io::Result<Self> {
        let fd = std::io::stdin().as_raw_fd();
        let original = get_termios(fd)?;
        let mut cbreak = original;
        cbreak.c_lflag &= !(libc::ICANON | libc::ECHO);
        cbreak.c_cc[libc::VMIN] = 1;
        cbreak.c_cc[libc::VTIME] = 0;
        set_termios(fd, &cbreak)?;

        let (tx, keys) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || read_keys(fd, &reader_stop, &tx));
        Ok(Self {
            keys,
            stop,
            reader: Some(reader),
            original,
        })
    }

    pub async fn next(&mut self) -> Option<Key> {
        self.keys.recv().await
    }
}

impl Drop for KeyListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = set_termios(std::io::stdin().as_raw_fd(), &self.original);
    }
}

/// Keeps typed keys off the screen until the turn's key listener reads them: a message
/// typed while the turn is still starting would otherwise echo into the output.
pub struct EchoOff {
    original: libc::termios,
}

impl EchoOff {
    pub fn start() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        let original = get_termios(fd).ok()?;
        let mut quiet = original;
        quiet.c_lflag &= !(libc::ICANON | libc::ECHO);
        set_termios(fd, &quiet).ok()?;
        Some(Self { original })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        let _ = set_termios(std::io::stdin().as_raw_fd(), &self.original);
    }
}

fn read_keys(fd: i32, stop: &AtomicBool, tx: &mpsc::UnboundedSender<Key>) {
    let mut parser = KeyParser::default();
    let mut buf = [0u8; 256];
    while !stop.load(Ordering::Relaxed) {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, POLL_MS) };
        if ready <= 0 {
            if let Some(key) = parser.flush_idle() {
                if tx.send(key).is_err() {
                    return;
                }
            }
            continue;
        }
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read <= 0 {
            return;
        }
        for key in parser.feed(&buf[..read as usize]) {
            if tx.send(key).is_err() {
                return;
            }
        }
    }
}

fn get_termios(fd: i32) -> std::io::Result<libc::termios> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { termios.assume_init() })
}

fn set_termios(fd: i32, termios: &libc::termios) -> std::io::Result<()> {
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The alternate screen with mouse reporting, restored on drop.
pub struct Screen;

impl Screen {
    pub fn enter() -> Self {
        super::dock::suspend();
        write_raw(ENTER_SCREEN);
        Self
    }

    pub fn size(&self) -> (usize, usize) {
        let (rows, cols) = console::Term::stdout().size();
        (cols as usize, rows as usize)
    }

    pub fn draw(&self, lines: &[String]) {
        let mut frame = String::from("\x1b[H");
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                frame.push_str("\r\n");
            }
            frame.push_str(line);
            frame.push_str("\x1b[K");
        }
        frame.push_str("\x1b[J");
        write_raw(&frame);
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        write_raw(LEAVE_SCREEN);
        super::dock::resume();
    }
}

fn write_raw(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}
