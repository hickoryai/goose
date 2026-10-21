use crate::session::builder::ExtensionFailure;
use anstream::{adapter::strip_str, eprintln};
use bat::WrappingMode;
use console::{measure_text_width, style, Color, StyledObject, Term};
use goose::config::Config;
use goose::conversation::message::{
    ActionRequiredData, Message, MessageContent, SystemNotificationContent, SystemNotificationType,
    ToolNameParts, ToolRequest, ToolResponse,
};
use goose::providers::canonical_cost::estimate_model_cost;
#[cfg(target_os = "windows")]
use goose::subprocess::SubprocessExt;
use goose::utils::safe_truncate;
use goose_providers::conversation::token_usage::Usage;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rmcp::model::{CallToolRequestParams, JsonObject, PromptArgument, Role};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Display;
use std::io::{Error, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use super::streaming_buffer::MarkdownBuffer;

pub const DEFAULT_MIN_PRIORITY: f32 = 0.0;
pub const DEFAULT_CLI_LIGHT_THEME: &str = "GitHub";
pub const DEFAULT_CLI_DARK_THEME: &str = "zenburn";
const OUTPUT_TOKEN_LIMIT_WARNING: &str =
    "Warning: Response reached the model's output-token limit and may be incomplete.";

/// Output collected by `capture_output`, and whether the output it continues ended a line.
struct Capture {
    text: String,
    line_start: bool,
}

thread_local! {
    static CAPTURED: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

/// Whether what this module last wrote to stdout ended its line, so a tool-call line can
/// start on a line of its own after streamed text that did not.
static LINE_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Runs `render` with everything this module prints collected into a string instead of
/// written to stdout, so output can be handed to something that owns the terminal (the
/// line editor's external printer) without disturbing it.
pub fn capture_output(render: impl FnOnce()) -> String {
    capture_output_after(true, render)
}

/// `capture_output` for output that continues text which `line_start` says ended its line
/// (or not).
pub fn capture_output_after(line_start: bool, render: impl FnOnce()) -> String {
    let outer = CAPTURED.with(|captured| {
        captured.replace(Some(Capture {
            text: String::new(),
            line_start,
        }))
    });
    render();
    CAPTURED
        .with(|captured| captured.replace(outer))
        .map(|capture| capture.text)
        .unwrap_or_default()
}

fn is_capturing() -> bool {
    CAPTURED.with(|captured| captured.borrow().is_some())
}

/// Whether `text` ends a line; `None` when it prints nothing visible to tell.
fn ends_line(text: &str) -> Option<bool> {
    console::strip_ansi_codes(text)
        .chars()
        .last()
        .map(|last| last == '\n')
}

/// Whether the next output starts at the beginning of a line.
fn at_line_start() -> bool {
    CAPTURED.with(|captured| match captured.borrow().as_ref() {
        Some(capture) => ends_line(&capture.text).unwrap_or(capture.line_start),
        None => LINE_START.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// Records that `text` went to stdout outside `emit` (bat's printer).
fn note_written(text: &str) {
    if let Some(ended) = ends_line(text) {
        LINE_START.store(ended, std::sync::atomic::Ordering::Relaxed);
    }
}

fn emit(args: std::fmt::Arguments<'_>) {
    let text = args.to_string();
    let printed = CAPTURED.with(|captured| match captured.borrow_mut().as_mut() {
        Some(capture) => {
            capture.text.push_str(&text);
            false
        }
        None => true,
    });
    if printed {
        anstream::print!("{text}");
        note_written(&text);
    }
}

macro_rules! out {
    ($($arg:tt)*) => { emit(format_args!($($arg)*)) };
}

macro_rules! outln {
    () => { emit(format_args!("\n")) };
    ($($arg:tt)*) => { emit(format_args!("{}\n", format_args!($($arg)*))) };
}

fn accent<T: Display>(value: T) -> StyledObject<T> {
    style(value).cyan()
}

fn success<T: Display>(value: T) -> StyledObject<T> {
    style(value).green()
}

fn warning<T: Display>(value: T) -> StyledObject<T> {
    style(value).yellow()
}

fn danger<T: Display>(value: T) -> StyledObject<T> {
    style(value).red()
}

// Re-export theme for use in main
#[derive(Clone, Copy)]
pub enum Theme {
    Light,
    Dark,
    Ansi,
}

impl Theme {
    fn as_str(&self) -> String {
        match self {
            Theme::Light => Config::global()
                .get_param::<String>("GOOSE_CLI_LIGHT_THEME")
                .unwrap_or(DEFAULT_CLI_LIGHT_THEME.to_string()),
            Theme::Dark => Config::global()
                .get_param::<String>("GOOSE_CLI_DARK_THEME")
                .unwrap_or(DEFAULT_CLI_DARK_THEME.to_string()),
            Theme::Ansi => "base16".to_string(),
        }
    }

    fn from_config_str(val: &str) -> Self {
        if val.eq_ignore_ascii_case("light") {
            Theme::Light
        } else if val.eq_ignore_ascii_case("ansi") {
            Theme::Ansi
        } else {
            Theme::Dark
        }
    }

    fn as_config_string(&self) -> String {
        match self {
            Theme::Light => "light".to_string(),
            Theme::Dark => "dark".to_string(),
            Theme::Ansi => "ansi".to_string(),
        }
    }
}

thread_local! {
    static CURRENT_THEME: RefCell<Theme> = RefCell::new(
        std::env::var("GOOSE_CLI_THEME").ok()
            .map(|val| Theme::from_config_str(&val))
            .unwrap_or_else(||
                Config::global().get_param::<String>("GOOSE_CLI_THEME").ok()
                    .map(|val| Theme::from_config_str(&val))
                    .unwrap_or(Theme::Ansi)
            )
    );
    static SHOW_FULL_TOOL_OUTPUT: RefCell<bool> = RefCell::new(
        Config::global().get_param::<bool>("GOOSE_SHOW_FULL_OUTPUT").unwrap_or(false)
    );
}

pub fn set_theme(theme: Theme) {
    let config = Config::global();
    config
        .set_param("GOOSE_CLI_THEME", theme.as_config_string())
        .expect("Failed to set theme");
    CURRENT_THEME.with(|t| *t.borrow_mut() = theme);

    let config = Config::global();
    let theme_str = match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
        Theme::Ansi => "ansi",
    };

    if let Err(e) = config.set_param("GOOSE_CLI_THEME", theme_str) {
        eprintln!("Failed to save theme setting to config: {}", e);
    }
}

/// Ring the terminal bell so an unfocused terminal can badge or chime.
/// Opt-in via `GOOSE_CLI_BELL=true` (environment or config); terminals
/// decide how to surface it, typically only when the window lacks focus.
pub fn emit_attention_bell() {
    if !bell_enabled() {
        return;
    }
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return;
    }
    let _ = stdout.write_all(b"\x07");
    let _ = stdout.flush();
}

fn bell_enabled() -> bool {
    Config::global()
        .get_param::<bool>("GOOSE_CLI_BELL")
        .unwrap_or(false)
}

pub fn get_theme() -> Theme {
    CURRENT_THEME.with(|t| *t.borrow())
}

pub fn toggle_full_tool_output() -> bool {
    SHOW_FULL_TOOL_OUTPUT.with(|s| {
        let mut val = s.borrow_mut();
        *val = !*val;
        *val
    })
}

pub fn get_show_full_tool_output() -> bool {
    SHOW_FULL_TOOL_OUTPUT.with(|s| *s.borrow())
}

// Simple wrapper around spinner to manage its state
#[derive(Default)]
pub struct ThinkingIndicator {
    spinner: Option<cliclack::ProgressBar>,
}

impl ThinkingIndicator {
    pub fn show(&mut self) {
        let spinner = cliclack::spinner();
        let hint = style("(Ctrl+C to interrupt)").dim();
        if Config::global()
            .get_param("RANDOM_THINKING_MESSAGES")
            .unwrap_or(true)
        {
            spinner.start(format!(
                "{}...  {}",
                super::thinking::get_random_thinking_message(),
                hint,
            ));
        } else {
            spinner.start(format!("Thinking...  {}", hint));
        }
        self.spinner = Some(spinner);
    }

    pub fn hide(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop("");
        }
    }

    pub fn is_shown(&self) -> bool {
        self.spinner.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct PromptInfo {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Option<Vec<PromptArgument>>,
    pub extension: Option<String>,
}

// Global thinking indicator
thread_local! {
    static THINKING: RefCell<ThinkingIndicator> = RefCell::new(ThinkingIndicator::default());
}

pub fn show_thinking() {
    if std::io::stdout().is_terminal() {
        THINKING.with(|t| t.borrow_mut().show());
    }
}

pub fn hide_thinking() {
    if std::io::stdout().is_terminal() {
        THINKING.with(|t| t.borrow_mut().hide());
    }
}

pub fn show_loading_extensions_background() {
    eprintln!(
        "  {}",
        style("⏳ loading extensions in background...").dim()
    );
}

pub fn show_waiting_for_extensions() {
    eprintln!(
        "  {}",
        style("⏳ waiting for extensions to finish loading...").dim()
    );
}

pub fn show_extensions_ready() {
    eprintln!("  {}", style("✓ extensions ready").green());
}

pub fn show_extension_failures(failures: &[ExtensionFailure]) {
    for failure in failures {
        eprintln!(
            "{}",
            style(format!(
                "  ⚠ Failed to start extension '{}' ({}), continuing without it",
                failure.label, failure.error
            ))
            .yellow()
        );
        eprintln!(
            "{}",
            style(format!(
                "    Hint: ask kory to help debug the '{}' extension",
                failure.label
            ))
            .dim()
        );
    }
}

pub fn run_status_hook(status: &str) {
    match status {
        "thinking" => LOCAL_BUSY.store(true, std::sync::atomic::Ordering::Relaxed),
        "waiting" => LOCAL_BUSY.store(false, std::sync::atomic::Ordering::Relaxed),
        _ => {}
    }
    if let Ok(hook) = Config::global().get_param::<String>("GOOSE_STATUS_HOOK") {
        let status = status.to_string();
        std::thread::spawn(move || {
            #[cfg(target_os = "windows")]
            let result = std::process::Command::new("cmd")
                .arg("/C")
                .arg(format!("{} {}", hook, status))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .set_no_window()
                .status();

            #[cfg(not(target_os = "windows"))]
            let result = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{} {}", hook, status))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            let _ = result;
        });
    }
}

pub fn is_showing_thinking() -> bool {
    THINKING.with(|t| t.borrow().is_shown())
}

pub fn set_thinking_message(s: &String) {
    if std::io::stdout().is_terminal() {
        THINKING.with(|t| {
            if let Some(spinner) = t.borrow_mut().spinner.as_mut() {
                spinner.set_message(s);
            }
        });
    }
}

pub fn render_message(message: &Message, debug: bool) {
    if !message.is_user_visible() {
        return;
    }
    let message = message.user_visible_content();
    let theme = get_theme();

    for content in &message.content {
        match content {
            MessageContent::ActionRequired(action) => match &action.data {
                ActionRequiredData::ToolConfirmation { tool_name, .. } => {
                    outln!("action_required(tool_confirmation): {}", tool_name)
                }
                ActionRequiredData::Elicitation { message, .. } => {
                    outln!("action_required(elicitation): {}", message)
                }
                ActionRequiredData::ElicitationResponse { id, .. } => {
                    outln!("action_required(elicitation_response): {}", id)
                }
                ActionRequiredData::ToolConfirmationResponse { id, .. } => {
                    outln!("action_required(tool_confirmation_response): {}", id)
                }
            },
            MessageContent::Text(text) => {
                let mut buffer = MarkdownBuffer::new();
                render_reply_text(&text.text, &mut buffer, theme);
                flush_markdown_buffer(&mut buffer, theme);
            }
            MessageContent::ToolRequest(req) => render_tool_request(req, theme, debug),
            MessageContent::ToolResponse(resp) => render_tool_response(resp, debug),
            MessageContent::Image(image) => {
                outln!("Image: [data: {}, type: {}]", image.data, image.mime_type);
            }
            MessageContent::Thinking(t) => render_thinking(&t.thinking, theme),
            MessageContent::RedactedThinking(_) => {
                outln!("\n{}", style("Thinking:").dim().italic());
                print_markdown("Thinking was redacted", theme);
            }
            MessageContent::SystemNotification(notification) => {
                match notification.notification_type {
                    SystemNotificationType::ThinkingMessage
                    | SystemNotificationType::ProgressMessage => {
                        show_thinking();
                        set_thinking_message(&notification.msg);
                    }
                    SystemNotificationType::InlineMessage => {
                        hide_thinking();
                        outln!("\n{} {}", style("·").dim(), &notification.msg);
                    }
                    SystemNotificationType::CreditsExhausted => {
                        render_credits_exhausted_notification(notification);
                    }
                }
            }
            MessageContent::Error(error) => {
                hide_thinking();
                outln!("\n{} {}", danger("error:").bold(), &error.message);
            }
            _ => {
                eprintln!("WARNING: Message content type could not be rendered");
            }
        }
    }

    if reached_output_token_limit(&message) {
        render_output_token_limit_warning();
    }
    let _ = std::io::stdout().flush();
}

/// Render a streaming message, using a buffer to accumulate text content
/// and only render when markdown constructs are complete.
pub fn render_message_streaming(
    message: &Message,
    buffer: &mut MarkdownBuffer,
    thinking_header_shown: &mut bool,
    debug: bool,
) {
    if !message.is_user_visible() {
        return;
    }
    // A message typed during the turn and handed to it, shown the way it was typed.
    if message.metadata.steer && message.role == Role::User {
        flush_markdown_buffer(buffer, get_theme());
        outln!("\n{} {}", accent(">"), message.as_concat_text());
        let _ = std::io::stdout().flush();
        return;
    }
    let message = message.user_visible_content();
    let theme = get_theme();

    for content in &message.content {
        if !matches!(content, MessageContent::Thinking(_)) {
            if *thinking_header_shown {
                outln!();
            }
            *thinking_header_shown = false;
        }

        match content {
            MessageContent::Text(text) => render_reply_text(&text.text, buffer, theme),
            MessageContent::ToolRequest(req) => {
                flush_markdown_buffer(buffer, theme);
                render_tool_request(req, theme, debug);
            }
            MessageContent::ToolResponse(resp) => {
                flush_markdown_buffer(buffer, theme);
                render_tool_response(resp, debug);
            }
            MessageContent::ActionRequired(action) => {
                flush_markdown_buffer(buffer, theme);
                match &action.data {
                    ActionRequiredData::ToolConfirmation { tool_name, .. } => {
                        outln!("action_required(tool_confirmation): {}", tool_name)
                    }
                    ActionRequiredData::Elicitation { message, .. } => {
                        outln!("action_required(elicitation): {}", message)
                    }
                    ActionRequiredData::ElicitationResponse { id, .. } => {
                        outln!("action_required(elicitation_response): {}", id)
                    }
                    ActionRequiredData::ToolConfirmationResponse { id, .. } => {
                        outln!("action_required(tool_confirmation_response): {}", id)
                    }
                }
            }
            MessageContent::Image(image) => {
                flush_markdown_buffer(buffer, theme);
                outln!("Image: [data: {}, type: {}]", image.data, image.mime_type);
            }
            MessageContent::Thinking(t) => {
                render_thinking_streaming(&t.thinking, buffer, thinking_header_shown, theme);
            }
            MessageContent::RedactedThinking(_) => {
                flush_markdown_buffer(buffer, theme);
                outln!("\n{}", style("Thinking:").dim().italic());
                print_markdown("Thinking was redacted", theme);
            }
            MessageContent::SystemNotification(notification) => {
                match notification.notification_type {
                    SystemNotificationType::ThinkingMessage
                    | SystemNotificationType::ProgressMessage => {
                        show_thinking();
                        set_thinking_message(&notification.msg);
                    }
                    SystemNotificationType::InlineMessage => {
                        flush_markdown_buffer(buffer, theme);
                        hide_thinking();
                        outln!("\n{} {}", style("·").dim(), &notification.msg);
                    }
                    SystemNotificationType::CreditsExhausted => {
                        flush_markdown_buffer(buffer, theme);
                        render_credits_exhausted_notification(notification);
                    }
                }
            }
            MessageContent::Error(error) => {
                flush_markdown_buffer(buffer, theme);
                hide_thinking();
                outln!("\n{} {}", danger("error:").bold(), &error.message);
            }
            _ => {
                flush_markdown_buffer(buffer, theme);
                eprintln!("WARNING: Message content type could not be rendered");
            }
        }
    }

    if reached_output_token_limit(&message) {
        flush_markdown_buffer(buffer, theme);
        render_output_token_limit_warning();
    }
    let _ = std::io::stdout().flush();
}

fn reached_output_token_limit(message: &Message) -> bool {
    message.role == Role::Assistant && message.metadata.output_token_limit_reached
}

fn render_output_token_limit_warning() {
    outln!("\n{}", warning(OUTPUT_TOKEN_LIMIT_WARNING));
}

fn render_credits_exhausted_notification(notification: &SystemNotificationContent) {
    hide_thinking();
    outln!("\n{} {}", warning("warning:").bold(), &notification.msg);

    if let Some(url) = notification
        .data
        .as_ref()
        .and_then(|d| d.get("top_up_url"))
        .and_then(|v| v.as_str())
    {
        outln!("{} {}", style("top up:").dim(), accent(url));
    }
}

pub fn get_credits_top_up_url(message: &Message) -> Option<String> {
    message.content.iter().find_map(|content| {
        let MessageContent::SystemNotification(notification) = content else {
            return None;
        };
        if notification.notification_type != SystemNotificationType::CreditsExhausted {
            return None;
        }
        notification
            .data
            .as_ref()
            .and_then(|d| d.get("top_up_url"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })
}

pub fn flush_markdown_buffer(buffer: &mut MarkdownBuffer, theme: Theme) {
    let remaining = buffer.flush();
    if !remaining.is_empty() {
        print_markdown(&remaining, theme);
    }
}

pub fn flush_markdown_buffer_current_theme(buffer: &mut MarkdownBuffer) {
    flush_markdown_buffer(buffer, get_theme());
}

pub fn render_text(text: &str, color: Option<Color>, dim: bool) {
    render_text_no_newlines(format!("\n{}\n\n", text).as_str(), color, dim);
}

pub fn render_text_no_newlines(text: &str, color: Option<Color>, dim: bool) {
    if !std::io::stdout().is_terminal() {
        outln!("{}", text);
        return;
    }
    let mut styled_text = style(text);
    if dim {
        styled_text = styled_text.dim();
    }
    if let Some(color) = color {
        styled_text = styled_text.fg(color);
    }
    out!("{}", styled_text);
}

pub fn goose_mode_message(text: &str) {
    outln!("\n{} {}", accent("mode:"), text);
}

/// Any value of the environment variable turns thinking output on, which is what the
/// documentation promises; the configured boolean is read only when it is not set.
fn thinking_enabled(config: &Config) -> bool {
    std::env::var_os("GOOSE_CLI_SHOW_THINKING").is_some()
        || config
            .get_param::<bool>("GOOSE_CLI_SHOW_THINKING")
            .unwrap_or(false)
}

fn should_show_thinking() -> bool {
    thinking_enabled(Config::global()) && std::io::stdout().is_terminal()
}

fn render_thinking(text: &str, theme: Theme) {
    if should_show_thinking() && !text.is_empty() {
        outln!("\n{}", style("Thinking:").dim().italic());
        print_markdown(text, theme);
    }
}

fn render_thinking_streaming(
    text: &str,
    buffer: &mut MarkdownBuffer,
    header_shown: &mut bool,
    theme: Theme,
) {
    if should_show_thinking() && !text.is_empty() {
        flush_markdown_buffer(buffer, theme);
        if !*header_shown {
            outln!("\n{}", style("Thinking:").dim().italic());
            *header_shown = true;
        }
        out!("{}", style(text).dim());
        let _ = std::io::stdout().flush();
    }
}

/// A tool call is one line of the transcript: `▸` and what the call does. Everything else
/// about it (arguments, output, errors) is in the tool-call detail view (↑ from the prompt),
/// and its output also prints under it with `/toggle` on.
fn render_tool_request(req: &ToolRequest, theme: Theme, debug: bool) {
    match &req.tool_call {
        Ok(call) => {
            let Some(summary) = tool_call_summary(call, debug) else {
                return;
            };
            print_tool_call_line(&summary);
            if debug {
                print_params(&call.arguments, 1, debug);
            }
        }
        Err(e) => print_markdown(&e.to_string(), theme),
    }
}

/// The one line a tool call shows in the transcript (without its `▸`), or `None` for a call
/// that shows nothing.
pub(super) fn tool_call_summary(call: &CallToolRequestParams, debug: bool) -> Option<String> {
    let name = call.name.as_ref();
    let arg = |key: &str| {
        call.arguments
            .as_ref()
            .and_then(|args| args.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let with_detail = |label: String, detail: Option<String>| match detail {
        Some(detail) => format!("{label} {detail}"),
        None => label,
    };
    let summary = match name {
        "load" => return None,
        name if is_shell_tool_name(name) => {
            with_detail(name.to_string(), arg("command").map(first_line))
        }
        name if is_file_tool_name(name) => with_detail(
            name.to_string(),
            arg("path").map(|path| shorten_path(path, debug)),
        ),
        "execute_javascript" | "execute_typescript" | "execute_code" => {
            let count = call
                .arguments
                .as_ref()
                .and_then(|args| args.get("tool_graph"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            match count {
                0 => tool_display_name(name),
                1 => "execute 1 tool call".to_string(),
                n => format!("execute {n} tool calls"),
            }
        }
        "delegate" | "subagent" => with_detail(
            name.to_string(),
            arg("source")
                .or_else(|| arg("instructions"))
                .map(first_line),
        ),
        _ => tool_display_name(name),
    };
    Some(sanitize_terminal_line(&summary))
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().to_string()
}

/// `tool extension` for an extension's tool, else the name (an ACP agent's call title).
fn tool_display_name(name: &str) -> String {
    let parts = ToolNameParts::from(name);
    match parts.extension_name {
        Some(extension_name) => format!(
            "{} {}",
            parts.tool_name,
            extension_display_name(extension_name)
        ),
        None => parts.tool_name.to_string(),
    }
}

/// `  ▸ summary`, cut to the terminal's width so it stays one row.
pub(super) fn tool_call_line(summary: &str, width: Option<usize>) -> String {
    let line = format!("  ▸ {summary}");
    match width {
        Some(width) if width > 1 => console::truncate_str(&line, width - 1, "…").into_owned(),
        _ => line,
    }
}

/// One dim line on a line of its own: text streamed without a line end is ended first, so
/// the call never joins the prose before it (and no blank line is added).
fn print_tool_call_line(summary: &str) {
    if !at_line_start() {
        outln!();
    }
    let width = std::io::stdout()
        .is_terminal()
        .then(|| Term::stdout().size().1 as usize);
    outln!("{}", style(tool_call_line(summary, width)).dim());
}

/// A session status line (e.g. `◼ shutdown · …`), dim and on a line of its own like a tool
/// call.
pub fn print_status_line(line: &str) {
    if !at_line_start() {
        outln!();
    }
    outln!("{}", style(sanitize_terminal_line(line)).dim());
    let _ = std::io::stdout().flush();
}

/// Reply text, with each router-acp notice quoted in it shown as a tool-call line.
fn render_reply_text(text: &str, buffer: &mut MarkdownBuffer, theme: Theme) {
    for segment in super::router_notices::split(text) {
        match segment {
            super::router_notices::Segment::Text(text) => {
                if let Some(safe_content) = buffer.push(&text) {
                    print_markdown(&safe_content, theme);
                }
            }
            super::router_notices::Segment::Notice(notice) => {
                flush_markdown_buffer(buffer, theme);
                print_tool_call_line(&sanitize_terminal_line(&super::router_notices::summary(
                    &notice,
                )));
            }
        }
    }
}

fn render_tool_response(resp: &ToolResponse, debug: bool) {
    // Output stays out of the transcript unless asked for; the detail view has all of it.
    if !debug && !get_show_full_tool_output() && std::io::stdout().is_terminal() {
        return;
    }
    let config = Config::global();

    match &resp.tool_result {
        Ok(result) => {
            for content in &result.content {
                let annotations = match content {
                    rmcp::model::ContentBlock::Text(t) => t.annotations.as_ref(),
                    rmcp::model::ContentBlock::Image(i) => i.annotations.as_ref(),
                    rmcp::model::ContentBlock::Audio(a) => a.annotations.as_ref(),
                    rmcp::model::ContentBlock::Resource(r) => r.annotations.as_ref(),
                    rmcp::model::ContentBlock::ResourceLink(r) => r.annotations.as_ref(),
                    _ => None,
                };
                if let Some(audience) = annotations.and_then(|a| a.audience.as_ref()) {
                    if !audience.contains(&rmcp::model::Role::User) {
                        continue;
                    }
                }

                let min_priority = config
                    .get_param::<f32>("GOOSE_CLI_MIN_PRIORITY")
                    .ok()
                    .unwrap_or(DEFAULT_MIN_PRIORITY);

                let priority = annotations.and_then(|a| a.priority);
                // Tools without a priority annotation default to 0.0 (the same
                // as DEFAULT_MIN_PRIORITY), so they are shown at the default
                // threshold and only hidden when the user raises it.
                if priority.unwrap_or(0.0) < min_priority {
                    continue;
                }

                if debug {
                    outln!("{:#?}", content);
                } else if let Some(text) = content.as_text() {
                    print_tool_output(&text.text);
                }
            }
        }
        Err(e) => {
            outln!("    {}", style(e.to_string()).red().dim());
        }
    }
}

pub(super) fn sanitize_terminal_line(line: &str) -> String {
    strip_str(line)
        .flat_map(str::chars)
        .filter(|character| *character == '\t' || !character.is_control())
        .collect()
}

fn sanitize_tool_confirmation_line(line: &str) -> String {
    let mut sanitized = String::with_capacity(line.len());
    for character in sanitize_terminal_line(line).chars() {
        if matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        ) {
            sanitized.push_str(&format!("\\u{:04x}", character as u32));
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

fn sanitize_tool_confirmation_text(text: &str) -> String {
    text.split('\n')
        .map(sanitize_tool_confirmation_line)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn format_tool_confirmation(tool_name: &str, arguments: &JsonObject) -> String {
    let mut request = JsonObject::new();
    request.insert(
        "tool_name".to_string(),
        Value::String(tool_name.to_string()),
    );
    request.insert("arguments".to_string(), Value::Object(arguments.clone()));

    serde_json::to_string_pretty(&Value::Object(request))
        .expect("tool confirmation request must serialize")
        .lines()
        .map(sanitize_tool_confirmation_line)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn render_tool_confirmation(
    tool_name: &str,
    arguments: &JsonObject,
    security_prompt: Option<&str>,
) {
    eprintln!();
    if let Some(security_prompt) = security_prompt {
        eprintln!("  {}", style("Provider-provided approval notice").bold());
        for line in sanitize_tool_confirmation_text(security_prompt).split('\n') {
            eprintln!("    {}", style(line).dim());
        }
        eprintln!();
    }
    eprintln!("  {}", style("Tool approval request").bold());
    for line in format_tool_confirmation(tool_name, arguments).lines() {
        eprintln!("    {}", style(line).dim());
    }
    eprintln!();
}

fn print_tool_output_line(line: &str) {
    outln!("    {}", style(sanitize_terminal_line(line)).dim());
}

fn print_tool_output(text: &str) {
    if text.is_empty() {
        return;
    }
    if !std::io::stdout().is_terminal() {
        out!("{}", text);
        return;
    }
    let max_lines = if get_show_full_tool_output() {
        usize::MAX
    } else {
        20
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        for line in &lines {
            print_tool_output_line(line);
        }
    } else {
        let head = max_lines / 2;
        let tail = max_lines - head;
        for line in &lines[..head] {
            print_tool_output_line(line);
        }
        outln!(
            "    {}",
            style(format!(
                "... ({} lines hidden, /toggle to show all)",
                lines.len() - head - tail
            ))
            .dim()
            .italic()
        );
        for line in &lines[lines.len() - tail..] {
            print_tool_output_line(line);
        }
    }
}

fn is_shell_tool_name(name: &str) -> bool {
    matches!(name, "shell")
}

fn is_file_tool_name(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

pub fn render_error(message: &str) {
    outln!("\n  {} {}\n", danger("error:").bold(), message);
}

pub fn render_prompts(prompts: &HashMap<String, Vec<String>>) {
    outln!();
    for (extension, prompts) in prompts {
        outln!(" {}", accent(extension));
        for prompt in prompts {
            outln!("  - {}", style(prompt).cyan());
        }
    }
    outln!();
}

pub fn render_prompt_info(info: &PromptInfo) {
    outln!();
    if let Some(ext) = &info.extension {
        outln!(" {}: {}", accent("Extension"), ext);
    }
    outln!(" Prompt: {}", style(&info.name).cyan().bold());
    if let Some(desc) = &info.description {
        outln!("\n {}", desc);
    }
    render_arguments(info);
    outln!();
}

fn render_arguments(info: &PromptInfo) {
    if let Some(args) = &info.arguments {
        outln!("\n Arguments:");
        for arg in args {
            let required = arg.required.unwrap_or(false);
            let req_str = if required {
                style("(required)").bold()
            } else {
                style("(optional)").dim()
            };

            outln!(
                "  {} {} {}",
                accent(&arg.name),
                req_str,
                arg.description.as_deref().unwrap_or("")
            );
        }
    }
}

pub fn render_extension_success(name: &str) {
    outln!();
    outln!("  {} extension `{}`", success("added"), accent(name),);
    outln!();
}

pub fn render_extension_error(name: &str, error: &str) {
    outln!();
    outln!("  {} to add extension {}", danger("failed"), danger(name));
    outln!();
    outln!("{}", style(error).dim());
    outln!();
}

pub fn render_builtin_success(names: &str) {
    outln!();
    outln!(
        "  {} builtin{}: {}",
        success("added"),
        if names.contains(',') { "s" } else { "" },
        accent(names)
    );
    outln!();
}

pub fn render_builtin_error(names: &str, error: &str) {
    outln!();
    outln!(
        "  {} to add builtin{}: {}",
        danger("failed"),
        if names.contains(',') { "s" } else { "" },
        danger(names)
    );
    outln!();
    outln!("{}", style(error).dim());
    outln!();
}

fn extension_display_name(name: &str) -> &str {
    match name {
        "code_execution" => "Code Mode",
        _ => name,
    }
}

pub fn format_subagent_tool_call_message(subagent_id: &str, tool_name: &str) -> String {
    let short_id = subagent_id.rsplit('_').next().unwrap_or(subagent_id);
    let parts = ToolNameParts::from(tool_name);

    match parts.extension_name {
        Some(extension_name) => format!(
            "[subagent:{}] {} | {}",
            short_id,
            parts.tool_name,
            extension_display_name(extension_name)
        ),
        None => format!("[subagent:{}] {}", short_id, parts.tool_name),
    }
}

pub fn render_subagent_tool_call(
    subagent_id: &str,
    tool_name: &str,
    arguments: Option<&JsonObject>,
    debug: bool,
) {
    let mut summary = format_subagent_tool_call_message(subagent_id, tool_name);
    if matches!(
        tool_name,
        "execute_javascript"
            | "code_execution__execute_javascript"
            | "execute_typescript"
            | "code_execution__execute_typescript"
    ) {
        let count = arguments
            .and_then(|args| args.get("tool_graph"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if count > 0 {
            let plural = if count == 1 { "" } else { "s" };
            summary = format!("{summary} · {count} tool call{plural}");
        }
    }
    print_tool_call_line(&sanitize_terminal_line(&summary));
    if debug {
        print_params(&arguments.cloned(), 1, debug);
    }
}

// Respect NO_COLOR, as https://crates.io/crates/console already does
pub fn env_no_color() -> bool {
    // if NO_COLOR is defined at all disable colors
    std::env::var_os("NO_COLOR").is_none()
}

fn print_markdown(content: &str, theme: Theme) {
    if std::io::stdout().is_terminal() {
        if let Some((before, table, after)) = extract_markdown_table(content) {
            if !before.is_empty() {
                print_markdown_raw(&before, theme);
            }
            print_table(&table, theme);
            if !after.is_empty() {
                print_markdown(after, theme);
            }
        } else {
            print_markdown_raw(content, theme);
        }
    } else {
        out!("{}", content);
    }
}

/// Renders markdown content using bat (no table processing)
///
/// The printer is cached per thread because `PrettyPrinter::new()`
/// deserializes bat's bundled syntax/theme assets; `print()` drains the
/// queued inputs but leaves the printer reusable.
fn print_markdown_raw(content: &str, theme: Theme) {
    use std::cell::RefCell;
    thread_local! {
        static PRINTER: RefCell<bat::PrettyPrinter<'static>> =
            RefCell::new(bat::PrettyPrinter::new());
    }
    PRINTER.with(|printer| {
        let mut printer = printer.borrow_mut();
        printer
            .input(bat::Input::from_reader(Box::new(std::io::Cursor::new(
                content.as_bytes().to_vec(),
            ))))
            .theme(theme.as_str())
            .colored_output(env_no_color())
            .language("Markdown")
            .wrapping_mode(WrappingMode::NoWrapping(true));
        if is_capturing() {
            let mut rendered = String::new();
            printer.print_with_writer(Some(&mut rendered)).unwrap();
            out!("{rendered}");
        } else {
            printer.print().unwrap();
            note_written(content);
        }
    });
}

fn extract_markdown_table(content: &str) -> Option<(String, Vec<&str>, &str)> {
    let lines: Vec<&str> = content.lines().collect();

    // Track newline positions for safe slicing later
    let newline_indices: Vec<usize> = content
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| if b == b'\n' { Some(i) } else { None })
        .collect();

    // Skip tables inside code blocks
    let mut in_code_block = false;
    let mut table_start = None;
    let mut table_end = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            continue;
        }

        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            if table_start.is_none() {
                table_start = Some(i);
            }
            table_end = Some(i);
        } else if table_start.is_some() {
            break;
        }
    }

    let start = table_start?;
    let end = table_end?;

    // Need at least header + separator (2 rows minimum)
    if end < start + 1 {
        return None;
    }

    // Require separator to be the second row with proper format
    let separator_line = lines.get(start + 1)?;
    let is_valid_separator = separator_line.trim().starts_with('|')
        && separator_line.trim().ends_with('|')
        && separator_line
            .trim()
            .trim_matches('|')
            .split('|')
            .all(|cell| {
                let t = cell.trim();
                !t.is_empty() && t.chars().all(|c| c == '-' || c == ':' || c == ' ')
            });

    if !is_valid_separator {
        return None;
    }

    let before = lines[..start].join("\n");
    let before = if before.is_empty() {
        before
    } else {
        before + "\n"
    };
    let table = lines[start..=end].to_vec();

    let after = if end + 1 >= lines.len() {
        ""
    } else if let Some(&newline_pos) = newline_indices.get(end) {
        content.get(newline_pos + 1..).unwrap_or("")
    } else {
        ""
    };

    Some((before, table, after))
}

fn print_table(table_lines: &[&str], theme: Theme) {
    use comfy_table::{presets, Cell, CellAlignment, ContentArrangement, Table};

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    table.load_style(presets::ASCII_MARKDOWN);

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut alignments: Vec<CellAlignment> = Vec::new();
    let mut separator_idx = None;

    for (i, line) in table_lines.iter().enumerate() {
        let cells: Vec<String> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|s| s.trim().to_string())
            .collect();

        let is_separator = cells.iter().all(|c| {
            let t = c.trim();
            t.chars().all(|ch| ch == '-' || ch == ':') && t.contains('-')
        });
        if is_separator {
            separator_idx = Some(i);
            alignments = cells
                .iter()
                .map(|c| {
                    let t = c.trim();
                    if t.starts_with(':') && t.ends_with(':') {
                        CellAlignment::Center
                    } else if t.ends_with(':') {
                        CellAlignment::Right
                    } else {
                        CellAlignment::Left
                    }
                })
                .collect();
        } else {
            rows.push(cells);
        }
    }

    if separator_idx.is_none() && !rows.is_empty() {
        alignments = vec![CellAlignment::Left; rows[0].len()];
    }

    if let Some(header) = rows.first() {
        let header_cells: Vec<Cell> = header
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let cell = Cell::new(text);
                if let Some(align) = alignments.get(i) {
                    cell.set_alignment(*align)
                } else {
                    cell
                }
            })
            .collect();
        table.set_header(header_cells);
    }

    for row in rows.iter().skip(1) {
        let cells: Vec<Cell> = row
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let cell = Cell::new(text);
                if let Some(align) = alignments.get(i) {
                    cell.set_alignment(*align)
                } else {
                    cell
                }
            })
            .collect();
        table.add_row(cells);
    }

    let table_str = table.to_string();
    print_markdown_raw(&table_str, theme);
}

const INDENT: &str = "    ";

fn print_value_with_prefix(prefix: &String, value: &Value, debug: bool) {
    let prefix_width = measure_text_width(prefix.as_str());
    out!("{}", prefix);
    print_value(value, debug, prefix_width)
}

fn print_value(value: &Value, debug: bool, reserve_width: usize) {
    let max_width = Term::stdout()
        .size_checked()
        .map(|(_h, w)| (w as usize).saturating_sub(reserve_width));
    let show_full = get_show_full_tool_output();
    let formatted = match value {
        Value::String(s) => match (max_width, debug || show_full) {
            (Some(w), false) if s.len() > w => style(safe_truncate(s, w)),
            _ => style(s.to_string()),
        }
        .green(),
        Value::Number(n) => style(n.to_string()).yellow(),
        Value::Bool(b) => style(b.to_string()).yellow(),
        Value::Null => style("null".to_string()).dim(),
        _ => unreachable!(),
    };
    outln!("{}", formatted);
}

fn print_params(value: &Option<JsonObject>, depth: usize, debug: bool) {
    let indent = INDENT.repeat(depth);

    if let Some(json_object) = value {
        for (key, val) in json_object.iter() {
            match val {
                Value::Object(obj) => {
                    outln!("{}{}:", indent, style(key).dim());
                    print_params(&Some(obj.clone()), depth + 1, debug);
                }
                Value::Array(arr) => {
                    // Check if all items are simple values (not objects or arrays)
                    let all_simple = arr.iter().all(|item| {
                        matches!(
                            item,
                            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                        )
                    });

                    if all_simple {
                        // Render inline for simple arrays, truncation will be handled by print_value if needed
                        let values: Vec<String> = arr
                            .iter()
                            .map(|item| match item {
                                Value::String(s) => s.clone(),
                                Value::Number(n) => n.to_string(),
                                Value::Bool(b) => b.to_string(),
                                Value::Null => "null".to_string(),
                                _ => unreachable!(),
                            })
                            .collect();
                        let joined_values = values.join(", ");
                        print_value_with_prefix(
                            &format!("{}{}: ", indent, style(key).dim()),
                            &Value::String(joined_values),
                            debug,
                        );
                    } else {
                        // Use the original multi-line format for complex arrays
                        outln!("{}{}:", indent, style(key).dim());
                        for item in arr.iter() {
                            if let Value::Object(obj) = item {
                                outln!("{}{}- ", indent, INDENT);
                                print_params(&Some(obj.clone()), depth + 2, debug);
                            } else {
                                outln!("{}{}- {}", indent, INDENT, item);
                            }
                        }
                    }
                }
                _ => {
                    print_value_with_prefix(
                        &format!("{}{}: ", indent, style(key).dim()),
                        val,
                        debug,
                    );
                }
            }
        }
    }
}

fn shorten_path(path: &str, debug: bool) -> String {
    // In debug mode, return the full path
    if debug {
        return path.to_string();
    }

    let path = Path::new(path);

    // First try to convert to ~ if it's in home directory
    let home = etcetera::home_dir().ok();
    let path_str = if let Some(home) = home {
        if let Ok(stripped) = path.strip_prefix(home) {
            format!("~/{}", stripped.display())
        } else {
            path.display().to_string()
        }
    } else {
        path.display().to_string()
    };

    // If path is already short enough, return as is
    if path_str.len() <= 60 {
        return path_str;
    }

    let parts: Vec<_> = path_str.split('/').collect();

    // If we have 3 or fewer parts, return as is
    if parts.len() <= 3 {
        return path_str;
    }

    // Keep the first component (empty string before root / or ~) and last two components intact
    let mut shortened = vec![parts[0].to_string()];

    // Shorten middle components to their first letter
    for component in &parts[1..parts.len() - 2] {
        if !component.is_empty() {
            shortened.push(component.chars().next().unwrap_or('?').to_string());
        }
    }

    // Add the last two components
    shortened.push(parts[parts.len() - 2].to_string());
    shortened.push(parts[parts.len() - 1].to_string());

    shortened.join("/")
}

pub fn display_session_info(
    resume: bool,
    provider: &str,
    model: &str,
    session_id: &Option<String>,
) {
    set_terminal_title();

    let status = if resume {
        "resuming"
    } else if session_id.is_none() {
        "ephemeral"
    } else {
        "new session"
    };

    let model_display = model.to_string();

    let cwd_display = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // kory's seedling with the session info beside its leaves
    let info = [
        format!(
            "{} {} {} {} {}",
            style("●").green(),
            style(status).dim(),
            style("·").dim(),
            style(provider).dim(),
            style(&model_display).cyan(),
        ),
        match session_id {
            Some(id) => format!(
                "{} {}",
                style(id).dim(),
                style(format!("· {cwd_display}")).dim()
            ),
            None => style(cwd_display).dim().to_string(),
        },
        format!("{}", style("🌱 kory is ready").white()),
    ];
    // A wrapped info line would split the seedling, so each fits beside it.
    let room = (console::Term::stdout().size().1 as usize)
        .saturating_sub(2 + SEEDLING[0].len() + 3)
        .max(10);
    outln!();
    for (row, art) in seedling_rows().iter().enumerate() {
        match row.checked_sub(1).and_then(|line| info.get(line)) {
            Some(text) => outln!("  {art}   {}", console::truncate_str(text, room, "…")),
            None => outln!("  {}", art.trim_end()),
        }
    }
}

/// kory's seedling, two pixels per text cell: `l` leaf, `d` midrib, `s` stem.
const SEEDLING: [&str; 14] = [
    "                    ",
    "              lllll ",
    "            lllldd  ",
    "  ddlll    lllddll  ",
    "  llddll   lldlll   ",
    "   lllddl lddlll    ",
    "    lllldllllll     ",
    "         s          ",
    "         ss         ",
    "         ss         ",
    "         ss         ",
    "         ss         ",
    "         ss         ",
    "         s          ",
];

/// The seedling as half-block text rows, in true color when the terminal takes color.
fn seedling_rows() -> Vec<String> {
    let color = console::colors_enabled();
    let rgb = |pixel: u8| match pixel {
        b'l' => Some((129, 199, 132)),
        b'd' => Some((56, 142, 60)),
        b's' => Some((67, 160, 71)),
        _ => None,
    };
    SEEDLING
        .chunks(2)
        .map(|pair| {
            let (top, bottom) = (pair[0].as_bytes(), pair[1].as_bytes());
            top.iter()
                .zip(bottom)
                .map(|(&t, &b)| seedling_cell(rgb(t), rgb(b), color))
                .collect()
        })
        .collect()
}

fn seedling_cell(top: Option<(u8, u8, u8)>, bottom: Option<(u8, u8, u8)>, color: bool) -> String {
    let fg = |(r, g, b): (u8, u8, u8)| format!("\x1b[38;2;{r};{g};{b}m");
    let (glyph, paint) = match (top, bottom) {
        (None, None) => return " ".into(),
        (Some(t), None) => ("▀", fg(t)),
        (None, Some(b)) => ("▄", fg(b)),
        (Some(t), Some(b)) if t == b => ("█", fg(t)),
        (Some(t), Some((r, g, b))) => ("▀", format!("{}\x1b[48;2;{r};{g};{b}m", fg(t))),
    };
    if color {
        format!("{paint}{glyph}\x1b[0m")
    } else if top.is_some() && bottom.is_some() {
        "█".into()
    } else {
        glyph.into()
    }
}

/// Goose's own prompt is in flight (between the "thinking" and "waiting" status hooks).
static LOCAL_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TITLE_TICKER: std::sync::Once = std::sync::Once::new();
const TITLE_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The terminal title for one tick: the session's reported title (router-acp
/// sends `<project> [<model>]`) or the directory name, behind a spinner while
/// the session works and a sprout while it is idle.
fn terminal_title(
    activity: &goose::acp::session_activity::SessionActivity,
    local_busy: bool,
    dir_name: &str,
    tick: usize,
) -> String {
    let title = activity.title.as_deref().unwrap_or(dir_name);
    // Sanitize: strip control characters (ESC, BEL, etc.) to prevent terminal escape injection
    let sanitized: String = title.chars().filter(|c| !c.is_control()).collect();
    let glyph = if activity.busy || local_busy {
        TITLE_SPINNER[tick % TITLE_SPINNER.len()]
    } else {
        "🌱"
    };
    format!("{glyph} {sanitized}")
}

fn set_terminal_title() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    TITLE_TICKER.call_once(|| {
        let dir_name = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        std::thread::spawn(move || {
            let mut shown = String::new();
            for tick in 0usize.. {
                let title = terminal_title(
                    &goose::acp::session_activity::current(),
                    LOCAL_BUSY.load(std::sync::atomic::Ordering::Relaxed),
                    &dir_name,
                    tick,
                );
                if title != shown {
                    // OSC 0 sets the terminal window/tab title; one write keeps it whole.
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(format!("\x1b]0;{title}\x07").as_bytes());
                    let _ = err.flush();
                    shown = title;
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        });
    });
}

pub fn display_banner(banners: &[String]) {
    for banner in banners {
        for line in banner.lines() {
            outln!("{}", line);
        }
    }
}

pub fn display_context_usage(total_tokens: usize, context_limit: usize) {
    use console::style;

    if context_limit == 0 {
        outln!(
            "  {}",
            style("context usage unavailable (context limit is 0)").dim()
        );
        return;
    }

    let percentage =
        (((total_tokens as f64 / context_limit as f64) * 100.0).round() as usize).min(100);

    let bar_width = 20;
    let filled = ((percentage as f64 / 100.0) * bar_width as f64).round() as usize;
    let empty = bar_width - filled.min(bar_width);

    let bar = format!("{}{}", "━".repeat(filled), "╌".repeat(empty));
    let colored_bar = if percentage < 50 {
        style(bar).green().dim()
    } else if percentage < 85 {
        style(bar).yellow()
    } else {
        style(bar).red()
    };

    fn format_tokens(n: usize) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.0}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    outln!(
        "  {} {} {}",
        colored_bar,
        style(format!("{}%", percentage)).dim(),
        style(format!(
            "{}/{}",
            format_tokens(total_tokens),
            format_tokens(context_limit)
        ))
        .dim(),
    );
}

fn estimate_cost_usd(provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    estimate_model_cost(provider, model, usage)
}

/// Display cost information, if price data is available.
pub fn display_cost_usage(provider: &str, model: &str, usage: &Usage) {
    if let Some(cost) = estimate_cost_usd(provider, model, usage) {
        use console::style;
        let input_tokens = usage.input_tokens.unwrap_or(0);
        let output_tokens = usage.output_tokens.unwrap_or(0);
        let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
        let cache_write = usage.cache_write_input_tokens.unwrap_or(0);

        let cache_breakdown = match (cache_read, cache_write) {
            (0, 0) => String::new(),
            (read, 0) => format!(" ({} cache read)", read),
            (0, write) => format!(" ({} cache write)", write),
            (read, write) => format!(" ({} cache read, {} cache write)", read, write),
        };

        eprintln!(
            "Cost: {} USD ({} tokens: in {}{}, out {})",
            style(format!("${:.4}", cost)).cyan(),
            input_tokens + output_tokens,
            input_tokens,
            cache_breakdown,
            output_tokens
        );
    }
}

pub struct McpSpinners {
    bars: HashMap<String, ProgressBar>,
    log_spinner: Option<ProgressBar>,
    shell_output_lines: VecDeque<String>,
    multi_bar: MultiProgress,
}

impl McpSpinners {
    pub fn new() -> Self {
        McpSpinners {
            bars: HashMap::new(),
            log_spinner: None,
            shell_output_lines: VecDeque::new(),
            multi_bar: MultiProgress::new(),
        }
    }

    pub fn log(&mut self, message: &str) {
        let spinner = self.log_spinner.get_or_insert_with(|| {
            let bar = self.multi_bar.add(
                ProgressBar::new_spinner()
                    .with_style(
                        ProgressStyle::with_template("{spinner:.green} {msg}")
                            .unwrap()
                            .tick_chars("⠋⠙⠚⠛⠓⠒⠊⠉"),
                    )
                    .with_message(message.to_string()),
            );
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        });

        spinner.set_message(message.to_string());
    }

    pub fn log_shell_output(&mut self, lines: Vec<String>, max_lines: usize) {
        let message = update_recent_lines(&mut self.shell_output_lines, lines, max_lines);
        if !message.is_empty() {
            self.log(&message);
        }
    }

    pub fn update(&mut self, token: &str, value: f64, total: Option<f64>, message: Option<&str>) {
        let bar = self.bars.entry(token.to_string()).or_insert_with(|| {
            if let Some(total) = total {
                self.multi_bar.add(
                    ProgressBar::new((total * 100_f64) as u64).with_style(
                        ProgressStyle::with_template("[{elapsed}] {bar:40} {pos:>3}/{len:3} {msg}")
                            .unwrap(),
                    ),
                )
            } else {
                self.multi_bar.add(ProgressBar::new_spinner())
            }
        });
        bar.set_position((value * 100_f64) as u64);
        if let Some(msg) = message {
            bar.set_message(msg.to_string());
        }
    }

    pub fn hide(&mut self) -> Result<(), Error> {
        self.bars.iter_mut().for_each(|(_, bar)| {
            bar.disable_steady_tick();
        });
        if let Some(spinner) = self.log_spinner.as_mut() {
            spinner.disable_steady_tick();
        }
        self.shell_output_lines.clear();
        self.multi_bar.clear()
    }
}

fn update_recent_lines(
    recent_lines: &mut VecDeque<String>,
    lines: impl IntoIterator<Item = String>,
    max_lines: usize,
) -> String {
    recent_lines.extend(lines);
    while recent_lines.len() > max_lines {
        recent_lines.pop_front();
    }
    recent_lines
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::env;
    use tempfile::NamedTempFile;

    #[test]
    fn captured_rendering_collects_what_would_print() {
        let mut buffer = MarkdownBuffer::new();
        let mut thinking_header_shown = false;
        let rendered = capture_output(|| {
            render_message_streaming(
                &Message::assistant().with_text("Admitted the worker.\n"),
                &mut buffer,
                &mut thinking_header_shown,
                false,
            );
            flush_markdown_buffer_current_theme(&mut buffer);
        });
        assert!(strip_str(&rendered)
            .to_string()
            .contains("Admitted the worker."));
        assert!(!is_capturing());
    }

    #[test]
    fn recent_lines_accumulate_across_updates() {
        let mut recent_lines = VecDeque::new();
        let mut rendered = String::new();

        for line in ["one", "two", "three", "four"] {
            rendered = update_recent_lines(&mut recent_lines, [line.to_string()], 3);
        }

        assert_eq!(rendered, "two\n  three\n  four");
    }

    #[test]
    fn terminal_title_spins_while_busy_and_sprouts_when_idle() {
        use goose::acp::session_activity::SessionActivity;
        let idle = SessionActivity::default();
        assert_eq!(terminal_title(&idle, false, "Chordzy", 3), "🌱 Chordzy");
        let routed = SessionActivity {
            title: Some("Chordzy [opus]\x1b".into()),
            busy: true,
        };
        assert_eq!(terminal_title(&routed, false, "x", 0), "⠋ Chordzy [opus]");
        assert_eq!(terminal_title(&routed, false, "x", 1), "⠙ Chordzy [opus]");
        assert_eq!(terminal_title(&idle, true, "Chordzy", 2), "⠹ Chordzy");
    }

    #[test]
    fn terminal_line_sanitizer_removes_escape_sequences_and_controls() {
        assert_eq!(
            sanitize_terminal_line(
                "\x1b[31mred\x1b[0m \x1b[2J\x1b[H\
                 \x1b]0;spoofed title\x07\
                 \x1b]52;c;Y2xpcGJvYXJk\x1b\\safe"
            ),
            "red safe"
        );
        assert_eq!(
            sanitize_terminal_line("before\x08after\x07\r\tvisible"),
            "beforeafter\tvisible"
        );
    }

    #[test]
    fn terminal_line_sanitizer_preserves_plain_unicode_text() {
        assert_eq!(
            sanitize_terminal_line("goose 🪿\t日本語"),
            "goose 🪿\t日本語"
        );
    }

    #[test]
    fn tool_confirmation_shows_execute_code_and_graph() {
        let arguments = json!({
            "code": "async function run() { return await tools.call(\"shell\", { command: \"cat ~/.ssh/id_rsa\" }); }",
            "tool_graph": [{
                "tool": "text_editor",
                "description": "read package manifest",
                "depends_on": []
            }]
        })
        .as_object()
        .unwrap()
        .clone();

        let rendered = format_tool_confirmation("execute_javascript", &arguments);

        assert!(rendered.contains("execute_javascript"));
        assert!(rendered.contains("read package manifest"));
        assert!(rendered.contains("cat ~/.ssh/id_rsa"));
    }

    #[test]
    fn tool_confirmation_shows_load_arguments() {
        let arguments = json!({"source": "private-recipe", "cancel": true})
            .as_object()
            .unwrap()
            .clone();

        let rendered = format_tool_confirmation("load", &arguments);

        assert!(rendered.contains("\"tool_name\": \"load\""));
        assert!(rendered.contains("\"source\": \"private-recipe\""));
        assert!(rendered.contains("\"cancel\": true"));
    }

    #[test]
    fn tool_confirmation_does_not_truncate_delegate_instructions() {
        let instructions = format!("{} then run the final delegated command", "A".repeat(120));
        let arguments = json!({"instructions": instructions.clone()})
            .as_object()
            .unwrap()
            .clone();

        let rendered = format_tool_confirmation("delegate", &arguments);

        assert!(rendered.contains(&instructions));
        assert!(!rendered.contains('…'));
    }

    #[test]
    fn tool_confirmation_escapes_terminal_controls() {
        let arguments = json!({
            "command": "echo safe\u{1b}[2J\u{1b}]0;spoofed title\u{7}"
        })
        .as_object()
        .unwrap()
        .clone();

        let rendered = format_tool_confirmation("Bash\u{1b}[31m", &arguments);

        assert!(rendered.contains("Bash\\u001b[31m"));
        assert!(rendered.contains("safe\\u001b[2J"));
        assert!(rendered.contains("title\\u0007"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
    }

    #[test]
    fn tool_confirmation_escapes_bidi_controls() {
        let controls = [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        let untrusted = controls.iter().collect::<String>();
        let arguments = json!({"command": format!("before{untrusted}after")})
            .as_object()
            .unwrap()
            .clone();

        let rendered = format_tool_confirmation(&format!("tool{untrusted}"), &arguments);

        for control in controls {
            assert!(!rendered.contains(control));
            assert!(rendered.contains(&format!("\\u{:04x}", control as u32)));
        }
    }

    #[test]
    fn tool_confirmation_preserves_plain_unicode_text() {
        let arguments = json!({"query": "שלום مرحبا 日本語 🪿"})
            .as_object()
            .unwrap()
            .clone();

        let rendered = format_tool_confirmation("検索", &arguments);

        assert!(rendered.contains("שלום مرحبا 日本語 🪿"));
        assert!(rendered.contains("検索"));
    }

    #[test]
    fn tool_confirmation_sanitizes_unterminated_control_strings() {
        let rendered = sanitize_tool_confirmation_text(
            "visible bidi\u{202e}\nvisible OSC\u{1b}]unterminated\nvisible DCS\u{1b}Punterminated",
        );

        assert!(rendered.contains("visible OSC"));
        assert!(rendered.contains("visible DCS"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("\\u202e"));
    }

    #[test]
    fn tool_confirmation_renders_authoritative_details_on_stderr() {
        const CHILD_ENV: &str = "GOOSE_TEST_TOOL_CONFIRMATION_STDERR_CHILD";
        const AUTHORITATIVE_TOKEN: &str = "authoritative-redirect-token";
        const PROVIDER_TOKEN: &str = "provider-prompt-token";

        if env::var_os(CHILD_ENV).is_some() {
            let forged_request = (0..80)
                .map(|line| format!("forged safe request line {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            let provider_prompt = format!(
                "{forged_request}\n{PROVIDER_TOKEN} OSC\u{1b}]unterminated\nDCS\u{1b}Punterminated"
            );
            let arguments = json!({"command": AUTHORITATIVE_TOKEN})
                .as_object()
                .unwrap()
                .clone();
            render_tool_confirmation("shell", &arguments, Some(&provider_prompt));
            return;
        }

        let output = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "session::output::tests::tool_confirmation_renders_authoritative_details_on_stderr",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(output.status.success());

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.contains(AUTHORITATIVE_TOKEN));
        assert!(!stdout.contains(PROVIDER_TOKEN));
        assert!(stderr.contains(AUTHORITATIVE_TOKEN));
        assert!(stderr.contains(PROVIDER_TOKEN));
        assert!(stderr.contains("forged safe request line 79"));
        let provider_notice = stderr.find("Provider-provided approval notice").unwrap();
        let provider_content = stderr.find(PROVIDER_TOKEN).unwrap();
        let authoritative_block = stderr.find("Tool approval request").unwrap();
        assert!(provider_notice < provider_content);
        assert!(provider_content < authoritative_block);
        let authoritative_tail = stderr.get(authoritative_block..).unwrap();
        assert!(authoritative_tail.contains(AUTHORITATIVE_TOKEN));
        assert!(!authoritative_tail.contains(PROVIDER_TOKEN));
        assert!(!stderr.contains("\u{1b}]"));
        assert!(!stderr.contains("\u{1b}P"));
    }

    fn tool_call(name: &str, args: Value) -> CallToolRequestParams {
        let call = CallToolRequestParams::new(name.to_string());
        match args {
            Value::Object(map) => call.with_arguments(map),
            _ => call,
        }
    }

    fn terminal_request(id: &str) -> Message {
        Message::assistant().with_tool_request(id, Ok(tool_call("Terminal", Value::Null)))
    }

    fn stream(messages: &[Message]) -> String {
        let printed = capture_output(|| {
            let mut buffer = MarkdownBuffer::new();
            let mut thinking = false;
            for message in messages {
                render_message_streaming(message, &mut buffer, &mut thinking, false);
            }
            flush_markdown_buffer(&mut buffer, Theme::Dark);
        });
        console::strip_ansi_codes(&printed).into_owned()
    }

    #[test]
    fn a_tool_call_after_unterminated_prose_starts_its_own_line() {
        let printed = stream(&[
            Message::assistant().with_text(
                "Now retrying the chord-spelling warm-up, which the memory guard deferred.",
            ),
            terminal_request("t1"),
            terminal_request("t2"),
        ]);
        assert_eq!(
            printed,
            "Now retrying the chord-spelling warm-up, which the memory guard deferred.\n  ▸ Terminal\n  ▸ Terminal\n"
        );
    }

    #[test]
    fn a_tool_call_after_a_finished_line_adds_no_blank_line() {
        let printed = stream(&[
            Message::assistant().with_text("Done.\n"),
            terminal_request("t1"),
        ]);
        assert_eq!(printed, "Done.\n  ▸ Terminal\n");
        // Output that continues text that did not end its line is ended first.
        let continued = capture_output_after(false, || print_tool_call_line("Edit"));
        assert_eq!(console::strip_ansi_codes(&continued), "\n  ▸ Edit\n");
    }

    #[test]
    fn router_notices_print_as_plain_tool_call_lines() {
        let notice = r#"router-acp · delegate_task → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · task Feature · planner → claude@07460b494a7f4c0b9474e2957345695f/opus[1m] · phase=implementation (pool: implementation_candidates + boost +3.00) · utility 3.63 = 0.84×quality 2.97→0.55 (Feature) + 0.16×quota (headroom 60%, cost rank 4) + pref 3.07 · tradeoff 3→1.6 (complexity-scaled) · "[router-acp planner role: implement-work] Policy identity...""#;
        let printed = stream(&[
            terminal_request("t0"),
            Message::assistant().with_text(format!(
                "> {notice}\n> **detail** that stays hidden\n\nOn it."
            )),
        ]);
        assert_eq!(
            printed,
            "  ▸ Terminal\n  ▸ router-acp · delegate_task Feature · planner → opus\nOn it."
        );
    }

    #[test]
    fn a_tool_call_is_one_line_without_its_arguments() {
        let printed = capture_output(|| {
            render_tool_request(
                &ToolRequest {
                    id: "t1".into(),
                    tool_call: Ok(tool_call(
                        "Terminal",
                        serde_json::json!({"command": "cargo test", "cwd": "/repo", "threadId": "x"}),
                    )),
                    metadata: None,
                    tool_meta: None,
                },
                Theme::Dark,
                false,
            )
        });
        assert_eq!(console::strip_ansi_codes(&printed), "  ▸ Terminal\n");
    }

    #[test]
    fn tool_call_lines_name_what_the_call_does() {
        let summary = |name: &str, args: Value| tool_call_summary(&tool_call(name, args), false);
        assert_eq!(
            summary("cat .agents/skills/implement-all/SKILL.md", Value::Null).as_deref(),
            Some("cat .agents/skills/implement-all/SKILL.md")
        );
        assert_eq!(
            summary("shell", serde_json::json!({"command": "ls -la\necho hi"})).as_deref(),
            Some("shell ls -la")
        );
        assert_eq!(
            summary("developer__shell", Value::Null).as_deref(),
            Some("shell developer")
        );
        assert_eq!(
            summary(
                "execute_code",
                serde_json::json!({"tool_graph": [{"tool": "a"}, {"tool": "b"}]})
            )
            .as_deref(),
            Some("execute 2 tool calls")
        );
        assert_eq!(summary("load", Value::Null), None);
        // Control characters in a title cannot break the line.
        assert_eq!(
            summary("Guardian\x1b[2J Review\n", Value::Null).as_deref(),
            Some("Guardian Review")
        );
    }

    #[test]
    fn tool_call_lines_fit_the_terminal_width() {
        let line = tool_call_line(
            "router-delegate__delegate_task mcp with a long tail",
            Some(20),
        );
        assert!(console::measure_text_width(&line) <= 19, "{line:?}");
        assert!(line.starts_with("  ▸ router"));
        assert_eq!(tool_call_line("Terminal", None), "  ▸ Terminal");
    }

    #[test]
    fn formats_subagent_tool_call_names() {
        assert_eq!(
            format_subagent_tool_call_message("subagent_42", "read"),
            "[subagent:42] read"
        );
        assert_eq!(
            format_subagent_tool_call_message("subagent_42", "developer__shell"),
            "[subagent:42] shell | developer"
        );
        assert_eq!(
            format_subagent_tool_call_message("subagent_42", "code_execution__execute_javascript"),
            "[subagent:42] execute_javascript | Code Mode"
        );
        assert_eq!(
            format_subagent_tool_call_message("subagent_42", "code_execution__execute_typescript"),
            "[subagent:42] execute_typescript | Code Mode"
        );
        assert_eq!(
            format_subagent_tool_call_message("subagent_42", "calendar__events__list"),
            "[subagent:42] events__list | calendar"
        );
    }

    #[test]
    fn test_short_paths_unchanged() {
        assert_eq!(shorten_path("/usr/bin", false), "/usr/bin");
        assert_eq!(shorten_path("/a/b/c", false), "/a/b/c");
        assert_eq!(shorten_path("file.txt", false), "file.txt");
    }

    #[test]
    fn test_debug_mode_returns_full_path() {
        assert_eq!(
            shorten_path("/very/long/path/that/would/normally/be/shortened", true),
            "/very/long/path/that/would/normally/be/shortened"
        );
    }

    #[test]
    fn test_home_directory_conversion() {
        // Save the current home dir
        let original_home = env::var("HOME").ok();

        // Set a test home directory
        env::set_var("HOME", "/Users/testuser");

        assert_eq!(
            shorten_path("/Users/testuser/documents/file.txt", false),
            "~/documents/file.txt"
        );

        // A path that starts similarly to home but isn't in home
        assert_eq!(
            shorten_path("/Users/testuser2/documents/file.txt", false),
            "/Users/testuser2/documents/file.txt"
        );

        // Restore the original home dir
        if let Some(home) = original_home {
            env::set_var("HOME", home);
        } else {
            env::remove_var("HOME");
        }
    }

    #[test]
    fn test_toggle_full_tool_output() {
        let initial = get_show_full_tool_output();

        let after_first_toggle = toggle_full_tool_output();
        assert_eq!(after_first_toggle, !initial);
        assert_eq!(get_show_full_tool_output(), after_first_toggle);

        let after_second_toggle = toggle_full_tool_output();
        assert_eq!(after_second_toggle, initial);
        assert_eq!(get_show_full_tool_output(), initial);
    }

    #[test]
    fn test_long_path_shortening() {
        assert_eq!(
            shorten_path(
                "/vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv/long/path/with/many/components/file.txt",
                false
            ),
            "/v/l/p/w/m/components/file.txt"
        );
    }

    #[test]
    fn test_get_credits_top_up_url_from_credits_notification() {
        let message = Message::assistant().with_system_notification_with_data(
            SystemNotificationType::CreditsExhausted,
            "Insufficient credits",
            json!({"top_up_url": "https://router.tetrate.ai/billing"}),
        );
        assert_eq!(
            get_credits_top_up_url(&message).as_deref(),
            Some("https://router.tetrate.ai/billing")
        );
    }

    #[test]
    fn test_get_credits_top_up_url_ignores_non_credits_notification() {
        let message = Message::assistant().with_system_notification_with_data(
            SystemNotificationType::InlineMessage,
            "hello",
            json!({"top_up_url": "https://router.tetrate.ai/billing"}),
        );
        assert_eq!(get_credits_top_up_url(&message), None);
    }

    fn config_with(contents: &str) -> (Config, NamedTempFile, NamedTempFile) {
        let config_file = NamedTempFile::new().unwrap();
        let secrets_file = NamedTempFile::new().unwrap();
        std::fs::write(config_file.path(), contents).unwrap();
        let config =
            Config::new_with_file_secrets(config_file.path(), secrets_file.path()).unwrap();
        (config, config_file, secrets_file)
    }

    #[test]
    fn any_environment_value_shows_thinking() {
        let _guard = env_lock::lock_env([("GOOSE_CLI_SHOW_THINKING", Some("1"))]);
        let (config, _config_file, _secrets_file) = config_with("");

        assert!(thinking_enabled(&config));
    }

    #[test]
    fn empty_environment_value_shows_thinking() {
        let _guard = env_lock::lock_env([("GOOSE_CLI_SHOW_THINKING", Some(""))]);
        let (config, _config_file, _secrets_file) = config_with("");

        assert!(thinking_enabled(&config));
    }

    #[test]
    fn configured_value_is_used_when_the_variable_is_unset() {
        let _guard = env_lock::lock_env([("GOOSE_CLI_SHOW_THINKING", None::<&str>)]);

        let (enabled, _c1, _s1) = config_with("GOOSE_CLI_SHOW_THINKING: true\n");
        assert!(thinking_enabled(&enabled));

        let (disabled, _c2, _s2) = config_with("GOOSE_CLI_SHOW_THINKING: false\n");
        assert!(!thinking_enabled(&disabled));

        let (unset, _c3, _s3) = config_with("");
        assert!(!thinking_enabled(&unset));
    }

    #[test]
    fn thinking_output_needs_a_terminal() {
        let _guard = env_lock::lock_env([("GOOSE_CLI_SHOW_THINKING", Some("1"))]);

        assert_eq!(should_show_thinking(), std::io::stdout().is_terminal());
    }
}
