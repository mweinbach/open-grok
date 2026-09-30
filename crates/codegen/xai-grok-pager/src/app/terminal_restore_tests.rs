use std::sync::atomic::AtomicBool;

use super::*;
use crate::app::teardown_fence::FenceDecision;

/// No tty I/O, and the pushed-flags global is never read (a parallel test sets it).
fn no_op_fence(reader: ReaderJoin, _writer_timed_out: bool) -> TeardownFenceReport {
    TeardownFenceReport {
        decision: FenceDecision::SkipNoFlags,
        fence: None,
        reader,
    }
}

/// The panic hook's teardown writes stay bounded so a wedged stderr lock cannot
/// keep the hook from restoring raw mode and reaching the delegated hook/abort.
#[test]
fn panic_teardown_is_bounded_when_the_stderr_lock_is_wedged() {
    fn takes_the_lock() {
        let _guard = xai_grok_shell::util::stderr_lock();
    }
    // Park a fake writer thread on the lock for the whole call.
    let _guard = xai_grok_shell::util::stderr_lock();
    let started = std::time::Instant::now();
    run_bounded_teardown(takes_the_lock, std::time::Duration::from_millis(100));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "bounded teardown must give up on a wedged stderr lock"
    );
}

fn test_terminal_and_writer_thread() -> (PagerTerminal, WriterThread) {
    use ratatui::backend::CrosstermBackend;
    use ratatui::{TerminalOptions, Viewport};
    // These tests never draw; the frame receiver can drop here.
    let (tx, _rx) = std::sync::mpsc::channel::<crate::render::draw::WriterPayload>();
    let sync = crate::render::draw::WriterSync::new();
    let backend =
        CrosstermBackend::new(crate::render::draw::TermWriter::new(tx, sync).expect("test writer"));
    let terminal = xai_ratatui_inline::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
        },
    )
    .expect("test terminal");
    let (writer_tx, _writer_sync, _events, writer_thread) =
        crate::render::draw::spawn_writer_thread();
    drop(writer_tx);
    (terminal, writer_thread)
}

/// The fence runs after teardown: the pop must be on the wire before the DA1 query.
#[test]
fn restore_runs_teardown_even_when_writer_failed() {
    xai_grok_telemetry::unified_log::redirect_to_temp_for_tests();
    let (terminal, writer_thread) = test_terminal_and_writer_thread();
    let teardown_called = std::sync::Arc::new(AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&teardown_called);
    let teardown_before_fence = std::sync::Arc::clone(&teardown_called);

    let result = restore_terminal_with(
        terminal,
        writer_thread,
        ReaderThread::detached(),
        ScreenMode::Inline,
        |terminal, writer_thread| {
            drop(terminal);
            drop(writer_thread);
            Err(io::Error::other("injected drain failure"))
        },
        move |_, _| observed.store(true, Ordering::Release),
        move |reader, writer_timed_out| {
            assert!(teardown_before_fence.load(Ordering::Acquire));
            assert_eq!((ReaderJoin::Absent, false), (reader, writer_timed_out));
            no_op_fence(reader, writer_timed_out)
        },
    );

    assert!(result.is_err());
    assert!(teardown_called.load(Ordering::Acquire));
}
