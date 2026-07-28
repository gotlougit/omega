//! Tests for the terminal: pure layout/metrics unit tests plus
//! end-to-end tests driving a virtual [`Term`] with injected input,
//! asserting on emulated screen state.

use super::*;
use crate::tui::emulator::{Capture, Emulator};
use crate::tui::style::StyledBlock;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const ROWS: usize = 8;
const COLS: usize = 20;

fn prompt() -> StyledText {
    StyledText::from("P> ")
}

struct TestTerm {
    term: Term,
    handle: TermHandle,
    input: mpsc::Sender<RawEvent>,
    buf: Arc<Mutex<Vec<u8>>>,
}

fn test_term() -> TestTerm {
    let (capture, buf) = Capture::new();
    let (term, handle, input) = Term::new_virtual(COLS, ROWS, prompt(), capture);
    TestTerm { term, handle, input, buf }
}

fn key(c: char) -> RawEvent {
    RawEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn enter() -> RawEvent {
    RawEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

fn next_event_timeout(term: &mut Term, ms: u64) -> Option<Event> {
    match term.input_rx.recv_timeout(Duration::from_millis(ms)) {
        Ok(InputMessage::Event(ev)) => Some(ev),
        _ => None,
    }
}

/// Waits for an event matching `pred`, skipping others. Panics on timeout.
fn wait_for(term: &mut Term, pred: impl Fn(&Event) -> bool, what: &str) -> Event {
    for _ in 0..64 {
        match next_event_timeout(term, 2000) {
            Some(ev) if pred(&ev) => return ev,
            Some(_) => continue,
            None => panic!("timed out waiting for {what}"),
        }
    }
    panic!("too many events while waiting for {what}");
}

/// Types text (one key event per char), waiting for each keystroke to be
/// processed so ordering is deterministic.
fn type_str(term: &mut Term, input: &mpsc::Sender<RawEvent>, s: &str) {
    for c in s.chars() {
        input.send(key(c)).expect("input channel open");
        wait_for(term, |e| matches!(e, Event::BufferChanged), "BufferChanged");
    }
}

fn block(text: &str) -> StyledBlock {
    StyledBlock::new(StyledText::from(Span::plain(text.to_string())))
}

/// Current emulated screen + scrollback for the whole capture buffer.
fn emulator(tt: &TestTerm) -> Emulator {
    Emulator::from_capture(ROWS, COLS, &tt.buf)
}

/// Emulated screen for only the bytes appended since `from` — valid after a
/// full render, which homes the cursor and clears everything first.
fn emulator_since(tt: &TestTerm, from: usize, rows: usize, cols: usize) -> Emulator {
    let data = tt.buf.lock().expect("capture poisoned")[from..].to_vec();
    let mut em = Emulator::new(rows, cols);
    em.feed_bytes(&data);
    em
}

fn raw(tt: &TestTerm) -> Vec<u8> {
    tt.buf.lock().expect("capture poisoned").clone()
}

// ---------------------------------------------------------------------------
// Unit: buffer_position_for_byte
// ---------------------------------------------------------------------------

#[test]
fn cursor_math_unwrapped() {
    assert_eq!(buffer_position_for_byte("", 0, 80, 3), (0, 3));
    assert_eq!(buffer_position_for_byte("abc", 0, 80, 3), (0, 3));
    assert_eq!(buffer_position_for_byte("abc", 3, 80, 3), (0, 6));
    assert_eq!(buffer_position_for_byte("abc", 1, 80, 3), (0, 4));
}

#[test]
fn cursor_math_wrapped() {
    // width 4, prompt 2 cols: "abcdef" fills row 0 (2+2), row 1 (4), row 2.
    assert_eq!(buffer_position_for_byte("abcdef", 6, 4, 2), (2, 0));
    assert_eq!(buffer_position_for_byte("abcdef", 2, 4, 2), (1, 0));
    assert_eq!(buffer_position_for_byte("abcdef", 3, 4, 2), (1, 1));
    // Exact-width wrap: prompt 0, width 5, "hello world" → rows "hello",
    // " worl", "d".
    assert_eq!(buffer_position_for_byte("hello world", 11, 5, 0), (2, 1));
    assert_eq!(buffer_position_for_byte("hello", 5, 5, 0), (1, 0));
}

// ---------------------------------------------------------------------------
// Unit: viewport_start_with_cursor
// ---------------------------------------------------------------------------

#[test]
fn viewport_clamps_to_cursor() {
    // Everything fits.
    assert_eq!(viewport_start_with_cursor(0, 0, 10, 24), 0);
    // Bottom-anchored, cursor inside.
    assert_eq!(viewport_start_with_cursor(5, 10, 30, 24), 5);
    // Cursor below the window pulls the start down.
    assert_eq!(viewport_start_with_cursor(5, 29, 30, 24), 6);
    // Cursor above the window pulls the start up.
    assert_eq!(viewport_start_with_cursor(5, 2, 30, 24), 2);
    // Start never exceeds the bottom anchor.
    assert_eq!(viewport_start_with_cursor(10, 29, 30, 24), 6);
}

// ---------------------------------------------------------------------------
// Unit: plan_metrics
// ---------------------------------------------------------------------------

fn metrics(
    viewport_start: usize,
    rubber: usize,
    log: usize,
    fixed: usize,
    cursor_row: usize,
    height: usize,
) -> PlanMetrics {
    let model = TermModel {
        viewport_start,
        rubber,
        known_lines: Vec::new(),
    };
    plan_metrics(&model, log, fixed, cursor_row, height)
}

#[test]
fn metrics_no_rubber_before_overflow() {
    // Content fits and the viewport never overflowed: no rubber, prompt
    // follows the transcript.
    let m = metrics(0, 0, 3, 1, 3, 24);
    assert_eq!(m.viewport_start, 0);
    assert_eq!(m.rubber_height, 0);
    assert_eq!(m.render_len, 4);
    assert_eq!(m.cursor_row, 3);
}

#[test]
fn metrics_overflow_pushes_viewport_down() {
    let m = metrics(0, 0, 30, 1, 30, 24);
    assert_eq!(m.viewport_start, 7);
    assert_eq!(m.rubber_height, 0);
    assert_eq!(m.render_len, 31);
}

#[test]
fn metrics_rubber_absorbs_shrink_after_overflow() {
    // Log shrank from ≥31 to 25 rows after overflow: rubber grows to keep
    // the viewport (and thus the fixed tail) in place.
    let m = metrics(7, 0, 25, 1, 25, 24);
    assert_eq!(m.viewport_start, 7);
    assert_eq!(m.rubber_height, 5);
    assert_eq!(m.render_len, 31);
    assert_eq!(m.cursor_row, 30);
}

#[test]
fn metrics_rubber_consumed_by_growth() {
    let m = metrics(7, 5, 28, 1, 28, 24);
    assert_eq!(m.viewport_start, 7);
    assert_eq!(m.rubber_height, 2);
    assert_eq!(m.render_len, 31);
}

#[test]
fn metrics_fixed_tail_taller_than_screen() {
    // Rubber is dropped; the viewport shows the tail around the cursor.
    let m = metrics(3, 4, 5, 24, 28, 24);
    assert_eq!(m.rubber_height, 0);
    assert_eq!(m.viewport_start, 5);
    assert_eq!(m.cursor_row, 28);
}

// ---------------------------------------------------------------------------
// Unit: hidden_lines_changed
// ---------------------------------------------------------------------------

#[test]
fn hidden_prefix_detection() {
    let line = |s: &str| vec![Cell::plain(s.chars().next().unwrap())];
    let prev = vec![line("a"), line("b"), line("c")];
    let same = prev.clone();
    assert!(!hidden_lines_changed(&prev, &same, 2));

    let mut changed = prev.clone();
    changed[0] = line("z");
    assert!(hidden_lines_changed(&prev, &changed, 2));

    // Change below the hidden prefix doesn't count.
    let mut changed_below = prev.clone();
    changed_below[2] = line("z");
    assert!(!hidden_lines_changed(&prev, &changed_below, 2));

    // A row that vanished from the hidden prefix counts.
    let shorter = vec![line("a")];
    assert!(hidden_lines_changed(&prev, &shorter, 2));
}

// ---------------------------------------------------------------------------
// Integration: virtual Term end-to-end
// ---------------------------------------------------------------------------

#[test]
fn launch_renders_without_clearing() {
    let tt = test_term();
    tt.handle.print_output(block("LINE0"));
    tt.handle.print_output(block("LINE1"));
    tt.handle.print_output(block("LINE2"));
    tt.handle.redraw_sync();

    let out = raw(&tt);
    assert!(!out.windows(4).any(|w| w == b"\x1b[2J"), "launch cleared screen");
    assert!(!out.windows(4).any(|w| w == b"\x1b[3J"), "launch cleared scrollback");
    assert!(!out.windows(3).any(|w| w == b"\x1b[H"), "launch homed cursor");

    let em = emulator(&tt);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "LINE0");
    assert_eq!(lines[1], "LINE1");
    assert_eq!(lines[2], "LINE2");
    assert_eq!(lines[3], "P>");
    assert!(lines[4..].iter().all(|l| l.is_empty()), "prompt bottom-pinned: {lines:?}");
    assert_eq!(em.cursor(), (3, 3));
    assert!(em.history().is_empty(), "scrollback polluted at launch");

    tt.handle.request_input_shutdown();
}

#[test]
fn typing_and_submit() {
    let mut tt = test_term();
    tt.handle.redraw_sync();

    type_str(&mut tt.term, &tt.input, "hi");
    tt.handle.redraw_sync();
    let em = emulator(&tt);
    assert_eq!(em.screen_lines()[0], "P> hi");
    assert_eq!(em.cursor(), (0, 5));

    tt.input.send(enter()).expect("input open");
    match wait_for(&mut tt.term, |e| matches!(e, Event::Line(_)), "Line") {
        Event::Line(line) => assert_eq!(line, "hi"),
        _ => unreachable!(),
    }
    tt.handle.print_output(block("ECHO:hi"));
    tt.handle.redraw_sync();

    let em = emulator(&tt);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "ECHO:hi");
    assert_eq!(lines[1], "P>");
    assert_eq!(em.cursor(), (1, 3));

    tt.handle.request_input_shutdown();
}

#[test]
fn print_output_overflow_scrolls_into_scrollback() {
    let tt = test_term();
    for i in 0..20 {
        tt.handle.print_output(block(&format!("LINE{i}")));
    }
    tt.handle.redraw_sync();

    let em = emulator(&tt);
    let lines = em.screen_lines();
    // 20 content rows + prompt on 8 rows: prompt at bottom, last 7 rows
    // visible, the first 13 pushed into scrollback.
    assert_eq!(lines[7], "P>");
    assert_eq!(
        lines[..7],
        (13..20).map(|i| format!("LINE{i}")).collect::<Vec<_>>(),
    );
    assert_eq!(
        em.history(),
        (0..13).map(|i| format!("LINE{i}")).collect::<Vec<_>>(),
    );
    assert_eq!(em.cursor(), (7, 3));

    tt.handle.request_input_shutdown();
}

#[test]
fn streaming_block_updates_in_place() {
    let tt = test_term();
    let id = tt.handle.print_output(block("Hello"));
    tt.handle.redraw_sync();
    tt.handle.set_block(id, block("Hello, "));
    tt.handle.redraw_sync();
    tt.handle.set_block(id, block("Hello, world"));
    tt.handle.redraw_sync();

    let em = emulator(&tt);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "Hello, world");
    assert_eq!(lines[1], "P>");
    // The block must appear exactly once — updates replace, not append.
    let count = lines
        .iter()
        .chain(em.history().iter())
        .filter(|l| l.contains("Hello"))
        .count();
    assert_eq!(count, 1, "streaming block duplicated: {lines:?}");

    tt.handle.request_input_shutdown();
}

#[test]
fn resize_triggers_full_render() {
    let mut tt = test_term();
    for i in 0..3 {
        tt.handle.print_output(block(&format!("LINE{i}")));
    }
    tt.handle.redraw_sync();
    let before = raw(&tt).len();

    tt.input.send(RawEvent::Resize(12, 5)).expect("input open");
    wait_for(
        &mut tt.term,
        |e| matches!(e, Event::Resize { width: 12, height: 5 }),
        "Resize",
    );
    tt.handle.redraw_sync();

    let out = raw(&tt);
    let new_bytes = &out[before..];
    assert!(
        new_bytes.windows(4).any(|w| w == b"\x1b[2J")
            && new_bytes.windows(4).any(|w| w == b"\x1b[3J"),
        "resize did not take the full-render path",
    );

    let em = emulator_since(&tt, before, 5, 12);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "LINE0");
    assert_eq!(lines[1], "LINE1");
    assert_eq!(lines[2], "LINE2");
    assert_eq!(lines[3], "P>");
    assert_eq!(em.cursor(), (3, 3));

    tt.handle.request_input_shutdown();
}

#[test]
fn clear_output_clears_screen_and_scrollback() {
    let tt = test_term();
    for i in 0..12 {
        tt.handle.print_output(block(&format!("LINE{i}")));
    }
    tt.handle.redraw_sync();
    assert!(!emulator(&tt).history().is_empty());

    tt.handle.clear_output();
    tt.handle.redraw_sync();

    let out = raw(&tt);
    assert!(out.windows(4).any(|w| w == b"\x1b[3J"), "no full render on clear");

    // Full render homes + clears, so one emulator over the whole stream works.
    let em = emulator(&tt);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "P>");
    assert!(lines[1..].iter().all(|l| l.is_empty()));
    assert!(em.history().is_empty(), "scrollback not cleared");
    assert_eq!(em.cursor(), (0, 3));

    tt.handle.request_input_shutdown();
}

#[test]
fn zones_render_in_order() {
    let tt = test_term();
    tt.handle.print_output(block("HIST"));
    let above = tt.handle.new_block(block("ABOVE"));
    tt.handle.push_above(above);
    let sugg = tt.handle.new_block(block("SUGG"));
    tt.handle.push_suggestions(sugg);
    let below = tt.handle.new_block(block("BELOW"));
    tt.handle.push_below(below);
    tt.handle.redraw_sync();

    let em = emulator(&tt);
    let lines = em.screen_lines();
    assert_eq!(lines[0], "HIST");
    assert_eq!(lines[1], "ABOVE");
    assert_eq!(lines[2], "P>");
    assert_eq!(lines[3], "SUGG");
    assert_eq!(lines[4], "BELOW");
    assert_eq!(em.cursor(), (2, 3));

    tt.handle.request_input_shutdown();
}

#[test]
fn wrapped_input_positions_cursor() {
    let mut tt = test_term();
    tt.handle.redraw_sync();
    type_str(&mut tt.term, &tt.input, &"a".repeat(30));
    tt.handle.redraw_sync();

    let em = emulator(&tt);
    let lines = em.screen_lines();
    // "P> " (3 cols) + 30 chars on 20-col screen: 17 on row 0, 13 on row 1.
    assert_eq!(lines[0], format!("P> {}", "a".repeat(17)));
    assert_eq!(lines[1], "a".repeat(13));
    assert_eq!(em.cursor(), (1, 13));

    tt.handle.request_input_shutdown();
}

#[test]
fn paste_inserts_text() {
    let mut tt = test_term();
    tt.handle.redraw_sync();
    tt.input
        .send(RawEvent::Paste("hello".to_string()))
        .expect("input open");
    wait_for(&mut tt.term, |e| matches!(e, Event::BufferChanged), "BufferChanged");
    tt.handle.redraw_sync();

    assert_eq!(tt.handle.get_buffer(), "hello");
    let em = emulator(&tt);
    assert_eq!(em.screen_lines()[0], "P> hello");
    assert_eq!(em.cursor(), (0, 8));

    tt.handle.request_input_shutdown();
}

#[test]
fn eof_when_input_channel_closes() {
    let mut tt = test_term();
    tt.handle.redraw_sync();
    drop(tt.input.clone());
    // The virtual input thread sees the disconnect once all senders drop.
    drop(tt.input);
    match next_event_timeout(&mut tt.term, 2000) {
        Some(Event::Eof) => {}
        other => panic!("expected Eof, got {other:?}"),
    }
}
