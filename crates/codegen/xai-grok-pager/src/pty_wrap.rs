//! Local PTY wrapper with OSC 52 clipboard interception and host image paste.
//!
//! Spawns a command inside a local pseudo-terminal, intercepts the OSC 52
//! clipboard escape sequences it emits, and writes their payload to the local
//! system clipboard. This is the engine behind `grok wrap` (see
//! [`crate::wrap_cmd`]); it makes clipboard "copy" work for programs running
//! somewhere that cannot reach the user's clipboard (containers, SSH) even when
//! the outer terminal does not handle OSC 52 itself (for example Apple
//! Terminal).
//!
//! Also consumes a private remote request OSC for host clipboard images and
//! injects a bracketed-paste response on PTY stdin (see
//! [`crate::wrap_clipboard_image`]). Trust model for auto-answering that OSC
//! (any PTY emitter can solicit the host pasteboard) is documented there.

use anyhow::Result;
use base64::Engine as _;
use std::io::Write;

/// Maximum size for a buffered escape sequence candidate (1 MiB).
///
/// This bounds the memory used while accumulating a candidate OSC 52 or DCS
/// sequence. Must be large enough to hold the base64-encoded form of
/// `MAX_CLIPBOARD_PAYLOAD` (~1.33x expansion) plus the escape envelope.
const MAX_ESC_BUFFER: usize = 1024 * 1024;

/// Maximum decoded clipboard payload size (768 KiB).
///
/// Aligned with `MAX_ESC_BUFFER`: a 768 KiB payload encodes to ~1 MiB of
/// base64, fitting within the buffer limit. Payloads larger than this are
/// unrealistic for clipboard content over SSH.
const MAX_CLIPBOARD_PAYLOAD: usize = 768 * 1024;

/// The prefix that identifies an OSC 52 sequence after the `ESC ]`.
const OSC52_PREFIX: &[u8] = b"52;";

/// The tmux DCS passthrough prefix after `ESC P`: `tmux;\x1b\x1b]`.
const TMUX_DCS_PREFIX: &[u8] = b"tmux;\x1b\x1b]";

/// Base64 engine that accepts both padded and unpadded input.
///
/// OSC 52 emitters in the wild (including some Go-based tools and terminals)
/// may omit `=` padding. Using `Indifferent` mode avoids silent decode
/// failures from legitimate clipboard sequences.
const BASE64_STANDARD_INDIFFERENT: base64::engine::GeneralPurpose =
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );

/// Run an arbitrary command inside a local PTY with OSC 52 output filtering.
///
/// This is the engine behind `grok wrap`: it spawns
/// `program` (with `args`) attached to a local pseudo-terminal, forwards the
/// outer terminal's size changes to it, and filters its output through
/// [`Osc52Filter`], which intercepts OSC 52 clipboard sequences and writes
/// their payload to the local system clipboard. All other output passes
/// through unchanged.
///
/// Returns the child exit code on success.
pub(crate) fn run_wrapped_command(program: &str, args: &[String]) -> Result<i32> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;

    // Get current terminal size.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Open PTY pair.
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Build the command.
    let mut cmd = CommandBuilder::new(program);
    args.iter().for_each(|arg| cmd.arg(arg));

    // Advertise to the wrapped program — and anything it spawns, e.g. a remote
    // `grok` reached over SSH — that its OSC 52 clipboard writes are being
    // intercepted here and copied to the real local clipboard. The inner grok
    // reads this (see `xai_grok_pager_render::clipboard::osc52_sink_active`) to
    // *trust* OSC 52 even when it can't detect an OSC-52-capable terminal,
    // which is the usual SSH case (only `TERM` propagates, so Apple Terminal /
    // unknown brands look incapable and the inner grok would otherwise report
    // "Copy failed" despite the copy actually working).
    //
    // `CommandBuilder::new` inherits the full parent environment; `env` overlays
    // these two without clearing it. The canonical `GROK_OSC52_SINK` is
    // inherited by local children; the `LC_`-prefixed alias rides the default
    // OpenSSH env forwarding (`SendEnv LANG LC_*` on the client,
    // `AcceptEnv LANG LC_*` on the server) so the signal survives the SSH hop.
    cmd.env("GROK_OSC52_SINK", "1");
    cmd.env("LC_GROK_OSC52_SINK", "1");

    // Spawn child in the PTY slave.
    let mut child = pair.slave.spawn_command(cmd)?;
    // Drop the slave so we get EOF when child exits.
    drop(pair.slave);

    // Obtain reader from the master PTY. All master *writes* (keystrokes +
    // host-image inject frames) go through one dedicated writer thread via a
    // channel. Cross-thread use of portable-pty's `Write` impl observed EIO
    // (errno 5) on macOS even for small inject payloads; confining `write_all`
    // to a single owner thread avoids that. Handles are intentionally
    // detached — `grok wrap` is short-lived and exits with the child.
    let mut pty_reader = pair.master.try_clone_reader()?;

    // NOTE: we intentionally do NOT block SIGWINCH here. The resize handler
    // (`sigwinch_loop`) installs a real signal handler via `signal-hook`,
    // which must be free to run when the signal is delivered. Blocking it and
    // waiting via `sigwait` looks correct but silently fails on macOS (see
    // `sigwinch_loop`).

    // Switch to raw mode so keystrokes pass through unchanged.
    crossterm::terminal::enable_raw_mode()?;
    let _raw_guard = RawModeGuard;

    let (write_tx, write_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    {
        let mut writer = pair.master.take_writer()?;
        std::thread::spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                // Single-thread write_all: no libc chunking. EIO here almost
                // always means the slave has closed (child exited); stop.
                if writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    // Local stdin -> writer thread.
    let stdin_tx = write_tx.clone();
    let _stdin_handle = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // SIGWINCH handling thread: resize the PTY when the outer terminal changes size.
    // We move `pair.master` here since we've already cloned the reader and
    // taken the writer above.
    //
    // Unix only: there is no SIGWINCH on Windows. On Windows `pair.master` is
    // instead kept alive inside `pair` until this function returns (after
    // `child.wait`), so the ConPTY stays open for the read loop — the OSC 52
    // clipboard bridge works identically, only live resize is unavailable.
    #[cfg(unix)]
    {
        let master = pair.master;
        std::thread::spawn(move || {
            sigwinch_loop(master);
        });
    }

    // Output forwarding with OSC 52 filtering: PTY reader -> filter -> stdout.
    // Host-image requests: spawn a short-lived worker for clipboard I/O (can be
    // hundreds of ms / osascript) then enqueue the bracketed-paste frame on the
    // writer thread (+ NL so ICANON slaves deliver without another key). Paste
    // mashing can spawn multiple workers; fine for a short-lived wrap process.
    {
        let mut stdout = std::io::stdout().lock();
        let mut filter = Osc52Filter::new().with_wrap_image_handler(move || {
            let tx = write_tx.clone();
            std::thread::spawn(move || {
                let mut bytes = host_clipboard_image_frame();
                bytes.push(b'\n');
                let _ = tx.send(bytes);
            });
        });
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let filtered = filter.feed(&buf[..n]);
                    if !filtered.is_empty() {
                        if stdout.write_all(&filtered).is_err() {
                            break;
                        }
                        let _ = stdout.flush();
                    }
                }
            }
        }
    }

    // Wait for child and extract exit code.
    let status = child.wait()?;
    let code = status.exit_code() as i32;

    Ok(code)
}

/// Wait for `SIGWINCH` and resize the PTY master to match the outer terminal.
///
/// Uses `signal-hook` to install a real signal handler (self-pipe based).
///
/// A previous implementation blocked SIGWINCH and dequeued it with `nix`'s
/// `sigwait`. That pattern is POSIX-correct but **silently fails on macOS**:
/// SIGWINCH's default disposition is "ignore", and macOS discards a blocked
/// default-ignore signal rather than leaving it pending for `sigwait`. The
/// handler never woke, so the inner PTY was never resized and the remote TUI
/// kept rendering at the original size — producing overlapping/stale frames
/// after the user resized their terminal. Installing an actual handler
/// overrides the default-ignore disposition, which is what makes it work.
#[cfg(unix)]
fn sigwinch_loop(master: Box<dyn portable_pty::MasterPty + Send>) {
    use portable_pty::PtySize;

    let mut signals = match signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]) {
        Ok(signals) => signals,
        Err(e) => {
            tracing::debug!("failed to install SIGWINCH handler: {e}");
            return;
        }
    };

    for _ in signals.forever() {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

/// Guard that restores terminal state when dropped (including on panic).
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// State machine states for the OSC 52 streaming parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    /// Normal output passthrough.
    Normal,
    /// Saw ESC (0x1b), waiting for next byte to determine sequence type.
    Esc,
    /// Inside OSC: saw `ESC ]` -- accumulating until BEL or ST.
    Osc,
    /// Inside DCS: saw `ESC P` -- checking for tmux passthrough prefix.
    Dcs,
    /// Inside DCS tmux passthrough, accumulating inner OSC 52.
    DcsTmuxOsc,
    /// Saw ESC inside an OSC, could be ST terminator (`ESC \`).
    OscEsc,
    /// Saw ESC inside a DCS tmux OSC, could be inner ST or DCS ST.
    DcsTmuxOscEsc,
}

/// Streaming filter that intercepts OSC 52 clipboard sequences from PTY
/// output and sends their decoded payload to the local clipboard.
///
/// All non-OSC-52 bytes pass through unchanged. The parser handles sequences
/// split across arbitrary byte boundaries.
/// Clipboard sink type: a boxed closure that receives decoded clipboard data.
type ClipboardSink = Box<dyn FnMut(&[u8])>;

type WrapImageRequestHandler = Box<dyn FnMut()>;

struct Osc52Filter {
    state: FilterState,
    buf: Vec<u8>,
    clipboard_sink: ClipboardSink,
    wrap_image_handler: Option<WrapImageRequestHandler>,
}

impl Osc52Filter {
    /// Create a new filter that sends clipboard data to the system clipboard.
    fn new() -> Self {
        Self {
            state: FilterState::Normal,
            buf: Vec::new(),
            clipboard_sink: Box::new(set_local_clipboard),
            wrap_image_handler: None,
        }
    }

    fn with_wrap_image_handler(mut self, handler: impl FnMut() + 'static) -> Self {
        self.wrap_image_handler = Some(Box::new(handler));
        self
    }

    /// Create a filter with a custom clipboard sink (for testing).
    #[cfg(test)]
    fn with_sink(sink: impl FnMut(&[u8]) + 'static) -> Self {
        Self {
            state: FilterState::Normal,
            buf: Vec::new(),
            clipboard_sink: Box::new(sink),
            wrap_image_handler: None,
        }
    }

    /// Process a chunk of bytes from PTY output.
    ///
    /// Returns bytes that should be written to stdout. OSC 52 clipboard
    /// sequences are consumed (not included in the output) and their decoded
    /// payload is sent to the clipboard sink.
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(data.len());
        for &byte in data {
            match self.state {
                FilterState::Normal => {
                    if byte == 0x1b {
                        self.state = FilterState::Esc;
                        self.buf.clear();
                        self.buf.push(byte);
                    } else {
                        output.push(byte);
                    }
                }
                FilterState::Esc => {
                    self.buf.push(byte);
                    match byte {
                        b']' => self.state = FilterState::Osc,
                        b'P' => self.state = FilterState::Dcs,
                        _ => {
                            // Not an OSC or DCS -- flush buffer and continue.
                            output.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::Normal;
                        }
                    }
                }
                FilterState::Osc => {
                    self.buf.push(byte);
                    match byte {
                        // BEL terminates the OSC sequence.
                        0x07 => {
                            if !self.try_handle_consumed_osc() {
                                output.extend_from_slice(&self.buf);
                            }
                            self.buf.clear();
                            self.state = FilterState::Normal;
                        }
                        // ESC could be the start of ST (ESC \).
                        0x1b => {
                            self.state = FilterState::OscEsc;
                        }
                        _ => {}
                    }
                }
                FilterState::OscEsc => {
                    self.buf.push(byte);
                    if byte == b'\\' {
                        // ST terminator: ESC \.
                        if !self.try_handle_consumed_osc() {
                            output.extend_from_slice(&self.buf);
                        }
                        self.buf.clear();
                        self.state = FilterState::Normal;
                    } else {
                        // Not ST -- continue accumulating in Osc state.
                        // The ESC we saw might be part of the payload in some
                        // broken sequence; just keep buffering.
                        self.state = FilterState::Osc;
                    }
                }
                FilterState::Dcs => {
                    self.buf.push(byte);
                    // buf starts with \x1bP so tmux prefix bytes start at offset 2.
                    let prefix_pos = self.buf.len() - 2;
                    if prefix_pos <= TMUX_DCS_PREFIX.len() {
                        if TMUX_DCS_PREFIX[prefix_pos - 1] == byte {
                            if prefix_pos == TMUX_DCS_PREFIX.len() {
                                // Full tmux prefix matched: \x1bPtmux;\x1b\x1b]
                                self.state = FilterState::DcsTmuxOsc;
                            }
                            // else keep matching prefix
                        } else {
                            // Prefix mismatch: not a tmux passthrough, flush.
                            output.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::Normal;
                        }
                    } else {
                        // Exceeded prefix length without matching; flush.
                        output.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::DcsTmuxOsc => {
                    self.buf.push(byte);
                    match byte {
                        // BEL terminates the inner OSC.
                        0x07 => {
                            // Inner OSC is done but we still need DCS ST
                            // (ESC \) to close the tmux wrapper.
                            // Remain in this state to catch the ESC.
                        }
                        0x1b => {
                            self.state = FilterState::DcsTmuxOscEsc;
                        }
                        _ => {}
                    }
                }
                FilterState::DcsTmuxOscEsc => {
                    self.buf.push(byte);
                    if byte == b'\\' {
                        // DCS ST: ESC \. The full tmux-wrapped sequence is done.
                        if !self.try_handle_tmux_osc52() {
                            output.extend_from_slice(&self.buf);
                        }
                        self.buf.clear();
                        self.state = FilterState::Normal;
                    } else {
                        // Not ST. Continue accumulating in DcsTmuxOsc.
                        self.state = FilterState::DcsTmuxOsc;
                    }
                }
            }

            // Guard: if the buffer grows beyond the limit, flush and reset.
            if self.buf.len() > MAX_ESC_BUFFER {
                output.extend_from_slice(&self.buf);
                self.buf.clear();
                self.state = FilterState::Normal;
            }
        }
        output
    }

    /// Handle OSC 52 clipboard or wrap image request; `true` if consumed.
    fn try_handle_consumed_osc(&mut self) -> bool {
        let body = self.buf[2..].to_vec();
        let body = strip_osc_terminator(&body);
        if self.try_handle_wrap_image_request(body) {
            return true;
        }
        self.extract_and_set_clipboard(body)
    }

    fn try_handle_wrap_image_request(&mut self, body: &[u8]) -> bool {
        if body != crate::wrap_clipboard_image::REQUEST_BODY {
            return false;
        }
        if let Some(handler) = self.wrap_image_handler.as_mut() {
            handler();
        }
        true
    }

    /// Try to handle the buffered bytes as a tmux-wrapped OSC 52 sequence.
    ///
    /// Expected buffer format:
    ///   `\x1bPtmux;\x1b\x1b]52;<sel>;<base64>\x07\x1b\\`
    ///
    /// Returns `true` if the sequence was a valid OSC 52 and was consumed.
    fn try_handle_tmux_osc52(&mut self) -> bool {
        // Strip the DCS tmux prefix: \x1bPtmux;\x1b\x1b]  (total 9 bytes)
        // and the DCS ST terminator: \x1b\  (2 bytes at the end).
        // Copy the body to avoid borrowing self.buf while calling &mut self.
        let prefix_len = 2 + TMUX_DCS_PREFIX.len(); // \x1bP + tmux;\x1b\x1b]
        if self.buf.len() < prefix_len + 2 {
            return false;
        }
        let body = self.buf[prefix_len..self.buf.len() - 2].to_vec(); // strip DCS ST
        let body = strip_osc_terminator(&body); // strip inner BEL if present
        self.extract_and_set_clipboard(body)
    }

    /// Parse OSC 52 body (`52;<sel>;<base64>`), decode, and set clipboard.
    ///
    /// Returns `true` if successfully handled.
    fn extract_and_set_clipboard(&mut self, body: &[u8]) -> bool {
        // Must start with "52;"
        if !body.starts_with(OSC52_PREFIX) {
            return false;
        }
        let after_52 = &body[OSC52_PREFIX.len()..];

        // Find the selection parameter separator (next ';').
        let payload_start = match after_52.iter().position(|&b| b == b';') {
            Some(pos) => pos + 1,
            None => return false,
        };
        let b64_payload = &after_52[payload_start..];

        // Decode base64.
        let decoded = match BASE64_STANDARD_INDIFFERENT.decode(b64_payload) {
            Ok(data) => data,
            Err(_) => return false,
        };

        // Check payload size limit.
        if decoded.len() > MAX_CLIPBOARD_PAYLOAD {
            tracing::warn!(
                "OSC 52 payload too large ({} bytes), ignoring",
                decoded.len()
            );
            return false;
        }

        (self.clipboard_sink)(&decoded);
        true
    }
}

/// Strip the OSC terminator from the end of a body slice.
///
/// Removes trailing BEL (`\x07`) or ST (`\x1b\x5c`) if present.
fn strip_osc_terminator(body: &[u8]) -> &[u8] {
    if body.ends_with(&[0x1b, b'\\']) {
        &body[..body.len() - 2]
    } else if body.ends_with(&[0x07]) {
        &body[..body.len() - 1]
    } else {
        body
    }
}

/// Write decoded clipboard payload to the local system clipboard.
///
/// Delegates to [`xai_grok_shell::util::clipboard::set_text`] which uses
/// `pbcopy` on macOS and `arboard` elsewhere. Failures are logged but do
/// not propagate -- clipboard access is best-effort.
fn set_local_clipboard(data: &[u8]) {
    let text = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("OSC 52 payload is not valid UTF-8: {e}");
            return;
        }
    };
    if let Err(e) = xai_grok_shell::util::clipboard::set_text(text) {
        tracing::warn!("clipboard copy failed: {e}");
    }
}

/// Encode a host clipboard image (or NONE) as a bracketed-paste frame.
fn host_clipboard_image_frame() -> Vec<u8> {
    let image = xai_grok_pager_render::clipboard::system_clipboard_get_image();
    crate::wrap_clipboard_image::encode_wrap_image_response(image.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Helper: run data through the filter with a capturing clipboard sink.
    /// Returns (stdout_output, captured_clipboard_payloads).
    fn filter_output(input: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let clips = Rc::new(RefCell::new(Vec::new()));
        let clips_clone = Rc::clone(&clips);
        let mut filter = Osc52Filter::with_sink(move |data: &[u8]| {
            clips_clone.borrow_mut().push(data.to_vec());
        });
        let output = filter.feed(input);
        let captured = clips.borrow().clone();
        (output, captured)
    }

    /// Helper: run data through the filter in multiple small chunks.
    fn filter_output_chunked(input: &[u8], chunk_size: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let clips = Rc::new(RefCell::new(Vec::new()));
        let clips_clone = Rc::clone(&clips);
        let mut filter = Osc52Filter::with_sink(move |data: &[u8]| {
            clips_clone.borrow_mut().push(data.to_vec());
        });
        let mut output = Vec::new();
        for chunk in input.chunks(chunk_size) {
            output.extend_from_slice(&filter.feed(chunk));
        }
        let captured = clips.borrow().clone();
        (output, captured)
    }

    /// Encode text as a plain OSC 52 sequence with BEL terminator.
    fn make_osc52_bel(text: &str) -> Vec<u8> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        format!("\x1b]52;c;{b64}\x07").into_bytes()
    }

    /// Encode text as a plain OSC 52 sequence with ST terminator.
    fn make_osc52_st(text: &str) -> Vec<u8> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        format!("\x1b]52;c;{b64}\x1b\\").into_bytes()
    }

    /// Encode text as a tmux-wrapped OSC 52 sequence.
    fn make_osc52_tmux(text: &str) -> Vec<u8> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\").into_bytes()
    }

    #[test]
    fn osc52_normal_text_unchanged() {
        let input = b"Hello, world!\r\n";
        let (output, clips) = filter_output(input);
        assert_eq!(output, input);
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_ansi_escapes_pass_through() {
        // SGR color: ESC [ 31 m
        let input = b"\x1b[31mred text\x1b[0m";
        let (output, clips) = filter_output(input);
        assert_eq!(output, input.as_slice());
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_plain_bel_terminated() {
        let seq = make_osc52_bel("hello");
        let (output, clips) = filter_output(&seq);
        assert!(
            output.is_empty(),
            "OSC 52 should be consumed, got: {output:?}"
        );
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"hello");
    }

    #[test]
    fn osc52_plain_st_terminated() {
        let seq = make_osc52_st("hello");
        let (output, clips) = filter_output(&seq);
        assert!(
            output.is_empty(),
            "OSC 52 should be consumed, got: {output:?}"
        );
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"hello");
    }

    #[test]
    fn osc52_with_s0_selection() {
        // Selection parameter "s0" instead of "c".
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"clipboard data");
        let seq = format!("\x1b]52;s0;{b64}\x07").into_bytes();
        let (output, clips) = filter_output(&seq);
        assert!(output.is_empty());
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"clipboard data");
    }

    #[test]
    fn osc52_tmux_wrapped() {
        let seq = make_osc52_tmux("hello from tmux");
        let (output, clips) = filter_output(&seq);
        assert!(
            output.is_empty(),
            "tmux OSC 52 should be consumed, got: {output:?}"
        );
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"hello from tmux");
    }

    #[test]
    fn osc52_surrounded_by_text() {
        let mut input = b"before ".to_vec();
        input.extend_from_slice(&make_osc52_bel("copied"));
        input.extend_from_slice(b" after");
        let (output, clips) = filter_output(&input);
        assert_eq!(output, b"before  after");
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"copied");
    }

    #[test]
    fn osc52_multiple_sequences() {
        let mut input = make_osc52_bel("first");
        input.extend_from_slice(b"gap");
        input.extend_from_slice(&make_osc52_st("second"));
        let (output, clips) = filter_output(&input);
        assert_eq!(output, b"gap");
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0], b"first");
        assert_eq!(clips[1], b"second");
    }

    #[test]
    fn osc52_split_across_chunks() {
        let seq = make_osc52_bel("split test");
        // Feed one byte at a time.
        let (output, clips) = filter_output_chunked(&seq, 1);
        assert!(output.is_empty(), "should be consumed even byte-by-byte");
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"split test");
    }

    #[test]
    fn osc52_split_at_various_sizes() {
        let seq = make_osc52_st("chunk test");
        for chunk_size in 2..=seq.len() {
            let (output, clips) = filter_output_chunked(&seq, chunk_size);
            assert!(
                output.is_empty(),
                "chunk_size={chunk_size}: should be consumed"
            );
            assert_eq!(clips.len(), 1, "chunk_size={chunk_size}: expected 1 clip");
            assert_eq!(clips[0], b"chunk test");
        }
    }

    #[test]
    fn osc52_tmux_split_across_chunks() {
        let seq = make_osc52_tmux("tmux split");
        let (output, clips) = filter_output_chunked(&seq, 3);
        assert!(output.is_empty());
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"tmux split");
    }

    #[test]
    fn osc52_invalid_base64_passes_through() {
        // Invalid base64 payload: "!!!" is not valid base64.
        let seq = b"\x1b]52;c;!!!\x07";
        let (output, clips) = filter_output(seq);
        assert_eq!(output, seq.as_slice(), "invalid base64 should pass through");
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_non_52_osc_passes_through() {
        // OSC 0 (window title) should pass through.
        let seq = b"\x1b]0;my title\x07";
        let (output, clips) = filter_output(seq);
        assert_eq!(output, seq.as_slice());
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_non_52_osc_st_passes_through() {
        // OSC 0 with ST terminator.
        let seq = b"\x1b]0;my title\x1b\\";
        let (output, clips) = filter_output(seq);
        assert_eq!(output, seq.as_slice());
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_oversized_buffer_flushes() {
        // Build a sequence that exceeds MAX_ESC_BUFFER.
        let mut seq = b"\x1b]52;c;".to_vec();
        // Fill with valid base64 chars until we exceed the limit.
        seq.resize(MAX_ESC_BUFFER + 100, b'A');
        seq.push(0x07);

        let (output, clips) = filter_output(&seq);
        // The oversized sequence should have been flushed through.
        assert!(
            !output.is_empty(),
            "oversized sequence should flush through"
        );
        assert!(
            clips.is_empty(),
            "oversized sequence should not set clipboard"
        );
    }

    #[test]
    fn osc52_empty_payload() {
        // Empty base64 payload should still work (copies empty string).
        let seq = b"\x1b]52;c;\x07";
        let (output, clips) = filter_output(seq);
        assert!(output.is_empty());
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0], b"");
    }

    #[test]
    fn osc52_non_tmux_dcs_passes_through() {
        // A DCS that doesn't start with the tmux prefix should flush.
        let seq = b"\x1bPother;stuff\x1b\\";
        let (output, clips) = filter_output(seq);
        // The flush happens when the prefix mismatch is detected.
        assert!(!output.is_empty(), "non-tmux DCS should pass through");
        assert!(clips.is_empty());
    }

    #[test]
    fn osc52_missing_selection_separator() {
        // No second ';' after "52;" -- missing selection param separator.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"data");
        let seq = format!("\x1b]52;{b64}\x07").into_bytes();
        // This has "52;" followed by base64 with no second ';'. The parser
        // will treat everything after "52;" up to the next ';' as the
        // selection param. If there's no ';', it returns false.
        let (output, clips) = filter_output(&seq);
        assert_eq!(output, seq, "should pass through without second ';'");
        assert!(clips.is_empty());
    }

    #[test]
    fn wrap_image_request_consumed_and_handler_runs() {
        let calls = Rc::new(RefCell::new(0usize));
        let calls_clone = Rc::clone(&calls);
        let clips = Rc::new(RefCell::new(Vec::new()));
        let clips_clone = Rc::clone(&clips);
        let mut filter = Osc52Filter::with_sink(move |data: &[u8]| {
            clips_clone.borrow_mut().push(data.to_vec());
        })
        .with_wrap_image_handler(move || {
            *calls_clone.borrow_mut() += 1;
        });
        let mut input = b"before".to_vec();
        input.extend_from_slice(&crate::wrap_clipboard_image::request_osc_bytes());
        input.extend_from_slice(b"after");
        let output = filter.feed(&input);
        assert_eq!(output, b"beforeafter");
        assert_eq!(*calls.borrow(), 1);
        assert!(clips.borrow().is_empty());
    }

    #[test]
    fn wrap_image_request_split_across_chunks() {
        let calls = Rc::new(RefCell::new(0usize));
        let calls_clone = Rc::clone(&calls);
        let mut filter = Osc52Filter::with_sink(|_| {}).with_wrap_image_handler(move || {
            *calls_clone.borrow_mut() += 1;
        });
        let seq = crate::wrap_clipboard_image::request_osc_bytes();
        let mut output = Vec::new();
        for chunk in seq.chunks(3) {
            output.extend_from_slice(&filter.feed(chunk));
        }
        assert!(output.is_empty(), "request OSC must be fully consumed");
        assert_eq!(*calls.borrow(), 1);
    }
}
