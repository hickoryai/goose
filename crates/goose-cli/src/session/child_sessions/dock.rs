//! The footer under the session's output: during a turn, an input line for messages to the
//! agent, and the dock, a persistent list of the session's children. A scroll region keeps the
//! session's own output and the line editor above these rows, and a background task repaints
//! them as the children work. The footer reads no input itself: the session feeds it keys.
//!
//! Enter queues the typed message; the session hands queued messages to the running turn one
//! at a time (`next_steer`, `finish_steer`) and sends whatever is left when the turn ends.
//! Up on an empty input line takes the newest undelivered message back for editing; Down
//! moves into the dock. While the input line shows, it draws the only cursor: the terminal's
//! own stays hidden until the turn ends.

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
/// Rows a long message may wrap onto; a longer one shows the rows around the caret.
const MAX_INPUT_ROWS: usize = 5;
/// The input line's "› " gutter.
const GUTTER: usize = 2;

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
    /// Enter: the message is queued for the running turn, without interrupting it.
    Queued,
    /// Ctrl+Enter: end the turn now and send what is queued.
    SendNow,
    Ignored,
}

#[derive(Default)]
struct State {
    rows: Vec<ChildRow>,
    /// Router sessions keep the list's rows at all times, at the prompt as well as during a
    /// turn. Rows can only be reserved on a fresh line (a turn's start or end), so a list sized
    /// by the children known at that moment vanished whenever they were not known yet (a new
    /// dock) or a query had just failed, and stayed gone until the next fresh line.
    always_list: bool,
    selected: Option<usize>,
    /// The first child row shown; follows the selection through a longer list.
    offset: usize,
    input: Option<InputLine>,
    /// Messages not yet delivered, oldest first.
    queued: Vec<String>,
    /// The queued message being handed to the running turn; counted as queued until then.
    steering: Option<String>,
    /// Text typed during a turn that was never sent; it fills the next prompt.
    draft: String,
    /// Rows reserved at the bottom of the screen; 0 while none are.
    reserved: usize,
    /// Terminal size (cols, lines) the reservation was made for.
    size: (usize, usize),
    suspended: bool,
    focus_requested: bool,
    turn_active: bool,
    /// A session-wide state the header leads with, e.g. `shutting down 1/3`.
    status: Option<String>,
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
                painter.update(load_rows(&sources, turn_active).await);
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
        show_cursor();
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

    /// Messages queued during the last turn and never delivered, joined into the next one to
    /// send.
    pub fn take_queued(&self) -> Option<String> {
        let queued = {
            let mut state = self.shared.state.lock().unwrap();
            let mut queued: Vec<String> = state.steering.take().into_iter().collect();
            queued.append(&mut state.queued);
            queued
        };
        (!queued.is_empty()).then(|| queued.join("\n\n"))
    }

    /// The oldest queued message, to hand to the running turn; none while one is on its way.
    pub fn next_steer(&self) -> Option<String> {
        let mut state = self.shared.state.lock().unwrap();
        if state.steering.is_some() || state.queued.is_empty() {
            return None;
        }
        let text = state.queued.remove(0);
        state.steering = Some(text.clone());
        Some(text)
    }

    /// The message from `next_steer` reached the turn, or goes back to the front of the queue.
    pub fn finish_steer(&self, delivered: bool) {
        {
            let mut state = self.shared.state.lock().unwrap();
            if let Some(text) = state.steering.take().filter(|_| !delivered) {
                state.queued.insert(0, text);
            }
        }
        self.shared.paint();
    }

    /// Messages that left the queue but never reached the turn go back to its front.
    pub fn requeue(&self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        self.shared.state.lock().unwrap().queued.splice(0..0, texts);
        self.shared.paint();
    }

    pub fn is_steering(&self) -> bool {
        self.shared.state.lock().unwrap().steering.is_some()
    }

    pub fn has_queued(&self) -> bool {
        !self.shared.state.lock().unwrap().queued.is_empty()
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
                // The newest undelivered message comes back for editing; Enter queues it again.
                Key::Up if input.text.is_empty() => match state.queued.pop() {
                    Some(text) => {
                        input.text = text.chars().collect();
                        input.cursor = input.text.len();
                        Typed::Edited
                    }
                    None => Typed::Ignored,
                },
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

    /// What the header leads with until cleared with `None`.
    pub fn set_status(&self, status: Option<String>) {
        self.shared.state.lock().unwrap().status = status;
        self.shared.paint();
    }

    /// The text typed into the turn's input line so far.
    pub fn input_text(&self) -> String {
        let state = self.shared.state.lock().unwrap();
        state
            .input
            .as_ref()
            .map(|input| input.text.iter().collect())
            .unwrap_or_default()
    }

    pub fn clear_input(&self) {
        if let Some(input) = self.shared.state.lock().unwrap().input.as_mut() {
            *input = InputLine::default();
        }
        self.shared.paint();
    }

    pub fn has_rows(&self) -> bool {
        !self.shared.state.lock().unwrap().rows.is_empty()
    }

    pub fn selected(&self) -> Option<ChildRow> {
        let state = self.shared.state.lock().unwrap();
        state
            .selected
            .and_then(|index| state.rows.get(index).cloned())
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
            follow_selection(&mut state);
            state.selected.is_some()
        };
        set_mouse(selecting);
        self.shared.paint();
        selecting
    }

    pub fn clear_selection(&self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.selected = None;
            follow_selection(&mut state);
        }
        set_mouse(false);
        self.shared.paint();
    }

    /// The child shown on screen row `row` (0-based), selecting it.
    pub fn select_row(&self, row: u16) -> Option<ChildRow> {
        let child = {
            let mut state = self.shared.state.lock().unwrap();
            let lines = state.size.1;
            let layout = shown_layout(&state, state.size);
            let first_child = lines.checked_sub(state.reserved)? + layout.input + 1;
            let slot = (row as usize).checked_sub(first_child)?;
            let (shown, _) = window(&state, layout);
            let index = state.offset + slot;
            let child = state.rows.get(index).filter(|_| slot < shown)?.clone();
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
        show_cursor();
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
    with_active(|shared| std::mem::take(&mut shared.state.lock().unwrap().draft))
        .unwrap_or_default()
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
        write_raw("\x1b7\x1b[r\x1b8\x1b[?25h");
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

/// How the footer's rows are split: the input line's rows (during a turn) above the dock's
/// header and child rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    input: usize,
    /// Dock rows, header included; 0 when there is no dock.
    list: usize,
}

impl Layout {
    fn of(state: &State, (cols, lines): (usize, usize)) -> Self {
        let wanted = state.always_list || !state.rows.is_empty();
        let list = if wanted && lines >= 8 {
            (MAX_ROWS + 1).min(lines / 3).max(2)
        } else {
            0
        };
        let input = match &state.input {
            Some(input) if lines >= 4 => input_lines(input.text.len(), cols).min(MAX_INPUT_ROWS),
            _ => 0,
        };
        Self { input, list }
    }

    fn height(self) -> usize {
        self.input + self.list
    }

    fn child_slots(self) -> usize {
        self.list.saturating_sub(1)
    }
}

impl Shared {
    /// New rows from a load; `None` (a failed query) keeps the rows already shown.
    fn update(&self, rows: Option<Vec<ChildRow>>) {
        if let Some(rows) = rows {
            let mut state = self.state.lock().unwrap();
            if let Some(selected) = state.selected {
                let id = state.rows.get(selected).map(|row| row.id.clone());
                state.selected = id
                    .and_then(|id| rows.iter().position(|row| row.id == id))
                    .or(Some(selected.min(rows.len().saturating_sub(1))))
                    .filter(|_| !rows.is_empty());
            }
            state.rows = rows;
            follow_selection(&mut state);
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
        let height = Layout::of(&state, (cols, lines)).height();
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
        state.reserved = Layout::of(&state, (cols, lines))
            .height()
            .min(state.reserved);
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
    let last = rows.saturating_sub(1);
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

/// The layout as painted: what the layout wants, within what is reserved until the next fresh
/// line lets the footer grow. A long message takes rows from the dock, which keeps its header.
fn shown_layout(state: &State, size: (usize, usize)) -> Layout {
    let mut layout = Layout::of(state, size);
    if layout.height() > state.reserved {
        let header = usize::from(layout.list > 0);
        layout.input = layout
            .input
            .min(state.reserved.saturating_sub(header))
            .max(usize::from(layout.input > 0 && state.reserved > 0));
        layout.list = state.reserved - layout.input;
    }
    layout
}

/// How many child rows fit, and whether the last slot instead says what is out of view.
fn window(state: &State, layout: Layout) -> (usize, bool) {
    let slots = layout.child_slots();
    let overflow = state.rows.len() > slots;
    (
        if overflow {
            slots.saturating_sub(1)
        } else {
            slots
        },
        overflow,
    )
}

/// Scrolls the list so the selected child is shown; without a selection it shows the top.
fn follow_selection(state: &mut State) {
    let (shown, _) = window(state, shown_layout(state, state.size));
    state.offset = match state.selected {
        None => 0,
        Some(selected) if selected < state.offset => selected,
        Some(selected) if shown > 0 && selected >= state.offset + shown => selected + 1 - shown,
        Some(_) => state.offset,
    };
    state.offset = state.offset.min(state.rows.len().saturating_sub(shown));
}

/// One write that paints every footer row and puts the cursor back where the output left it.
/// While the input line shows, its caret is the only cursor: the terminal's own is hidden.
fn frame(state: &State, cols: usize, lines: usize) -> String {
    let layout = shown_layout(state, (cols, lines));
    let mut row = lines.saturating_sub(state.reserved) + 1;
    let mut out = String::from("\x1b7");
    if state.input.is_some() {
        out.push_str("\x1b[?25l");
    }
    let mut line = |out: &mut String, text: String| {
        out.push_str(&format!("\x1b[{row};1H{text}\x1b[K"));
        row += 1;
    };
    for text in input_rows(state, cols, layout.input) {
        line(&mut out, text);
    }
    if layout.list > 0 {
        line(&mut out, header(state, cols));
        let slots = layout.child_slots();
        let (shown, overflow) = window(state, layout);
        for slot in 0..slots {
            let index = state.offset + slot;
            let text = match state.rows.get(index) {
                Some(child) if slot < shown => {
                    view::child_row(child, state.selected == Some(index), cols)
                }
                _ if overflow && slot == shown => {
                    let above = state.offset;
                    let below = state.rows.len().saturating_sub(state.offset + shown);
                    let mut parts = Vec::new();
                    if above > 0 {
                        parts.push(format!("↑ {above} above"));
                    }
                    if below > 0 {
                        parts.push(format!("↓ {below} more"));
                    }
                    parts.push("/children for all".to_string());
                    style(format!("  {}", parts.join(" · "))).dim().to_string()
                }
                _ => String::new(),
            };
            line(&mut out, text);
        }
    }
    out.push_str("\x1b8");
    out
}

/// Rows a message of `len` characters wraps onto behind the gutter, with a cell for the caret
/// after its last character.
fn input_lines(len: usize, cols: usize) -> usize {
    (len + 1).div_ceil(cols.saturating_sub(GUTTER).max(1))
}

/// The input line's `rows` rows: the message wrapped behind the "› " gutter, and the hint after
/// it on the last row, shortened to the queue count or dropped when the text needs the room.
/// A message taller than `rows` shows the rows around the caret.
fn input_rows(state: &State, cols: usize, rows: usize) -> Vec<String> {
    let Some(input) = &state.input else {
        return vec![String::new(); rows];
    };
    if rows == 0 {
        return Vec::new();
    }
    let width = cols.saturating_sub(GUTTER).max(1);
    let total = input_lines(input.text.len(), cols);
    let caret_line = input.cursor / width;
    let first = if total <= rows {
        0
    } else {
        caret_line.saturating_sub(rows - 1).min(total - rows)
    };
    let queued = state.queued.len() + usize::from(state.steering.is_some());
    let hints = match queued {
        0 => vec![" Enter sends mid-turn · Ctrl+Enter interrupts".to_string()],
        n => vec![
            format!(" {n} queued · ↑ edits · Enter sends mid-turn · Ctrl+Enter interrupts"),
            format!(" {n} queued · ↑ edits"),
            format!(" {n} queued"),
        ],
    };
    (first..first + rows.min(total))
        .map(|line| {
            let start = line * width;
            let chars: Vec<char> = input.text.iter().skip(start).take(width).copied().collect();
            let mut text = if line == 0 {
                format!("{} ", style("›").cyan().bold())
            } else {
                " ".repeat(GUTTER)
            };
            let mut used = GUTTER + chars.len();
            for (offset, c) in chars.iter().enumerate() {
                if start + offset == input.cursor {
                    text.push_str(&style(c).reverse().to_string());
                } else {
                    text.push(*c);
                }
            }
            if input.cursor >= start + chars.len() && input.cursor < start + width {
                text.push_str(&style(' ').reverse().to_string());
                used += 1;
            }
            if line + 1 == total {
                let room = cols.saturating_sub(used);
                if let Some(hint) = hints
                    .iter()
                    .find(|hint| console::measure_text_width(hint) <= room)
                {
                    let pad = room - console::measure_text_width(hint);
                    text.push_str(&format!("{}{}", " ".repeat(pad), style(hint).dim()));
                }
            }
            text
        })
        .collect()
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
        "none running · /children for all".to_string()
    } else {
        counts
            .iter()
            .map(|(name, count)| format!("{count} {name}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let keys = match (state.selected.is_some(), state.input.is_some()) {
        (true, _) => " ── ↑↓ select · Enter open · Esc back",
        (false, true) if !state.rows.is_empty() => " ── ↓ to select",
        (false, false) if !state.rows.is_empty() => " ── ↓ on an empty prompt to select",
        _ => "",
    };
    let lead = state
        .status
        .as_ref()
        .map(|status| format!("── {status} "))
        .unwrap_or_default();
    let text = format!("{lead}── children · {summary}{keys} ");
    let width = console::measure_text_width(&text);
    let line = format!("{text}{}", "─".repeat(cols.saturating_sub(width)));
    style(console::truncate_str(&line, cols, "").into_owned())
        .dim()
        .to_string()
}

/// The dock lists running children only, newest first; `/children` lists the rest. `None`
/// when a source could not be read (router-acp's state query times out under load), so a
/// failed read never blanks a list of children that are still running.
async fn load_rows(sources: &Sources, turn_active: bool) -> Option<Vec<ChildRow>> {
    let mut rows =
        super::goose_children(&sources.session_manager, &sources.session_id, turn_active)
            .await
            .ok()?;
    if let Some((source, root)) = &sources.router {
        let delegates = source
            .delegates(std::slice::from_ref(root), QUERY_TIMEOUT)
            .await
            .ok()?;
        rows.extend(router::dock_rows(&delegates, root, chrono::Utc::now()));
    }
    rows.retain(|row| row.status == ChildStatus::Running);
    rows.reverse();
    Some(rows)
}

fn terminal_size() -> (usize, usize) {
    let (rows, cols) = console::Term::stdout().size();
    (cols as usize, rows as usize)
}

/// Mouse reporting only while a dock row is selected, so the wheel and text selection keep
/// working on the terminal's scrollback the rest of the time.
fn show_cursor() {
    if is_capable() {
        write_raw("\x1b[?25h");
    }
}

fn set_mouse(on: bool) {
    if !is_capable() {
        return;
    }
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
            model: "opus 5.5".to_string(),
            started_at: None,
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
        assert_eq!(Layout::of(&state, (80, 40)).height(), 8);
        assert_eq!(Layout::of(&state, (80, 12)).height(), 5);
        assert_eq!(Layout::of(&State::default(), (80, 40)).height(), 0);
    }

    #[test]
    fn a_frame_paints_only_the_footer_rows_and_restores_the_cursor() {
        let state = State {
            rows: vec![
                row("a", ChildStatus::Running),
                row("b", ChildStatus::Completed),
            ],
            input: Some(InputLine::default()),
            reserved: 4,
            size: (80, 30),
            ..State::default()
        };
        let frame = frame(&state, 80, 30);
        assert!(frame.starts_with("\x1b7\x1b[?25l\x1b[27;1H"));
        assert!(frame.ends_with("\x1b8"));
        assert!(frame.contains("\x1b[30;1H") && !frame.contains("\x1b[26;1H"));
        let plain = console::strip_ansi_codes(&frame).to_string();
        assert!(
            frame.contains("\x1b[?25l"),
            "the input line's caret is the only cursor"
        );
        assert!(plain.contains("Ctrl+Enter interrupts") && plain.contains("children · 1 running"));
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_in_view() {
        let mut state = State {
            rows: (0..10)
                .map(|i| row(&i.to_string(), ChildStatus::Running))
                .collect(),
            reserved: 4,
            size: (80, 30),
            ..State::default()
        };
        // 4 rows: header, two children, and the "out of view" row.
        for _ in 0..6 {
            state.selected = next_selection(state.selected, state.rows.len(), 1);
            follow_selection(&mut state);
        }
        assert_eq!((state.selected, state.offset), (Some(5), 4));
        let plain = console::strip_ansi_codes(&frame(&state, 80, 30)).to_string();
        assert!(plain.contains("› opus 5.5  5"), "{plain}");
        assert!(plain.contains("↑ 4 above · ↓ 4 more"), "{plain}");
        state.selected = None;
        follow_selection(&mut state);
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn more_children_than_rows_collapse_into_a_count() {
        let state = State {
            rows: (0..5)
                .map(|i| row(&i.to_string(), ChildStatus::Running))
                .collect(),
            reserved: 3,
            size: (80, 30),
            ..State::default()
        };
        assert!(console::strip_ansi_codes(&frame(&state, 80, 30)).contains("↓ 4 more"));
    }

    #[test]
    fn up_from_the_first_row_leaves_the_dock() {
        assert_eq!(next_selection(None, 2, 1), Some(0));
        assert_eq!(next_selection(Some(0), 2, 1), Some(1));
        assert_eq!(next_selection(Some(1), 2, 1), Some(1));
        assert_eq!(
            next_selection(Some(7), 9, 1),
            Some(8),
            "no cap at the visible rows"
        );
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
        assert_eq!(
            dock.type_key(Key::Enter),
            Typed::Ignored,
            "an empty line queues nothing"
        );
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

    fn typed(dock: &Dock, text: &str) {
        for c in text.chars() {
            dock.type_key(Key::Char(c));
        }
    }

    fn queued(dock: &Dock) -> (Vec<String>, Option<String>) {
        let state = dock.shared.state.lock().unwrap();
        (state.queued.clone(), state.steering.clone())
    }

    #[tokio::test]
    async fn queued_messages_reach_the_turn_one_at_a_time() {
        let dock = typing();
        typed(&dock, "first");
        dock.type_key(Key::Enter);
        typed(&dock, "second");
        dock.type_key(Key::Enter);

        assert_eq!(dock.next_steer().as_deref(), Some("first"));
        assert_eq!(dock.next_steer(), None, "one message on its way at a time");
        let plain =
            console::strip_ansi_codes(&input_rows(&dock.shared.state.lock().unwrap(), 80, 1)[0])
                .to_string();
        assert!(
            plain.contains("2 queued"),
            "the one on its way still counts: {plain}"
        );

        dock.finish_steer(false);
        assert_eq!(queued(&dock), (vec!["first".into(), "second".into()], None));
        assert_eq!(
            dock.next_steer().as_deref(),
            Some("first"),
            "a declined one goes first"
        );
        dock.finish_steer(true);
        assert_eq!(queued(&dock), (vec!["second".into()], None));

        assert_eq!(dock.next_steer().as_deref(), Some("second"));
        typed(&dock, "third");
        dock.type_key(Key::Enter);
        assert_eq!(
            dock.take_queued().as_deref(),
            Some("second\n\nthird"),
            "a turn that ends sends what never arrived"
        );
        dock.requeue(vec!["lost".into()]);
        assert_eq!(dock.take_queued().as_deref(), Some("lost"));
    }

    #[tokio::test]
    async fn up_on_an_empty_line_takes_back_the_newest_queued_message() {
        let dock = typing();
        assert_eq!(
            dock.type_key(Key::Up),
            Typed::Ignored,
            "nothing queued: the dock's"
        );
        typed(&dock, "one");
        dock.type_key(Key::Enter);
        typed(&dock, "two");
        dock.type_key(Key::Enter);

        assert_eq!(dock.type_key(Key::Up), Typed::Edited);
        assert_eq!(queued(&dock).0, ["one"]);
        assert_eq!(
            dock.type_key(Key::Up),
            Typed::Ignored,
            "a line being edited keeps it"
        );
        typed(&dock, "!");
        {
            let state = dock.shared.state.lock().unwrap();
            let input = state.input.as_ref().unwrap();
            assert_eq!(input.text.iter().collect::<String>(), "two!");
            assert_eq!(input.cursor, 4);
        }
        assert_eq!(dock.type_key(Key::Enter), Typed::Queued);
        assert_eq!(queued(&dock).0, ["one", "two!"]);

        assert_eq!(dock.next_steer().as_deref(), Some("one"));
        dock.type_key(Key::Up);
        assert_eq!(dock.type_key(Key::Up), Typed::Ignored);
        assert_eq!(
            queued(&dock),
            (vec![], Some("one".into())),
            "the one on its way stays"
        );
    }

    fn input_state(text: &str, cursor: usize, queued: usize) -> State {
        State {
            input: Some(InputLine {
                text: text.chars().collect(),
                cursor,
            }),
            queued: (0..queued).map(|i| i.to_string()).collect(),
            ..State::default()
        }
    }

    fn plain_rows(state: &State, cols: usize, rows: usize) -> Vec<String> {
        input_rows(state, cols, rows)
            .iter()
            .map(|row| console::strip_ansi_codes(row).trim_end().to_string())
            .collect()
    }

    #[test]
    fn the_hint_gives_way_to_the_text() {
        let short = plain_rows(&input_state("hi", 2, 2), 80, 1);
        assert!(short[0].starts_with("› hi"), "{short:?}");
        assert!(
            short[0].ends_with("2 queued · ↑ edits · Enter sends mid-turn · Ctrl+Enter interrupts")
        );

        let text = "x".repeat(55);
        let long = plain_rows(&input_state(&text, 55, 2), 80, 1);
        assert!(long[0].contains(&text), "the text is never cut: {long:?}");
        assert!(long[0].ends_with("2 queued · ↑ edits"), "{long:?}");

        let text = "x".repeat(74);
        let full = plain_rows(&input_state(&text, 74, 2), 80, 1);
        assert!(
            full[0].contains(&text) && !full[0].contains("queued"),
            "{full:?}"
        );
    }

    #[test]
    fn a_long_message_wraps_and_keeps_the_caret_in_view() {
        let text: String = ('a'..='z').cycle().take(50).collect();
        let state = input_state(&text, 50, 0);
        assert_eq!(Layout::of(&state, (20, 40)).input, 3);
        let rows = plain_rows(&state, 20, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.concat().replace(['›', ' '], ""), text);

        // Two rows for three lines of text: the caret's line and the one before it show.
        let rows = plain_rows(&state, 20, 2);
        assert_eq!(
            rows,
            [
                format!("  {}", text.get(18..36).unwrap()),
                format!("  {}", text.get(36..).unwrap())
            ]
        );
        let rows = plain_rows(&input_state(&text, 0, 0), 20, 2);
        assert!(
            rows[0].starts_with(&format!("› {}", text.get(..18).unwrap())),
            "{rows:?}"
        );

        // Within a reservation the long message takes rows from the dock, never its header.
        let mut state = input_state(&text, 50, 0);
        state.rows = vec![row("a", ChildStatus::Running)];
        state.reserved = 4;
        assert_eq!(shown_layout(&state, (20, 40)), Layout { input: 3, list: 1 });
    }

    /// The operator's report: after a message the dock and even its empty area went away
    /// until `/children` was closed. The turn's end re-reserved the footer from the rows known
    /// at that instant, and at the prompt a router dock was only wanted with rows in hand.
    #[test]
    fn a_router_dock_keeps_its_rows_at_the_prompt_without_children_in_hand() {
        let prompt = State {
            always_list: true,
            ..State::default()
        };
        assert_eq!(Layout::of(&prompt, (80, 40)), Layout { input: 0, list: 7 });
        let turn = State {
            always_list: true,
            input: Some(InputLine::default()),
            ..State::default()
        };
        assert_eq!(Layout::of(&turn, (80, 40)), Layout { input: 1, list: 7 });
        let plain = console::strip_ansi_codes(&frame(
            &State {
                reserved: 7,
                size: (80, 40),
                ..prompt
            },
            80,
            40,
        ))
        .to_string();
        assert!(plain.contains("children · none running"), "{plain}");
        // Goose's own subagents keep a dock only while there are children to list.
        assert_eq!(Layout::of(&State::default(), (80, 40)).height(), 0);
    }

    #[test]
    fn a_five_row_message_leaves_the_dock_its_header_and_a_child() {
        let text = "x".repeat(400);
        let mut state = input_state(&text, 400, 0);
        state.always_list = true;
        state.rows = (0..4)
            .map(|i| row(&i.to_string(), ChildStatus::Running))
            .collect();
        state.size = (80, 40);
        // Reserved at the turn's start for a one-row input line under seven dock rows.
        state.reserved = 8;
        let layout = shown_layout(&state, state.size);
        assert_eq!(layout, Layout { input: 5, list: 3 });
        let plain = console::strip_ansi_codes(&frame(&state, 80, 40)).to_string();
        assert!(plain.contains("children · 4 running"), "{plain}");
        assert!(plain.contains("↓ 3 more"), "{plain}");
    }

    #[tokio::test]
    async fn a_failed_load_keeps_the_children_already_shown() {
        let dock = typing();
        let rows = |dock: &Dock| dock.shared.state.lock().unwrap().rows.len();
        dock.shared.update(Some(vec![
            row("a", ChildStatus::Running),
            row("b", ChildStatus::Running),
        ]));
        assert_eq!(rows(&dock), 2);
        dock.shared.update(None);
        assert_eq!(rows(&dock), 2, "a timed-out query is not an empty list");
        dock.shared.update(Some(Vec::new()));
        assert_eq!(rows(&dock), 0);
    }
}
