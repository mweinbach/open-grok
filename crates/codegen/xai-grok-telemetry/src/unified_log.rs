//! Centralized unified log for cross-component session observability.
//!
//! Shell writes directly via [`emit()`]. Pager and desktop forward entries
//! over ACP (`x.ai/log` notifications); shell receives them in
//! [`ingest_client_entries()`] and writes on their behalf.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use xai_grok_config::grok_home;

/// Binary version stamped into every log entry. Set once at startup via
/// [`set_version()`]; entries emitted before that get `None`.
static VERSION: OnceLock<String> = OnceLock::new();

/// Register the binary version (e.g. shell's `CARGO_PKG_VERSION`).
/// Call once at startup; subsequent calls are no-ops.
pub fn set_version(ver: &str) {
    let _ = VERSION.set(ver.to_owned());
}

pub const LOG_DIR: &str = "logs";
const LOG_FILE: &str = "unified.jsonl";
pub const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// ACP method name for unified log notifications.
pub const LOG_METHOD: &str = "x.ai/log";

// ---------------------------------------------------------------------------
// Log entry types
// ---------------------------------------------------------------------------

/// Log level for a unified log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, Serialize, Deserialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// Component that produced a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, Serialize, Deserialize)]
pub enum LogSource {
    #[strum(serialize = "shell")]
    #[serde(rename = "shell")]
    Shell,
    #[strum(serialize = "grok-pager")]
    #[serde(rename = "grok-pager")]
    GrokPager,
    #[strum(serialize = "grok-desktop")]
    #[serde(rename = "grok-desktop")]
    GrokDesktop,
}

/// A single unified log entry, written as one JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// RFC 3339 timestamp (millisecond precision, UTC).
    pub ts: String,
    /// Component that produced the entry.
    pub src: LogSource,
    /// OS process id of the producer. Critical for cross-process trace
    /// reconstruction because shell/pager/desktop all append to the same
    /// `unified.jsonl`, so multiple shell processes' lines interleave
    /// indistinguishably without it.
    ///
    /// `Option<u32>` is for wire compatibility only -- shell, pager, and
    /// desktop all stamp `Some(std::process::id())` at emit time. A
    /// `None` here means the entry came from an older client/server that
    /// predates this field; current code never emits one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Binary version (e.g. `"0.1.211"`). Stamped by [`set_version()`]
    /// at startup so stale zombie processes are identifiable in logs.
    /// `None` for entries from older binaries that predate this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    /// Log level.
    pub lvl: LogLevel,
    /// Session ID, if one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Human-readable message.
    pub msg: String,
    /// Structured context fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<serde_json::Value>,
}

/// Wire format for the `x.ai/log` ACP notification params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogNotificationParams {
    /// Source component identifier.
    pub src: LogSource,
    pub entries: Vec<ClientLogEntry>,
}

/// Entry as sent by a client (no `src` field -- shell stamps it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientLogEntry {
    pub ts: String,
    /// Client process id. Stamped by the client when the entry is
    /// created; preserved through ACP forwarding so the on-disk log
    /// reflects the originating process.
    ///
    /// Optional only for wire compatibility with clients that predate
    /// this field; in-tree clients always populate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Binary version. Optional for wire compatibility with older clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    pub lvl: LogLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

struct LogWriter {
    file: File,
    path: PathBuf,
    written: u64,
}

static WRITER: LazyLock<Mutex<Option<LogWriter>>> = LazyLock::new(|| Mutex::new(open_writer()));

fn log_path() -> PathBuf {
    grok_home().join(LOG_DIR).join(LOG_FILE)
}

pub fn file_size(path: &std::path::Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn open_writer() -> Option<LogWriter> {
    let path = log_path();
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!("[unified_log] failed to create log dir: {e}");
        return None;
    }

    if file_size(&path) >= MAX_SIZE {
        trim_file(&path);
    }

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Some(LogWriter {
            written: file_size(&path),
            file,
            path,
        }),
        Err(e) => {
            tracing::warn!("[unified_log] failed to open log file: {e}");
            None
        }
    }
}

fn write_lines(lines: &[u8]) {
    let Ok(mut guard) = WRITER.lock() else { return };
    let writer = match guard.as_mut() {
        Some(w) => w,
        None => return,
    };

    let len = lines.len() as u64;
    if let Err(e) = writer.file.write_all(lines) {
        tracing::warn!("[unified_log] write failed: {e}");
        return;
    }
    writer.written += len;

    // Trim under the lock to avoid a race where concurrent writers see stale
    // state between drop + re-acquire. Trim is fast (~2.5 MB read+write) and
    // this is a low-volume diagnostic log.
    if writer.written >= MAX_SIZE {
        let _ = writer.file.flush();
        trim_file(&writer.path);
        if let Ok(new_file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&writer.path)
        {
            writer.file = new_file;
            writer.written = file_size(&writer.path);
        }
    }
}

fn write_entry(entry: &LogEntry) {
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    write_lines(&line);
}

/// Drop the oldest lines from the file, keeping roughly the last half.
///
/// Uses write-to-temp + rename so a crash mid-trim cannot lose the entire log.
pub fn trim_file(path: &std::path::Path) {
    let Ok(data) = fs::read(path) else { return };
    let half = data.len() / 2;
    // Find the first newline after the halfway point so we don't split a line.
    let start = match data[half..].iter().position(|&b| b == b'\n') {
        Some(pos) => half + pos + 1,
        None => return,
    };
    let tmp = path.with_extension("jsonl.tmp");
    if fs::write(&tmp, &data[start..]).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return a new timestamp string in the unified log format.
fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Emit a log entry from shell itself.
pub fn emit(lvl: LogLevel, msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    let entry = LogEntry {
        ts: now_ts(),
        src: LogSource::Shell,
        pid: Some(std::process::id()),
        ver: VERSION.get().cloned(),
        lvl,
        sid: sid.map(Into::into),
        msg: msg.into(),
        ctx,
    };
    write_entry(&entry);
}

/// Ingest a batch of log entries from a client (pager or desktop).
///
/// Called by the `x.ai/log` notification handler. Entries from
/// [`LogSource::Shell`] are rejected to prevent spoofing.
pub fn ingest_client_entries(src: LogSource, entries: &[ClientLogEntry]) {
    if matches!(src, LogSource::Shell) || entries.is_empty() {
        return;
    }
    // Serialize all entries up front, then write in a single lock acquisition.
    let mut buf = Vec::new();
    for client_entry in entries {
        let entry = LogEntry {
            ts: client_entry.ts.clone(),
            src,
            pid: client_entry.pid,
            ver: client_entry.ver.clone(),
            lvl: client_entry.lvl,
            sid: client_entry.sid.clone(),
            msg: client_entry.msg.clone(),
            ctx: client_entry.ctx.clone(),
        };
        if let Ok(mut line) = serde_json::to_vec(&entry) {
            line.push(b'\n');
            buf.extend_from_slice(&line);
        }
    }
    if !buf.is_empty() {
        write_lines(&buf);
    }
}

/// Convenience: emit an info-level entry from shell.
pub fn info(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Info, msg, sid, ctx);
}

/// Convenience: emit a warn-level entry from shell.
pub fn warn(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Warn, msg, sid, ctx);
}

/// Convenience: emit an error-level entry from shell.
pub fn error(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Error, msg, sid, ctx);
}

/// Convenience: emit a debug-level entry from shell.
pub fn debug(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Debug, msg, sid, ctx);
}

/// Read the current unified log file and return its contents.
///
/// Returns `None` if the log file doesn't exist or can't be read.
/// Used by diagnostic uploads to capture the log state at a point in time.
pub fn snapshot_log() -> Option<Vec<u8>> {
    let path = log_path();
    // Flush pending writes before reading.
    if let Ok(mut guard) = WRITER.lock()
        && let Some(ref mut w) = *guard
    {
        let _ = w.file.flush();
    }
    // Lock released intentionally — snapshot is approximate.
    match fs::read(&path) {
        Ok(data) if !data.is_empty() => Some(data),
        _ => None,
    }
}

/// Read the unified log and return only entries belonging to the given session.
///
/// Parses each JSONL line, keeps entries where `"sid"` matches `session_id`,
/// and returns the filtered lines as JSONL bytes. Returns `None` if the log
/// is empty or contains no entries for this session.
pub fn snapshot_session_log(session_id: &str) -> Option<Vec<u8>> {
    let path = log_path();
    if let Ok(mut guard) = WRITER.lock()
        && let Some(ref mut w) = *guard
    {
        let _ = w.file.flush();
    }
    let data = match fs::read(&path) {
        Ok(d) if !d.is_empty() => d,
        _ => return None,
    };
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line)
            && entry.get("sid").and_then(|v| v.as_str()) == Some(session_id)
        {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_entry_serializes_minimal() {
        let entry = LogEntry {
            ts: "2025-07-14T10:30:00.123Z".into(),
            src: LogSource::Shell,
            pid: None,
            ver: None,
            lvl: LogLevel::Info,
            sid: None,
            msg: "test".into(),
            ctx: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("sid"));
        assert!(!json.contains("ctx"));
        assert!(!json.contains("pid"));
        assert!(!json.contains("ver"));
        assert!(json.contains("\"src\":\"shell\""));
    }

    #[test]
    fn log_entry_serializes_full() {
        let entry = LogEntry {
            ts: "2025-07-14T10:30:00.123Z".into(),
            src: LogSource::GrokPager,
            pid: Some(4242),
            ver: Some("0.1.211".into()),
            lvl: LogLevel::Warn,
            sid: Some("abc123".into()),
            msg: "connection lost".into(),
            ctx: Some(serde_json::json!({"retry": 3})),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"sid\":\"abc123\""));
        assert!(json.contains("\"retry\":3"));
        assert!(json.contains("\"pid\":4242"));
        assert!(json.contains("\"ver\":\"0.1.211\""));
    }

    #[test]
    fn client_entry_round_trip() {
        let wire = r#"{"ts":"2025-07-14T10:30:00.123Z","lvl":"info","msg":"hello"}"#;
        let entry: ClientLogEntry = serde_json::from_str(wire).unwrap();
        assert_eq!(entry.msg, "hello");
        assert!(entry.sid.is_none());
        assert!(entry.ctx.is_none());
    }

    #[test]
    fn trim_file_keeps_recent_half() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &content).unwrap();
        trim_file(&path);
        let result = fs::read_to_string(&path).unwrap();
        // Should keep roughly the second half, starting at a line boundary.
        assert!(!result.contains("line 0"));
        assert!(result.contains("line 9"));
        assert!(result.len() < content.len());
        // Every line should be complete (no partial lines).
        for line in result.lines() {
            assert!(line.starts_with("line "));
        }
    }

    #[test]
    fn trim_file_no_newline_in_second_half_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = "single-line-no-newline";
        fs::write(&path, content).unwrap();
        trim_file(&path);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn trim_file_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        trim_file(&path);
        assert!(!path.exists());
    }

    #[test]
    fn ingest_rejects_shell_src() {
        ingest_client_entries(
            LogSource::Shell,
            &[ClientLogEntry {
                ts: "2025-01-01T00:00:00.000Z".into(),
                pid: None,
                ver: None,
                lvl: LogLevel::Info,
                sid: None,
                msg: "sneaky".into(),
                ctx: None,
            }],
        );
    }

    #[test]
    fn unknown_src_rejected_at_deserialization() {
        for bad in &[
            r#"{"src":"evil","entries":[]}"#,
            r#"{"src":"","entries":[]}"#,
            r#"{"src":"GROK-PAGER","entries":[]}"#,
        ] {
            assert!(serde_json::from_str::<LogNotificationParams>(bad).is_err());
        }
    }

    #[test]
    fn notification_params_round_trip() {
        let params = LogNotificationParams {
            src: LogSource::GrokPager,
            entries: vec![
                ClientLogEntry {
                    ts: "2025-07-14T10:30:00.123Z".into(),
                    pid: Some(1234),
                    ver: None,
                    lvl: LogLevel::Info,
                    sid: Some("s1".into()),
                    msg: "first".into(),
                    ctx: None,
                },
                ClientLogEntry {
                    ts: "2025-07-14T10:30:00.456Z".into(),
                    pid: Some(1234),
                    ver: Some("0.1.211".into()),
                    lvl: LogLevel::Error,
                    sid: None,
                    msg: "second".into(),
                    ctx: Some(serde_json::json!({"code": 42})),
                },
            ],
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: LogNotificationParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].msg, "first");
        assert_eq!(parsed.entries[1].msg, "second");
    }
}
