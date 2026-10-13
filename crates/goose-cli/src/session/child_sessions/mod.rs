//! Read-only navigator for the subagent sessions a parent session delegates to.
//! Opened with Tab while a turn runs, or with `/children` between turns.

mod keys;
#[cfg(unix)]
mod terminal;
#[cfg(not(unix))]
#[path = "terminal_unsupported.rs"]
mod terminal;
mod view;

use console::style;
use goose::agents::state_machine::{subagent_states, SubagentState};
use goose::conversation::message::{Message, MessageContent};
use goose::session::SessionManager;
use goose::utils::safe_truncate;
pub use keys::Key;
use rmcp::model::{ContentBlock, Role};
use std::sync::Arc;
use std::time::Duration;
use view::{ChildRow, ChildStatus, LineKind, Navigator, Outcome, Snapshot, TranscriptLine};

pub const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const TOOL_RESULT_CHARS: usize = 200;

struct OpenView {
    screen: terminal::Screen,
    navigator: Navigator,
    snapshot: Snapshot,
    frame: view::Frame,
}

pub struct ChildSessionNavigator {
    session_manager: Arc<SessionManager>,
    root_id: String,
    available: bool,
    has_children: bool,
    hinted: bool,
    listener: Option<terminal::KeyListener>,
    open: Option<OpenView>,
}

impl ChildSessionNavigator {
    /// `enabled` should only be true for interactive, human-readable output; the terminal
    /// itself must also be capable before any control sequence is written.
    pub fn new(session_manager: Arc<SessionManager>, root_id: String, enabled: bool) -> Self {
        Self {
            session_manager,
            root_id,
            available: enabled && terminal::is_capable(),
            has_children: false,
            hinted: false,
            listener: None,
            open: None,
        }
    }

    pub fn observe(&mut self, message: &Message) {
        self.has_children |= delegates_child(message);
        self.listen();
    }

    fn listen(&mut self) {
        if !self.available || !self.has_children || self.listener.is_some() {
            return;
        }
        match terminal::KeyListener::start() {
            Ok(listener) => self.listener = Some(listener),
            Err(_) => {
                self.available = false;
                return;
            }
        }
        if !self.hinted {
            self.hinted = true;
            println!("{}", style("  Press Tab to inspect child sessions.").dim());
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub async fn next_key(&mut self) -> Key {
        match self.listener.as_mut() {
            Some(listener) => match listener.next().await {
                Some(key) => key,
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }

    pub async fn handle_key(&mut self, key: Key, turn_active: bool) {
        let Some(view) = self.open.as_mut() else {
            if key == Key::Open {
                self.open = Some(OpenView {
                    screen: terminal::Screen::enter(),
                    navigator: Navigator::default(),
                    snapshot: Snapshot::default(),
                    frame: view::Frame::default(),
                });
                self.refresh(turn_active).await;
            }
            return;
        };
        if view.navigator.handle(key, &view.snapshot, &view.frame) == Outcome::Close {
            self.close();
        } else {
            self.refresh(turn_active).await;
        }
    }

    pub async fn refresh(&mut self, turn_active: bool) {
        let Some(view) = self.open.as_mut() else {
            return;
        };
        if let Ok(snapshot) = load_snapshot(
            &self.session_manager,
            &self.root_id,
            view.navigator.current(),
            turn_active,
        )
        .await
        {
            view.snapshot = snapshot;
        }
        let (width, height) = view.screen.size();
        view.frame = view.navigator.render(&view.snapshot, width, height);
        view.screen.draw(&view.frame.lines);
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    /// Hands stdin back to prompts that read it themselves; listening resumes on the next
    /// observed message.
    pub fn suspend(&mut self) {
        self.open = None;
        self.listener = None;
    }

    /// Opens the navigator between turns, when completed and failed children stay inspectable.
    pub async fn browse(&mut self) {
        if !self.available {
            println!(
                "{}",
                style("Child sessions can only be browsed in an interactive terminal.").dim()
            );
            return;
        }
        self.has_children = true;
        self.hinted = true;
        self.listen();
        self.handle_key(Key::Open, false).await;
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        while self.is_open() {
            tokio::select! {
                key = self.next_key() => self.handle_key(key, false).await,
                _ = tick.tick() => self.refresh(false).await,
            }
        }
        self.suspend();
    }
}

fn delegates_child(message: &Message) -> bool {
    message.content.iter().any(|content| {
        let MessageContent::ToolResponse(response) = content else {
            return false;
        };
        response.tool_result.as_ref().is_ok_and(|result| {
            result
                .meta
                .as_ref()
                .is_some_and(|meta| meta.0.contains_key("subagent_session_id"))
        })
    })
}

async fn load_snapshot(
    session_manager: &SessionManager,
    root_id: &str,
    current: Option<&str>,
    turn_active: bool,
) -> anyhow::Result<Snapshot> {
    let id = current.unwrap_or(root_id);
    let session = session_manager.get_session(id, true).await?;
    let messages = session
        .conversation
        .map(|conversation| conversation.messages().clone())
        .unwrap_or_default();
    let states = subagent_states(&messages);
    let children = session_manager
        .list_child_sessions(id)
        .await?
        .into_iter()
        .map(|child| ChildRow {
            status: child_status(states.get(&child.id).copied(), turn_active),
            label: child.name,
            activity: child.last_message_snippet.unwrap_or_default(),
            id: child.id,
        })
        .collect();
    Ok(Snapshot {
        children,
        transcript: current.map(|_| transcript_lines(&messages)),
    })
}

/// A child the parent still counts as running is only actually running while the parent's
/// turn drives it; between turns it is stopped.
fn child_status(state: Option<SubagentState>, turn_active: bool) -> ChildStatus {
    match state {
        Some(SubagentState::Completed) => ChildStatus::Completed,
        Some(SubagentState::Failed) => ChildStatus::Failed,
        Some(SubagentState::Cancelled) => ChildStatus::Cancelled,
        Some(SubagentState::Running) | None if turn_active => ChildStatus::Running,
        Some(SubagentState::Running) | None => ChildStatus::Stopped,
    }
}

fn transcript_lines(messages: &[Message]) -> Vec<TranscriptLine> {
    let line = |kind, text: String| TranscriptLine { kind, text };
    let mut lines = Vec::new();
    for message in messages.iter().filter(|message| message.is_user_visible()) {
        let mut body = Vec::new();
        for content in &message.content {
            match content {
                MessageContent::Text(text) if !text.text.trim().is_empty() => {
                    body.push(line(LineKind::Body, text.text.clone()))
                }
                MessageContent::ToolRequest(request) => {
                    if let Ok(call) = &request.tool_call {
                        body.push(line(LineKind::Tool, format!("→ {}", call.name)));
                    }
                }
                MessageContent::ToolResponse(response) => {
                    let summary = match &response.tool_result {
                        Ok(result) => result
                            .content
                            .iter()
                            .find_map(|block| match block {
                                ContentBlock::Text(text) => text.text.lines().next(),
                                _ => None,
                            })
                            .unwrap_or("done")
                            .to_string(),
                        Err(error) => format!("error: {}", error.message),
                    };
                    body.push(line(
                        LineKind::Tool,
                        format!("← {}", safe_truncate(&summary, TOOL_RESULT_CHARS)),
                    ));
                }
                MessageContent::Error(error) => {
                    body.push(line(LineKind::Body, format!("error: {}", error.message)))
                }
                _ => {}
            }
        }
        if body.is_empty() {
            continue;
        }
        let speaker = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        lines.push(line(LineKind::Heading, speaker.to_string()));
        lines.extend(body);
        lines.push(line(LineKind::Body, String::new()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_children_are_stopped_between_turns() {
        assert_eq!(
            child_status(Some(SubagentState::Running), true),
            ChildStatus::Running
        );
        assert_eq!(
            child_status(Some(SubagentState::Running), false),
            ChildStatus::Stopped
        );
        assert_eq!(child_status(None, false), ChildStatus::Stopped);
        assert_eq!(
            child_status(Some(SubagentState::Failed), true),
            ChildStatus::Failed
        );
    }

    #[test]
    fn transcript_shows_visible_text_and_tool_activity() {
        let hidden = Message::user()
            .with_text("internal")
            .with_visibility(false, true);
        let messages = vec![
            Message::user().with_text("Subagent task"),
            hidden,
            Message::assistant().with_text("Working on it"),
        ];

        let lines: Vec<(LineKind, String)> = transcript_lines(&messages)
            .into_iter()
            .map(|line| (line.kind, line.text))
            .collect();

        assert_eq!(
            lines,
            [
                (LineKind::Heading, "user".to_string()),
                (LineKind::Body, "Subagent task".to_string()),
                (LineKind::Body, String::new()),
                (LineKind::Heading, "assistant".to_string()),
                (LineKind::Body, "Working on it".to_string()),
                (LineKind::Body, String::new()),
            ]
        );
    }

    #[tokio::test]
    async fn navigator_stays_inert_without_a_capable_terminal() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let session_manager = Arc::new(SessionManager::new(temp_dir.path().to_path_buf()));
        let mut navigator = ChildSessionNavigator::new(session_manager, "root".into(), false);
        let mut delegate = Message::user();
        let mut meta = rmcp::model::MetaObject::new();
        meta.0
            .insert("subagent_session_id".into(), serde_json::json!("child"));
        delegate.content.push(MessageContent::tool_response(
            "call",
            Ok(rmcp::model::CallToolResult::success(vec![]).with_meta(Some(meta))),
        ));
        assert!(delegates_child(&delegate));

        navigator.observe(&delegate);

        assert!(navigator.listener.is_none());
        assert!(!navigator.is_open());
    }
}
