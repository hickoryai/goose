use super::keys::Key;
use chrono::{DateTime, Utc};
use console::{measure_text_width, style, truncate_str};
use goose::providers::router_acp::DELEGATE_ID_SEPARATOR;
use unicode_segmentation::UnicodeSegmentation;

const SHORT_ID_CHARS: usize = 8;
const WHEEL_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    Running,
    Stopped,
    Completed,
    Failed,
    Cancelled,
}

impl ChildStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            ChildStatus::Running => "running",
            ChildStatus::Stopped => "stopped",
            ChildStatus::Completed => "done",
            ChildStatus::Failed => "failed",
            ChildStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChildRow {
    pub id: String,
    pub label: String,
    pub status: ChildStatus,
    pub model: String,
    pub started_at: Option<DateTime<Utc>>,
    pub activity: String,
}

impl ChildRow {
    fn running_label(&self, now: DateTime<Utc>) -> String {
        let model = sanitize(&self.model);
        let Some(started_at) = self.started_at else {
            return model;
        };
        let seconds = (now - started_at).num_seconds().max(0);
        let elapsed = if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{}m", seconds / 60)
        } else {
            format!("{}h{}m", seconds / 3600, seconds / 60 % 60)
        };
        format!("{model} [{elapsed}]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Heading,
    Body,
    Tool,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

/// What the navigator shows for the session at the top of its path. The parent session has
/// no transcript here because its own output is the main CLI view.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub children: Vec<ChildRow>,
    pub transcript: Option<Vec<TranscriptLine>>,
    /// Why some children may be missing, e.g. router-acp could not be queried.
    pub notice: Option<String>,
}

#[derive(Debug, Default)]
pub struct Frame {
    pub lines: Vec<String>,
    row_children: Vec<(usize, usize)>,
    transcript_rows: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
}

struct Crumb {
    id: String,
    label: String,
    selected: usize,
}

#[derive(Default)]
pub struct Navigator {
    path: Vec<Crumb>,
    selected: usize,
    list_offset: usize,
    scroll_back: usize,
}

impl Navigator {
    pub fn current(&self) -> Option<&str> {
        self.path.last().map(|crumb| crumb.id.as_str())
    }

    pub fn path_ids(&self) -> Vec<&str> {
        self.path.iter().map(|crumb| crumb.id.as_str()).collect()
    }

    pub fn handle(&mut self, key: Key, snapshot: &Snapshot, frame: &Frame) -> Outcome {
        let page = frame.transcript_rows.max(1);
        match key {
            Key::Open | Key::Char('q') => return Outcome::Close,
            Key::Escape | Key::Backspace | Key::Left if self.path.is_empty() => {
                return Outcome::Close
            }
            Key::Escape | Key::Backspace | Key::Left => self.ascend(),
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.selected = (self.selected + 1).min(snapshot.children.len().saturating_sub(1))
            }
            Key::Enter | Key::Right => self.descend(snapshot, self.selected),
            Key::PageUp => self.scroll_back += page,
            Key::PageDown => self.scroll_back = self.scroll_back.saturating_sub(page),
            Key::WheelUp if snapshot.transcript.is_some() => self.scroll_back += WHEEL_LINES,
            Key::WheelDown if snapshot.transcript.is_some() => {
                self.scroll_back = self.scroll_back.saturating_sub(WHEEL_LINES)
            }
            Key::WheelUp => return self.handle(Key::Up, snapshot, frame),
            Key::WheelDown => return self.handle(Key::Down, snapshot, frame),
            Key::Click { row, .. } => {
                if let Some(&(_, index)) = frame
                    .row_children
                    .iter()
                    .find(|(screen_row, _)| *screen_row == row as usize)
                {
                    self.descend(snapshot, index);
                }
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Starts inside `child`, as if it had been chosen from the top-level list.
    pub fn enter(&mut self, child: &ChildRow) {
        self.path.push(Crumb {
            id: child.id.clone(),
            label: child.label.clone(),
            selected: 0,
        });
    }

    fn descend(&mut self, snapshot: &Snapshot, index: usize) {
        let Some(child) = snapshot.children.get(index) else {
            return;
        };
        self.path.push(Crumb {
            id: child.id.clone(),
            label: child.label.clone(),
            selected: index,
        });
        self.selected = 0;
        self.list_offset = 0;
        self.scroll_back = 0;
    }

    fn ascend(&mut self) {
        if let Some(crumb) = self.path.pop() {
            self.selected = crumb.selected;
            self.list_offset = 0;
            self.scroll_back = 0;
        }
    }

    pub fn render(&mut self, snapshot: &Snapshot, width: usize, height: usize) -> Frame {
        let width = width.max(1);
        let body_rows = height.saturating_sub(3);
        let mut frame = Frame::default();

        let mut breadcrumb = String::from("Child sessions");
        for crumb in &self.path {
            breadcrumb.push_str(" › ");
            breadcrumb.push_str(&crumb.label);
        }
        frame
            .lines
            .push(fit(&breadcrumb, width, |s| style(s).bold()));
        frame.lines.push(style("─".repeat(width)).dim().to_string());

        self.selected = self.selected.min(snapshot.children.len().saturating_sub(1));
        let list_rows = match &snapshot.transcript {
            None => body_rows,
            Some(_) if snapshot.children.is_empty() => 0,
            Some(_) => snapshot.children.len().min(body_rows / 3).max(1),
        };
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if list_rows > 0 && self.selected >= self.list_offset + list_rows {
            self.list_offset = self.selected + 1 - list_rows;
        }

        if let Some(notice) = &snapshot.notice {
            frame.lines.push(fit(notice, width, |s| style(s).yellow()));
        }
        if snapshot.children.is_empty() && snapshot.transcript.is_none() {
            frame
                .lines
                .push(fit("No child sessions yet.", width, |s| style(s).dim()));
        }
        for (index, child) in snapshot
            .children
            .iter()
            .enumerate()
            .skip(self.list_offset)
            .take(list_rows)
        {
            frame.row_children.push((frame.lines.len(), index));
            frame
                .lines
                .push(child_row(child, index == self.selected, width));
        }

        if let Some(transcript) = &snapshot.transcript {
            if list_rows > 0 {
                frame.lines.push(style("─".repeat(width)).dim().to_string());
            }
            let rows = height.saturating_sub(frame.lines.len() + 1);
            frame.transcript_rows = rows;
            let wrapped: Vec<(LineKind, String)> = transcript
                .iter()
                .flat_map(|line| {
                    wrap(&line.text, width)
                        .into_iter()
                        .map(move |text| (line.kind, text))
                })
                .collect();
            self.scroll_back = self.scroll_back.min(wrapped.len().saturating_sub(rows));
            let end = wrapped.len() - self.scroll_back;
            let start = end.saturating_sub(rows);
            if wrapped.is_empty() {
                frame
                    .lines
                    .push(fit("No messages yet.", width, |s| style(s).dim()));
            }
            for (kind, text) in &wrapped[start..end] {
                frame.lines.push(match kind {
                    LineKind::Heading => style(text).bold().to_string(),
                    LineKind::Body => text.clone(),
                    LineKind::Tool => style(text).dim().to_string(),
                });
            }
        }

        frame.lines.resize(
            height.saturating_sub(1).max(frame.lines.len()),
            String::new(),
        );
        let keys = "↑↓ select · Enter open · Esc back · PgUp/PgDn scroll · q close";
        let help = if self.scroll_back > 0 {
            format!("scrolled · {keys}")
        } else {
            keys.to_string()
        };
        frame.lines.push(fit(&help, width, |s| style(s).dim()));
        frame
    }
}

pub(super) fn child_row(child: &ChildRow, selected: bool, width: usize) -> String {
    let marker = if selected { "›" } else { " " };
    // Router delegate ids all start with their parent's id; the tail tells them apart.
    let distinct = child
        .id
        .rsplit_once(DELEGATE_ID_SEPARATOR)
        .map_or(child.id.as_str(), |(_, tail)| tail);
    let short_id: String = distinct.chars().take(SHORT_ID_CHARS).collect();
    let detail = if child.status == ChildStatus::Running {
        child.running_label(Utc::now())
    } else {
        child.status.label().to_string()
    };
    let row = format!(
        "{marker} {:<9} {}  {short_id}  {}",
        detail,
        sanitize(&child.label),
        sanitize(&child.activity)
    );
    if selected {
        fit(&row, width, |s| style(s).cyan().bold())
    } else {
        fit(&row, width, style)
    }
}

fn fit(
    text: &str,
    width: usize,
    paint: impl Fn(String) -> console::StyledObject<String>,
) -> String {
    paint(truncate_str(text, width, "…").into_owned()).to_string()
}

/// Session content can carry its own escape sequences, which would corrupt the frame.
fn sanitize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let clean: String = raw
            .replace('\t', "    ")
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let mut line = String::new();
        let mut used = 0;
        for grapheme in clean.graphemes(true) {
            let grapheme_width = measure_text_width(grapheme);
            if used + grapheme_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push_str(grapheme);
            used += grapheme_width;
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: ChildStatus) -> ChildRow {
        ChildRow {
            id: id.to_string(),
            label: format!("task {id}"),
            status,
            model: "opus 5.5".to_string(),
            started_at: None,
            activity: "reading files".to_string(),
        }
    }

    fn plain(frame: &Frame) -> Vec<String> {
        frame
            .lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect()
    }

    fn transcript(lines: usize) -> Option<Vec<TranscriptLine>> {
        Some(
            (0..lines)
                .map(|n| TranscriptLine {
                    kind: LineKind::Body,
                    text: format!("line {n}"),
                })
                .collect(),
        )
    }

    #[test]
    fn root_lists_children_with_state_and_activity() {
        let snapshot = Snapshot {
            children: vec![
                row("aaaaaaaaaa", ChildStatus::Running),
                row("bbbbbbbbbb", ChildStatus::Failed),
            ],
            transcript: None,
            notice: None,
        };
        let mut navigator = Navigator::default();

        let lines = plain(&navigator.render(&snapshot, 80, 10));

        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "Child sessions");
        assert_eq!(
            lines[2],
            "› opus 5.5  task aaaaaaaaaa  aaaaaaaa  reading files"
        );
        assert_eq!(
            lines[3],
            "  failed    task bbbbbbbbbb  bbbbbbbb  reading files"
        );
        assert!(lines[9].starts_with("↑↓ select"));
    }

    #[test]
    fn running_label_advances_without_new_child_data() {
        let start: DateTime<Utc> = "2026-10-19T10:00:00Z".parse().unwrap();
        let mut child = row("child", ChildStatus::Running);
        child.started_at = Some(start);
        for (seconds, expected) in [
            (-1, "0s"),
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h0m"),
            (10920, "3h2m"),
        ] {
            assert_eq!(
                child.running_label(start + chrono::Duration::seconds(seconds)),
                format!("opus 5.5 [{expected}]")
            );
        }
        child.started_at = Some(Utc::now() - chrono::Duration::minutes(182));
        let rendered = child_row(&child, false, 100);
        assert!(rendered.contains("opus 5.5 [3h2m]"), "{rendered}");
        assert!(!rendered.contains("running"));
    }

    #[test]
    fn empty_root_says_so() {
        let lines = plain(&Navigator::default().render(&Snapshot::default(), 40, 5));
        assert_eq!(lines[2], "No child sessions yet.");
    }

    #[test]
    fn notice_and_router_ids_render_inline() {
        let snapshot = Snapshot {
            children: vec![row(
                "rtr-parent::delegate-3485c19f-e580",
                ChildStatus::Running,
            )],
            transcript: None,
            notice: Some("router-acp delegates unavailable: timed out".to_string()),
        };
        let lines = plain(&Navigator::default().render(&snapshot, 80, 8));
        assert_eq!(lines[2], "router-acp delegates unavailable: timed out");
        assert!(lines[3].contains("  3485c19f  "));
    }

    #[test]
    fn lines_never_exceed_the_width() {
        let snapshot = Snapshot {
            children: vec![row(&"x".repeat(60), ChildStatus::Completed)],
            transcript: Some(vec![TranscriptLine {
                kind: LineKind::Body,
                text: "word ".repeat(40),
            }]),
            notice: None,
        };
        let frame = Navigator::default().render(&snapshot, 30, 12);
        assert!(plain(&frame)
            .iter()
            .all(|line| measure_text_width(line) <= 30));
    }

    #[test]
    fn strips_escape_sequences_from_session_content() {
        let snapshot = Snapshot {
            children: vec![],
            transcript: Some(vec![TranscriptLine {
                kind: LineKind::Body,
                text: "safe\x1b[2Jtext".to_string(),
            }]),
            notice: None,
        };
        let frame = Navigator::default().render(&snapshot, 40, 6);
        assert!(frame.lines.iter().all(|line| !line.contains("\x1b[2J")));
    }

    #[test]
    fn transcript_follows_the_tail_until_scrolled() {
        let snapshot = Snapshot {
            children: vec![],
            transcript: transcript(20),
            notice: None,
        };
        let mut navigator = Navigator::default();
        let frame = navigator.render(&snapshot, 40, 8);
        let lines = plain(&frame);
        assert_eq!(
            &lines[2..7],
            ["line 15", "line 16", "line 17", "line 18", "line 19"]
        );

        navigator.handle(Key::PageUp, &snapshot, &frame);
        let lines = plain(&navigator.render(&snapshot, 40, 8));
        assert_eq!(lines[2], "line 10");
        assert!(lines[7].starts_with("scrolled"));

        for _ in 0..10 {
            navigator.handle(Key::PageUp, &snapshot, &frame);
        }
        assert_eq!(plain(&navigator.render(&snapshot, 40, 8))[2], "line 0");

        navigator.handle(Key::PageDown, &snapshot, &frame);
        navigator.handle(Key::PageDown, &snapshot, &frame);
        navigator.handle(Key::PageDown, &snapshot, &frame);
        assert_eq!(plain(&navigator.render(&snapshot, 40, 8))[2], "line 15");
    }

    #[test]
    fn navigates_into_children_and_back() {
        let root = Snapshot {
            children: vec![
                row("first", ChildStatus::Running),
                row("second", ChildStatus::Running),
            ],
            transcript: None,
            notice: None,
        };
        let mut navigator = Navigator::default();
        let frame = navigator.render(&root, 60, 10);

        navigator.handle(Key::Down, &root, &frame);
        navigator.handle(Key::Down, &root, &frame);
        assert_eq!(navigator.handle(Key::Enter, &root, &frame), Outcome::Stay);
        assert_eq!(navigator.current(), Some("second"));

        let child = Snapshot {
            children: vec![row("grandchild", ChildStatus::Completed)],
            transcript: transcript(3),
            notice: None,
        };
        let lines = plain(&navigator.render(&child, 60, 10));
        assert_eq!(lines[0], "Child sessions › task second");
        assert!(lines[2].contains("task grandchild"));

        navigator.handle(Key::Escape, &child, &frame);
        assert_eq!(navigator.current(), None);
        let lines = plain(&navigator.render(&root, 60, 10));
        assert!(lines[3].starts_with("› "));

        assert_eq!(navigator.handle(Key::Escape, &root, &frame), Outcome::Close);
        assert_eq!(navigator.handle(Key::Open, &root, &frame), Outcome::Close);
    }

    #[test]
    fn clicking_a_row_opens_that_child() {
        let root = Snapshot {
            children: vec![
                row("first", ChildStatus::Running),
                row("second", ChildStatus::Stopped),
            ],
            transcript: None,
            notice: None,
        };
        let mut navigator = Navigator::default();
        let frame = navigator.render(&root, 60, 10);

        navigator.handle(Key::Click { row: 1, col: 0 }, &root, &frame);
        assert_eq!(navigator.current(), None);
        navigator.handle(Key::Click { row: 3, col: 5 }, &root, &frame);
        assert_eq!(navigator.current(), Some("second"));
    }

    #[test]
    fn keeps_the_selection_visible_in_long_lists() {
        let root = Snapshot {
            children: (0..10)
                .map(|n| row(&format!("child{n}"), ChildStatus::Running))
                .collect(),
            transcript: None,
            notice: None,
        };
        let mut navigator = Navigator::default();
        let mut frame = navigator.render(&root, 60, 6);
        for _ in 0..9 {
            navigator.handle(Key::Down, &root, &frame);
            frame = navigator.render(&root, 60, 6);
        }
        let lines = plain(&frame);
        assert!(lines[4].starts_with("› ") && lines[4].contains("child9"));
    }
}
