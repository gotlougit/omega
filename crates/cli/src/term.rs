//! Minimal terminal prompt with block-based output — adapted from tau-cli-term-raw.
//!
//! Architecture:
//! - **Redraw thread**: owns stdout, diffs against a [`Screen`], renders on notify.
//! - **Input thread**: reads crossterm events, pushes to an mpsc channel.
//! - **SharedState**: protected by a Mutex, holds blocks, zones, input buffer, prompt.
//! - **TermHandle**: cloneable handle for pushing output blocks from any thread.
//!
//! Renders directly to the **normal terminal buffer** (no alternate screen) so
//! the terminal's native scrollback is preserved. Three rendering paths:
//!
//! - **Differential update** — common case, diffs the visible viewport via
//!   [`Screen::update`].
//! - **Scrolling render** — when content overflows the viewport, diffs the full
//!   content and renders in order via [`Screen::render_scrolling`]; `\r\n` at
//!   the bottom edge pushes rows into native scrollback.
//! - **Full render** — on resize/invalidation, clears screen + scrollback and
//!   replays all content (rubber-free), letting overflow rebuild scrollback
//!   naturally.
//!
//! Output blocks live in zones. Layout top-to-bottom: `history`, `above`
//! (scrollable log), then the bottom-anchored fixed tail: prompt, `suggestions`,
//! `below`. Temporary blank "rubber" rows are inserted between log and fixed
//! rows to absorb visible shrinkage without pulling rows back from scrollback.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::{ExecutableCommand, QueueableCommand};
use unicode_segmentation::UnicodeSegmentation;

use crate::screen::Screen;
use crate::style::{
    BlockId, Cell, Span, StyledBlock, StyledText, display_width, emit_styled_cells,
    layout_block, layout_lines,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct SharedState {
    // --- blocks ---
    blocks: HashMap<BlockId, StyledBlock>,
    next_id: u64,

    // --- zones ---
    history: Vec<BlockId>,
    above: Vec<BlockId>,
    suggestions: Vec<BlockId>,
    below: Vec<BlockId>,

    // --- prompt ---
    buffer: String,
    cursor: usize,
    left_prompt: StyledText,

    // --- status line ---
    status_line: Option<StyledBlock>,

    // --- history ---
    input_history: Vec<String>,
    history_index: Option<usize>,

    // --- kill ring ---
    kill_ring: Vec<String>,

    // --- terminal ---
    width: usize,
    height: usize,

    // --- control ---
    /// Set by Term::drop to signal the redraw thread to exit.
    shutdown: bool,
    /// Set by request_input_shutdown to ask the input thread to exit.
    input_shutdown: bool,
    /// Set to force the next redraw to take the full-render path.
    invalidate_screen: bool,
    /// Generation counters for `TermHandle::redraw_sync`. Caller bumps
    /// `sync_requested`; the redraw thread sets `sync_completed`
    /// atomically with going idle.
    sync_requested: u64,
    sync_completed: u64,
}

impl SharedState {
    fn new(width: usize, height: usize, left_prompt: StyledText) -> Self {
        Self {
            blocks: HashMap::new(),
            next_id: 0,
            history: Vec::new(),
            above: Vec::new(),
            suggestions: Vec::new(),
            below: Vec::new(),
            buffer: String::new(),
            cursor: 0,
            left_prompt,
            status_line: None,
            input_history: Vec::new(),
            history_index: None,
            kill_ring: Vec::new(),
            width,
            height,
            shutdown: false,
            input_shutdown: false,
            invalidate_screen: false,
            sync_requested: 0,
            sync_completed: 0,
        }
    }

    fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        id
    }
}

// ---------------------------------------------------------------------------
// Notify channel (simplified from tau-blocking-notify-channel)
// ---------------------------------------------------------------------------

struct NotifyChannel {
    flag: Mutex<bool>,
    condvar: Condvar,
}

impl NotifyChannel {
    fn new() -> Self {
        Self {
            flag: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    fn notify(&self) {
        let mut flag = self.flag.lock().expect("notify mutex poisoned");
        *flag = true;
        self.condvar.notify_one();
    }

    fn wait(&self) {
        let mut flag = self.flag.lock().expect("notify mutex poisoned");
        while !*flag {
            flag = self.condvar.wait(flag).expect("notify mutex poisoned");
        }
        *flag = false;
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// High-level events from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// User submitted a line.
    Line(String),
    /// EOF (Ctrl-D on empty line).
    Eof,
    /// Cancel prompt (Ctrl-C twice).
    CancelPrompt,
    /// Terminal resized.
    Resize { width: u16, height: u16 },
    /// Input buffer changed.
    BufferChanged,
    /// Escape pressed.
    Escape,
}

// ---------------------------------------------------------------------------
// Input thread → UI messages
// ---------------------------------------------------------------------------

enum InputMessage {
    Event(Event),
    Shutdown,
}

// ---------------------------------------------------------------------------
// TermHandle
// ---------------------------------------------------------------------------

/// Cloneable handle for mutating terminal output from any thread.
#[derive(Clone)]
pub struct TermHandle {
    state: Arc<Mutex<SharedState>>,
    redraw: Arc<NotifyChannel>,
    sync_condvar: Arc<Condvar>,
    input_tx: mpsc::Sender<InputMessage>,
}

impl TermHandle {
    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("term state mutex poisoned")
    }

    /// Trigger a redraw.
    pub fn redraw(&self) {
        self.redraw.notify();
    }

    /// Triggers a redraw and blocks until the redraw thread has
    /// processed it. Useful for tests and for callers that need the
    /// output to be on the terminal before continuing.
    pub fn redraw_sync(&self) {
        let target = {
            let mut st = self.lock();
            if st.shutdown {
                return;
            }
            st.sync_requested += 1;
            st.sync_requested
        };
        self.redraw.notify();
        let st = self.state.lock().expect("term state mutex poisoned");
        let _st = self
            .sync_condvar
            .wait_while(st, |s| s.sync_completed < target && !s.shutdown)
            .expect("term state mutex poisoned");
    }

    /// Force the next redraw to clear the screen and repaint from scratch.
    pub fn invalidate_screen(&self) {
        self.lock().invalidate_screen = true;
        self.redraw.notify();
    }

    /// Current terminal size.
    pub fn size(&self) -> (usize, usize) {
        let st = self.lock();
        (st.width, st.height)
    }

    // --- blocks ---

    /// Create a new block and return its id.
    pub fn new_block(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.blocks.insert(id, block.into());
        id
    }

    /// Update an existing block. Re-adds it to the history zone if it was
    /// removed by a prior `clear_output`.
    pub fn set_block(&self, id: BlockId, block: impl Into<StyledBlock>) {
        let mut st = self.lock();
        st.blocks.insert(id, block.into());
        // If the block is not in any zone (e.g. after a clear), re-add it
        // to history so it becomes visible again.
        let in_any_zone = st.history.contains(&id)
            || st.above.contains(&id)
            || st.suggestions.contains(&id)
            || st.below.contains(&id);
        if !in_any_zone {
            st.history.push(id);
        }
    }

    /// Remove a block from all zones and storage.
    pub fn remove_block(&self, id: BlockId) {
        let mut st = self.lock();
        st.blocks.remove(&id);
        st.history.retain(|&x| x != id);
        st.above.retain(|&x| x != id);
        st.suggestions.retain(|&x| x != id);
        st.below.retain(|&x| x != id);
    }

    /// Create a block, append to history, and trigger redraw.
    pub fn print_output(&self, block: impl Into<StyledBlock>) -> BlockId {
        let mut st = self.lock();
        let id = st.alloc_id();
        st.blocks.insert(id, block.into());
        st.history.push(id);
        drop(st);
        self.redraw.notify();
        id
    }

    /// Clear all output blocks and redraw.
    pub fn clear_output(&self) {
        let mut st = self.lock();
        st.blocks.clear();
        st.history.clear();
        st.above.clear();
        st.suggestions.clear();
        st.below.clear();
        st.invalidate_screen = true;
        drop(st);
        self.redraw.notify();
    }

    // --- zones ---

    pub fn push_above(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.above.contains(&id) {
            st.above.push(id);
        }
    }

    pub fn remove_above(&self, id: BlockId) {
        self.lock().above.retain(|&x| x != id);
    }

    pub fn push_suggestions(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.suggestions.contains(&id) {
            st.suggestions.push(id);
        }
    }

    pub fn remove_suggestions(&self, id: BlockId) {
        self.lock().suggestions.retain(|&x| x != id);
    }

    pub fn push_below(&self, id: BlockId) {
        let mut st = self.lock();
        if !st.below.contains(&id) {
            st.below.push(id);
        }
    }

    // --- prompt ---

    pub fn set_left_prompt(&self, text: impl Into<StyledText>) {
        self.lock().left_prompt = text.into();
    }

    // --- status line ---

    /// Set the persistent status line shown between the log and the prompt.
    /// Pass an empty block to clear.
    pub fn set_status_line(&self, block: StyledBlock) {
        self.lock().status_line = Some(block);
        self.redraw.notify();
    }

    /// Remove the status line.
    pub fn clear_status_line(&self) {
        self.lock().status_line = None;
        self.redraw.notify();
    }


    pub fn get_buffer(&self) -> String {
        self.lock().buffer.clone()
    }

    pub fn get_cursor(&self) -> usize {
        self.lock().cursor
    }

    pub fn set_buffer(&self, text: String, cursor: usize) {
        let mut st = self.lock();
        let cursor = cursor.min(text.len());
        st.buffer = text;
        st.cursor = cursor;
        drop(st);
        self.redraw.notify();
    }

    /// Request the input thread to shut down.
    pub fn request_input_shutdown(&self) {
        self.lock().input_shutdown = true;
        let _ = self.input_tx.send(InputMessage::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// RawEvent (virtual terminal input)
// ---------------------------------------------------------------------------

/// Raw input events that can be injected into a virtual terminal
/// (see [`Term::new_virtual`]).
#[derive(Debug, Clone)]
pub enum RawEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    Paste(String),
}

// ---------------------------------------------------------------------------
// Term
// ---------------------------------------------------------------------------

/// Where the input thread gets its events.
enum InputSource {
    /// Real terminal: blocking crossterm reads.
    Terminal,
    /// Virtual terminal: injected raw events (tests).
    Virtual(mpsc::Receiver<RawEvent>),
}

/// Owns the terminal — prompt input + block output.
pub struct Term {
    handle: TermHandle,
    input_rx: mpsc::Receiver<InputMessage>,
    _input_thread: JoinHandle<()>,
    redraw_thread: Option<JoinHandle<()>>,
    owns_raw_mode: bool,
}

impl Term {
    /// Create a new Term with the given prompt text.
    ///
    /// Takes ownership of stdout, enters raw mode, and spawns
    /// background input+redraw threads.
    pub fn new(left_prompt: impl Into<StyledText>) -> io::Result<(Self, TermHandle)> {
        let (w, h) = terminal::size()?;
        let width = w.max(1) as usize;
        let height = h.max(1) as usize;

        // --- enter raw mode ---
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Opt into bracketed paste so the terminal wraps pasted content in
        // `ESC[200~` / `ESC[201~` and crossterm surfaces it as one
        // `CtEvent::Paste(String)` instead of a stream of individual key
        // events (which would leak escape-sequence bytes into the buffer).
        if let Err(error) = stdout
            .execute(crossterm::event::EnableBracketedPaste)
            .and_then(|out| out.execute(crossterm::cursor::SetCursorStyle::SteadyBar))
        {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        Ok(Self::new_inner(
            width,
            height,
            left_prompt.into(),
            Box::new(io::stdout()),
            InputSource::Terminal,
            true,
        ))
    }

    /// Creates a virtual terminal for testing.
    ///
    /// No raw mode, no crossterm input reader. Output goes to `writer`
    /// instead of stdout; input is injected via the returned
    /// [`RawEvent`] sender. Dropping every sender makes the input loop
    /// emit a final [`Event::Eof`] and exit.
    pub fn new_virtual(
        width: usize,
        height: usize,
        left_prompt: impl Into<StyledText>,
        writer: impl Write + Send + 'static,
    ) -> (Self, TermHandle, mpsc::Sender<RawEvent>) {
        let (raw_tx, raw_rx) = mpsc::channel();
        let (term, handle) = Self::new_inner(
            width.max(1),
            height.max(1),
            left_prompt.into(),
            Box::new(writer),
            InputSource::Virtual(raw_rx),
            false,
        );
        (term, handle, raw_tx)
    }

    fn new_inner(
        width: usize,
        height: usize,
        left_prompt: StyledText,
        writer: impl Write + Send + 'static,
        input_source: InputSource,
        owns_raw_mode: bool,
    ) -> (Self, TermHandle) {
        let state = Arc::new(Mutex::new(SharedState::new(width, height, left_prompt)));

        let redraw_notify = Arc::new(NotifyChannel::new());
        let sync_condvar = Arc::new(Condvar::new());

        let (input_tx, input_rx) = mpsc::channel();

        let handle = TermHandle {
            state: Arc::clone(&state),
            redraw: Arc::clone(&redraw_notify),
            sync_condvar: Arc::clone(&sync_condvar),
            input_tx: input_tx.clone(),
        };

        // --- redraw thread ---
        let redraw_state = Arc::clone(&state);
        let redraw_notify2 = Arc::clone(&redraw_notify);
        let redraw_sync_cv = Arc::clone(&sync_condvar);
        let redraw_handle: JoinHandle<()> = thread::spawn(move || {
            redraw_thread(redraw_state, redraw_notify2, writer, redraw_sync_cv);
        });

        // --- input thread ---
        let input_state = Arc::clone(&state);
        let input_notify = Arc::clone(&redraw_notify);
        let input_handle: JoinHandle<()> = thread::spawn(move || {
            input_thread(input_state, input_tx, input_notify, input_source);
        });

        // Trigger initial render
        redraw_notify.notify();

        (
            Self {
                handle: handle.clone(),
                input_rx,
                _input_thread: input_handle,
                redraw_thread: Some(redraw_handle),
                owns_raw_mode,
            },
            handle,
        )
    }

    /// Returns a handle for mutating output.
    pub fn handle(&self) -> &TermHandle {
        &self.handle
    }

    /// Block until the next event (Line, Eof, Cancel, Resize, etc.).
    pub fn next_event(&mut self) -> Option<Event> {
        match self.input_rx.recv() {
            Ok(InputMessage::Event(event)) => Some(event),
            Ok(InputMessage::Shutdown) => None,
            Err(_) => None,
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Signal threads to stop. Set the flag first, then notify — the
        // redraw thread checks the flag right after waking.
        {
            let mut st = self.handle.lock();
            st.shutdown = true;
            st.input_shutdown = true;
        }
        self.handle.redraw.notify();
        let _ = self.handle.input_tx.send(InputMessage::Shutdown);

        // Block until the redraw thread's final render completes so raw
        // mode isn't disabled mid-frame.
        if let Some(handle) = self.redraw_thread.take() {
            let _ = handle.join();
        }

        if self.owns_raw_mode {
            // Restore terminal: pair the modes set in `new` and return the
            // cursor shape to the user's default.
            let _ = terminal::disable_raw_mode();
            let mut stdout = io::stdout();
            let _ = stdout.execute(crossterm::event::DisableBracketedPaste);
            let _ = stdout.execute(crossterm::cursor::SetCursorStyle::DefaultUserShape);
        }
    }
}

// ---------------------------------------------------------------------------
// Input thread
// ---------------------------------------------------------------------------

fn input_thread(
    state: Arc<Mutex<SharedState>>,
    tx: mpsc::Sender<InputMessage>,
    redraw: Arc<NotifyChannel>,
    source: InputSource,
) {
    match source {
        InputSource::Terminal => real_input_loop(&state, &tx, &redraw),
        InputSource::Virtual(rx) => virtual_input_loop(&state, &tx, &redraw, rx),
    }
}

fn real_input_loop(
    state: &Arc<Mutex<SharedState>>,
    tx: &mpsc::Sender<InputMessage>,
    redraw: &Arc<NotifyChannel>,
) {
    loop {
        // Check shutdown
        {
            let st = state.lock().expect("mutex");
            if st.input_shutdown {
                return;
            }
        }

        match event::read() {
            Ok(ev) => dispatch_input_event(state, ev, tx, redraw),
            Err(_) => {
                // Input error, maybe terminal closed
                return;
            }
        }
    }
}

fn virtual_input_loop(
    state: &Arc<Mutex<SharedState>>,
    tx: &mpsc::Sender<InputMessage>,
    redraw: &Arc<NotifyChannel>,
    rx: mpsc::Receiver<RawEvent>,
) {
    loop {
        // Poll with a timeout so shutdown is noticed even when no
        // further events are injected.
        let raw = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(raw) => raw,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let st = state.lock().expect("mutex");
                if st.input_shutdown {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // All injectors dropped: emit a final EOF and exit.
                let _ = tx.send(InputMessage::Event(Event::Eof));
                return;
            }
        };
        {
            let st = state.lock().expect("mutex");
            if st.input_shutdown {
                return;
            }
        }
        let ev = match raw {
            RawEvent::Key(key) => CtEvent::Key(key),
            RawEvent::Resize(w, h) => CtEvent::Resize(w, h),
            RawEvent::Paste(text) => CtEvent::Paste(text),
        };
        dispatch_input_event(state, ev, tx, redraw);
    }
}

fn dispatch_input_event(
    state: &Arc<Mutex<SharedState>>,
    ev: CtEvent,
    tx: &mpsc::Sender<InputMessage>,
    redraw: &Arc<NotifyChannel>,
) {
    match ev {
        CtEvent::Key(key) if key.kind != KeyEventKind::Release => {
            let mut st = state.lock().expect("mutex");
            handle_key_locked(&mut st, key, tx);
            drop(st);
            redraw.notify();
        }
        CtEvent::Resize(w, h) => {
            let mut st = state.lock().expect("mutex");
            st.width = w.max(1) as usize;
            st.height = h.max(1) as usize;
            st.invalidate_screen = true;
            drop(st);
            // Wake the redraw thread — without this the UI stays stale
            // (wrapped at the old width) until the next keypress/output.
            redraw.notify();
            let _ = tx.send(InputMessage::Event(Event::Resize {
                width: w,
                height: h,
            }));
        }
        CtEvent::Paste(data) => {
            let mut st = state.lock().expect("mutex");
            for c in data.chars() {
                if c == '\n' || c == '\r' {
                    continue;
                }
                let pos = st.cursor;
                st.buffer.insert(pos, c);
                st.cursor = pos + c.len_utf8();
            }
            drop(st);
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            redraw.notify();
        }
        _ => {}
    }
}

fn handle_key_locked(st: &mut SharedState, key: KeyEvent, tx: &mpsc::Sender<InputMessage>) {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => match c {
            'a' => {
                st.cursor = 0;
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
            'c' => {
                if st.buffer.is_empty() {
                    let _ = tx.send(InputMessage::Event(Event::CancelPrompt));
                } else {
                    st.buffer.clear();
                    st.cursor = 0;
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'd' => {
                if st.buffer.is_empty() {
                    let _ = tx.send(InputMessage::Event(Event::Eof));
                } else {
                    let cur = st.cursor;
                    if cur < st.buffer.len() {
                        let next = next_char_boundary(&st.buffer, cur);
                        let killed = st.buffer[cur..next].to_string();
                        st.buffer.drain(cur..next);
                        st.kill_ring.push(killed);
                        let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                    }
                }
            }
            'e' => {
                let end = st.buffer.len();
                st.cursor = end;
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
            'l' => {
                st.blocks.clear();
                st.history.clear();
                st.above.clear();
                st.suggestions.clear();
                st.below.clear();
                st.invalidate_screen = true;
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
            'u' => {
                let cur = st.cursor;
                st.buffer.drain(..cur);
                st.cursor = 0;
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
            'h' => {
                let cur = st.cursor;
                if cur > 0 {
                    let prev = prev_char_boundary(&st.buffer, cur);
                    st.buffer.drain(prev..cur);
                    st.cursor = prev;
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'k' => {
                let cur = st.cursor;
                if cur < st.buffer.len() {
                    let killed = st.buffer[cur..].to_string();
                    st.buffer.truncate(cur);
                    st.kill_ring.push(killed);
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'n' => {
                if st.history_index.is_some() {
                    let idx = st.history_index.unwrap() + 1;
                    if idx >= st.input_history.len() {
                        st.buffer.clear();
                        st.cursor = 0;
                        st.history_index = None;
                    } else {
                        st.buffer = st.input_history[idx].clone();
                        st.cursor = st.buffer.len();
                        st.history_index = Some(idx);
                    }
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'p' => {
                if !st.input_history.is_empty() {
                    let idx = match st.history_index {
                        None => st.input_history.len() - 1,
                        Some(0) => 0,
                        Some(i) => i - 1,
                    };
                    st.buffer = st.input_history[idx].clone();
                    st.cursor = st.buffer.len();
                    st.history_index = Some(idx);
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            't' => {
                let cur = st.cursor;
                let len = st.buffer.len();
                if len >= 2 {
                    if cur >= len {
                        // Transpose last two chars
                        let chars: Vec<(usize, char)> = st.buffer.char_indices().collect();
                        let last = chars.len() - 1;
                        let second_last = chars.len() - 2;
                        let mut buf = String::new();
                        for (i, (_, ch)) in chars.iter().enumerate() {
                            if i == second_last {
                                buf.push(chars[last].1);
                            } else if i == last {
                                buf.push(chars[second_last].1);
                            } else {
                                buf.push(*ch);
                            }
                        }
                        st.buffer = buf;
                        st.cursor = len;
                        let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                    } else if cur >= 1 {
                        // Transpose chars before cursor
                        let chars: Vec<(usize, char)> = st.buffer.char_indices().collect();
                        let pos = chars.iter().position(|&(i, _)| i == cur)
                            .unwrap_or(chars.len());
                        if pos >= 2 {
                            let a = pos - 2;
                            let b = pos - 1;
                            let mut buf = String::new();
                            for (i, (_, ch)) in chars.iter().enumerate() {
                                if i == a {
                                    buf.push(chars[b].1);
                                } else if i == b {
                                    buf.push(chars[a].1);
                                } else {
                                    buf.push(*ch);
                                }
                            }
                            st.buffer = buf;
                            st.cursor = cur;
                            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                        }
                    }
                }
            }
            'w' => {
                let cur = st.cursor;
                if cur > 0 {
                    // Find start of the word before the cursor.
                    let before = &st.buffer[..cur];
                    // Skip trailing whitespace.
                    let trimmed_end = before.trim_end_matches(|c: char| c.is_ascii_whitespace());
                    // Find the last whitespace before that (word boundary).
                    let delete_start =
                        if let Some(last_space) = trimmed_end.rfind(|c: char| c.is_ascii_whitespace()) {
                            last_space + 1
                        } else {
                            0
                        };
                    let killed = st.buffer[delete_start..cur].to_string();
                    st.buffer.drain(delete_start..cur);
                    st.cursor = delete_start;
                    st.kill_ring.push(killed);
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'y' => {
                if let Some(killed) = st.kill_ring.last() {
                    let cur = st.cursor;
                    let mut new_buf = st.buffer[..cur].to_string();
                    new_buf.push_str(killed);
                    new_buf.push_str(&st.buffer[cur..]);
                    st.buffer = new_buf;
                    st.cursor = cur + killed.len();
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            _ => {}
        },
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => match c {
            'b' => {
                // Move backward one word
                let cur = st.cursor;
                if cur > 0 {
                    let before = &st.buffer[..cur];
                    // Skip trailing whitespace before cursor
                    let trimmed = before.trim_end_matches(|c: char| c.is_ascii_whitespace());
                    if let Some(prev_space) = trimmed.rfind(|c: char| c.is_ascii_whitespace()) {
                        let word_start = prev_space + 1;
                        st.cursor = word_start;
                    } else {
                        st.cursor = 0;
                    }
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'd' => {
                // Delete word forward
                let cur = st.cursor;
                let after = &st.buffer[cur..];
                // Skip leading whitespace
                let trimmed_start = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
                let skipped = after.len() - trimmed_start.len();
                if let Some(next_space) = trimmed_start.find(|c: char| c.is_ascii_whitespace()) {
                    let end = cur + skipped + next_space;
                    let killed = st.buffer[cur..end].to_string();
                    st.buffer.drain(cur..end);
                    st.kill_ring.push(killed);
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                } else if !trimmed_start.is_empty() {
                    // Delete from cursor to end
                    let killed = st.buffer[cur..].to_string();
                    st.buffer.truncate(cur);
                    st.kill_ring.push(killed);
                    let _ = tx.send(InputMessage::Event(Event::BufferChanged));
                }
            }
            'f' => {
                // Move forward to start of next word
                let cur = st.cursor;
                let after = &st.buffer[cur..];
                // Skip leading whitespace first
                let trimmed_start = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
                let skipped = after.len() - trimmed_start.len();
                // Now find the end of the next word (next whitespace after it)
                if let Some(next_space) = trimmed_start.find(|c: char| c.is_ascii_whitespace()) {
                    // Skip the word AND the following whitespace to land at start of next word
                    let word_end = cur + skipped + next_space;
                    let rest = &st.buffer[word_end..];
                    let after_word_ws = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
                    let ws_skipped = rest.len() - after_word_ws.len();
                    if ws_skipped > 0 || after_word_ws.is_empty() {
                        st.cursor = word_end + ws_skipped;
                    } else {
                        st.cursor = word_end;
                    }
                } else if !trimmed_start.is_empty() {
                    if skipped > 0 {
                        // We were on whitespace before a word — land at the word start
                        st.cursor = cur + skipped;
                    } else {
                        // No whitespace: we're on the last word, go to end
                        st.cursor = st.buffer.len();
                    }
                } else {
                    st.cursor = st.buffer.len();
                }
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
            _ => {}
        }
        KeyCode::Char(c) => {
            let pos = st.cursor;
            st.buffer.insert(pos, c);
            st.cursor = pos + c.len_utf8();
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut st.buffer);
            st.cursor = 0;
            // Save to input history.
            if !line.is_empty() {
                st.input_history.push(line.clone());
                st.history_index = None;
            }
            let _ = tx.send(InputMessage::Event(Event::Line(line)));
        }
        KeyCode::Esc => {
            let _ = tx.send(InputMessage::Event(Event::Escape));
        }
        KeyCode::Up => {
            if !st.input_history.is_empty() {
                let idx = match st.history_index {
                    None => st.input_history.len() - 1,
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                st.buffer = st.input_history[idx].clone();
                st.cursor = st.buffer.len();
                st.history_index = Some(idx);
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Down => {
            if st.history_index.is_some() {
                let idx = st.history_index.unwrap() + 1;
                if idx >= st.input_history.len() {
                    st.buffer.clear();
                    st.cursor = 0;
                    st.history_index = None;
                } else {
                    st.buffer = st.input_history[idx].clone();
                    st.cursor = st.buffer.len();
                    st.history_index = Some(idx);
                }
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+Left: move backward one word
            let cur = st.cursor;
            if cur > 0 {
                let before = &st.buffer[..cur];
                let trimmed = before.trim_end_matches(|c: char| c.is_ascii_whitespace());
                if let Some(prev_space) = trimmed.rfind(|c: char| c.is_ascii_whitespace()) {
                    st.cursor = prev_space + 1;
                } else {
                    st.cursor = 0;
                }
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Left => {
            let cur = st.cursor;
            if cur > 0 {
                st.cursor = prev_char_boundary(&st.buffer, cur);
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+Right: move forward to start of next word
            let cur = st.cursor;
            let after = &st.buffer[cur..];
            // Skip leading whitespace first
            let trimmed_start = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
            let skipped = after.len() - trimmed_start.len();
            // Now find the end of the next word (next whitespace after it)
            if let Some(next_space) = trimmed_start.find(|c: char| c.is_ascii_whitespace()) {
                // Skip the word AND the following whitespace to land at start of next word
                let word_end = cur + skipped + next_space;
                let rest = &st.buffer[word_end..];
                let after_word_ws = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
                let ws_skipped = rest.len() - after_word_ws.len();
                if ws_skipped > 0 || after_word_ws.is_empty() {
                    st.cursor = word_end + ws_skipped;
                } else {
                    st.cursor = word_end;
                }
            } else if !trimmed_start.is_empty() {
                if skipped > 0 {
                    // We were on whitespace before a word — land at the word start
                    st.cursor = cur + skipped;
                } else {
                    // No whitespace: we're on the last word, go to end
                    st.cursor = st.buffer.len();
                }
            } else {
                st.cursor = st.buffer.len();
            }
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
        }
        KeyCode::Right => {
            let cur = st.cursor;
            if cur < st.buffer.len() {
                st.cursor = next_char_boundary(&st.buffer, cur);
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
            // Alt+Backspace: delete previous word
            let cur = st.cursor;
            if cur > 0 {
                let before = &st.buffer[..cur];
                let trimmed = before.trim_end_matches(|c: char| c.is_ascii_whitespace());
                let delete_start = if let Some(prev_space) = trimmed.rfind(|c: char| c.is_ascii_whitespace()) {
                    prev_space + 1
                } else {
                    0
                };
                let killed = st.buffer[delete_start..cur].to_string();
                st.buffer.drain(delete_start..cur);
                st.cursor = delete_start;
                st.kill_ring.push(killed);
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Backspace => {
            let cur = st.cursor;
            if cur > 0 {
                let prev = prev_char_boundary(&st.buffer, cur);
                st.buffer.drain(prev..cur);
                st.cursor = prev;
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Delete => {
            let cur = st.cursor;
            if cur < st.buffer.len() {
                let next = next_char_boundary(&st.buffer, cur);
                st.buffer.drain(cur..next);
                let _ = tx.send(InputMessage::Event(Event::BufferChanged));
            }
        }
        KeyCode::Tab => {
            // Insert a literal tab character so the run_loop's
            // BufferChanged handler can detect it and trigger completion.
            let pos = st.cursor;
            st.buffer.insert(pos, '\t');
            st.cursor = pos + 1;
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
        }
        KeyCode::Home => {
            st.cursor = 0;
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
        }
        KeyCode::End => {
            let end = st.buffer.len();
            st.cursor = end;
            let _ = tx.send(InputMessage::Event(Event::BufferChanged));
        }
        _ => {}
    }
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    s.char_indices()
        .rev()
        .find(|(i, _)| *i < pos)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    s.char_indices()
        .find(|(i, _)| *i > pos)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

// ---------------------------------------------------------------------------
// Redraw thread
// ---------------------------------------------------------------------------

/// Renderer-side model of what the terminal is believed to display.
///
/// `viewport_start` is the top row of the physical terminal viewport within
/// the most recent planned render lines. Rows before it live in terminal
/// scrollback and cannot be repainted incrementally. `rubber` is temporary
/// blank space inserted between log and fixed rows to absorb visible
/// shrinkage without pulling rows back from scrollback.
#[derive(Default)]
struct TermModel {
    viewport_start: usize,
    rubber: usize,
    /// Log rows (before `log_end`) from the last rendered frame, used to
    /// detect mutations to content that may already be in scrollback.
    known_lines: Vec<Vec<Cell>>,
}

/// A fully laid-out frame: all content rows plus the cursor position.
struct FrameLayout {
    /// All rendered lines without rubber (log + fixed area).
    all_lines: Vec<Vec<Cell>>,
    /// Index in `all_lines` where the fixed area (prompt/suggestions/below)
    /// starts. Lines before this are scrollable log content.
    log_end: usize,
    /// Absolute cursor row in `all_lines`.
    cursor_row: usize,
    /// Cursor column.
    cursor_col: usize,
}

struct PlanMetrics {
    viewport_start: usize,
    rubber_height: usize,
    render_len: usize,
    cursor_row: usize,
}

/// Everything the redraw thread needs from shared state for one frame.
struct Snapshot {
    shutdown: bool,
    force_full: bool,
    sync_gen: u64,
    width: usize,
    height: usize,
    history: Vec<StyledBlock>,
    above: Vec<StyledBlock>,
    status_line: Option<StyledBlock>,
    suggestions: Vec<StyledBlock>,
    below: Vec<StyledBlock>,
    left_prompt: StyledText,
    buffer: String,
    cursor: usize,
}

fn take_snapshot(st: &mut SharedState) -> Snapshot {
    let grab = |ids: &[BlockId], blocks: &HashMap<BlockId, StyledBlock>| {
        ids.iter().filter_map(|id| blocks.get(id).cloned()).collect()
    };
    Snapshot {
        shutdown: st.shutdown,
        force_full: std::mem::take(&mut st.invalidate_screen),
        sync_gen: st.sync_requested,
        width: st.width,
        height: st.height.max(1),
        history: grab(&st.history, &st.blocks),
        above: grab(&st.above, &st.blocks),
        status_line: st.status_line.clone(),
        suggestions: grab(&st.suggestions, &st.blocks),
        below: grab(&st.below, &st.blocks),
        left_prompt: st.left_prompt.clone(),
        buffer: st.buffer.clone(),
        cursor: st.cursor,
    }
}

/// Lays out all zones into physical rows: log (history + above) first, then
/// the bottom-anchored fixed tail (prompt + suggestions + below).
fn layout_frame(snap: &Snapshot) -> FrameLayout {
    let width = snap.width;
    let mut all_lines: Vec<Vec<Cell>> = Vec::new();

    // --- Log area (scrollable) ---
    for block in snap.history.iter().chain(snap.above.iter()) {
        // Skip empty blocks so callers can "hide" a block by clearing its
        // content without leaving a blank row.
        if block.content.is_empty() {
            continue;
        }
        all_lines.extend(layout_block(block, width));
    }
    let log_end = all_lines.len();

    // --- Status line (between log and prompt, fixed) ---
    let status_h = if let Some(ref status) = snap.status_line {
        if !status.content.is_empty() {
            let lines = layout_block(status, width);
            let h = lines.len();
            all_lines.extend(lines);
            h
        } else {
            0
        }
    } else {
        0
    };

    // --- Prompt ---
    let prompt_text = {
        let mut t = StyledText::new();
        for span in snap.left_prompt.spans() {
            t.push(span.clone());
        }
        if snap.buffer.is_empty() {
            t.push(Span::plain(" "));
        } else {
            t.push(Span::plain(&snap.buffer));
        }
        t
    };
    let prompt_lines = layout_lines(&prompt_text, width, true);
    let (cursor_row_in_input, cursor_col) = buffer_position_for_byte(
        &snap.buffer,
        snap.cursor,
        width,
        snap.left_prompt.char_count(),
    );
    let cursor_row = log_end + status_h + cursor_row_in_input;
    all_lines.extend(prompt_lines);

    // --- Rest of the fixed tail ---
    for block in snap.suggestions.iter().chain(snap.below.iter()) {
        if block.content.is_empty() {
            continue;
        }
        all_lines.extend(layout_block(block, width));
    }

    FrameLayout {
        all_lines,
        log_end,
        cursor_row,
        cursor_col,
    }
}

fn redraw_thread(
    state: Arc<Mutex<SharedState>>,
    notify: Arc<NotifyChannel>,
    writer: impl Write + Send,
    sync_condvar: Arc<Condvar>,
) {
    let mut stdout = BufWriter::new(writer);
    let (w0, h0) = {
        let st = state.lock().expect("term state mutex poisoned");
        (st.width, st.height.max(1))
    };
    let mut screen = Screen::new(w0);
    let mut prev_width = w0;
    let mut prev_height = h0;
    let mut model = TermModel::default();

    loop {
        notify.wait();

        let snap = {
            let mut st = state.lock().expect("term state mutex poisoned");
            take_snapshot(&mut st)
        };

        if snap.shutdown {
            render_shutdown(&mut stdout, &mut screen, &model, &snap);
            let _ = stdout.flush();
            complete_redraw_sync(&state, &snap, &sync_condvar);
            return;
        }

        let width = snap.width;
        let height = snap.height;
        let size_changed = prev_width != width || prev_height != height;
        let layout = layout_frame(&snap);
        let log_end = layout.log_end;
        let fixed_height = layout.all_lines.len() - log_end;

        if size_changed || snap.force_full {
            // Full render: clear screen + scrollback and replay from scratch.
            if let Ok(viewport_start) =
                full_render(&mut stdout, &mut screen, &layout, width, height)
            {
                model.viewport_start = viewport_start;
                model.rubber = 0;
                model.known_lines = layout.all_lines[..log_end].to_vec();
            }
        } else {
            let metrics = plan_metrics(&model, log_end, fixed_height, layout.cursor_row, height);
            let mut render_lines = build_render_lines(&layout, metrics.rubber_height);
            render_lines.truncate(metrics.render_len);
            let desired_cursor = (metrics.cursor_row, layout.cursor_col);

            if metrics.viewport_start < model.viewport_start
                || hidden_lines_changed(
                    &model.known_lines,
                    &layout.all_lines[..log_end],
                    model.viewport_start.min(log_end),
                )
            {
                // The viewport moved up (rows would have to be pulled back
                // from scrollback) or content that may already be in
                // scrollback changed — repaint from scratch, dropping rubber.
                if let Ok(viewport_start) =
                    full_render(&mut stdout, &mut screen, &layout, width, height)
                {
                    model.viewport_start = viewport_start;
                    model.rubber = 0;
                    model.known_lines = layout.all_lines[..log_end].to_vec();
                }
            } else if model.viewport_start < metrics.viewport_start {
                // Content pushed log rows off the top: scrolling render so
                // overflow reaches native scrollback.
                screen.set_width(width);
                let _ = screen.render_scrolling(
                    &mut stdout,
                    &render_lines,
                    model.viewport_start,
                    height,
                    desired_cursor,
                );
                model.viewport_start = metrics.viewport_start;
                model.rubber = metrics.rubber_height;
                model.known_lines = layout.all_lines[..log_end].to_vec();
            } else {
                // Common case: differential update of the visible viewport.
                screen.set_width(width);
                let visible_start = metrics.viewport_start.min(render_lines.len());
                let visible_end = (visible_start + height).min(render_lines.len());
                let cursor_in_visible = metrics.cursor_row.saturating_sub(visible_start);
                let _ = screen.update(
                    &mut stdout,
                    &render_lines[visible_start..visible_end],
                    (cursor_in_visible, desired_cursor.1),
                );
                model.viewport_start = metrics.viewport_start;
                model.rubber = metrics.rubber_height;
                model.known_lines = layout.all_lines[..log_end].to_vec();
            }
        }

        if let Err(e) = stdout.flush() {
            tracing::error!(target: "tui::term::redraw", error = %e, "render flush error");
        }
        complete_redraw_sync(&state, &snap, &sync_condvar);
        prev_width = width;
        prev_height = height;
    }
}

/// Advances the sync generation so `redraw_sync` callers unblock once the
/// frame they requested has been flushed.
fn complete_redraw_sync(
    state: &Arc<Mutex<SharedState>>,
    snap: &Snapshot,
    sync_condvar: &Arc<Condvar>,
) {
    let mut st = state.lock().expect("term state mutex poisoned");
    st.sync_completed = st.sync_completed.max(snap.sync_gen);
    drop(st);
    sync_condvar.notify_all();
}

/// Final render on shutdown: repaint the current frame, then move the cursor
/// below all content so the shell prompt lands below the UI.
fn render_shutdown(
    stdout: &mut BufWriter<impl Write>,
    screen: &mut Screen,
    model: &TermModel,
    snap: &Snapshot,
) {
    let layout = layout_frame(snap);
    let height = snap.height;
    let fixed_height = layout.all_lines.len() - layout.log_end;
    let metrics = plan_metrics(model, layout.log_end, fixed_height, layout.cursor_row, height);
    let mut render_lines = build_render_lines(&layout, metrics.rubber_height);
    render_lines.truncate(metrics.render_len);

    screen.set_width(snap.width);
    let visible_start = metrics.viewport_start.min(render_lines.len());
    let visible_end = (visible_start + height).min(render_lines.len());
    let cursor_in_visible = metrics.cursor_row.saturating_sub(visible_start);
    let _ = screen.update(
        stdout,
        &render_lines[visible_start..visible_end],
        (cursor_in_visible, layout.cursor_col),
    );
    let below = render_lines.len().saturating_sub(metrics.cursor_row + 1);
    for _ in 0..=below {
        let _ = stdout.queue(Print("\r\n"));
    }
}

/// Computes viewport/rubber for the next frame from the previous model.
///
/// Rubber grows to keep the fixed tail pinned once the viewport has
/// overflowed (before that, the prompt simply follows the transcript), and
/// shrinks first when content overflows. The final viewport start is
/// bottom-anchored, nudged upward only to keep the cursor visible.
fn plan_metrics(
    model: &TermModel,
    log_height: usize,
    fixed_height: usize,
    cursor_row: usize,
    height: usize,
) -> PlanMetrics {
    let height = height.max(1);
    let viewport_start = model.viewport_start.min(log_height);
    let mut rubber_height = model.rubber;

    if fixed_height < height {
        let occupied = log_height.saturating_sub(viewport_start) + rubber_height + fixed_height;
        if occupied < height {
            // Only create rubber after the viewport has overflowed once.
            // Before that, keep the normal terminal behavior where the
            // prompt follows the transcript instead of being bottom-pinned.
            if 0 < model.viewport_start || 0 < rubber_height {
                rubber_height += height - occupied;
            }
        } else if height < occupied {
            let overflow = occupied - height;
            let consume_rubber = rubber_height.min(overflow);
            rubber_height -= consume_rubber;
        }
    } else {
        rubber_height = 0;
    }

    let render_len = log_height + rubber_height + fixed_height;
    let cursor_row = if log_height <= cursor_row {
        cursor_row + rubber_height
    } else {
        cursor_row
    };
    let bottom_start = render_len.saturating_sub(height);
    let visible_start = viewport_start_with_cursor(bottom_start, cursor_row, render_len, height);
    let render_len = if visible_start < bottom_start {
        (visible_start + height).min(render_len)
    } else {
        render_len
    };

    PlanMetrics {
        viewport_start: render_len.saturating_sub(height),
        rubber_height,
        render_len,
        cursor_row,
    }
}

/// Builds the physical render array: log rows, then rubber blanks, then the
/// fixed tail.
fn build_render_lines(layout: &FrameLayout, rubber_height: usize) -> Vec<Vec<Cell>> {
    let mut render_lines = Vec::with_capacity(layout.all_lines.len() + rubber_height);
    render_lines.extend_from_slice(&layout.all_lines[..layout.log_end]);
    render_lines.extend(std::iter::repeat_with(Vec::new).take(rubber_height));
    render_lines.extend_from_slice(&layout.all_lines[layout.log_end..]);
    render_lines
}

/// Clamps a viewport start so `cursor_row` stays inside the visible window.
fn viewport_start_with_cursor(
    viewport_start: usize,
    cursor_row: usize,
    total_rows: usize,
    height: usize,
) -> usize {
    let height = height.max(1);
    let max_start = total_rows.saturating_sub(height);
    let mut start = viewport_start.min(max_start);

    if cursor_row < start {
        start = cursor_row;
    } else if start + height <= cursor_row {
        start = (cursor_row + 1).saturating_sub(height);
    }

    start.min(max_start)
}

/// Returns true if any row before `hidden_rows` differs between the previous
/// and current log content — such rows may live in terminal scrollback and
/// cannot be patched incrementally.
fn hidden_lines_changed(
    prev_known: &[Vec<Cell>],
    new_log: &[Vec<Cell>],
    hidden_rows: usize,
) -> bool {
    (0..hidden_rows).any(|idx| prev_known.get(idx) != new_log.get(idx))
}

/// Full re-render: clear screen + scrollback, replay all content (without
/// rubber), and position the cursor. Used on resize and after invalidation.
/// Overflow rebuilds recent terminal scrollback naturally. Returns the
/// effective viewport start for the renderer's model.
fn full_render(
    w: &mut impl Write,
    screen: &mut Screen,
    layout: &FrameLayout,
    width: usize,
    height: usize,
) -> io::Result<usize> {
    screen.set_width(width);
    let height = height.max(1);

    // Bottom-anchored plan without rubber, nudged up to keep the cursor
    // visible; rows above the (possibly raised) viewport are not replayed.
    let bottom_start = layout.all_lines.len().saturating_sub(height);
    let viewport_start = viewport_start_with_cursor(
        bottom_start,
        layout.cursor_row,
        layout.all_lines.len(),
        height,
    );
    let render_lines = if viewport_start < bottom_start {
        &layout.all_lines[..(viewport_start + height).min(layout.all_lines.len())]
    } else {
        &layout.all_lines[..]
    };

    w.queue(terminal::BeginSynchronizedUpdate)?;
    // Clear screen, home cursor, and clear scrollback. Disable autowrap
    // while replaying so exact-width rows don't create phantom blank rows
    // before the explicit CRLF between logical rows.
    w.queue(Print("\x1b[2J\x1b[H\x1b[3J\x1b[?7l"))?;
    for (i, line) in render_lines.iter().enumerate() {
        if i > 0 {
            w.queue(Print("\r\n"))?;
        }
        emit_styled_cells(w, line)?;
    }
    w.queue(Print("\x1b[?7h"))?;

    // After replay, the cursor is on the last content row: the terminal's
    // bottom row when content overflowed, otherwise its natural row below
    // the transcript.
    let replay_total = render_lines.len();
    let current_screen_row = if height <= replay_total {
        height - 1
    } else {
        replay_total.saturating_sub(1)
    };
    let cursor_screen_row = layout.cursor_row.saturating_sub(viewport_start);
    let up = current_screen_row.saturating_sub(cursor_screen_row);
    if up > 0 {
        w.queue(MoveUp(up as u16))?;
    }
    w.queue(MoveToColumn(layout.cursor_col as u16))?;
    w.queue(terminal::EndSynchronizedUpdate)?;

    // Track what's visible so the next screen.update() can diff correctly.
    let visible_end = (viewport_start + height).min(render_lines.len());
    screen.reset_to(
        render_lines[viewport_start..visible_end].to_vec(),
        cursor_screen_row,
        layout.cursor_col,
    );

    Ok(viewport_start)
}

// ---------------------------------------------------------------------------
// Prompt cursor math
// ---------------------------------------------------------------------------

fn is_prompt_line_break(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r")
}

fn initial_buffer_position(initial_cols: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    (initial_cols / width, initial_cols % width)
}

/// Visual `(row, col)` of the cursor at `byte_pos` within `s`, given the
/// prompt occupies `initial_cols` columns before the buffer starts. Handles
/// wrapping at `width`, including exact-width wraps.
fn buffer_position_for_byte(
    s: &str,
    byte_pos: usize,
    width: usize,
    initial_cols: usize,
) -> (usize, usize) {
    let width = width.max(1);
    let mut pos = initial_buffer_position(initial_cols, width);
    let mut pending_exact_wrap = false;

    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(s, true) {
        if byte_pos <= byte || byte_pos < byte + grapheme.len() {
            break;
        }
        advance_prompt_cursor_position(
            &mut pos.0,
            &mut pos.1,
            &mut pending_exact_wrap,
            grapheme,
            width,
        );
    }

    pos
}

fn advance_prompt_cursor_position(
    row: &mut usize,
    col: &mut usize,
    pending_exact_wrap: &mut bool,
    grapheme: &str,
    width: usize,
) {
    let width = width.max(1);
    if is_prompt_line_break(grapheme) {
        if *pending_exact_wrap {
            // A printable character exactly filled the previous visual row, so
            // the cursor is already at column 0 of this row. An explicit
            // newline here consumes that pending wrap, not a second blank row.
            *pending_exact_wrap = false;
        } else {
            *row += 1;
            *col = 0;
        }
        return;
    }

    *pending_exact_wrap = false;
    let grapheme_width = display_width(grapheme);
    if 0 < *col && width < *col + grapheme_width {
        *row += 1;
        *col = 0;
    }
    *col += grapheme_width;
    if width <= *col {
        *row += *col / width;
        *col %= width;
        *pending_exact_wrap = grapheme_width != 0 && *col == 0;
    }
}

#[cfg(test)]
mod tests;
