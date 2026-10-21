//! Read-only navigator for the subagent sessions a parent session delegates to.
//! Opened with Tab while a turn runs, with `/children` between turns, or on a child chosen
//! from the dock (`dock.rs`), the persistent child list under the prompt.

pub mod dock;
mod keys;
pub mod router;
#[cfg(unix)]
mod terminal;
#[cfg(not(unix))]
#[path = "terminal_unsupported.rs"]
mod terminal;
mod view;

use console::style;
use goose::agents::state_machine::{subagent_states, SubagentState};
use goose::conversation::message::{Message, MessageContent};
use goose::providers::router_acp::{RouterStateSource, DELEGATE_ID_SEPARATOR};
use goose::session::SessionManager;
use goose::utils::safe_truncate;
pub use keys::Key;
use rmcp::model::{ContentBlock, Role};
use std::sync::Arc;
use std::time::Duration;
pub use terminal::EchoOff;
/// Raw key input for a view the session drives itself (the dock's selection).
pub use terminal::KeyListener as KeyReader;
/// The alternate screen, for other full-screen views (the tool-call list).
pub use terminal::Screen;
pub use view::ChildRow;
use view::{ChildStatus, LineKind, Navigator, Outcome, Snapshot, TranscriptLine};

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
    router: Option<router::RouterChildren>,
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
            router: None,
        }
    }

    /// Also lists the delegates of router session `router_session_id`, which live in
    /// router-acp's state rather than goose's.
    pub fn with_router(
        mut self,
        source: anyhow::Result<RouterStateSource>,
        router_session_id: String,
    ) -> Self {
        if self.available {
            self.router = Some(router::RouterChildren::start(source, router_session_id));
        }
        self
    }

    /// Reads keys for the whole turn, so text can be typed while the agent works.
    pub fn listen_for_input(&mut self) {
        self.has_children = true;
        self.hinted = true;
        self.listen();
    }

    pub fn watches_router(&self) -> bool {
        self.router.is_some()
    }

    /// Offers the navigator once router-acp reports a delegate at work, since router
    /// delegates never show up in goose's own subagent state.
    pub fn check_router(&mut self) {
        if !self.has_children
            && self
                .router
                .as_ref()
                .is_some_and(|router| router.any_running(chrono::Utc::now()))
        {
            self.has_children = true;
            self.listen();
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
        if let Some(router) = &self.router {
            let router_path: Vec<&str> = view
                .navigator
                .path_ids()
                .into_iter()
                .filter(|id| is_router_delegate(id))
                .collect();
            router.follow(&router_path);
        }
        if let Ok(snapshot) = load_snapshot(
            &self.session_manager,
            &self.root_id,
            self.router.as_ref(),
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

    /// Opens the navigator inside `child` (chosen from the dock) while a turn runs.
    pub async fn open_at(&mut self, child: &ChildRow, turn_active: bool) {
        if !self.available {
            return;
        }
        let mut navigator = Navigator::default();
        navigator.enter(child);
        self.open = Some(OpenView {
            screen: terminal::Screen::enter(),
            navigator,
            snapshot: Snapshot::default(),
            frame: view::Frame::default(),
        });
        self.refresh(turn_active).await;
    }

    /// `browse`, starting inside `child`.
    pub async fn browse_at(&mut self, child: &ChildRow) {
        if !self.available {
            return;
        }
        self.has_children = true;
        self.hinted = true;
        self.listen();
        self.open_at(child, false).await;
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        while self.is_open() {
            tokio::select! {
                key = self.next_key() => self.handle_key(key, false).await,
                _ = tick.tick() => self.refresh(false).await,
            }
        }
        self.suspend();
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

fn is_router_delegate(id: &str) -> bool {
    id.contains(DELEGATE_ID_SEPARATOR)
}

async fn load_snapshot(
    session_manager: &SessionManager,
    root_id: &str,
    router: Option<&router::RouterChildren>,
    current: Option<&str>,
    turn_active: bool,
) -> anyhow::Result<Snapshot> {
    let mut snapshot = match current {
        Some(id) if is_router_delegate(id) => Snapshot {
            transcript: Some(Vec::new()),
            ..Default::default()
        },
        _ => load_goose_snapshot(session_manager, root_id, current, turn_active).await?,
    };
    let Some(router) = router else {
        return Ok(snapshot);
    };
    let delegates = match router.latest() {
        router::Fetched::Ready(delegates) => delegates,
        router::Fetched::Pending => {
            snapshot.notice = Some("Loading router-acp delegates…".to_string());
            return Ok(snapshot);
        }
        router::Fetched::Failed(error) => {
            snapshot.notice = Some(format!("router-acp delegates unavailable: {error}"));
            return Ok(snapshot);
        }
    };
    let now = chrono::Utc::now();
    match current {
        None => snapshot
            .children
            .extend(router::rows_for(&delegates, router.root(), now)),
        Some(id) if is_router_delegate(id) => {
            snapshot.children = router::rows_for(&delegates, id, now);
            match delegates.iter().find(|delegate| delegate.id == id) {
                Some(delegate) => {
                    snapshot.transcript = Some(router::transcript_lines(&delegate.log))
                }
                None => snapshot.notice = Some("router-acp no longer lists this delegate".into()),
            }
        }
        Some(_) => {}
    }
    Ok(snapshot)
}

/// A session's own goose subagents as top-level rows, for the dock.
pub(super) async fn goose_children(
    session_manager: &SessionManager,
    session_id: &str,
    turn_active: bool,
) -> anyhow::Result<Vec<ChildRow>> {
    Ok(
        load_goose_snapshot(session_manager, session_id, None, turn_active)
            .await?
            .children,
    )
}

async fn load_goose_snapshot(
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
            model: child
                .model_config
                .map(|config| config.model_name)
                .unwrap_or_else(|| "unknown model".to_string()),
            started_at: Some(child.created_at),
            label: child.name,
            activity: child.last_message_snippet.unwrap_or_default(),
            id: child.id,
        })
        .collect();
    Ok(Snapshot {
        children,
        transcript: current.map(|_| transcript_lines(&messages)),
        notice: None,
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
