//! Output the session's agent produces while no goose prompt is in flight — e.g. a router
//! running a turn on the session after a worker handed off. It is recorded into the session
//! as each burst settles, and at an interactive prompt it is printed above the line being
//! edited through the line editor's external printer.

use super::output;
use super::streaming_buffer::MarkdownBuffer;
use goose::conversation::message::Message;
use goose::conversation::Conversation;
use goose::providers::base::{Provider, UnsolicitedUpdate};
use goose::session::SessionManager;
use std::collections::HashSet;
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::task::AbortOnDropHandle;

/// How long leaving the prompt waits for output already handed to the printer.
const PRINT_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

pub type Printer = Box<dyn rustyline::ExternalPrinter + Send>;

pub struct UnsolicitedRelay {
    provider: Arc<dyn Provider>,
    session_id: String,
    shared: Arc<Shared>,
    _task: AbortOnDropHandle<()>,
}

struct Shared {
    session_manager: Arc<SessionManager>,
    session_id: String,
    state: Mutex<State>,
    persisting: tokio::sync::Mutex<HashSet<String>>,
}

#[derive(Default)]
struct State {
    pending: Conversation,
    recorded: Vec<Message>,
    display: Option<Display>,
}

impl UnsolicitedRelay {
    /// Starts relaying `provider`'s unsolicited output into `session_id`. With a printer
    /// the output is also rendered live whenever the prompt is showing.
    pub fn start(
        provider: Arc<dyn Provider>,
        session_manager: Arc<SessionManager>,
        session_id: String,
        printer: Option<Printer>,
        debug: bool,
    ) -> Option<Self> {
        let mut updates = provider.subscribe_unsolicited()?;
        let shared = Arc::new(Shared {
            session_manager,
            session_id: session_id.clone(),
            state: Mutex::new(State {
                display: printer.map(|printer| Display::new(printer, debug)),
                ..State::default()
            }),
            persisting: tokio::sync::Mutex::new(HashSet::new()),
        });
        let relayed = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                match updates.recv().await {
                    Ok(UnsolicitedUpdate::Message(message)) => relayed.stream(message),
                    Ok(UnsolicitedUpdate::Settled) => relayed.settle().await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        relayed.settle().await;
                        break;
                    }
                }
            }
        });
        Some(Self {
            provider,
            session_id,
            shared,
            _task: AbortOnDropHandle::new(task),
        })
    }

    pub fn follows(&self, provider: &Arc<dyn Provider>, session_id: &str) -> bool {
        Arc::ptr_eq(&self.provider, provider) && self.session_id == session_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn displays(&self) -> bool {
        self.shared.state.lock().unwrap().display.is_some()
    }

    /// The prompt is about to show: print what arrived meanwhile and print live from now on.
    pub fn resume(&self) {
        if let Some(display) = self.shared.state.lock().unwrap().display.as_mut() {
            display.resume();
        }
    }

    /// The prompt is gone: hold new output until it shows again, and let output already
    /// handed to the printer land before the caller writes to the terminal.
    pub fn pause(&self) {
        let drained = self
            .shared
            .state
            .lock()
            .unwrap()
            .display
            .as_mut()
            .map(Display::pause);
        if let Some(drained) = drained {
            let _ = drained.recv_timeout(PRINT_DRAIN_TIMEOUT);
        }
    }

    /// Records whatever has streamed so far, e.g. before a user prompt starts so that the
    /// background output stays ahead of it in the history.
    pub async fn settle(&self) {
        self.shared.settle().await;
    }

    /// Messages recorded into the session since the last call, for the caller's own copy of
    /// the conversation.
    pub fn take_recorded(&self) -> Vec<Message> {
        std::mem::take(&mut self.shared.state.lock().unwrap().recorded)
    }
}

impl Shared {
    fn stream(&self, message: Message) {
        let mut state = self.state.lock().unwrap();
        if let Some(display) = state.display.as_mut() {
            display.render(&message);
        }
        state.pending.push(message);
    }

    async fn settle(&self) {
        let mut persisted_ids = self.persisting.lock().await;
        let messages = {
            let mut state = self.state.lock().unwrap();
            if let Some(display) = state.display.as_mut() {
                display.finish();
            }
            std::mem::take(&mut state.pending).messages().clone()
        };
        let mut recorded = Vec::with_capacity(messages.len());
        for mut message in messages {
            if let Some(id) = &message.id {
                if !persisted_ids.insert(id.clone()) {
                    message.id = None;
                }
            }
            if let Err(error) = self
                .session_manager
                .add_message(&self.session_id, &message)
                .await
            {
                tracing::warn!(%error, "could not record background agent output");
            }
            recorded.push(message);
        }
        self.state.lock().unwrap().recorded.extend(recorded);
    }
}

enum PrintJob {
    Print(String),
    Drained(std_mpsc::SyncSender<()>),
}

/// Renders streamed messages the way a turn renders them and hands complete lines to the
/// printer. Printing runs on its own thread because the external printer can block until
/// the line editor picks the text up.
struct Display {
    jobs: std_mpsc::Sender<PrintJob>,
    at_prompt: bool,
    held: String,
    partial: String,
    markdown: MarkdownBuffer,
    thinking_header_shown: bool,
    in_burst: bool,
    debug: bool,
}

impl Display {
    fn new(mut printer: Printer, debug: bool) -> Self {
        let (jobs, queue) = std_mpsc::channel::<PrintJob>();
        std::thread::spawn(move || {
            for job in queue {
                match job {
                    PrintJob::Print(text) => {
                        let _ = printer.print(text);
                    }
                    PrintJob::Drained(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self {
            jobs,
            at_prompt: false,
            held: String::new(),
            partial: String::new(),
            markdown: MarkdownBuffer::new(),
            thinking_header_shown: false,
            in_burst: false,
            debug,
        }
    }

    fn resume(&mut self) {
        self.at_prompt = true;
        let held = std::mem::take(&mut self.held);
        self.print(held);
    }

    fn pause(&mut self) -> std_mpsc::Receiver<()> {
        self.at_prompt = false;
        let (done, drained) = std_mpsc::sync_channel(1);
        let _ = self.jobs.send(PrintJob::Drained(done));
        drained
    }

    /// A burst prints as it would during a turn, with no banner of its own: the dock and the
    /// child sessions it opens already say which agent is working.
    fn render(&mut self, message: &Message) {
        self.in_burst = true;
        let (markdown, thinking_header_shown, debug) = (
            &mut self.markdown,
            &mut self.thinking_header_shown,
            self.debug,
        );
        // Text after the last line end has not gone out yet: a tool-call line must end it.
        let line_start = self.partial.is_empty();
        let text = output::capture_output_after(line_start, || {
            output::render_message_streaming(message, markdown, thinking_header_shown, debug)
        });
        self.emit(text, false);
    }

    fn finish(&mut self) {
        if !self.in_burst {
            return;
        }
        let markdown = &mut self.markdown;
        let tail = output::capture_output(|| output::flush_markdown_buffer_current_theme(markdown));
        self.emit(tail, true);
        self.in_burst = false;
        self.thinking_header_shown = false;
    }

    /// The external printer ends every print with a newline, so only whole lines go out
    /// until the burst finishes.
    fn emit(&mut self, text: String, flush: bool) {
        self.partial.push_str(&text);
        let ready = if flush {
            std::mem::take(&mut self.partial)
        } else {
            match self.partial.rfind('\n') {
                Some(end) => {
                    let rest = self.partial.split_off(end + 1);
                    std::mem::replace(&mut self.partial, rest)
                }
                None => return,
            }
        };
        if ready.trim().is_empty() {
            return;
        }
        if self.at_prompt {
            self.print(ready);
        } else {
            self.held.push_str(&ready);
        }
    }

    fn print(&self, text: String) {
        if !text.is_empty() {
            let _ = self.jobs.send(PrintJob::Print(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Captured(Arc<Mutex<Vec<String>>>);

    impl rustyline::ExternalPrinter for Captured {
        fn print(&mut self, msg: String) -> rustyline::Result<()> {
            self.0.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn display() -> (Display, Arc<Mutex<Vec<String>>>) {
        let printed = Arc::new(Mutex::new(Vec::new()));
        let display = Display::new(Box::new(Captured(printed.clone())), false);
        (display, printed)
    }

    fn printed_text(display: &mut Display, printed: &Arc<Mutex<Vec<String>>>) -> String {
        display
            .pause()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let text = printed.lock().unwrap().concat();
        console::strip_ansi_codes(&text).into_owned()
    }

    #[test]
    fn output_waits_for_the_prompt_and_prints_whole_lines() {
        let (mut display, printed) = display();
        display.emit("held line\npartial".to_string(), false);
        assert!(printed_text(&mut display, &printed).is_empty());

        display.resume();
        display.emit(" line\n".to_string(), false);
        display.emit("tail".to_string(), true);
        assert_eq!(
            printed_text(&mut display, &printed),
            "held line\npartial line\ntail"
        );
    }

    #[tokio::test]
    async fn settled_output_is_recorded_as_assistant_messages() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let session_manager = Arc::new(SessionManager::new(temp_dir.path().to_path_buf()));
        let session_id = session_manager
            .create_session(
                temp_dir.path().to_path_buf(),
                "parent".to_string(),
                goose::session::session_manager::SessionType::User,
                goose::config::GooseMode::default(),
            )
            .await
            .unwrap()
            .id;
        let shared = Shared {
            session_manager: session_manager.clone(),
            session_id: session_id.clone(),
            state: Mutex::new(State::default()),
            persisting: tokio::sync::Mutex::new(HashSet::new()),
        };
        for chunk in ["Worker handed ", "off."] {
            shared.stream(Message::assistant().with_text(chunk).with_id("wake-1"));
        }
        shared.settle().await;
        shared.stream(Message::assistant().with_text("late").with_id("wake-1"));
        shared.settle().await;

        let stored = session_manager
            .get_session(&session_id, true)
            .await
            .unwrap()
            .conversation
            .unwrap();
        let texts: Vec<(rmcp::model::Role, String)> = stored
            .messages()
            .iter()
            .map(|message| (message.role.clone(), message.as_concat_text()))
            .collect();
        assert_eq!(
            texts,
            [
                (
                    rmcp::model::Role::Assistant,
                    "Worker handed off.".to_string()
                ),
                (rmcp::model::Role::Assistant, "late".to_string()),
            ]
        );
        assert_eq!(shared.state.lock().unwrap().recorded.len(), 2);
    }
}
