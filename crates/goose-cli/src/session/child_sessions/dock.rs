//! The footer under the session's output: during a turn, an input line for messages to the
//! agent, and the dock, a persistent list of the session's children. A scroll region keeps the
//! session's own output and the line editor above these rows, and a background task repaints
//! them as the children work. The footer reads no input itself: the session feeds it keys.

use super::router;
use super::view::{self, ChildRow, ChildStatus};
use super::Key;
use console::style;
use goose::providers::router_acp::RouterStateSource;
use goose::session::SessionManager;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

const REPAINT: Duration = Duration::from_secs(1);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Child rows the dock shows below its header; more collapse into a "+N more" row.
const MAX_ROWS: usize = 6;

/// The live footer, so a full-screen view or child process can hand the screen back and forth.
static ACTIVE: OnceLock<Mutex<Weak<Shared>>> = OnceLock::new();

/// The message being written while a turn runs.
#[derive(Default)]
struct InputLine {
    text: Vec<char>,
    cursor: usize,
}

/// What a key typed during a turn did.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    Edited,
    /// Enter: the message waits for the turn to end.
    Queued,
    /// Ctrl+Enter: end the turn now and send what is queued.
    SendNow,
    Ignored,
}

#[derive(Default)]
struct State {
    rows: Vec<ChildRow>,
    /// Router sessions keep the list's rows for a whole turn, since their children appear
    /// mid-turn and rows can only be reserved on a fresh line.
    always_list: bool,
    selected: Option<usize>,
    input: Option<InputLine>,
    queued: Vec<String>,
    /// Text typed during a turn that was never sent; it fills the next prompt.
    draft: String,
    /// Rows reserved at the bottom of the screen; 0 while none are.
    reserved: usize,
    /// Terminal size (cols, lines) the reservation was made for.
    size: (usize, usize),
    suspended: bool,
    focus_requested: bool,
    turn_active: bool,
}

struct Shared {
    state: Mutex<State>,
}

pub struct Dock {
    shared: Arc<Shared>,
    _task: AbortOnDropHandle<()>,
}

/// Where the dock's children come from: goose's own subagents and router-acp's delegates.
pub struct Sources {
    pub session_manager: Arc<SessionManager>,
    pub session_id: String,
    pub router: Option<(RouterStateSource, String)>,
}

impl Dock {
    pub fn start(sources: Sources) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                always_list: sources.router.is_some(),
                ..State::default()
            }),
        });
        *ACTIVE.get_or_init(Mutex::default).lock().unwrap() = Arc::downgrade(&shared);
        let painter = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                let turn_active = painter.state.lock().unwrap().turn_active;
                let rows = load_rows(&sources, turn_active).await;
                painter.update(rows);
                tokio::time::sleep(REPAINT).await;
            }
        });
        Self {
            shared,
            _task: AbortOnDropHandle::new(task),
        }
    }

    /// A turn starts: the footer gains its input line. Called where the session's output is
    /// on a fresh line, the only moment rows can be reserved without overwriting output.
    pub fn begin_turn(&self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.turn_active = true;
            state.input = Some(InputLine::default());
        }
        self.shared.fit();
    }

    /// Before the prompt: the input line goes (unsent text becomes the prompt's draft) and the
    /// footer shrinks to the dock.
    pub fn end_turn(&self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.turn_active = false;
            if let Some(input) = state.input.take() {
                let text: String = input.text.into_iter().collect();
                if !text.trim().is_empty() {
                    state.draft = text;
                }
            }
        }
        self.shared.fit();
    }

    /// Messages queued during the last turn, joined into the next one to send.
    pub fn take_queued(&self) -> Option<String> {
        let queued = std::mem::take(&mut self.shared.state.lock().unwrap().queued);
        (!queued.is_empty()).then(|| queued.join("\n\n"))
    }

    /// A key typed while a turn runs and the dock is not selecting.
    pub fn type_key(&self, key: Key) -> Typed {
        let typed = {
            let mut state = self.shared.state.lock().unwrap();
            let state = &mut *state;
            let Some(input) = state.input.as_mut() else {
                return Typed::Ignored;
            };
            match key {
                Key::Char(c) => {
                    input.text.insert(input.cursor, c);
                    input.cursor += 1;
                    Typed::Edited
                }
                Key::Backspace if input.cursor > 0 => {
                    input.cursor -= 1;
                    input.text.remove(input.cursor);
                    Typed::Edited
                }
                Key::Left => {
                    input.cursor = input.cursor.saturating_sub(1);
                    Typed::Edited
                }
                Key::Right => {
                    input.cursor = (input.cursor + 1).min(input.text.len());
                    Typed::Edited
                }
                Key::Home => {
                    input.cursor = 0;
                    Typed::Edited
                }
                Key::End => {
                    input.cursor = input.text.len();
                    Typed::Edited
                }
                Key::KillLine | Key::Escape => {
                    *input = InputLine::default();
                    Typed::Edited
                }
                Key::Enter | Key::SendNow => {
                    let text: String = std::mem::take(input).text.into_iter().collect();
                    if !text.trim().is_empty() {
                        state.queued.push(text.trim().to_string());
                    }
                    match key {
                        Key::SendNow if !state.queued.is_empty() => Typed::SendNow,
                        _ if text.trim().is_empty() => Typed::Ignored,
                        _ => Typed::Queued,
                    }
                }
                _ => Typed::Ignored,
            }
        };
        self.shared.paint();
        typed
    }

    pub fn has_rows(&self) -> bool {
        !self.shared.state.lock().unwrap().rows.is_empty()
    }

    pub fn selected(&self) -> Option<ChildRow> {
        let state = self.shared.state.lock().unwrap();
        state.selected.and_then(|index| state.rows.get(index).cloned())
    }

    pub fn is_selecting(&self) -> bool {
        self.shared.state.lock().unwrap().selected.is_some()
    }

    /// Moves the selection by `delta` rows; moving up past the first row ends the selection.
    /// Returns whether a row is still selected.
    pub fn step(&self, delta: isize) -> bool {
        let selecting = {
            let mut state = self.shared.state.lock().unwrap();
            state.selected = next_selection(state.selected, state.rows.len(), delta);
            state.selected.is_some()
        };
        set_mouse(selecting);
        self.shared.paint();
        selecting
    }

    pub fn clear_selection(&self) {
        self.shared.state.lock().unwrap().selected = None;
        set_mouse(false);
        self.shared.paint();
    }

    /// The child shown on screen row `row` (0-based), selecting it.
    pub fn select_row(&self, row: u16) -> Option<ChildRow> {
        let child = {
            let mut state = self.shared.state.lock().unwrap();
            let layout = Layout::of(&state, state.size.1);
            let first_child = layout.list_top(state.size.1)?;
            let index = (row as usize).checked_sub(first_child)?;
            let child = state
                .rows
                .get(index)
                .filter(|_| index < layout.child_slots())?
                .clone();
            state.selected = Some(index);
            child
        };
        self.shared.paint();
        Some(child)
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        set_mouse(false);
        self.shared.release();
    }
}

/// The kitty keyboard protocol's "disambiguate" level for as long as it lives: Ctrl+Enter
/// arrives as its own key. Ctrl-C then arrives as a key too, so its holder must cancel on
/// `Key::Interrupt` itself.
pub struct ExtendedKeys;

impl ExtendedKeys {
    pub fn enable() -> Option<Self> {
        is_capable().then(|| {
            write_raw("\x1b[>1u");
            Self
        })
    }
}

impl Drop for ExtendedKeys {
    fn drop(&mut self) {
        write_raw("\x1b[<u");
    }
}

/// Text typed during the last turn and never sent, for the next prompt to start from.
pub fn take_draft() -> String {
    with_active(|shared| std::mem::take(&mut shared.state.lock().unwrap().draft)).unwrap_or_default()
}

pub fn is_capable() -> bool {
    super::terminal::is_capable()
}

/// From the line editor's key handler: true when the dock has children to move into.
pub fn request_focus() -> bool {
    with_active(|shared| {
        let mut state = shared.state.lock().unwrap();
        state.focus_requested = !state.rows.is_empty();
        state.focus_requested
    })
    .unwrap_or(false)
}

/// The prompt's ↓ key on an empty line asks for the dock; the input reader takes the request
/// once the line editor has returned.
pub fn take_focus_request() -> bool {
    with_active(|shared| std::mem::take(&mut shared.state.lock().unwrap().focus_requested))
        .unwrap_or(false)
}

/// A full-screen view or an external program takes the whole screen; the footer stops
/// painting and gives the scroll region back until `resume`.
pub fn suspend() {
    with_active(|shared| {
        shared.state.lock().unwrap().suspended = true;
        write_raw("\x1b7\x1b[r\x1b8");
    });
}

pub fn resume() {
    with_active(|shared| {
        let (reserved, lines) = {
            let mut state = shared.state.lock().unwrap();
            state.suspended = false;
            (state.reserved, state.size.1)
        };
        if reserved > 0 {
            write_raw(&region_sequence(reserved, lines));
        }
        shared.paint();
    });
}

fn with_active<R>(f: impl FnOnce(&Shared) -> R) -> Option<R> {
    let shared = ACTIVE.get()?.lock().unwrap().upgrade()?;
    Some(f(&shared))
}

/// How the footer's rows are split: the input line (during a turn) above the dock's header
/// and child rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    input: bool,
    /// Dock rows, header included; 0 when there is no dock.
    list: usize,
}

impl Layout {
    fn of(state: &State, lines: usize) -> Self {
        let wanted = !state.rows.is_empty() || (state.always_list && state.input.is_some());
        let list = if wanted && lines >= 8 {
            (MAX_ROWS + 1).min(lines / 3).max(2)
        } else {
            0
        };
        Self {
            input: state.input.is_some() && lines >= 4,
            list,
        }
    }

    fn height(self) -> usize {
        usize::from(self.input) + self.list
    }

    /// The first child row's 0-based screen row (the header is the row above it).
    fn list_top(self, lines: usize) -> Option<usize> {
        (self.list > 0).then(|| lines - self.list + 1)
    }

    fn child_slots(self) -> usize {
        self.list.saturating_sub(1)
    }
}

impl Shared {
    fn update(&self, rows: Vec<ChildRow>) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(selected) = state.selected {
                let id = state.rows.get(selected).map(|row| row.id.clone());
                state.selected = id
                    .and_then(|id| rows.iter().position(|row| row.id == id))
                    .or(Some(selected.min(rows.len().saturating_sub(1))))
                    .filter(|_| !rows.is_empty());
            }
            state.rows = rows;
        }
        self.follow_resize();
        self.paint();
    }

    /// Reserves exactly the rows the current layout needs. Only called on a fresh line.
    fn fit(&self) {
        if !is_capable() {
            return;
        }
        let (cols, lines) = terminal_size();
        let mut state = self.state.lock().unwrap();
        if state.suspended {
            return;
        }
        let height = Layout::of(&state, lines).height();
        if height == state.reserved && state.size == (cols, lines) {
            drop(state);
            self.paint();
            return;
        }
        let mut out = String::new();
        if state.reserved > 0 {
            out.push_str(&release_sequence(state.reserved, state.size.1));
        }
        if height > 0 {
            // Newlines push existing output up so the cursor sits above the reserved rows;
            // the cursor then returns to the line it was on.
            out.push_str(&"\n".repeat(height));
            out.push_str(&format!("\x1b[{height}A"));
            out.push_str(&region_sequence(height, lines));
        }
        write_raw(&out);
        state.reserved = height;
        state.size = (cols, lines);
        drop(state);
        self.paint();
    }

    /// Terminals reset the scroll region on resize; reapply it for the new size.
    fn follow_resize(&self) {
        let (cols, lines) = terminal_size();
        let mut state = self.state.lock().unwrap();
        if state.reserved == 0 || state.suspended || state.size == (cols, lines) {
            return;
        }
        write_raw(&release_sequence(state.reserved, state.size.1));
        state.reserved = Layout::of(&state, lines).height().min(state.reserved);
        state.size = (cols, lines);
        write_raw(&region_sequence(state.reserved, lines));
    }

    fn paint(&self) {
        let state = self.state.lock().unwrap();
        if state.reserved == 0 || state.suspended {
            return;
        }
        write_raw(&frame(&state, state.size.0, state.size.1));
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        if state.reserved > 0 {
            write_raw(&release_sequence(state.reserved, state.size.1));
            state.reserved = 0;
        }
    }
}

fn next_selection(selected: Option<usize>, rows: usize, delta: isize) -> Option<usize> {
    let last = rows.saturating_sub(1).min(MAX_ROWS - 1);
    match selected {
        _ if rows == 0 => None,
        None if delta > 0 => Some(0),
        None => None,
        Some(0) if delta < 0 => None,
        Some(index) => Some(index.saturating_add_signed(delta).min(last)),
    }
}

/// Save the cursor, confine scrolling to the rows above the footer, restore the cursor.
fn region_sequence(height: usize, lines: usize) -> String {
    format!("\x1b7\x1b[1;{}r\x1b8", lines.saturating_sub(height).max(1))
}

/// Clear the footer rows and give the whole screen back to scrolling.
fn release_sequence(height: usize, lines: usize) -> String {
    let mut out = String::from("\x1b7\x1b[r");
    for row in lines.saturating_sub(height) + 1..=lines {
        out.push_str(&format!("\x1b[{row};1H\x1b[2K"));
    }
    out.push_str("\x1b8");
    out
}

/// One write that paints every footer row and puts the cursor back where the output left it.
/// The rows are laid out within what is reserved, which may be less than the layout wants
/// until the next fresh line lets the footer grow.
fn frame(state: &State, cols: usize, lines: usize) -> String {
    let mut layout = Layout::of(state, lines);
    if layout.height() > state.reserved {
        layout.input &= state.reserved > 0;
        layout.list = state.reserved - usize::from(layout.input);
    }
    let mut row = lines.saturating_sub(state.reserved) + 1;
    let mut out = String::from("\x1b7");
    let mut line = |out: &mut String, text: String| {
        out.push_str(&format!("\x1b[{row};1H{text}\x1b[K"));
        row += 1;
    };
    if layout.input {
        line(&mut out, input_row(state, cols));
    }
    if layout.list > 0 {
        line(&mut out, header(state, cols));
        let slots = layout.child_slots();
        let overflow = state.rows.len() > slots;
        let shown = if overflow { slots.saturating_sub(1) } else { slots };
        for slot in 0..slots {
            let text = match state.rows.get(slot) {
                Some(child) if slot < shown => {
                    view::child_row(child, state.selected == Some(slot), cols)
                }
                _ if overflow && slot == shown => {
                    let more = state.rows.len() - shown;
                    style(format!("  +{more} more · /children for all"))
                        .dim()
                        .to_string()
                }
                _ => String::new(),
            };
            line(&mut out, text);
        }
    }
    out.push_str("\x1b8");
    out
}

fn input_row(state: &State, cols: usize) -> String {
    let Some(input) = &state.input else {
        return String::new();
    };
    let queued = match state.queued.len() {
        0 => String::new(),
        n => format!("{n} queued · "),
    };
    let hint = format!(" {queued}Enter queues for the turn's end · Ctrl+Enter sends now");
    let room = cols
        .saturating_sub(console::measure_text_width(&hint) + 3)
        .max(10);
    // Keep the cursor in view: show the end of a line too long for the row.
    let start = input.cursor.saturating_sub(room.saturating_sub(1));
    let visible: String = input.text.iter().skip(start).take(room).collect();
    let at = input.cursor - start;
    let (before, rest): (String, String) = (
        visible.chars().take(at).collect(),
        visible.chars().skip(at).collect(),
    );
    let mut rest = rest.chars();
    let under = rest.next().unwrap_or(' ');
    let text = format!(
        "{} {before}{}{}",
        style("›").cyan().bold(),
        style(under).reverse(),
        rest.collect::<String>()
    );
    let width = console::measure_text_width(&text);
    let hint = console::truncate_str(&hint, cols.saturating_sub(width), "").into_owned();
    format!(
        "{text}{}{}",
        " ".repeat(cols.saturating_sub(width + console::measure_text_width(&hint))),
        style(hint).dim()
    )
}

fn header(state: &State, cols: usize) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for row in &state.rows {
        let label = row.status.label();
        match counts.iter_mut().find(|(name, _)| *name == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    let summary = if counts.is_empty() {
        "none yet".to_string()
    } else {
        counts
            .iter()
            .map(|(name, count)| format!("{count} {name}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let keys = match (state.selected.is_some(), state.input.is_some()) {
        (true, _) => " ── ↑↓ select · Enter open · Esc back",
        (false, true) if !state.rows.is_empty() => " ── ↑↓ to select",
        (false, false) if !state.rows.is_empty() => " ── ↓ on an empty prompt to select",
        _ => "",
    };
    let text = format!("── children · {summary}{keys} ");
    let width = console::measure_text_width(&text);
    let line = format!("{text}{}", "─".repeat(cols.saturating_sub(width)));
    style(console::truncate_str(&line, cols, "").into_owned())
        .dim()
        .to_string()
}

/// The children worth a row: every running one first, then the most recent others.
async fn load_rows(sources: &Sources, turn_active: bool) -> Vec<ChildRow> {
    let mut rows =
        super::goose_children(&sources.session_manager, &sources.session_id, turn_active)
            .await
            .unwrap_or_default();
    if let Some((source, root)) = &sources.router {
        if let Ok(delegates) = source
            .delegates(std::slice::from_ref(root), QUERY_TIMEOUT)
            .await
        {
            rows.extend(router::dock_rows(&delegates, root, chrono::Utc::now()));
        }
    }
    rows.reverse();
    rows.sort_by_key(|row| row.status != ChildStatus::Running);
    rows
}

fn terminal_size() -> (usize, usize) {
    let (rows, cols) = console::Term::stdout().size();
    (cols as usize, rows as usize)
}

/// Mouse reporting only while a dock row is selected, so the wheel and text selection keep
/// working on the terminal's scrollback the rest of the time.
fn set_mouse(on: bool) {
    write_raw(if on {
        "\x1b[?1000h\x1b[?1006h"
    } else {
        "\x1b[?1006l\x1b[?1000l"
    });
}

/// One write per paint: the line editor writes to the same terminal from its own buffer, so
/// a paint must never be split around its writes.
fn write_raw(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: ChildStatus) -> ChildRow {
        ChildRow {
            id: id.into(),
            label: id.into(),
            status,
            activity: String::new(),
        }
    }

    fn typing() -> Dock {
        Dock {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    input: Some(InputLine::default()),
                    ..State::default()
                }),
            }),
            _task: AbortOnDropHandle::new(tokio::spawn(async {})),
        }
    }

    #[test]
    fn the_region_leaves_the_bottom_rows_to_the_footer() {
        assert_eq!(region_sequence(7, 40), "\x1b7\x1b[1;33r\x1b8");
        let state = State {
            rows: vec![row("a", ChildStatus::Running)],
            input: Some(InputLine::default()),
            ..State::default()
        };
        assert_eq!(Layout::of(&state, 40).height(), 8);
        assert_eq!(Layout::of(&state, 12).height(), 5);
        assert_eq!(Layout::of(&State::default(), 40).height(), 0);
    }

    #[test]
    fn a_frame_paints_only_the_footer_rows_and_restores_the_cursor() {
        let state = State {
            rows: vec![row("a", ChildStatus::Running), row("b", ChildStatus::Completed)],
            input: Some(InputLine::default()),
            reserved: 4,
            size: (80, 30),
            ..State::default()
        };
        let frame = frame(&state, 80, 30);
        assert!(frame.starts_with("\x1b7\x1b[27;1H"));
        assert!(frame.ends_with("\x1b8"));
        assert!(frame.contains("\x1b[30;1H") && !frame.contains("\x1b[26;1H"));
        let plain = console::strip_ansi_codes(&frame).to_string();
        assert!(plain.contains("Ctrl+Enter sends now") && plain.contains("children · 1 running"));
    }

    #[test]
    fn more_children_than_rows_collapse_into_a_count() {
        let state = State {
            rows: (0..5).map(|i| row(&i.to_string(), ChildStatus::Running)).collect(),
            reserved: 3,
            size: (80, 30),
            ..State::default()
        };
        assert!(console::strip_ansi_codes(&frame(&state, 80, 30)).contains("+4 more"));
    }

    #[test]
    fn up_from_the_first_row_leaves_the_dock() {
        assert_eq!(next_selection(None, 2, 1), Some(0));
        assert_eq!(next_selection(Some(0), 2, 1), Some(1));
        assert_eq!(next_selection(Some(1), 2, 1), Some(1));
        assert_eq!(next_selection(Some(1), 2, -1), Some(0));
        assert_eq!(next_selection(Some(0), 2, -1), None);
        assert_eq!(next_selection(None, 0, 1), None);
    }

    #[tokio::test]
    async fn enter_queues_and_ctrl_enter_sends_everything_queued() {
        let dock = typing();
        for c in "first".chars() {
            assert_eq!(dock.type_key(Key::Char(c)), Typed::Edited);
        }
        assert_eq!(dock.type_key(Key::Enter), Typed::Queued);
        assert_eq!(dock.type_key(Key::Enter), Typed::Ignored, "an empty line queues nothing");
        for c in "secnd".chars() {
            dock.type_key(Key::Char(c));
        }
        dock.type_key(Key::Left);
        dock.type_key(Key::Left);
        dock.type_key(Key::Char('o'));
        assert_eq!(dock.type_key(Key::SendNow), Typed::SendNow);
        assert_eq!(dock.take_queued().as_deref(), Some("first\n\nsecond"));
        assert_eq!(dock.take_queued(), None);
    }

    #[tokio::test]
    async fn unsent_text_becomes_the_next_prompts_draft() {
        let dock = typing();
        for c in "half a thought".chars() {
            dock.type_key(Key::Char(c));
        }
        dock.end_turn();
        let draft = |dock: &Dock| std::mem::take(&mut dock.shared.state.lock().unwrap().draft);
        assert_eq!(draft(&dock), "half a thought");
        assert_eq!(draft(&dock), "");
    }
}
