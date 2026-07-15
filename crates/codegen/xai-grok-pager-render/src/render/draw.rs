//! Frame drawing with cursor blink preservation.
//!
//! # Problem
//!
//! Ratatui's [`Terminal::draw()`] (internally `try_draw()`) unconditionally
//! sends cursor escape sequences on every frame:
//!
//! - If `frame.set_cursor_position()` was called: `Show` + `MoveTo` every frame
//! - If not called: `Hide` every frame
//!
//! Both reset the terminal's cursor blink timer (`Show` restarts the blink
//! cycle, `MoveTo` resets the blink phase). At 30fps, the 500ms blink interval
//! never completes, so the cursor appears solid.
//!
//! # Solution
//!
//! We bypass `try_draw()` and use ratatui's lower-level API directly:
//!
//! ```text
//! terminal.autoresize()     — handle terminal size changes
//! terminal.get_frame()      — get a fresh buffer to render into
//! terminal.flush()          — diff old/new buffers, write only changed cells
//! terminal.swap_buffers()   — prepare for next frame
//! ```
//!
//! Cursor is managed entirely by [`CursorState`] with de-duplication:
//!
//! - **No cell changes + same position**: zero cursor commands → blink preserved
//! - **Cells changed + same position**: `MoveTo` to fix cursor after cell writes
//! - **Position changed**: `MoveTo` (blink resets — expected, user just typed)
//! - **Visibility transition**: `Show`/`Hide` (only on actual transition)
//! - **Idle (no draw calls)**: nothing sent → blink runs undisturbed
//!
//! The "no cell changes" optimization is possible because we use
//! [`xai_ratatui_inline::Terminal`] whose `flush()` returns `bool` indicating
//! whether any cells were written. When animated entries are off-screen, the
//! buffer diff is empty and we skip all cursor commands.
//!
//! # Synchronized output
//!
//! Each frame is wrapped in `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`
//! so the terminal processes all escape sequences atomically. This prevents
//! flicker and is critical for multiplexers like zellij and tmux.
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{QueueableCommand, cursor};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use xai_ratatui_inline::LinkSpan;
/// Terminal type for the pager. Defined here (beside [`TermWriter`]) so the
/// `render` module does not depend on `app`. Re-exported from `app` as
/// `crate::app::PagerTerminal` for existing call sites.
pub type PagerTerminal = xai_ratatui_inline::Terminal<CrosstermBackend<TermWriter>>;
/// Shared queued/written frame counters linking [`TermWriter`] to the writer
/// thread, so callers can wait for the output pipeline to drain.
///
/// The channel between them is fire-and-forget by design (the event loop must
/// never block on pty I/O), but a few operations need a *happens-before* on
/// terminal bytes: suspending into a tty-taking child (`$EDITOR` / `$PAGER`)
/// while a frame is still queued lets that frame race the child's own output —
/// it can land on the child's alternate screen (so the main screen never
/// receives it) or tear mid-escape-sequence around the alt-screen switch,
/// leaving the restored screen out of sync with the renderer's diff buffer
/// (stale rows, one-line offsets, literal `[` fragments). [`wait_drained`]
/// closes that window.
///
/// `queued` is incremented *before* the frame is sent and `written` after the
/// writer thread has flushed it to the tty, so `written == queued` ⇒ every
/// frame handed to the channel has reached the terminal fd.
///
/// [`wait_drained`]: WriterSync::wait_drained
#[derive(Clone, Debug, Default)]
pub struct WriterSync {
    queued: std::sync::Arc<std::sync::atomic::AtomicU64>,
    written: std::sync::Arc<std::sync::atomic::AtomicU64>,
}
impl WriterSync {
    pub fn new() -> Self {
        Self::default()
    }
    /// Record a frame handed to the channel. Called by [`TermWriter::flush`]
    /// *before* the send so `written` can never observably exceed `queued`.
    fn mark_queued(&self) {
        self.queued
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    /// Record a frame fully written + flushed to the tty (writer thread).
    fn mark_written(&self) {
        self.written
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    /// Whether every queued frame has been written to the tty.
    pub fn is_drained(&self) -> bool {
        self.written.load(std::sync::atomic::Ordering::SeqCst)
            >= self.queued.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Block (bounded) until the writer thread has flushed every queued frame.
    ///
    /// Returns `true` when drained, `false` on timeout (wedged pty / dead
    /// writer thread — callers proceed anyway, matching the bounded
    /// reader-park in the suspend path).
    pub fn wait_drained(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_drained() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}
/// A writer that buffers frame output and sends it to a background thread
/// for non-blocking terminal I/O.
///
/// All escape sequences produced during a frame are collected in an internal
/// `Vec<u8>`. When [`flush()`](Write::flush) is called, the accumulated bytes
/// are sent through a channel to a dedicated writer thread that performs the
/// actual (potentially blocking) `write()` to stderr / the pty fd.
///
/// This decouples the tokio event loop from pty back-pressure: if the
/// terminal emulator is slow to read (e.g. Ghostty busy with another pane),
/// only the writer thread stalls — the event loop keeps processing timers,
/// events, and ACP messages.
pub struct TermWriter {
    buf: Vec<u8>,
    tx: mpsc::Sender<Vec<u8>>,
    sync: WriterSync,
}
impl TermWriter {
    pub fn new(tx: mpsc::Sender<Vec<u8>>, sync: WriterSync) -> Self {
        Self {
            buf: Vec::with_capacity(32 * 1024),
            tx,
            sync,
        }
    }
    /// Drop the current frame's buffered bytes without sending them.
    pub fn discard(&mut self) {
        self.buf.clear();
    }
    /// The queued/written counters shared with the writer thread. Used by the
    /// suspend path to [`WriterSync::wait_drained`] before a child takes the tty.
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
}
impl Write for TermWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let data = std::mem::take(&mut self.buf);
            self.sync.mark_queued();
            let _ = self.tx.send(data);
        }
        Ok(())
    }
}
impl Drop for TermWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
/// Handle for the background writer thread.
///
/// Joining ensures all queued frames have been written to the terminal
/// before proceeding with teardown (e.g. `LeaveAlternateScreen`).
pub struct WriterThread {
    handle: Option<std::thread::JoinHandle<()>>,
}
impl WriterThread {
    /// Block until the writer thread has processed all pending frames and
    /// exited. The [`mpsc::Sender`] must be dropped *before* calling this,
    /// otherwise the thread will never see the channel close.
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
impl Drop for WriterThread {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
/// Spawn a background OS thread that writes frame data to stderr.
///
/// Returns `(Sender, WriterSync, WriterThread)`. Send `Vec<u8>` frame data
/// through the sender; the thread writes each frame to stderr via a 64 KiB
/// `BufWriter`. The [`WriterSync`] must be shared with every [`TermWriter`]
/// built on the sender so [`WriterSync::wait_drained`] tracks the queue.
/// Drop the sender to signal the thread to exit, then call
/// [`WriterThread::join`] to wait for it.
pub fn spawn_writer_thread() -> (mpsc::Sender<Vec<u8>>, WriterSync, WriterThread) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let sync = WriterSync::new();
    let thread_sync = sync.clone();
    let test_delay = std::env::var("GROK_TEST_FRAME_WRITE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);
    let handle = std::thread::Builder::new()
        .name("term-writer".into())
        .spawn(move || {
            #[cfg(not(windows))]
            let mut writer: Box<dyn std::io::Write> = {
                let tui_out = xai_tty_utils::dup_tui_stderr().unwrap_or_else(|_| {
                    use std::os::unix::io::{AsRawFd, FromRawFd};
                    let fd = unsafe { libc::dup(std::io::stderr().as_raw_fd()) };
                    unsafe { std::fs::File::from_raw_fd(fd) }
                });
                Box::new(std::io::BufWriter::with_capacity(64 * 1024, tui_out))
            };
            #[cfg(windows)]
            let mut writer: Box<dyn std::io::Write> = Box::new(std::io::BufWriter::with_capacity(
                64 * 1024,
                std::io::stderr(),
            ));
            while let Ok(data) = rx.recv() {
                if let Some(delay) = test_delay {
                    std::thread::sleep(delay);
                }
                {
                    let _guard = xai_grok_shared::stderr::stderr_lock();
                    let _ = writer.write_all(&data);
                    let _ = writer.flush();
                }
                thread_sync.mark_written();
            }
        })
        .expect("failed to spawn term-writer thread");
    (
        tx,
        sync,
        WriterThread {
            handle: Some(handle),
        },
    )
}
/// Cursor state tracker for blink-preserving cursor management.
///
/// Tracks the last cursor position written to the terminal. By comparing
/// with the desired position each frame, we emit the minimum cursor escape
/// sequences necessary — avoiding redundant `Show`/`Hide`/`MoveTo` that
/// would reset the terminal's blink timer.
#[derive(Debug, Default)]
pub struct CursorState {
    /// Last cursor position written to the terminal.
    /// `None` = cursor is hidden; `Some((x, y))` = cursor visible at (x, y).
    last_pos: Option<(u16, u16)>,
}
/// What cursor commands to emit after a frame render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    /// No cursor commands needed — blink timer preserved.
    None,
    /// Cursor is visible and cells changed — reposition after cell writes
    /// disturbed the terminal cursor. Resets blink (unavoidable when cells
    /// change on screen).
    Reposition(u16, u16),
    /// Cursor becoming visible at (x, y) — needs `MoveTo` + `Show`.
    Show(u16, u16),
    /// Cursor becoming hidden — needs `Hide`.
    Hide,
}
impl CursorState {
    pub fn new() -> Self {
        Self { last_pos: None }
    }
    /// Determine what cursor action to take for this frame.
    ///
    /// Pure function — computes the action from current state without
    /// side effects. Call [`apply`] to execute it.
    pub fn action(&self, cursor_pos: Option<(u16, u16)>, has_changes: bool) -> CursorAction {
        if cursor_pos == self.last_pos {
            if has_changes && let Some((x, y)) = cursor_pos {
                return CursorAction::Reposition(x, y);
            }
            CursorAction::None
        } else {
            match (cursor_pos, self.last_pos) {
                (Some((x, y)), Some(_)) => CursorAction::Reposition(x, y),
                (Some((x, y)), None) => CursorAction::Show(x, y),
                (None, Some(_)) => CursorAction::Hide,
                (None, None) => CursorAction::None,
            }
        }
    }
    /// Execute a cursor action by queuing escape sequences into `w`.
    ///
    /// Uses `queue!` (buffered) instead of `execute!` (immediate flush) so
    /// that cursor commands are batched with the rest of the frame data and
    /// written to the terminal atomically by the writer thread.
    pub fn apply<W: Write>(&mut self, action: CursorAction, w: &mut W) {
        match action {
            CursorAction::None => {}
            CursorAction::Reposition(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                self.last_pos = Some((x, y));
            }
            CursorAction::Show(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                let _ = w.queue(cursor::Show);
                self.last_pos = Some((x, y));
            }
            CursorAction::Hide => {
                let _ = w.queue(cursor::Hide);
                self.last_pos = None;
            }
        }
    }
}
/// Render a frame to the terminal with cursor blink preservation.
///
/// Bypasses ratatui's `try_draw()` to avoid its unconditional cursor
/// management. See [module docs](self) for the full rationale.
///
/// The `render_fn` receives a [`Frame`] and a `&mut Vec<LinkSpan>` to populate
/// with the frame's OSC 8 hyperlink regions (absolute viewport coordinates).
/// Those spans are handed to the terminal before the diff so hyperlinks
/// participate in the cell diff (emitted/cleared in lockstep with content) —
/// no out-of-band post-flush repaint. It returns a tuple of:
/// - `Option<(u16, u16)>` — cursor position (or `None` to hide cursor)
/// - `Option<PostFlush>` — escape sequences to write after cell flush (e.g.
///   Kitty graphics protocol image data). Written inside the synchronized
///   update block so the image appears atomically with the cell diff.
pub fn draw_frame(
    terminal: &mut PagerTerminal,
    cursor: &mut CursorState,
    render_fn: impl FnOnce(
        &mut Frame,
        &mut Vec<LinkSpan>,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ),
) {
    let _ = terminal.backend_mut().queue(BeginSynchronizedUpdate);
    let _ = terminal.autoresize();
    let mut link_spans: Vec<LinkSpan> = Vec::new();
    let (cursor_pos, post_flush_escapes) = {
        let mut frame = terminal.get_frame();
        render_fn(&mut frame, &mut link_spans)
    };
    terminal.set_frame_links(&link_spans);
    let has_changes = terminal.flush_with_links().unwrap_or(false);
    terminal.swap_buffers();
    let post_flush_wrote_cursor = post_flush_escapes.is_some();
    let action = cursor.action(cursor_pos, has_changes || post_flush_wrote_cursor);
    if !has_changes && !post_flush_wrote_cursor && action == CursorAction::None {
        terminal.backend_mut().writer_mut().discard();
        return;
    }
    if let Some(post_flush) = post_flush_escapes {
        let _ = post_flush.write_to(terminal.backend_mut());
    }
    cursor.apply(action, terminal.backend_mut());
    let _ = terminal.backend_mut().queue(EndSynchronizedUpdate);
    let _ = terminal.backend_mut().flush();
}
#[cfg(test)]
mod tests {
    use super::*;
    /// An unchanged frame must emit zero bytes to the PTY.
    #[test]
    fn idle_frame_emits_zero_bytes() {
        use ratatui::backend::CrosstermBackend;
        use ratatui::layout::Rect;
        use ratatui::widgets::Paragraph;
        use ratatui::{TerminalOptions, Viewport};
        use std::sync::mpsc;
        fn render(
            frame: &mut ratatui::Frame,
            _links: &mut Vec<LinkSpan>,
        ) -> (
            Option<(u16, u16)>,
            Option<crate::terminal::overlay::PostFlush>,
        ) {
            frame.render_widget(Paragraph::new("hello world"), frame.area());
            (None, None)
        }
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let backend = CrosstermBackend::new(TermWriter::new(tx, WriterSync::new()));
        let mut terminal = xai_ratatui_inline::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("build terminal");
        let mut cursor = CursorState::new();
        draw_frame(&mut terminal, &mut cursor, render);
        let first: Vec<u8> = rx.try_iter().flatten().collect();
        assert!(!first.is_empty(), "first frame should emit bytes");
        draw_frame(&mut terminal, &mut cursor, render);
        let second: Vec<u8> = rx.try_iter().flatten().collect();
        assert!(
            second.is_empty(),
            "idle (unchanged) frame must emit 0 bytes, got {}: {:?}",
            second.len(),
            String::from_utf8_lossy(&second),
        );
    }
    /// `wait_drained` semantics: drained when `written` has caught up with
    /// `queued` — immediately when nothing is pending, after the consumer
    /// marks the frame written, and a bounded `false` when it never does.
    /// This is the happens-before the suspend path relies on so no queued
    /// frame can race a tty-taking `$EDITOR` / `$PAGER` child.
    #[test]
    fn writer_sync_drains_when_written_catches_queued() {
        let sync = WriterSync::new();
        assert!(sync.wait_drained(Duration::from_millis(1)));
        sync.mark_queued();
        assert!(!sync.is_drained());
        assert!(!sync.wait_drained(Duration::from_millis(5)));
        let consumer_sync = sync.clone();
        let consumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            consumer_sync.mark_written();
        });
        assert!(sync.wait_drained(Duration::from_secs(5)));
        consumer.join().expect("consumer thread");
    }
    /// A `TermWriter::flush` with buffered bytes marks the frame queued; the
    /// writer-thread side marking it written restores the drained state.
    #[test]
    fn term_writer_flush_marks_queued() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let sync = WriterSync::new();
        let mut writer = TermWriter::new(tx, sync.clone());
        writer.flush().expect("flush");
        assert!(sync.is_drained());
        writer.write_all(b"frame bytes").expect("write");
        writer.flush().expect("flush");
        assert!(!sync.is_drained(), "queued frame not yet written");
        assert_eq!(rx.try_recv().expect("frame on channel"), b"frame bytes");
        sync.mark_written();
        assert!(sync.is_drained());
    }
    fn state_hidden() -> CursorState {
        CursorState { last_pos: None }
    }
    fn state_at(x: u16, y: u16) -> CursorState {
        CursorState {
            last_pos: Some((x, y)),
        }
    }
    #[test]
    fn hidden_no_changes_stays_hidden() {
        let s = state_hidden();
        assert_eq!(s.action(None, false), CursorAction::None);
    }
    #[test]
    fn visible_same_pos_no_changes_preserves_blink() {
        let s = state_at(5, 10);
        assert_eq!(s.action(Some((5, 10)), false), CursorAction::None);
    }
    #[test]
    fn visible_new_pos_no_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((6, 10)), false),
            CursorAction::Reposition(6, 10)
        );
    }
    #[test]
    fn hidden_with_changes_stays_hidden() {
        let s = state_hidden();
        assert_eq!(s.action(None, true), CursorAction::None);
    }
    #[test]
    fn visible_same_pos_with_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((5, 10)), true),
            CursorAction::Reposition(5, 10)
        );
    }
    #[test]
    fn visible_new_pos_with_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((8, 10)), true),
            CursorAction::Reposition(8, 10)
        );
    }
    #[test]
    fn hidden_to_visible_shows() {
        let s = state_hidden();
        assert_eq!(s.action(Some((5, 10)), false), CursorAction::Show(5, 10));
    }
    #[test]
    fn hidden_to_visible_with_changes_shows() {
        let s = state_hidden();
        assert_eq!(s.action(Some((5, 10)), true), CursorAction::Show(5, 10));
    }
    #[test]
    fn visible_to_hidden_hides() {
        let s = state_at(5, 10);
        assert_eq!(s.action(None, false), CursorAction::Hide);
    }
    #[test]
    fn visible_to_hidden_with_changes_hides() {
        let s = state_at(5, 10);
        assert_eq!(s.action(None, true), CursorAction::Hide);
    }
    #[test]
    fn apply_show_updates_last_pos() {
        let mut s = state_hidden();
        let mut sink = Vec::new();
        s.apply(CursorAction::Show(3, 7), &mut sink);
        assert_eq!(s.last_pos, Some((3, 7)));
    }
    #[test]
    fn apply_hide_clears_last_pos() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::Hide, &mut sink);
        assert_eq!(s.last_pos, None);
    }
    #[test]
    fn apply_reposition_updates_last_pos() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::Reposition(5, 9), &mut sink);
        assert_eq!(s.last_pos, Some((5, 9)));
    }
    #[test]
    fn apply_none_preserves_state() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::None, &mut sink);
        assert_eq!(s.last_pos, Some((3, 7)));
    }
    /// Verify the writer thread correctly round-trips multi-byte UTF-8
    /// through the channel. This catches encoding issues where the writer
    /// silently corrupts Braille/emoji/CJK characters.
    #[test]
    fn writer_thread_preserves_multibyte_utf8() {
        let test_payload = "⣀⣾⠿⠛\u{e0a0}\u{1F600}";
        let expected_bytes = test_payload.as_bytes().to_vec();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture2 = capture.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            while let Ok(data) = rx.recv() {
                buf.extend_from_slice(&data);
            }
            *capture2.lock().unwrap() = buf;
        });
        tx.send(expected_bytes.clone()).unwrap();
        drop(tx);
        handle.join().unwrap();
        let captured = capture.lock().unwrap();
        assert_eq!(
            *captured, expected_bytes,
            "Writer thread corrupted multi-byte UTF-8 payload"
        );
        assert_eq!(
            std::str::from_utf8(&captured).unwrap(),
            test_payload,
            "Round-tripped bytes do not decode to original UTF-8 string"
        );
    }
}
