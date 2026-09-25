//! Terminal teardown shared by the post-loop restore, the panic hook and the signal path.
//!
//! [`emit_terminal_teardown_sequences`] defines the on-wire teardown byte order exactly once; the kitty keyboard pop
//! inside it happens at most once per push. The panic hook runs it through [`run_bounded_teardown`]; the signal path
//! calls it directly, without draining the writer.
//! The post-loop restore alone joins the stdin reader first and fences the pop with a DA1 round trip before raw mode ends.
//!
//! Fork note: the writer join stays unbounded (`WriterThread::join`), so the restore cannot observe a timed-out writer
//! the way upstream does. The fence therefore always sees `writer_timed_out = false`; threading a real signal through
//! needs the render crate to port upstream's bounded `WriterJoin::join_within` first.

use std::io::{self, Write};
use std::panic;
use std::sync::atomic::Ordering;

use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{self, LeaveAlternateScreen};

use crate::app::reader_thread::{READER_JOIN_GRACE, ReaderJoin, ReaderThread};
use crate::app::teardown_fence::{TeardownFence, TeardownFenceReport};
use crate::app::{
    CURSOR_STYLE_FORCED, MOUSE_CAPTURE_ENABLED, ScreenMode, current_screen_mode,
    pop_gboom_keyboard_flags, signal_handler,
};
use crate::render::draw::{PagerTerminal, WriterThread};

/// Drop the terminal (closing the writer mpsc channel) and join the
/// writer thread. After this returns, subsequent direct stderr writes
/// are guaranteed to land strictly after every queued frame.
fn drain_writer_thread_before_teardown(
    terminal: PagerTerminal,
    writer_thread: WriterThread,
) -> io::Result<()> {
    drop(terminal);
    writer_thread.join()
}

/// Write raw CSI sequences to disable mouse tracking and bracketed paste.
///
/// Best-effort: failures are silently ignored since this runs on teardown
/// and panic paths where stderr may already be broken.
fn disable_mouse_paste_raw() {
    xai_grok_shell::util::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(xai_crash_handler::terminal::MOUSE_PASTE_RESET);
        let _ = stderr.flush();
    });
}

/// Inline teardown escape sequences in the canonical order, shared by
/// `restore_terminal` and `set_panic_hook` so the on-wire byte order is
/// defined exactly once.
///
/// Order: EndSynchronizedUpdate -> reset_cursor_color ->
/// disable_mouse_paste_raw -> DisableFocusChange -> pop kitty (if pushed)
/// -> mode-specific final block. EndSynchronizedUpdate is emitted first so multiplexers
/// (zellij/tmux) stop buffering before the resets arrive. Does NOT call
/// `disable_raw_mode`. Callers should drain queued writer-thread frames
/// first when possible; the panic hook can't (would deadlock).
pub(super) fn emit_terminal_teardown_sequences(mode: ScreenMode, inline_cursor_row: Option<u16>) {
    xai_grok_shell::util::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(crate::notifications::progress::OSC_CLEAR.as_bytes());
        let _ = stderr.flush();
    });
    xai_grok_shell::util::with_locked_stderr(|stderr| {
        let _ = execute!(stderr, crossterm::terminal::EndSynchronizedUpdate);
    });
    crate::theme::reset_cursor_color();
    disable_mouse_paste_raw();
    if MOUSE_CAPTURE_ENABLED.swap(false, Ordering::AcqRel) {
        #[cfg(windows)]
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            let _ = execute!(stderr, event::DisableMouseCapture);
        });
    }
    xai_grok_shell::util::with_locked_stderr(|stderr| {
        let _ = execute!(stderr, event::DisableFocusChange);
    });
    pop_gboom_keyboard_flags();
    if crate::terminal::take_kitty_flags_pushed() {
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            let _ = execute!(stderr, event::PopKeyboardEnhancementFlags);
        });
    }
    let restore_style = CURSOR_STYLE_FORCED.load(Ordering::Acquire);
    if mode.is_fullscreen() {
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            if restore_style {
                let _ = execute!(stderr, SetCursorStyle::DefaultUserShape);
            }
            let _ = execute!(stderr, cursor::Show, LeaveAlternateScreen);
        });
    } else {
        let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
        let last = rows.saturating_sub(1);
        let target = inline_cursor_row.unwrap_or(last).min(last);
        xai_grok_shell::util::with_locked_stderr(|stderr| {
            if restore_style {
                let _ = execute!(stderr, SetCursorStyle::DefaultUserShape);
            }
            let _ = execute!(stderr, cursor::MoveTo(0, target), cursor::Show);
            let _ = writeln!(stderr);
            let _ = stderr.flush();
        });
    }
    #[cfg(windows)]
    crate::app::win_native_selection::restore_stdin_mode();
}

/// Bound on teardown writes when the stderr lock may be wedged (the panic hook). An unbounded teardown would hang
/// forever, never restoring raw mode.
const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Teardown still runs if draining fails, so terminal state is restored before returning that error.
/// Draining first prevents a late frame after `LeaveAlternateScreen`; `fence` runs after teardown, still in raw mode, and
/// receives the reader join plus whether the writer join timed out (always false here: the fork's writer join is unbounded).
fn restore_terminal_with(
    mut terminal: PagerTerminal,
    writer_thread: WriterThread,
    reader_thread: ReaderThread,
    mode: ScreenMode,
    drain: impl FnOnce(PagerTerminal, WriterThread) -> io::Result<()>,
    teardown: impl FnOnce(ScreenMode, Option<u16>),
    fence: impl FnOnce(ReaderJoin, bool) -> TeardownFenceReport,
) -> io::Result<()> {
    // Joined first so the fence is the sole stdin reader; `input_rx` died with the event loop, so this takes one poll cycle
    let reader = reader_thread.join_within(READER_JOIN_GRACE);
    if mode.is_fullscreen() && !writer_thread.writer_sync().failed() {
        let _ = terminal.clear();
        {
            use std::io::Write;
            let _ = terminal.backend_mut().flush();
        }
    }
    // Capture the live viewport's bottom row before dropping the terminal
    // Teardown can then place the cursor directly below the (non-bottom-pinned) live region rather than at the screen bottom
    let inline_cursor_row = (!mode.is_fullscreen()).then(|| terminal.viewport_area().bottom());
    let drain_result = drain(terminal, writer_thread);
    teardown(mode, inline_cursor_row);
    // Release events the terminal emitted before applying the pop may still be in flight; left alone they reach the shell as keystrokes
    // Still in raw mode: the fence reads the raw fd, and `disable_raw_mode` must follow the last read
    // The fork's writer join is unbounded, so a returned join means the writer exited: never report it wedged.
    let report = fence(reader, false);
    let _ = terminal::disable_raw_mode();
    // Tell the signal handlers that the user's shell now owns the terminal
    // A SIGPIPE arriving on a late stderr write must not paint escape sequences into the user's prompt
    signal_handler::mark_restored();
    xai_crash_handler::disable_terminal_escape_restore();
    // Restore fd 2 to the real terminal so any post-TUI output (tracing flushes, Sentry flush, etc.) is visible
    xai_tty_utils::restore_native_stderr();
    report.record();
    drain_result
}

pub(super) fn restore_terminal(
    terminal: PagerTerminal,
    writer_thread: WriterThread,
    reader_thread: ReaderThread,
    _mode: ScreenMode,
) -> io::Result<()> {
    // Read here, before `emit_terminal_teardown_sequences` swaps the record to zero while popping
    let flags_pushed = crate::terminal::kitty_flags_pushed();
    restore_terminal_with(
        terminal,
        writer_thread,
        reader_thread,
        // Re-read at teardown time: an in-process mode switch would otherwise tear down the wrong screen.
        current_screen_mode(),
        drain_writer_thread_before_teardown,
        emit_terminal_teardown_sequences,
        move |reader, writer_timed_out| {
            TeardownFence {
                reader,
                writer_timed_out,
                flags_pushed,
            }
            .run()
        },
    )
}

/// Run a best-effort teardown `f` on a helper thread, waiting at most `grace` for it.
/// For paths where the stderr lock may be wedged (the panic hook): an unbounded teardown would hang forever, never restoring raw mode. On timeout the helper is detached; the process is exiting anyway. Runs `f` inline if no thread can spawn.
fn run_bounded_teardown(f: impl FnOnce() + Send + 'static, grace: std::time::Duration) {
    // Shared slot so the closure survives a failed spawn for the inline fallback.
    let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(f)));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let worker_slot = std::sync::Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("bounded-teardown".into())
        .spawn(move || {
            if let Some(f) = worker_slot.lock().take() {
                f();
            }
            let _ = done_tx.send(());
        });
    match spawned {
        Ok(_) => {
            let _ = done_rx.recv_timeout(grace);
        }
        Err(_) => {
            if let Some(f) = slot.lock().take() {
                f();
            }
        }
    }
}

/// Reads [`current_screen_mode`] at panic time; never capture a mode here, or an in-process mode switch tears down the wrong screen.
pub(super) fn set_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        run_bounded_teardown(
            || emit_terminal_teardown_sequences(current_screen_mode(), None),
            TEARDOWN_GRACE,
        );
        let _ = terminal::disable_raw_mode();
        signal_handler::mark_restored();
        xai_crash_handler::disable_terminal_escape_restore();
        xai_tty_utils::restore_native_stderr();
        xai_tty_utils::global_process_scope().kill_all();
        crate::memory_trace::record_crash_sample();
        hook(info);
    }));
}

#[cfg(test)]
#[path = "terminal_restore_tests.rs"]
mod tests;
