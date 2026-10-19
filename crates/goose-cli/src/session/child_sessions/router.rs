//! Children of a router-acp session. Its delegates are router sessions in router-acp's own
//! state, not goose sessions, so they are read through router-acp's versioned read-only
//! `state-query` boundary — off the render path, on a timer.

use super::view::{ChildRow, ChildStatus, LineKind, TranscriptLine};
use chrono::{DateTime, Utc};
use goose::providers::router_acp::{RouterDelegate, RouterDelegateLog, RouterStateSource};
use goose::utils::safe_truncate;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// A delegate router-acp reports no lifecycle state for (not a planner worker) that has
/// logged nothing for this long without having answered is shown as stopped.
const LIVE_WINDOW: chrono::Duration = chrono::Duration::minutes(3);
const TASK_LINES: usize = 12;
const ACTIVITY_CHARS: usize = 120;

#[derive(Debug, Clone)]
pub enum Fetched {
    Pending,
    Ready(Arc<Vec<RouterDelegate>>),
    Failed(String),
}

/// Polls `state-query v1 delegates` in the background for the root router session and
/// every router session the navigator has descended into.
pub struct RouterChildren {
    root: String,
    targets: watch::Sender<Vec<String>>,
    latest: Arc<Mutex<Fetched>>,
    _poller: Option<AbortOnDropHandle<()>>,
}

impl RouterChildren {
    pub fn start(source: anyhow::Result<RouterStateSource>, root: String) -> Self {
        let (targets, mut watched) = watch::channel(vec![root.clone()]);
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                return Self {
                    root,
                    targets,
                    latest: Arc::new(Mutex::new(Fetched::Failed(error.to_string()))),
                    _poller: None,
                }
            }
        };
        let latest = Arc::new(Mutex::new(Fetched::Pending));
        let published = latest.clone();
        let poller = tokio::spawn(async move {
            loop {
                let ids = watched.borrow_and_update().clone();
                let fetched = match source.delegates(&ids, QUERY_TIMEOUT).await {
                    Ok(delegates) => Fetched::Ready(Arc::new(delegates)),
                    Err(error) => Fetched::Failed(error.to_string()),
                };
                *published.lock().unwrap() = fetched;
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    changed = watched.changed() => if changed.is_err() { break },
                }
            }
        });
        Self {
            root,
            targets,
            latest,
            _poller: Some(AbortOnDropHandle::new(poller)),
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// Router sessions the navigator currently shows, root first; the poller queries them all.
    pub fn follow(&self, path: &[&str]) {
        let mut ids = vec![self.root.clone()];
        ids.extend(path.iter().map(|id| id.to_string()));
        self.targets.send_if_modified(|current| {
            let changed = *current != ids;
            if changed {
                *current = ids;
            }
            changed
        });
    }

    pub fn latest(&self) -> Fetched {
        self.latest.lock().unwrap().clone()
    }

    pub fn any_running(&self, now: DateTime<Utc>) -> bool {
        match self.latest() {
            Fetched::Ready(delegates) => delegates
                .iter()
                .any(|delegate| delegate_status(delegate, now) == ChildStatus::Running),
            _ => false,
        }
    }
}

/// The rows the child dock shows under `root`: a planner work's latest session stands for it
/// and sessions a later one replaced (`ended`) are left out.
pub fn dock_rows(delegates: &[RouterDelegate], root: &str, now: DateTime<Utc>) -> Vec<ChildRow> {
    let mut rows: Vec<ChildRow> = Vec::new();
    for delegate in delegates.iter().filter(|d| d.parent_id() == Some(root)) {
        if delegate.state.as_deref() == Some("ended") {
            continue;
        }
        let label = delegate_label(delegate);
        rows.retain(|row| row.label != label);
        rows.push(ChildRow {
            id: delegate.id.clone(),
            label,
            status: delegate_status(delegate, now),
            model: delegate
                .model
                .clone()
                .unwrap_or_else(|| "unknown model".to_string()),
            started_at: delegate.created_at,
            activity: latest_activity(&delegate.log),
        });
    }
    rows
}

pub fn rows_for(delegates: &[RouterDelegate], parent: &str, now: DateTime<Utc>) -> Vec<ChildRow> {
    delegates
        .iter()
        .filter(|delegate| delegate.parent_id() == Some(parent))
        .map(|delegate| ChildRow {
            id: delegate.id.clone(),
            label: delegate_label(delegate),
            status: delegate_status(delegate, now),
            model: delegate
                .model
                .clone()
                .unwrap_or_else(|| "unknown model".to_string()),
            started_at: delegate.created_at,
            activity: latest_activity(&delegate.log),
        })
        .collect()
}

pub fn delegate_label(delegate: &RouterDelegate) -> String {
    if let Some(work_id) = &delegate.work_id {
        return work_id.clone();
    }
    let work_id = delegate.log.iter().find_map(|entry| {
        (entry.kind == "delegate_task")
            .then(|| entry.detail.as_ref()?.get("work_id")?.as_str())
            .flatten()
    });
    match work_id {
        Some(work_id) => work_id.to_string(),
        None if delegate.title.is_empty() => "delegate".to_string(),
        None => delegate.title.clone(),
    }
}

pub fn delegate_status(delegate: &RouterDelegate, now: DateTime<Utc>) -> ChildStatus {
    if let Some(state) = &delegate.state {
        return match state.as_str() {
            "running" | "allocating" | "admitted" => ChildStatus::Running,
            "interrupted" | "blocked" | "corrections" => ChildStatus::Stopped,
            _ => ChildStatus::Completed,
        };
    }
    let last = delegate
        .log
        .iter()
        .rev()
        .find(|entry| !matches!(entry.kind.as_str(), "session_update"));
    match last.map(|entry| entry.kind.as_str()) {
        Some("agent_response") => return ChildStatus::Completed,
        Some("error") => return ChildStatus::Failed,
        _ => {}
    }
    let last_seen = delegate
        .log
        .iter()
        .filter_map(|entry| entry.ts)
        .chain(delegate.updated_at)
        .max();
    match last_seen {
        Some(seen) if now - seen <= LIVE_WINDOW => ChildStatus::Running,
        _ if delegate.has_response => ChildStatus::Completed,
        _ => ChildStatus::Stopped,
    }
}

fn latest_activity(log: &[RouterDelegateLog]) -> String {
    let lines = transcript_lines(log);
    let latest = lines
        .iter()
        .rev()
        .find(|line| line.kind != LineKind::Heading && !line.text.trim().is_empty())
        .map(|line| {
            line.text
                .lines()
                .last()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    safe_truncate(&latest, ACTIVITY_CHARS)
}

/// The delegate's chronological log as a transcript: its task, its streamed text joined back
/// into paragraphs, its tool calls with their latest status, and its final response.
pub fn transcript_lines(log: &[RouterDelegateLog]) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = Vec::new();
    let mut speaker: Option<&str> = None;
    let mut open_tool: Option<usize> = None;
    let mut heading = |lines: &mut Vec<TranscriptLine>, name: &'static str| {
        if speaker != Some(name) {
            if !lines.is_empty() {
                lines.push(line(LineKind::Body, String::new()));
            }
            lines.push(line(LineKind::Heading, name.to_string()));
            speaker = Some(name);
        }
    };
    for entry in log {
        match entry.kind.as_str() {
            "session_update" | "llm_request" | "llm_response" => {}
            "delegate_task" | "delegate_followup" => {
                open_tool = None;
                heading(&mut lines, "task");
                let text = detail_text(entry, "task").unwrap_or(&entry.summary);
                let task: Vec<&str> = text.lines().collect();
                for row in task.iter().take(TASK_LINES) {
                    lines.push(line(LineKind::Body, row.to_string()));
                }
                if task.len() > TASK_LINES {
                    lines.push(line(
                        LineKind::Tool,
                        format!("… {} more lines", task.len() - TASK_LINES),
                    ));
                }
            }
            "agent_progress" => {
                open_tool = None;
                heading(&mut lines, "assistant");
                match lines.last_mut() {
                    Some(last) if last.kind == LineKind::Body => last.text.push_str(&entry.summary),
                    _ => lines.push(line(LineKind::Body, entry.summary.clone())),
                }
            }
            "agent_response" => {
                open_tool = None;
                heading(&mut lines, "response");
                let text = detail_text(entry, "text").unwrap_or(&entry.summary);
                lines.push(line(LineKind::Body, text.to_string()));
            }
            "tool_call" => {
                heading(&mut lines, "assistant");
                let (title, status) = split_status(&entry.summary);
                match open_tool {
                    Some(index) if is_call_id(title) || tool_pending(&lines[index]) => {
                        let (previous, _) = split_tool_line(&lines[index].text);
                        let title = if is_call_id(title) {
                            previous.to_string()
                        } else {
                            title.to_string()
                        };
                        lines[index].text = tool_line(&title, status);
                    }
                    _ => {
                        lines.push(line(LineKind::Tool, tool_line(title, status)));
                        open_tool = Some(lines.len() - 1);
                    }
                }
                if matches!(status, Some("completed" | "failed")) {
                    open_tool = None;
                }
            }
            _ if !entry.summary.trim().is_empty() => {
                lines.push(line(
                    LineKind::Tool,
                    format!("· {}: {}", entry.kind, entry.summary),
                ));
            }
            _ => {}
        }
    }
    lines
}

fn line(kind: LineKind, text: String) -> TranscriptLine {
    TranscriptLine { kind, text }
}

fn detail_text<'a>(entry: &'a RouterDelegateLog, field: &str) -> Option<&'a str> {
    entry.detail.as_ref()?.get(field)?.as_str()
}

/// router-acp logs a tool call as `<title or call id> [<status>]`.
fn split_status(summary: &str) -> (&str, Option<&str>) {
    match summary
        .trim_end()
        .strip_suffix(']')
        .and_then(|s| s.rsplit_once(" ["))
    {
        Some((title, status)) => (title.trim(), Some(status)),
        None => (summary.trim(), None),
    }
}

fn is_call_id(title: &str) -> bool {
    !title.contains(char::is_whitespace)
        && title.len() >= 12
        && title.contains(['_', '-'])
        && title.chars().any(|c| c.is_ascii_digit())
}

const TOOL_PREFIX: &str = "→ ";

fn tool_line(title: &str, status: Option<&str>) -> String {
    match status {
        Some(status) => format!("{TOOL_PREFIX}{title} · {status}"),
        None => format!("{TOOL_PREFIX}{title}"),
    }
}

fn split_tool_line(text: &str) -> (&str, Option<&str>) {
    let text = text.strip_prefix(TOOL_PREFIX).unwrap_or(text);
    match text.rsplit_once(" · ") {
        Some((title, status)) => (title, Some(status)),
        None => (text, None),
    }
}

fn tool_pending(line: &TranscriptLine) -> bool {
    split_tool_line(&line.text).1 == Some("pending")
}

#[cfg(test)]
mod tests {
    use super::*;
    use goose::providers::router_acp::parse_delegates;

    const ROOT: &str = "rtr-1577032920c1d25e7e70d2dee3929e87-d44e999c-0de3-4e9f-89be-48fb5eb668f4";

    fn captured() -> Vec<RouterDelegate> {
        parse_delegates(include_bytes!("testdata/router_delegates.json")).unwrap()
    }

    fn at(ts: &str) -> DateTime<Utc> {
        ts.parse().unwrap()
    }

    #[test]
    fn captured_delegates_become_rows_under_their_parent() {
        let delegates = captured();
        let rows = rows_for(&delegates, ROOT, at("2026-10-13T20:41:00Z"));

        let summary: Vec<(&str, ChildStatus)> = rows
            .iter()
            .map(|row| (row.label.as_str(), row.status))
            .collect();
        assert_eq!(
            summary,
            [
                ("admin-hub-counts-20261012", ChildStatus::Running),
                ("smokescreen-deterministic", ChildStatus::Running),
            ]
        );
        assert!(rows[1].activity.starts_with("→ P=/work/repo; for i in"));
        assert_eq!(rows[0].model, "opus[1m]");
        assert_eq!(rows[0].started_at, Some(at("2026-10-13T20:32:10Z")));
        let dock = dock_rows(&delegates, ROOT, at("2026-10-13T20:41:00Z"));
        assert_eq!(dock[0].model, rows[0].model);
        assert_eq!(dock[0].started_at, rows[0].started_at);
        assert!(rows_for(&delegates, &rows[0].id, at("2026-10-13T20:41:00Z")).is_empty());
    }

    #[test]
    fn quiet_delegates_without_a_response_are_stopped() {
        let delegates = captured();
        let rows = rows_for(&delegates, ROOT, at("2026-10-13T21:30:00Z"));
        assert!(rows.iter().all(|row| row.status == ChildStatus::Stopped));
    }

    #[test]
    fn captured_log_reads_as_a_transcript() {
        let delegates = captured();
        let lines: Vec<(LineKind, String)> = transcript_lines(&delegates[1].log)
            .into_iter()
            .map(|line| (line.kind, line.text))
            .collect();

        assert_eq!(lines[0], (LineKind::Heading, "task".to_string()));
        assert_eq!(
            lines[1],
            (
                LineKind::Body,
                "[router-acp planner role: implement-work]".to_string()
            )
        );
        let assistant = lines
            .iter()
            .position(|line| *line == (LineKind::Heading, "assistant".to_string()))
            .unwrap();
        assert_eq!(
            lines[assistant + 1..],
            [
                (
                    LineKind::Tool,
                    "→ toolu_01EbpFS2J6Rhzzzp2gLz1KXR · completed".to_string()
                ),
                (
                    LineKind::Body,
                    "Pushed `b4141188f`. Waiting for the Buildkite branch build.".to_string()
                ),
                (
                    LineKind::Tool,
                    format!(
                        "→ {} · running",
                        split_status(&delegates[1].log[18].summary).0
                    )
                ),
                (
                    LineKind::Tool,
                    format!(
                        "→ {} · in_progress",
                        split_status(&delegates[1].log[20].summary).0
                    )
                ),
            ]
        );
    }

    #[test]
    fn responses_complete_a_delegate_and_errors_fail_it() {
        let entry = |kind: &str, summary: &str| RouterDelegateLog {
            ts: Some(at("2026-10-13T20:00:00Z")),
            kind: kind.to_string(),
            role: "agent".to_string(),
            summary: summary.to_string(),
            detail: (kind == "agent_response")
                .then(|| serde_json::json!({"text": "All checks pass.\nReady for review."})),
        };
        let mut delegate = captured().remove(0);
        delegate.log = vec![
            entry("delegate_task", "Fix it"),
            entry("agent_response", ""),
        ];
        delegate.has_response = true;
        let now = at("2026-10-13T20:00:30Z");
        assert_eq!(delegate_status(&delegate, now), ChildStatus::Completed);
        assert_eq!(latest_activity(&delegate.log), "Ready for review.");

        delegate.log.push(entry("error", "delegated prompt failed"));
        assert_eq!(delegate_status(&delegate, now), ChildStatus::Failed);
    }

    #[test]
    fn nested_delegates_are_found_by_their_parent() {
        let mut delegates = captured();
        let parent = delegates[0].id.clone();
        delegates[1].id = format!("{parent}::delegate-nested");
        delegates[1].routing = None;
        let now = at("2026-10-13T20:41:00Z");
        assert_eq!(rows_for(&delegates, ROOT, now).len(), 1);
        let nested = rows_for(&delegates, &parent, now);
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].id, format!("{parent}::delegate-nested"));
    }

    fn worker(id: &str, work_id: &str, state: &str) -> RouterDelegate {
        parse_delegates(
            serde_json::json!({"version": 1, "command": "delegates", "data": {"children": [{
                "id": format!("{ROOT}::delegate-{id}"),
                "title": "[router-acp planner role: implement-work] Policy identity...",
                "workId": work_id,
                "state": state,
                "routing": {"parent": ROOT},
                "log": [],
            }]}})
            .to_string()
            .as_bytes(),
        )
        .unwrap()
        .remove(0)
    }

    #[test]
    fn router_lifecycle_state_and_work_id_win_over_log_recency() {
        let long_ago = at("2020-01-01T00:00:00Z");
        let running = worker("a", "lead-magnets", "running");
        assert_eq!(delegate_label(&running), "lead-magnets");
        assert_eq!(delegate_status(&running, long_ago), ChildStatus::Running);
        assert_eq!(
            delegate_status(&worker("b", "x", "review-pending"), long_ago),
            ChildStatus::Completed
        );
        assert_eq!(
            delegate_status(&worker("c", "x", "interrupted"), long_ago),
            ChildStatus::Stopped
        );
    }

    #[test]
    fn dock_rows_keep_each_works_latest_session() {
        let delegates = vec![
            worker("1", "rhythm-training", "ended"),
            worker("2", "rhythm-training", "running"),
            worker("3", "lead-magnets", "review-pending"),
        ];
        let rows = dock_rows(&delegates, ROOT, at("2020-01-01T00:00:00Z"));
        let shown: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.status)).collect();
        assert_eq!(
            shown,
            [
                ("rhythm-training", ChildStatus::Running),
                ("lead-magnets", ChildStatus::Completed)
            ]
        );
        assert!(dock_rows(&delegates, "another-session", at("2020-01-01T00:00:00Z")).is_empty());
    }
}
