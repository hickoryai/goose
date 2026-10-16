//! The session's tool calls. The transcript shows each as one `▸` line; ↑ from an empty
//! prompt (or, during a turn, from the input line when nothing is queued) moves into them.
//! They are listed full screen, oldest at the top and the newest selected just above where
//! the input was: ↑/↓ move, ↓ past the newest goes back to the input, Enter shows everything
//! about one call (title, arguments, command, output, status, error), and Esc, Enter or q
//! closes that again.

use super::child_sessions::{Key, Screen};
use super::output::{sanitize_terminal_line, tool_call_line, tool_call_summary};
use super::router_notices::{self, Segment};
use console::style;
use goose::conversation::message::{Message, MessageContent};
use rmcp::model::JsonObject;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the session has tool calls to move into, for the line editor's ↑ handler.
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Rows PageUp/PageDown move the list by.
const PAGE: usize = 10;

pub fn set_available(available: bool) {
    AVAILABLE.store(available, Ordering::Relaxed);
}

/// From the line editor's ↑ handler: true when there are tool calls to move into.
pub fn request_focus() -> bool {
    let available = AVAILABLE.load(Ordering::Relaxed);
    REQUESTED.store(available, Ordering::Relaxed);
    available
}

/// The prompt's ↑ asked for the tool calls; taken once the line editor has returned.
pub fn take_focus_request() -> bool {
    REQUESTED.swap(false, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    Running,
    Done,
    Failed,
}

impl CallStatus {
    fn label(self) -> &'static str {
        match self {
            CallStatus::Running => "running",
            CallStatus::Done => "done",
            CallStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// What the transcript line shows.
    pub summary: String,
    /// ACP's category for the call (`execute`, `read`, …), when the agent sent one.
    pub kind: Option<String>,
    pub arguments: Option<JsonObject>,
    pub output: Vec<String>,
    pub error: Option<String>,
    pub status: CallStatus,
    /// A router-acp notice from the reply text rather than a tool call; `output` holds its
    /// full text.
    pub notice: bool,
}

pub fn has_tool_calls(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.content.iter().any(|content| match content {
            MessageContent::ToolRequest(_) => true,
            MessageContent::Text(text) => router_notices::split(&text.text)
                .iter()
                .any(|segment| matches!(segment, Segment::Notice(_))),
            _ => false,
        })
    })
}

/// Every tool call in `messages`, in order, with its response when one arrived.
pub fn collect(messages: &[Message]) -> Vec<ToolCall> {
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut notices = 0;
    for content in messages.iter().flat_map(|message| &message.content) {
        match content {
            MessageContent::ToolRequest(request) => {
                let kind = request
                    .tool_meta
                    .as_ref()
                    .and_then(|meta| meta.get("goose.acp.kind"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                calls.push(match &request.tool_call {
                    Ok(call) => ToolCall {
                        id: request.id.clone(),
                        name: call.name.to_string(),
                        summary: tool_call_summary(call, false)
                            .unwrap_or_else(|| call.name.to_string()),
                        kind,
                        arguments: call.arguments.clone(),
                        output: Vec::new(),
                        error: None,
                        status: CallStatus::Running,
                        notice: false,
                    },
                    Err(error) => ToolCall {
                        id: request.id.clone(),
                        name: "invalid tool call".to_string(),
                        summary: "invalid tool call".to_string(),
                        kind,
                        arguments: None,
                        output: Vec::new(),
                        error: Some(error.message.to_string()),
                        status: CallStatus::Failed,
                        notice: false,
                    },
                });
            }
            MessageContent::ToolResponse(response) => {
                let Some(call) = calls.iter_mut().rev().find(|call| call.id == response.id) else {
                    continue;
                };
                match &response.tool_result {
                    Ok(result) => {
                        call.output = result
                            .content
                            .iter()
                            .map(|block| match block.as_text() {
                                Some(text) => text.text.clone(),
                                None => serde_json::to_string(block).unwrap_or_default(),
                            })
                            .collect();
                        call.status = if result.is_error == Some(true) {
                            CallStatus::Failed
                        } else {
                            CallStatus::Done
                        };
                    }
                    Err(error) => {
                        call.error = Some(error.message.to_string());
                        call.status = CallStatus::Failed;
                    }
                }
            }
            MessageContent::Text(text) => {
                for segment in router_notices::split(&text.text) {
                    if let Segment::Notice(notice) = segment {
                        notices += 1;
                        calls.push(ToolCall {
                            id: format!("router-notice-{notices}"),
                            name: "router-acp".to_string(),
                            summary: router_notices::summary(&notice),
                            kind: Some("router notice".to_string()),
                            arguments: None,
                            output: vec![notice],
                            error: None,
                            status: CallStatus::Done,
                            notice: true,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    calls
}

/// What a key does in the list.
#[derive(Debug, PartialEq, Eq)]
enum ListStep {
    Select(usize),
    Open,
    /// Back to the input: ↓ past the newest call, Esc or q.
    Leave,
    Stay,
}

fn list_step(selected: usize, len: usize, key: Key) -> ListStep {
    let last = len.saturating_sub(1);
    match key {
        Key::Up | Key::WheelUp => ListStep::Select(selected.saturating_sub(1)),
        Key::Down if selected >= last => ListStep::Leave,
        Key::Down | Key::WheelDown => ListStep::Select((selected + 1).min(last)),
        Key::PageUp => ListStep::Select(selected.saturating_sub(PAGE)),
        Key::PageDown => ListStep::Select((selected + PAGE).min(last)),
        Key::Home => ListStep::Select(0),
        Key::End => ListStep::Select(last),
        Key::Enter | Key::Right => ListStep::Open,
        Key::Escape | Key::Char('q') => ListStep::Leave,
        _ => ListStep::Stay,
    }
}

/// What a key does in a call's detail view: scroll to a new offset, or close it.
#[derive(Debug, PartialEq, Eq)]
enum DetailStep {
    Scroll(usize),
    Close,
    Stay,
}

fn detail_step(scroll: usize, max_scroll: usize, page: usize, key: Key) -> DetailStep {
    match key {
        Key::Escape | Key::Enter | Key::Char('q') | Key::Left => DetailStep::Close,
        Key::Up | Key::WheelUp => DetailStep::Scroll(scroll.saturating_sub(1)),
        Key::Down | Key::WheelDown => DetailStep::Scroll((scroll + 1).min(max_scroll)),
        Key::PageUp => DetailStep::Scroll(scroll.saturating_sub(page.max(1))),
        Key::PageDown | Key::Char(' ') => {
            DetailStep::Scroll((scroll + page.max(1)).min(max_scroll))
        }
        Key::Home => DetailStep::Scroll(0),
        Key::End => DetailStep::Scroll(max_scroll),
        _ => DetailStep::Stay,
    }
}

/// Everything about one call, as plain lines before wrapping.
pub fn detail_lines(call: &ToolCall) -> Vec<String> {
    let mut lines = vec![format!("▸ {}", call.summary), String::new()];
    if call.notice {
        lines.push("router-acp notice".to_string());
        lines.push(String::new());
        lines.extend(
            call.output
                .iter()
                .flat_map(|text| text.lines())
                .map(str::to_string),
        );
        return lines
            .into_iter()
            .map(|line| sanitize_terminal_line(&line))
            .collect();
    }
    lines.push(format!("tool     {}", call.name));
    if let Some(kind) = &call.kind {
        lines.push(format!("kind     {kind}"));
    }
    lines.push(format!("status   {}", call.status.label()));
    lines.push(format!("id       {}", call.id));
    let arg = |key: &str| {
        call.arguments
            .as_ref()
            .and_then(|args| args.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    if let Some(command) = arg("command").or_else(|| arg("cmd")) {
        lines.push(String::new());
        lines.push("command".to_string());
        lines.extend(command.lines().map(|line| format!("  {line}")));
    }
    if let Some(cwd) = arg("cwd") {
        lines.push(format!("cwd      {cwd}"));
    }
    if let Some(arguments) = call.arguments.as_ref().filter(|args| !args.is_empty()) {
        lines.push(String::new());
        lines.push("arguments".to_string());
        let pretty =
            serde_json::to_string_pretty(&Value::Object(arguments.clone())).unwrap_or_default();
        lines.extend(pretty.lines().map(|line| format!("  {line}")));
    }
    lines.push(String::new());
    match call.status {
        CallStatus::Running if call.output.is_empty() => lines.push("output   (running)".into()),
        _ if call.output.iter().all(|text| text.trim().is_empty()) => {
            lines.push("output   (none)".into())
        }
        _ => {
            lines.push("output".to_string());
            for text in &call.output {
                lines.extend(text.lines().map(|line| format!("  {line}")));
            }
        }
    }
    if let Some(error) = &call.error {
        lines.push(String::new());
        lines.push("error".to_string());
        lines.extend(error.lines().map(|line| format!("  {line}")));
    }
    lines
        .into_iter()
        .map(|line| sanitize_terminal_line(&line))
        .collect()
}

/// `lines` wrapped to `cols` columns.
fn wrap(lines: &[String], cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut wrapped = Vec::new();
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut width = 0;
        for c in chars {
            let w = console::measure_text_width(c.encode_utf8(&mut [0; 4]));
            if width + w > cols && width > 0 {
                wrapped.push(std::mem::take(&mut row));
                width = 0;
            }
            row.push(c);
            width += w;
        }
        wrapped.push(row);
    }
    wrapped
}

fn rule(text: &str, cols: usize) -> String {
    let width = console::measure_text_width(text);
    let line = format!("{text}{}", "─".repeat(cols.saturating_sub(width)));
    style(console::truncate_str(&line, cols, "").into_owned())
        .dim()
        .to_string()
}

/// The first call shown so that `selected` is in view, moving the window as little as possible.
fn follow(offset: usize, selected: usize, slots: usize, len: usize) -> usize {
    let offset = if selected < offset {
        selected
    } else if slots > 0 && selected >= offset + slots {
        selected + 1 - slots
    } else {
        offset
    };
    offset.min(len.saturating_sub(slots))
}

/// The list, bottom-anchored like the transcript it stands for: header, then the calls with
/// the newest on the last row.
fn list_frame(
    calls: &[ToolCall],
    selected: usize,
    offset: usize,
    size: (usize, usize),
) -> Vec<String> {
    let (cols, rows) = size;
    let slots = rows.saturating_sub(1);
    let mut frame = vec![rule(
        &format!(
            "── tool calls · {} · ↑↓ select · Enter details · ↓ past the last or Esc back ",
            calls.len()
        ),
        cols,
    )];
    let shown = calls.len().saturating_sub(offset).min(slots);
    frame.extend(std::iter::repeat_n(String::new(), slots - shown));
    for (index, call) in calls.iter().enumerate().skip(offset).take(shown) {
        let mark = match call.status {
            CallStatus::Running => " …",
            CallStatus::Failed => " ✗",
            CallStatus::Done => "",
        };
        let line = tool_call_line(&format!("{}{mark}", call.summary), Some(cols));
        frame.push(if index == selected {
            let line = format!("›{}", line.get(1..).unwrap_or_default());
            style(line).reverse().to_string()
        } else if call.status == CallStatus::Failed {
            style(line).red().dim().to_string()
        } else {
            style(line).dim().to_string()
        });
    }
    frame
}

/// A call's details: header, the wrapped lines from `scroll`, and the keys at the bottom.
fn detail_frame(call: &ToolCall, scroll: usize, size: (usize, usize)) -> (Vec<String>, usize) {
    let (cols, rows) = size;
    let body = wrap(&detail_lines(call), cols);
    let page = rows.saturating_sub(2);
    let max_scroll = body.len().saturating_sub(page);
    let scroll = scroll.min(max_scroll);
    let mut frame = vec![rule("── tool call ", cols)];
    frame.extend(body.iter().skip(scroll).take(page).cloned());
    frame.resize(rows.saturating_sub(1).max(1), String::new());
    frame.push(rule(
        &format!(
            "── Esc, Enter or q back · ↑↓ PgUp PgDn scroll · {}–{} of {} ",
            (scroll + 1).min(body.len()),
            (scroll + page).min(body.len()),
            body.len()
        ),
        cols,
    ));
    (frame, max_scroll)
}

/// The list (and, over it, one call's details) on the alternate screen while it lives.
pub struct ToolCallsView {
    screen: Screen,
    calls: Vec<ToolCall>,
    selected: usize,
    offset: usize,
    detail: Option<usize>,
}

impl ToolCallsView {
    /// Opens on the newest call; `None` when there are no calls.
    pub fn open(calls: Vec<ToolCall>) -> Option<Self> {
        if calls.is_empty() {
            return None;
        }
        let selected = calls.len() - 1;
        let mut view = Self {
            screen: Screen::enter(),
            calls,
            selected,
            offset: usize::MAX,
            detail: None,
        };
        view.draw();
        Some(view)
    }

    /// New calls (or responses) arrived while the view is open.
    pub fn refresh(&mut self, calls: Vec<ToolCall>) {
        if calls.is_empty() {
            return;
        }
        let id = self.calls.get(self.selected).map(|call| call.id.clone());
        self.selected = id
            .and_then(|id| calls.iter().position(|call| call.id == id))
            .unwrap_or(self.selected)
            .min(calls.len() - 1);
        self.calls = calls;
        self.draw();
    }

    /// Handles one key; false once the view has closed back to the input.
    pub fn key(&mut self, key: Key) -> bool {
        let (_, rows) = self.screen.size();
        if let Some(scroll) = self.detail {
            let (_, max_scroll) =
                detail_frame(&self.calls[self.selected], scroll, self.screen.size());
            match detail_step(scroll, max_scroll, rows.saturating_sub(2), key) {
                DetailStep::Scroll(scroll) => self.detail = Some(scroll),
                DetailStep::Close => self.detail = None,
                DetailStep::Stay => return true,
            }
        } else {
            match list_step(self.selected, self.calls.len(), key) {
                ListStep::Select(selected) => self.selected = selected,
                ListStep::Open => self.detail = Some(0),
                ListStep::Leave => return false,
                ListStep::Stay => return true,
            }
        }
        self.draw();
        true
    }

    fn draw(&mut self) {
        let size = self.screen.size();
        let lines = match self.detail {
            Some(scroll) => detail_frame(&self.calls[self.selected], scroll, size).0,
            None => {
                let slots = size.1.saturating_sub(1);
                self.offset = follow(
                    self.offset.min(self.calls.len().saturating_sub(slots)),
                    self.selected,
                    slots,
                    self.calls.len(),
                );
                list_frame(&self.calls, self.selected, self.offset, size)
            }
        };
        self.screen.draw(&lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goose::conversation::message::Message;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock as Content, ErrorCode, ErrorData,
    };

    fn call(name: &str, args: Value) -> CallToolRequestParams {
        let params = CallToolRequestParams::new(name.to_string());
        match args {
            Value::Object(map) => params.with_arguments(map),
            _ => params,
        }
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::assistant().with_tool_request(
                "a",
                Ok(call(
                    "Terminal",
                    serde_json::json!({"command": "cargo test", "cwd": "/repo"}),
                )),
            ),
            Message::user().with_tool_response(
                "a",
                Ok(CallToolResult::success(vec![Content::text("ok\n2 passed")])),
            ),
            Message::assistant().with_tool_request(
                "b",
                Ok(call("mcp.router-delegate.planner_workflow", Value::Null)),
            ),
            Message::user().with_tool_response(
                "b",
                Err(ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    "router timed out",
                    None,
                )),
            ),
            Message::assistant().with_tool_request("c", Ok(call("Guardian Review", Value::Null))),
        ]
    }

    const DELEGATE: &str = r#"router-acp · delegate_task → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · task Feature · planner → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · phase=implementation (pool: implementation_candidates + boost +3.00) · utility 3.63 = 0.84×quality 2.97→0.55 (Feature) + 0.16×quota (headroom 60%, cost rank 4) + pref 3.07 · tradeoff 3→1.6 (complexity-scaled) · "[router-acp planner role: implement-work] Policy identity...""#;

    #[test]
    fn router_notices_are_listed_with_their_full_text() {
        let mut messages = conversation();
        messages.insert(
            2,
            Message::assistant().with_text(format!(
                "Handing this off.\n\n> {DELEGATE}\n> candidates: claude@07460b494a7f4c0b9474e2957345695f/opus[1m]\n\n"
            )),
        );
        let calls = collect(&messages);
        let summaries: Vec<&str> = calls.iter().map(|call| call.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "Terminal",
                "router-acp · delegate_task Feature · planner → opus",
                "mcp.router-delegate.planner_workflow",
                "Guardian Review",
            ]
        );
        assert!(calls[1].notice);
        let detail = detail_lines(&calls[1]).join("\n");
        for expected in [
            "utility 3.63 = 0.84×quality",
            "claude@07460b494a7f4c0b9474e2957345695f/opus[1m]",
            "\"[router-acp planner role: implement-work] Policy identity...\"",
            "candidates: claude@07460b494a7f4c0b9474e2957345695f/opus[1m]",
        ] {
            assert!(
                detail.contains(expected),
                "missing {expected:?} in\n{detail}"
            );
        }
        assert!(has_tool_calls(&[
            Message::assistant().with_text(format!("> {DELEGATE}\n"))
        ]));
    }

    #[test]
    fn collects_calls_with_their_responses_in_order() {
        let calls = collect(&conversation());
        let seen: Vec<(&str, CallStatus)> = calls
            .iter()
            .map(|call| (call.summary.as_str(), call.status))
            .collect();
        assert_eq!(
            seen,
            [
                ("Terminal", CallStatus::Done),
                ("mcp.router-delegate.planner_workflow", CallStatus::Failed),
                ("Guardian Review", CallStatus::Running),
            ]
        );
        assert_eq!(calls[0].output, ["ok\n2 passed"]);
        assert_eq!(calls[1].error.as_deref(), Some("router timed out"));
        assert!(has_tool_calls(&conversation()));
        assert!(!has_tool_calls(&[Message::user().with_text("hi")]));
    }

    #[test]
    fn detail_shows_command_arguments_output_and_errors() {
        let calls = collect(&conversation());
        let text = detail_lines(&calls[0]).join("\n");
        for expected in [
            "▸ Terminal",
            "status   done",
            "command\n  cargo test",
            "cwd      /repo",
            "\"cwd\": \"/repo\"",
            "output\n  ok\n  2 passed",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        let failed = detail_lines(&calls[1]).join("\n");
        assert!(failed.contains("status   failed"));
        assert!(failed.contains("error\n  router timed out"));
        assert!(detail_lines(&calls[2])
            .join("\n")
            .contains("output   (running)"));
    }

    #[test]
    fn list_moves_between_calls_and_leaves_past_the_newest() {
        assert_eq!(list_step(2, 3, Key::Up), ListStep::Select(1));
        assert_eq!(list_step(0, 3, Key::Up), ListStep::Select(0));
        assert_eq!(list_step(1, 3, Key::Down), ListStep::Select(2));
        assert_eq!(list_step(2, 3, Key::Down), ListStep::Leave);
        assert_eq!(list_step(2, 3, Key::WheelDown), ListStep::Select(2));
        assert_eq!(list_step(1, 3, Key::Enter), ListStep::Open);
        assert_eq!(list_step(1, 3, Key::Escape), ListStep::Leave);
        assert_eq!(list_step(1, 3, Key::Char('q')), ListStep::Leave);
        assert_eq!(list_step(1, 3, Key::Char('x')), ListStep::Stay);
    }

    #[test]
    fn detail_scrolls_and_closes_on_esc_enter_or_q() {
        assert_eq!(detail_step(0, 5, 3, Key::Down), DetailStep::Scroll(1));
        assert_eq!(detail_step(5, 5, 3, Key::Down), DetailStep::Scroll(5));
        assert_eq!(detail_step(4, 5, 3, Key::PageUp), DetailStep::Scroll(1));
        for key in [Key::Escape, Key::Enter, Key::Char('q')] {
            assert_eq!(detail_step(2, 5, 3, key), DetailStep::Close);
        }
    }

    #[test]
    fn list_is_bottom_anchored_with_the_selection_marked() {
        let calls = collect(&conversation());
        let frame: Vec<String> = list_frame(&calls, 2, 0, (60, 6))
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect();
        assert_eq!(frame.len(), 6);
        assert!(frame[0].starts_with("── tool calls · 3"));
        assert_eq!(&frame[1..3], ["", ""]);
        assert_eq!(frame[3], "  ▸ Terminal");
        assert_eq!(frame[4], "  ▸ mcp.router-delegate.planner_workflow ✗");
        assert_eq!(frame[5], "› ▸ Guardian Review …");
    }

    #[test]
    fn window_follows_the_selection() {
        assert_eq!(follow(7, 9, 3, 10), 7);
        assert_eq!(follow(7, 5, 3, 10), 5);
        assert_eq!(follow(0, 9, 3, 10), 7);
        assert_eq!(follow(9, 9, 3, 10), 7);
    }

    #[test]
    fn long_lines_wrap_to_the_screen() {
        assert_eq!(
            wrap(&["abcdef".to_string(), String::new()], 4),
            ["abcd", "ef", ""]
        );
    }
}
