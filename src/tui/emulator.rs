//! Minimal terminal emulator for tests.
//!
//! Interprets exactly the escape sequences the [`crate::tui`] renderer
//! emits and tracks the visible screen plus scrollback history, so tests
//! can assert on final screen state instead of raw byte streams.
//!
//! Supported sequences: printable text (incl. wide chars and pending
//! wrap), `CR`/`LF`, `CSI n A` (up), `CSI n G` (column), `CSI H` (home),
//! `CSI n J` (erase below / screen / scrollback), `CSI K` (erase to EOL),
//! `CSI ? 7 h/l` (autowrap). Style (`m`), sync-update (`?2026`),
//! bracketed-paste (`?2004`) and cursor-shape (`q`) sequences are parsed
//! and ignored.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use unicode_width::UnicodeWidthChar;

/// A [`Write`] sink that appends to a shared buffer — feed to
/// [`crate::tui::Term::new_virtual`] and inspect afterwards.
#[derive(Clone)]
pub struct Capture {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Capture {
    pub fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Self { buf: Arc::clone(&buf) }, buf)
    }
}

impl Write for Capture {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.lock().expect("capture poisoned").extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What the emulator believes a real terminal would display.
pub struct Emulator {
    rows: usize,
    cols: usize,
    /// Visible rows; `\0` marks the continuation cell of a wide char.
    screen: Vec<Vec<char>>,
    /// Rows that scrolled off the top (terminal scrollback).
    history: Vec<String>,
    row: usize,
    col: usize,
    pending_wrap: bool,
    autowrap: bool,
}

enum State {
    Normal,
    Esc,
    Csi(String),
}

impl Emulator {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows: rows.max(1),
            cols: cols.max(1),
            screen: vec![vec![' '; cols.max(1)]; rows.max(1)],
            history: Vec::new(),
            row: 0,
            col: 0,
            pending_wrap: false,
            autowrap: true,
        }
    }

    /// Feeds captured bytes (from a [`Capture`] buffer) into the emulator.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        self.feed(&text);
    }

    /// Convenience: builds an emulator and feeds the whole capture buffer.
    pub fn from_capture(rows: usize, cols: usize, buf: &Arc<Mutex<Vec<u8>>>) -> Self {
        let mut em = Self::new(rows, cols);
        let data = buf.lock().expect("capture poisoned").clone();
        em.feed_bytes(&data);
        em
    }

    pub fn feed(&mut self, text: &str) {
        let mut state = State::Normal;
        for ch in text.chars() {
            state = match state {
                State::Normal => match ch {
                    '\x1b' => State::Esc,
                    '\r' => {
                        self.col = 0;
                        self.pending_wrap = false;
                        State::Normal
                    }
                    '\n' => {
                        self.linefeed();
                        State::Normal
                    }
                    c if c.is_control() => State::Normal,
                    c => {
                        self.put_char(c);
                        State::Normal
                    }
                },
                State::Esc => match ch {
                    '[' => State::Csi(String::new()),
                    _ => State::Normal,
                },
                State::Csi(mut params) => {
                    if ch.is_ascii_digit() || ch == ';' || ch == '?' {
                        params.push(ch);
                        State::Csi(params)
                    } else {
                        self.handle_csi(&params, ch);
                        State::Normal
                    }
                }
            };
        }
    }

    /// Visible screen rows, trailing whitespace trimmed.
    pub fn screen_lines(&self) -> Vec<String> {
        self.screen
            .iter()
            .map(|row| Self::row_string(row).trim_end().to_string())
            .collect()
    }

    /// Scrollback rows (oldest first).
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Cursor position as `(row, col)` within the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    // --- internals -------------------------------------------------------

    fn row_string(row: &[char]) -> String {
        row.iter().filter(|&&c| c != '\0').collect()
    }

    fn handle_csi(&mut self, params: &str, final_byte: char) {
        let private = params.starts_with('?');
        let bare = params.trim_start_matches('?');
        let first: usize = bare
            .split(';')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        match final_byte {
            'A' => {
                // Cursor up (default 1).
                self.row = self.row.saturating_sub(first.max(1));
                self.pending_wrap = false;
            }
            'G' => {
                // CHA is 1-based; crossterm emits col+1 for 0-based cols.
                self.col = first.saturating_sub(1).min(self.cols - 1);
                self.pending_wrap = false;
            }
            'H' => {
                self.row = 0;
                self.col = 0;
                self.pending_wrap = false;
            }
            'J' => match first {
                2 => {
                    for row in &mut self.screen {
                        row.fill(' ');
                    }
                }
                3 => self.history.clear(),
                _ => {
                    // Erase from cursor down.
                    for c in self.col..self.cols {
                        self.screen[self.row][c] = ' ';
                    }
                    for r in self.row + 1..self.rows {
                        self.screen[r].fill(' ');
                    }
                }
            },
            'K' => {
                for c in self.col.min(self.cols - 1)..self.cols {
                    self.screen[self.row][c] = ' ';
                }
            }
            'h' | 'l' if private => {
                for param in bare.split(';') {
                    if param == "7" {
                        self.autowrap = final_byte == 'h';
                    }
                }
            }
            _ => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.pending_wrap {
            if self.autowrap {
                self.col = 0;
                self.linefeed();
            }
            self.pending_wrap = false;
        }
        let width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if self.col < self.cols {
            self.screen[self.row][self.col] = ch;
            if width == 2 && self.col + 1 < self.cols {
                self.screen[self.row][self.col + 1] = '\0';
            }
        }
        self.col += width;
        if self.col >= self.cols {
            self.col = self.cols;
            if self.autowrap {
                self.pending_wrap = true;
            }
        }
    }

    fn linefeed(&mut self) {
        self.pending_wrap = false;
        if self.row + 1 >= self.rows {
            let top = self.screen.remove(0);
            self.history.push(Self::row_string(&top).trim_end().to_string());
            self.screen.push(vec![' '; self.cols]);
        } else {
            self.row += 1;
        }
    }
}
