//! Layer 1: PTY management — spawn, inject keys, resize, drain output.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Raw key byte constants for terminal input injection.
pub mod keys {
    pub const J: &[u8] = b"j";
    pub const K: &[u8] = b"k";
    pub const Q: &[u8] = b"q";
    pub const DOWN: &[u8] = b"\x1b[B";
    pub const UP: &[u8] = b"\x1b[A";
    pub const PGDN: &[u8] = b"\x1b[6~";
    pub const PGUP: &[u8] = b"\x1b[5~";
    pub const ENTER: &[u8] = b"\r";
    pub const CTRL_C: &[u8] = b"\x03";
    /// Ctrl+R (0x12) — prompt history search / scrollback mouse-reporting toggle.
    pub const CTRL_R: &[u8] = b"\x12";
    pub const ESC: &[u8] = b"\x1b";
}

/// Low-level PTY controller: spawns a child process inside a PTY and provides
/// methods to inject input, resize, and drain output.
pub struct PtyController {
    child: Box<dyn portable_pty::Child + Send>,
    writer: Box<dyn Write + Send>,
    reader_rx: mpsc::Receiver<Vec<u8>>,
    #[allow(dead_code)] // Kept alive to hold the PTY open; used by resize().
    master: Box<dyn portable_pty::MasterPty + Send>,
}

impl PtyController {
    /// Spawn a binary inside a PTY with the given terminal size.
    ///
    /// `env` is a list of `(key, value)` pairs to set on the child process.
    pub fn spawn(
        binary: &Path,
        size: PtySize,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Self> {
        Self::spawn_in_dir(binary, size, args, env, None)
    }

    /// Like [`spawn`](Self::spawn), with an optional child working directory.
    pub fn spawn_in_dir(
        binary: &Path,
        size: PtySize,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size)?;

        let mut cmd = CommandBuilder::new(binary);
        for arg in args {
            cmd.arg(*arg);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        apply_child_env(&mut cmd, env);

        let child = pair.slave.spawn_command(cmd)?;
        // Drop the slave so we get EOF when the child exits.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let reader_rx = spawn_reader(reader);

        Ok(Self {
            child,
            writer,
            reader_rx,
            master: pair.master,
        })
    }

    /// Write raw key bytes into the PTY stdin.
    pub fn inject_keys(&mut self, keys: &[u8]) -> Result<()> {
        self.writer
            .write_all(keys)
            .context("failed to write to PTY stdin")
    }

    /// Resize the PTY (sends SIGWINCH to the child). Arguments are `(rows, cols)`.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }

    /// Drain the reader channel, collecting all available chunks within `timeout`.
    pub fn drain_output(&self, timeout: Duration) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        let deadline = std::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.reader_rx.recv_timeout(remaining) {
                Ok(chunk) => chunks.push(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        chunks
    }

    /// Send 'q' to trigger the pager's quit handler and wait for exit.
    ///
    /// Key injection is best-effort — the child may have already exited
    /// (e.g. no ACP server), so write failures are silently ignored.
    pub fn quit(&mut self) -> Result<()> {
        let _ = self.inject_keys(keys::Q);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait()? {
                Some(_) => return Ok(()),
                None if std::time::Instant::now() >= deadline => {
                    self.child.kill()?;
                    self.child
                        .wait()
                        .context("failed to wait for pager child after kill")?;
                    return Ok(());
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// Receive a single chunk from the reader channel, blocking up to `timeout`.
    ///
    /// Returns `None` on timeout or channel disconnect (child exited).
    /// Use this instead of [`drain_output`] when timing accuracy matters —
    /// processing each chunk inline preserves inter-chunk timing.
    pub fn recv_chunk(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.reader_rx.recv_timeout(timeout).ok()
    }

    /// Check whether the child process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait up to `timeout` for the child to exit, returning its exit code
    /// (`None` if it's still running at the deadline). Call once and cache the
    /// result — `try_wait` reaps the child, so the status isn't re-readable.
    pub fn wait_exit_code(&mut self, timeout: Duration) -> Option<u32> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.exit_code()),
                Ok(None) if std::time::Instant::now() >= deadline => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
    }

    /// Child PID, falling back to the PTY's foreground process group.
    #[cfg(unix)]
    pub fn child_pid(&self) -> Option<u32> {
        self.child
            .process_id()
            .or_else(|| self.master.process_group_leader().map(|p| p as u32))
    }

    /// Child PID (no foreground-group fallback — ConPTY has no process groups).
    #[cfg(windows)]
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Deliver a signal directly to the child (unix), bypassing the PTY line
    /// discipline. Exercises the real SIGINT/SIGTERM/SIGHUP paths (distinct from
    /// injected Ctrl+C key bytes, which are key events under raw mode).
    /// Call before the child is reaped: a reaped pid can be reused.
    #[cfg(unix)]
    pub fn send_signal(&self, signal: i32) -> Result<()> {
        let pid = self.child_pid().context("no child pid to signal")?;
        // SAFETY: libc::kill has no memory-safety preconditions, and child_pid()
        // only yields a positive live-child pid (never the kill(0)/kill(-1) broadcast).
        let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if rc != 0 {
            return Err(anyhow::Error::from(std::io::Error::last_os_error())).context("libc::kill");
        }
        Ok(())
    }
}

impl Drop for PtyController {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Host terminal identity markers stripped from the child environment.
///
/// The pager's terminal detection
/// (`xai-grok-pager-render/src/terminal/mod.rs`:
/// `detect_terminal_brand_from_env` / `detect_byobu_from_env` /
/// `detect_multiplexer_from_env` / `detect_tmux_meta_from_env`, plus
/// `embedded_editor.rs`'s `embedded_editor_from_env`) reads all of these,
/// so any one leaking from the harness's own host terminal reclassifies the
/// child: a dev running tests inside tmux leaks `TMUX` (every cell becomes
/// the remuxed profile), inside Cursor leaks `CURSOR_TRACE_ID` (checked
/// *before* `TERM_PROGRAM`, so it overrides even a test-injected brand),
/// inside nvim's `:terminal` leaks `NVIM` (clipboard OSC 52 wrapping).
/// Keep this list in sync with the detection source above.
const HOST_TERMINAL_ENV_VARS: &[&str] = &[
    // Brand chain (detect_terminal_brand_from_env), in detection order.
    "CURSOR_TRACE_ID",
    "VSCODE_GIT_ASKPASS_MAIN",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERMINAL_EMULATOR",
    "WEZTERM_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "TERMINATOR_UUID",
    "VTE_VERSION",
    "WT_SESSION",
    // Multiplexer / Byobu markers (detect_multiplexer_from_env,
    // detect_byobu_from_env, detect_tmux_meta_from_env).
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "STY",
    "BYOBU_BACKEND",
    "BYOBU_CONFIG_DIR",
    "BYOBU_DISTRO",
    "CMUX_SOCKET_PATH",
    "CMUX_PANEL_ID",
    "CMUX_BUNDLE_ID",
    // Embedded editor markers (embedded_editor_from_env).
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "VIM_TERMINAL",
    "INSIDE_EMACS",
];

/// Prepare the child environment: fixed `TERM`, color and host-terminal
/// hygiene strips, then the caller's `env` pairs.
///
/// Strips run BEFORE the caller env is applied, preserving the contract
/// that tests may re-inject any marker (e.g. `TERM_PROGRAM=vscode`, or a
/// fake `NVIM` socket) to simulate that host — see
/// `tests/pty_e2e/doubled_lines_out_of_band_repro.rs` in the pager crate.
fn apply_child_env(cmd: &mut CommandBuilder, env: &[(&str, &str)]) {
    // Set TERM so the pager renders with full color support.
    cmd.env("TERM", "xterm-256color");
    // Strip inherited color opt-outs/overrides for the same reason: a
    // leaked NO_COLOR (common in agent/CI shells) renders the pager
    // colorless, making style-sensitive assertions (e.g. the selection
    // highlight color swap) silently untestable on some hosts. Tests may
    // re-set these via the `env` list (applied after this).
    for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        cmd.env_remove(color_var);
    }
    // Strip SSH vars inherited from the parent: the harness PTY is a
    // local terminal, but `SSH_CONNECTION`/`SSH_TTY` leaking through
    // makes the pager's terminal detector report SSH and disable the
    // drag-drop image classifier (see
    // `try_handle_dropped_paths_paste` in `agent_view.rs`).
    for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
        cmd.env_remove(ssh_var);
    }
    // Neutralize parent-terminal identity bleed: agent hosts often export
    // TERM_PROGRAM=ghostty/iTerm/etc. (and mux/editor markers) which make
    // the child pager adopt that host's key/modifier/clipboard quirks even
    // though we only set TERM above.
    for term_var in HOST_TERMINAL_ENV_VARS {
        cmd.env_remove(term_var);
    }
    for &(key, val) in env {
        cmd.env(key, val);
    }
}

/// Spawn a background thread that reads from the PTY master and sends
/// chunks over an `mpsc` channel. The reader is blocking (WezTerm pattern),
/// so it must live on its own thread.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("failed to spawn pty-reader thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every host-terminal marker the pager's detection chain reads must be
    /// stripped from the child env — polluted entries are seeded via
    /// `cmd.env` (same `CommandBuilder` map that inherited base-env entries
    /// live in, so `env_remove` takes the identical path) rather than
    /// process-global `set_var` (racy under parallel tests).
    #[test]
    fn apply_child_env_strips_all_host_terminal_markers() {
        let mut cmd = CommandBuilder::new("true");
        for var in HOST_TERMINAL_ENV_VARS {
            cmd.env(var, "polluted");
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            cmd.env(ssh_var, "polluted");
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            cmd.env(color_var, "polluted");
        }
        // Unrelated vars must survive the hygiene pass untouched.
        cmd.env("GROK_SCROLL_LOG", "/tmp/scroll.jsonl");

        apply_child_env(&mut cmd, &[]);

        for var in HOST_TERMINAL_ENV_VARS {
            assert!(
                cmd.get_env(var).is_none(),
                "host terminal marker {var} leaked into the child env"
            );
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            assert!(
                cmd.get_env(ssh_var).is_none(),
                "SSH marker {ssh_var} leaked into the child env"
            );
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            assert!(
                cmd.get_env(color_var).is_none(),
                "color override {color_var} leaked into the child env"
            );
        }
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            cmd.get_env("GROK_SCROLL_LOG").and_then(|v| v.to_str()),
            Some("/tmp/scroll.jsonl"),
            "hygiene must not touch unrelated vars"
        );
    }

    /// The documented override contract: strips run BEFORE the caller env,
    /// so tests can re-inject any marker to simulate a specific host
    /// (e.g. the fake-nvim wrapper repro or the xtversion brand fixtures).
    #[test]
    fn apply_child_env_caller_env_overrides_survive_strips() {
        let mut cmd = CommandBuilder::new("true");
        cmd.env("TMUX", "/tmp/host-tmux,999,0");
        cmd.env("CURSOR_TRACE_ID", "host-cursor");

        apply_child_env(
            &mut cmd,
            &[
                ("TERM_PROGRAM", "vscode"),
                ("NVIM", "/tmp/fake-nvim.sock"),
                ("TERM", "xterm-kitty"),
            ],
        );

        // Host pollution is gone…
        assert!(cmd.get_env("TMUX").is_none());
        assert!(cmd.get_env("CURSOR_TRACE_ID").is_none());
        // …but caller-injected markers (applied after strips) survive,
        // including overriding the harness's own TERM default.
        assert_eq!(
            cmd.get_env("TERM_PROGRAM").and_then(|v| v.to_str()),
            Some("vscode")
        );
        assert_eq!(
            cmd.get_env("NVIM").and_then(|v| v.to_str()),
            Some("/tmp/fake-nvim.sock")
        );
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-kitty")
        );
    }
}
