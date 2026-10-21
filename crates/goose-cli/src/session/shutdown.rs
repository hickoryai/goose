//! `/shutdown` for an `/implement` (router-acp planner) session. kory asks router-acp to save
//! the run's resume state and drain its workers (`_router/planner_shutdown`), then prints the
//! `_router/shutdown_progress` notifications router-acp sends as one-line status updates. New
//! prompts are blocked until the workers have drained, and then for good: the next `/implement`
//! in a new session resumes from the saved state.

use goose::config::GooseMode;
use goose::conversation::message::Message;
use goose::providers::base::{ExtensionNotification, Provider};
use rmcp::model::Role;
use serde_json::Value;
use tokio::sync::broadcast;

pub const REQUEST: &str = "_router/planner_shutdown";
pub const PROGRESS: &str = "_router/shutdown_progress";
pub const STATUS: &str = "_router/planner_status";

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Census {
    pub live: Option<usize>,
    pub admissible: Option<usize>,
}

impl Census {
    pub fn interrupted_line(&self) -> String {
        let count = |n: Option<usize>| n.map_or_else(|| "unknown".into(), |n| n.to_string());
        format!("run still active: {} live workers, {} admissible; /shutdown drains, /exit quits and cuts workers off", count(self.live), count(self.admissible))
    }

    pub fn exit_warning(&self, draining: bool) -> Option<&'static str> {
        (draining || self.live != Some(0)).then_some(EXIT_WARNING)
    }
}

pub async fn census(provider: &dyn Provider) -> Census {
    // Cancellation must return control promptly even with an older/unresponsive router.
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        provider.extension_request(STATUS, serde_json::json!({})),
    )
    .await
    {
        Ok(Ok(value)) => serde_json::from_value(value).unwrap_or_default(),
        _ => Census::default(),
    }
}

pub const REFUSED: &str = "  /shutdown only works in an /implement (router-acp planner) session.";
pub const BLOCKED: &str =
    "◼ shutdown in progress · new prompts are blocked until the workers drain (Ctrl-C quits)";
pub const SHUT_DOWN: &str =
    "◼ shutdown · this session has shut down; /exit, then /implement in a new session resumes";
pub const CLOSE_HINT: &str = "  The session can be closed now (/exit).";
pub const QUIT_CONFIRM: &str = "◼ shutdown · press Ctrl-C again to quit now; workers may be cut off, and the saved state resumes in a new /implement session";
pub const EXIT_WARNING: &str = "◼ shutdown · kory is exiting before the workers drained: they may be terminated; the saved resume state lets /implement in a new session pick up where this one left off";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Saving,
    Saved,
    Draining,
    Drained,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub work_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    pub message: String,
    pub saved: Vec<String>,
    pub workers_total: usize,
    pub workers_stopped: usize,
    pub workers: Vec<Worker>,
}

/// A shutdown progress notification for router session `session_id`, if this is one.
pub fn parse(notification: &ExtensionNotification, session_id: &str) -> Option<Progress> {
    if notification.method != PROGRESS {
        return None;
    }
    let params = &notification.params;
    if params
        .get("sessionId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != session_id)
    {
        return None;
    }
    let phase = match params.get("phase").and_then(Value::as_str)? {
        "saving" => Phase::Saving,
        "saved" => Phase::Saved,
        "draining" => Phase::Draining,
        "drained" => Phase::Drained,
        "failed" => Phase::Failed,
        _ => return None,
    };
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let count = |key: &str| params.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
    Some(Progress {
        phase,
        message: text("message"),
        saved: params
            .get("saved")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        workers_total: count("workersTotal"),
        workers_stopped: count("workersStopped"),
        workers: params
            .get("workers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|worker| {
                Some(Worker {
                    work_id: worker.get("workId")?.as_str()?.to_string(),
                    status: worker
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect(),
    })
}

/// Paths named in a status line before the rest collapse into `…`.
const SAVED_SHOWN: usize = 3;

/// The transcript's status line for one progress notification.
pub fn line(progress: &Progress) -> String {
    let body = match progress.phase {
        Phase::Saving => "saving resume state…".to_string(),
        Phase::Saved if progress.saved.is_empty() => "resume state saved".to_string(),
        Phase::Saved => {
            let mut shown: Vec<&str> = progress
                .saved
                .iter()
                .take(SAVED_SHOWN)
                .map(String::as_str)
                .collect();
            if progress.saved.len() > SAVED_SHOWN {
                shown.push("…");
            }
            format!("resume state saved ({})", shown.join(", "))
        }
        Phase::Draining => {
            let mut body = format!(
                "draining {}/{} workers",
                progress.workers_stopped, progress.workers_total
            );
            let workers: Vec<String> = progress
                .workers
                .iter()
                .map(|worker| match worker.status.as_str() {
                    "" => worker.work_id.clone(),
                    status => format!("{}: {status}", worker.work_id),
                })
                .collect();
            if !workers.is_empty() {
                body.push_str(&format!(" ({})", workers.join(", ")));
            } else if !progress.message.is_empty() {
                body.push_str(&format!(" · {}", progress.message));
            }
            body
        }
        Phase::Drained => "drained: session can exit; /implement in a new session resumes".into(),
        Phase::Failed if progress.message.is_empty() => "failed".to_string(),
        Phase::Failed => format!("failed: {}", progress.message),
    };
    super::output::sanitize_terminal_line(&format!("◼ shutdown · {body}"))
}

/// What the footer shows while the shutdown runs; `None` once it has ended.
pub fn footer(progress: &Progress) -> Option<String> {
    match progress.phase {
        Phase::Saving | Phase::Saved => Some("shutting down · saving".to_string()),
        Phase::Draining => Some(format!(
            "shutting down {}/{}",
            progress.workers_stopped, progress.workers_total
        )),
        Phase::Drained | Phase::Failed => None,
    }
}

/// Whether the session runs router-acp's planner: an operator `/implement` (or `/plan`) opted
/// it in.
pub fn is_planner_session(messages: &[Message]) -> bool {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .any(|message| {
            let text = message.as_concat_text();
            let command = text.split_whitespace().next().unwrap_or("");
            command.eq_ignore_ascii_case("/implement") || command.eq_ignore_ascii_case("/plan")
        })
}

pub const AUTO_MODE_NOTICE: &str = "Planner session switched to Auto mode for its lifetime; worker starts and planner tools are authorized by /implement.";

/// Keep the opt-in after compaction removes the original command from history.
pub fn planner_mode(planner: &mut bool, messages: &[Message], requested: GooseMode) -> GooseMode {
    *planner |= is_planner_session(messages);
    if *planner {
        GooseMode::Auto
    } else {
        requested
    }
}

/// The answer to `_router/planner_shutdown`: accepted, or why not.
pub fn accepted(answer: &Value) -> Result<(), String> {
    if answer.get("accepted").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(answer
        .get("error")
        .or_else(|| answer.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("router-acp did not accept the shutdown")
        .to_string())
}

/// How long router-acp has to accept or refuse; it answers at once and reports the rest as
/// progress.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Asks the provider's router to shut the planner run down. On acceptance, the receiver of
/// its progress notifications (subscribed before asking, so none is missed).
pub async fn request(
    provider: &dyn Provider,
) -> Result<Option<broadcast::Receiver<ExtensionNotification>>, String> {
    let notifications = provider.subscribe_extension_notifications();
    let answer = tokio::time::timeout(
        REQUEST_TIMEOUT,
        provider.extension_request(REQUEST, serde_json::json!({})),
    )
    .await
    .map_err(|_| "router-acp did not answer the shutdown request".to_string())?
    .map_err(|error| error.to_string())?;
    accepted(&answer)?;
    Ok(notifications)
}

/// A shutdown router-acp accepted for this session.
pub struct Run {
    pub session_id: String,
    pub notifications: Option<broadcast::Receiver<ExtensionNotification>>,
    pub drained: bool,
}

/// Whether a typed prompt may go out while a shutdown has been requested.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Open,
    /// The workers are still draining.
    Draining,
    /// The shutdown finished; this session takes no new work.
    ShutDown,
}

impl Gate {
    pub fn of(run: Option<&Run>) -> Self {
        match run {
            None => Gate::Open,
            Some(run) if run.drained => Gate::ShutDown,
            Some(_) => Gate::Draining,
        }
    }

    pub fn notice(&self) -> Option<&'static str> {
        match self {
            Gate::Open => None,
            Gate::Draining => Some(BLOCKED),
            Gate::ShutDown => Some(SHUT_DOWN),
        }
    }
}

/// Ctrl-C while the prompt waits on a shutdown: the first press warns, the second quits.
#[derive(Debug, Default)]
pub struct QuitGuard {
    pressed_at: Option<std::time::Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum QuitStep {
    Confirm,
    Quit,
}

impl QuitGuard {
    pub fn press(&mut self) -> QuitStep {
        self.press_at(std::time::Instant::now())
    }

    pub fn press_at(&mut self, now: std::time::Instant) -> QuitStep {
        if self.pressed_at.replace(now).is_some_and(|last| {
            now.saturating_duration_since(last) <= std::time::Duration::from_secs(5)
        }) {
            QuitStep::Quit
        } else {
            QuitStep::Confirm
        }
    }
}

/// The next extension notification, or never once there is no receiver (or it closed).
pub async fn next_notification(
    receiver: &mut Option<broadcast::Receiver<ExtensionNotification>>,
) -> ExtensionNotification {
    loop {
        match receiver.as_mut() {
            Some(rx) => match rx.recv().await {
                Ok(notification) => return notification,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => *receiver = None,
            },
            None => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn progress(params: Value) -> Progress {
        let mut params = params;
        params["sessionId"] = json!("r1");
        parse(
            &ExtensionNotification {
                method: PROGRESS.to_string(),
                params,
            },
            "r1",
        )
        .unwrap()
    }

    #[test]
    fn each_phase_prints_one_status_line() {
        let cases = [
            (
                json!({"phase": "saving"}),
                "◼ shutdown · saving resume state…",
            ),
            (
                json!({"phase": "saved", "saved": ["Docs/CONTINUE.md", "ticket-12", "a", "b"]}),
                "◼ shutdown · resume state saved (Docs/CONTINUE.md, ticket-12, a, …)",
            ),
            (
                json!({"phase": "draining", "workersTotal": 3, "workersStopped": 1,
                       "workers": [{"workId": "dev-speedup", "status": "stopped"}]}),
                "◼ shutdown · draining 1/3 workers (dev-speedup: stopped)",
            ),
            (
                json!({"phase": "drained", "workersTotal": 3, "workersStopped": 3}),
                "◼ shutdown · drained: session can exit; /implement in a new session resumes",
            ),
            (
                json!({"phase": "failed", "message": "could not write Docs/CONTINUE.md"}),
                "◼ shutdown · failed: could not write Docs/CONTINUE.md",
            ),
        ];
        for (params, expected) in cases {
            assert_eq!(line(&progress(params)), expected);
        }
    }

    #[test]
    fn the_footer_counts_workers_while_draining() {
        let draining =
            progress(json!({"phase": "draining", "workersTotal": 3, "workersStopped": 2}));
        assert_eq!(footer(&draining).as_deref(), Some("shutting down 2/3"));
        assert_eq!(
            footer(&progress(json!({"phase": "saving"}))).as_deref(),
            Some("shutting down · saving")
        );
        assert_eq!(footer(&progress(json!({"phase": "drained"}))), None);
        assert_eq!(footer(&progress(json!({"phase": "failed"}))), None);
    }

    #[test]
    fn parent_shutdown_wait_is_visible_before_workers_start_draining() {
        let waiting = progress(
            json!({"phase":"draining","workersTotal":0,"workersStopped":0,
            "message":"waiting for the parent's shutdown handoff"}),
        );
        assert!(line(&waiting).contains("waiting for the parent's shutdown handoff"));
    }

    #[test]
    fn only_this_sessions_progress_is_read() {
        let other = ExtensionNotification {
            method: PROGRESS.to_string(),
            params: json!({"sessionId": "r2", "phase": "saving"}),
        };
        assert_eq!(parse(&other, "r1"), None);
        let unrelated = ExtensionNotification {
            method: "_router/something_else".to_string(),
            params: json!({"sessionId": "r1", "phase": "saving"}),
        };
        assert_eq!(parse(&unrelated, "r1"), None);
        let unknown_phase = ExtensionNotification {
            method: PROGRESS.to_string(),
            params: json!({"sessionId": "r1", "phase": "exploding"}),
        };
        assert_eq!(parse(&unknown_phase, "r1"), None);
    }

    #[test]
    fn shutdown_is_offered_only_in_planner_sessions() {
        assert!(!is_planner_session(&[
            Message::user().with_text("fix the bug")
        ]));
        assert!(!is_planner_session(&[
            Message::assistant().with_text("/implement is how you start")
        ]));
        assert!(is_planner_session(&[
            Message::user().with_text("hello"),
            Message::user().with_text("/implement Docs/Plans/X.md"),
        ]));
        assert!(is_planner_session(&[Message::user().with_text("  /plan")]));
        assert!(!is_planner_session(&[
            Message::user().with_text("/implementation notes")
        ]));
    }

    #[test]
    fn planner_auto_mode_overrides_config_for_the_whole_session() {
        for configured in [
            GooseMode::Approve,
            GooseMode::SmartApprove,
            GooseMode::Chat,
            GooseMode::Auto,
        ] {
            let mut planner = false;
            assert_eq!(
                planner_mode(
                    &mut planner,
                    &[Message::user().with_text("hello")],
                    configured
                ),
                configured
            );
            assert_eq!(
                planner_mode(
                    &mut planner,
                    &[Message::user().with_text("/implement Docs/Plans/X.md")],
                    configured
                ),
                GooseMode::Auto
            );
            // A later operator turn or compaction cannot return to approval mode.
            assert_eq!(
                planner_mode(&mut planner, &[], GooseMode::Approve),
                GooseMode::Auto
            );
        }
        let mut planner = false;
        assert_eq!(
            planner_mode(
                &mut planner,
                &[Message::assistant().with_text("/implement")],
                GooseMode::Approve
            ),
            GooseMode::Approve
        );
    }

    #[test]
    fn the_routers_answer_is_accepted_or_its_error_shown() {
        assert_eq!(accepted(&json!({"accepted": true})), Ok(()));
        assert_eq!(
            accepted(&json!({"accepted": false, "error": "session is not in planner mode"})),
            Err("session is not in planner mode".to_string())
        );
        assert!(accepted(&json!({})).is_err());
    }

    #[test]
    fn prompts_are_blocked_while_draining_and_after() {
        assert_eq!(Gate::of(None), Gate::Open);
        let mut run = Run {
            session_id: "r1".into(),
            notifications: None,
            drained: false,
        };
        assert_eq!(Gate::of(Some(&run)), Gate::Draining);
        assert_eq!(Gate::of(Some(&run)).notice(), Some(BLOCKED));
        run.drained = true;
        assert_eq!(Gate::of(Some(&run)).notice(), Some(SHUT_DOWN));
        assert_eq!(Gate::Open.notice(), None);
    }

    #[test]
    fn ctrl_c_while_waiting_asks_once_then_quits() {
        let mut guard = QuitGuard::default();
        assert_eq!(guard.press(), QuitStep::Confirm);
        assert_eq!(guard.press(), QuitStep::Quit);
    }

    #[test]
    fn ctrl_c_during_a_planner_turn_arms_prompt_exit_for_five_seconds() {
        let now = std::time::Instant::now();
        let mut guard = QuitGuard::default();
        assert_eq!(guard.press_at(now), QuitStep::Confirm);
        assert_eq!(
            guard.press_at(now + std::time::Duration::from_secs(5)),
            QuitStep::Quit
        );
        let mut expired = QuitGuard::default();
        assert_eq!(expired.press_at(now), QuitStep::Confirm);
        assert_eq!(
            expired.press_at(now + std::time::Duration::from_secs(6)),
            QuitStep::Confirm
        );
    }

    #[tokio::test]
    async fn cancelled_planner_turn_reports_census_and_exit_warns_about_live_workers() {
        let router = router(Ok(json!({"status":"running","live":2,"admissible":7})));
        let census = census(&router).await;
        assert_eq!(census.interrupted_line(), "run still active: 2 live workers, 7 admissible; /shutdown drains, /exit quits and cuts workers off");
        assert_eq!(census.exit_warning(false), Some(EXIT_WARNING));
        assert_eq!(router.asked.lock().unwrap()[0], (STATUS.into(), json!({})));
        let idle = Census {
            live: Some(0),
            admissible: Some(0),
        };
        assert_eq!(idle.exit_warning(false), None);
        assert_eq!(idle.exit_warning(true), Some(EXIT_WARNING));
    }

    #[tokio::test]
    async fn an_old_router_does_not_report_unknown_workers_as_zero() {
        let router = router(Err("method unavailable".into()));
        let census = census(&router).await;
        assert!(census
            .interrupted_line()
            .contains("unknown live workers, unknown admissible"));
        assert_eq!(census.exit_warning(false), Some(EXIT_WARNING));
    }

    struct Router {
        answer: Result<Value, String>,
        asked: std::sync::Mutex<Vec<(String, Value)>>,
        progress: broadcast::Sender<ExtensionNotification>,
    }

    #[async_trait::async_trait]
    impl Provider for Router {
        fn get_name(&self) -> &str {
            "router-acp"
        }

        async fn stream(
            &self,
            _model_config: &goose_providers::model::ModelConfig,
            _system: &str,
            _messages: &[Message],
            _tools: &[rmcp::model::Tool],
        ) -> Result<goose::providers::base::MessageStream, goose_providers::errors::ProviderError>
        {
            unreachable!("no prompt is sent")
        }

        fn subscribe_extension_notifications(
            &self,
        ) -> Option<broadcast::Receiver<ExtensionNotification>> {
            Some(self.progress.subscribe())
        }

        async fn extension_request(
            &self,
            method: &str,
            params: Value,
        ) -> Result<Value, goose_providers::errors::ProviderError> {
            self.asked
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            self.answer
                .clone()
                .map_err(goose_providers::errors::ProviderError::RequestFailed)
        }
    }

    fn router(answer: Result<Value, String>) -> Router {
        Router {
            answer,
            asked: std::sync::Mutex::new(Vec::new()),
            progress: broadcast::channel(4).0,
        }
    }

    #[tokio::test]
    async fn shutdown_sends_the_planner_request_and_listens_first() {
        let router = router(Ok(json!({"accepted": true})));
        let mut receiver = request(&router).await.unwrap();
        assert_eq!(
            router.asked.lock().unwrap().clone(),
            [(REQUEST.to_string(), json!({}))]
        );
        // Progress sent right after the answer is not missed.
        router
            .progress
            .send(ExtensionNotification {
                method: PROGRESS.to_string(),
                params: json!({"sessionId": "r1", "phase": "saving"}),
            })
            .unwrap();
        let first = next_notification(&mut receiver).await;
        assert_eq!(parse(&first, "r1").unwrap().phase, Phase::Saving);
    }

    #[tokio::test]
    async fn a_refused_shutdown_reports_the_routers_error() {
        let refused = router(Err("session is not in planner mode".to_string()));
        let error = request(&refused).await.unwrap_err();
        assert!(error.contains("session is not in planner mode"), "{error}");
        let declined = router(Ok(
            json!({"accepted": false, "error": "already shutting down"}),
        ));
        assert_eq!(
            request(&declined).await.unwrap_err(),
            "already shutting down"
        );
    }

    #[tokio::test]
    async fn notifications_are_read_until_the_channel_closes() {
        let (tx, rx) = broadcast::channel(4);
        let mut receiver = Some(rx);
        let sent = ExtensionNotification {
            method: PROGRESS.to_string(),
            params: json!({"phase": "saving"}),
        };
        tx.send(sent.clone()).unwrap();
        assert_eq!(next_notification(&mut receiver).await, sent);
        drop(tx);
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            next_notification(&mut receiver),
        )
        .await;
        assert!(waited.is_err());
        assert!(receiver.is_none());
    }
}
