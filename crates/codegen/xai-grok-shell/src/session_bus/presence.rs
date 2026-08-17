//! Presence files: the shared rendezvous of the session bus.
//!
//! Each hosting process owns one JSON file under
//! `$OPENGROK_HOME/session-bus/` named `p<pid>-<rand8>.json`, rewritten
//! atomically (tmp + rename) on heartbeat and on session state changes. The
//! file lists the live root sessions hosted by that process plus the path of
//! its IPC socket. Readers treat corrupt files as absent and garbage-collect
//! entries whose heartbeat is too old or whose PID is gone.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::protocol::PROTOCOL_VERSION;

/// Directory under `$OPENGROK_HOME` holding presence files and sockets.
pub const BUS_DIR_NAME: &str = "session-bus";

/// Presence freshness: a file whose heartbeat is older than this is stale.
pub const STALE_TTL_MS: u64 = 20_000;

pub fn session_bus_dir(home: &Path) -> PathBuf {
    home.join(BUS_DIR_NAME)
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A process's announcement: its socket and the live sessions it hosts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresenceFile {
    /// File stem (`p<pid>-<rand8>`); unique per hosting instance, so two
    /// agent instances in one OS process (tests) never collide.
    pub instance_id: String,
    pub pid: u32,
    /// Path of the process's local IPC socket (on Windows this is the
    /// pseudo-path hashed into a named-pipe name by the transport).
    pub socket_path: String,
    pub protocol_version: u32,
    pub heartbeat_at_ms: u64,
    pub started_at_ms: u64,
    pub sessions: Vec<PresenceSession>,
}

/// One live root session hosted by a process. Field updates come from the
/// session itself; the heartbeat rewrites the enclosing file periodically.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresenceSession {
    pub session_id: String,
    /// Working directory of the session (canonical).
    pub cwd: String,
    /// Display name for the project (cwd file name).
    pub project_name: String,
    pub model_id: Option<String>,
    pub title: Option<String>,
    /// `"busy"` (turn running) or `"idle"`.
    pub status: String,
    pub updated_at_ms: u64,
}

/// A live session merged from the presence directory, carrying the routing
/// info needed to reach its host process.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    pub session: PresenceSession,
    pub socket_path: String,
    pub pid: u32,
    pub instance_id: String,
    /// True when more than one presence file claimed this session id
    /// (e.g. a session resumed in a second process). The freshest
    /// heartbeat won; the conflict is surfaced, not hidden.
    pub conflict: bool,
}

/// Build a fresh instance id for this process (`p<pid>-<rand8>`).
pub fn new_instance_id() -> String {
    // UUIDv7's leading hex digits are its millisecond timestamp; slice the
    // random tail so same-pid hosts started in the same millisecond (tests,
    // rapid in-process restarts) still get distinct ids.
    let rand = uuid::Uuid::now_v7().simple().to_string()[24..32].to_string();
    format!("p{}-{}", std::process::id(), rand)
}

/// Whether a directory entry name is a presence file we own (only such
/// files are ever written or garbage-collected here).
fn is_presence_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some(rest) = stem.strip_prefix('p') else {
        return false;
    };
    let Some((pid, rand)) = rest.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && rand.len() == 8
        && rand.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Write a presence file atomically (tmp + rename).
pub fn write_presence_atomic(dir: &Path, file: &PresenceFile) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("{}.json", file.instance_id));
    let tmp_path = dir.join(format!("{}.json.tmp", file.instance_id));
    let json =
        serde_json::to_string(file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp_path, json.as_bytes())?;
    fs::rename(&tmp_path, &final_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })
}

/// Read one presence file. Corrupt or unreadable files return `None`
/// (treated as absent; GC removes them by age).
pub fn read_presence(path: &Path) -> Option<PresenceFile> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice::<PresenceFile>(&bytes) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "corrupt session-bus presence file; ignoring");
            None
        }
    }
}

/// Remove presence entries whose PID is dead or whose heartbeat is older
/// than [`STALE_TTL_MS`]. Corrupt files whose mtime is also past the TTL
/// are removed; corrupt-but-recent files are left alone (a concurrent
/// writer may be mid-rename). Returns the removed instance ids.
pub fn gc_stale(dir: &Path, now: u64) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_presence_file_name(&name) {
            continue;
        }
        let path = entry.path();
        let mtime_ms = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let too_old = now.saturating_sub(mtime_ms) > STALE_TTL_MS;
        match read_presence(&path) {
            Some(file) => {
                let pid_dead = file.pid != std::process::id() && !pid_is_alive(file.pid);
                let beat_stale = now.saturating_sub(file.heartbeat_at_ms) > STALE_TTL_MS;
                if pid_dead || beat_stale || too_old {
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(dir.join(format!("{}.sock", file.instance_id)));
                    removed.push(file.instance_id);
                }
            }
            None if too_old => {
                let _ = fs::remove_file(&path);
            }
            None => {}
        }
    }
    removed
}

/// Scan the presence directory for live sessions, dropping stale entries
/// (but not deleting them — deletion is [`gc_stale`]'s job). Duplicate
/// session ids across files resolve to the freshest heartbeat and are
/// flagged `conflict`.
pub fn live_sessions_in(dir: &Path, now: u64) -> Vec<LiveSession> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut best: std::collections::HashMap<String, LiveSession> = std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_presence_file_name(&name) {
            continue;
        }
        let Some(file) = read_presence(&entry.path()) else {
            continue;
        };
        let fresh = now.saturating_sub(file.heartbeat_at_ms) <= STALE_TTL_MS;
        let pid_live = file.pid == std::process::id() || pid_is_alive(file.pid);
        if !fresh || !pid_live {
            continue;
        }
        for session in file.sessions {
            let live = LiveSession {
                session,
                socket_path: file.socket_path.clone(),
                pid: file.pid,
                instance_id: file.instance_id.clone(),
                conflict: false,
            };
            match best.entry(live.session.session_id.clone()) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(live);
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let incumbent = o.get_mut();
                    if live.session.updated_at_ms > incumbent.session.updated_at_ms {
                        *incumbent = live;
                    }
                    incumbent.conflict = true;
                }
            }
        }
    }
    let mut all: Vec<LiveSession> = best.into_values().collect();
    all.sort_by(|a, b| {
        b.session
            .updated_at_ms
            .cmp(&a.session.updated_at_ms)
            .then_with(|| a.session.session_id.cmp(&b.session.session_id))
    });
    all
}

/// PID liveness probe shared with `active_sessions` (crash recovery).
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    crate::active_sessions::is_pid_alive(pid)
}

/// Default protocol version stamp for new presence files.
pub fn current_protocol_version() -> u32 {
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn presence(
        instance: &str,
        pid: u32,
        beat: u64,
        sessions: Vec<PresenceSession>,
    ) -> PresenceFile {
        PresenceFile {
            instance_id: instance.into(),
            pid,
            socket_path: format!("/tmp/{instance}.sock"),
            protocol_version: PROTOCOL_VERSION,
            heartbeat_at_ms: beat,
            started_at_ms: beat,
            sessions,
        }
    }

    fn session(id: &str, updated: u64) -> PresenceSession {
        PresenceSession {
            session_id: id.into(),
            cwd: "/repo".into(),
            project_name: "repo".into(),
            model_id: None,
            title: None,
            status: "idle".into(),
            updated_at_ms: updated,
        }
    }

    #[test]
    fn presence_file_name_shape() {
        assert!(is_presence_file_name("p123-abcd0123.json"));
        assert!(!is_presence_file_name("p123-abcd012"));
        assert!(!is_presence_file_name("p1234.json"));
        assert!(!is_presence_file_name("other.json"));
        assert!(!is_presence_file_name("p-abc.json"));
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = temp_dir();
        let file = presence("p1-0000000a", 42, 1_000, vec![session("s1", 1_000)]);
        write_presence_atomic(dir.path(), &file).unwrap();
        assert_eq!(
            read_presence(&dir.path().join("p1-0000000a.json")),
            Some(file)
        );
    }

    #[test]
    fn corrupt_file_reads_as_absent() {
        let dir = temp_dir();
        fs::write(dir.path().join("p1-0000000a.json"), "garbage{{{").unwrap();
        assert_eq!(read_presence(&dir.path().join("p1-0000000a.json")), None);
    }

    #[test]
    fn live_sessions_filters_stale_and_dead() {
        let dir = temp_dir();
        let now = 100_000;
        // Fresh, live pid.
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000001",
                std::process::id(),
                now,
                vec![session("s1", now)],
            ),
        )
        .unwrap();
        // Stale heartbeat.
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000002",
                std::process::id(),
                now - STALE_TTL_MS - 1,
                vec![session("s2", 1)],
            ),
        )
        .unwrap();
        // Dead pid.
        write_presence_atomic(
            dir.path(),
            &presence("p1-00000003", 2_000_000_000, now, vec![session("s3", now)]),
        )
        .unwrap();

        let live = live_sessions_in(dir.path(), now);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session.session_id, "s1");
    }

    #[test]
    fn duplicate_session_ids_flag_conflict_and_pick_freshest() {
        let dir = temp_dir();
        let now = 100_000;
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000001",
                std::process::id(),
                now,
                vec![session("dup", now - 50)],
            ),
        )
        .unwrap();
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000002",
                std::process::id(),
                now,
                vec![session("dup", now)],
            ),
        )
        .unwrap();

        let live = live_sessions_in(dir.path(), now);
        assert_eq!(live.len(), 1);
        assert!(live[0].conflict);
        assert_eq!(live[0].instance_id, "p1-00000002");
    }

    #[test]
    fn gc_removes_dead_and_stale_but_keeps_live() {
        let dir = temp_dir();
        let now = 100_000;
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000001",
                std::process::id(),
                now,
                vec![session("keep", now)],
            ),
        )
        .unwrap();
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000002",
                2_000_000_000,
                now,
                vec![session("dead", now)],
            ),
        )
        .unwrap();
        write_presence_atomic(
            dir.path(),
            &presence(
                "p1-00000003",
                std::process::id(),
                now - STALE_TTL_MS - 1,
                vec![session("old", 1)],
            ),
        )
        .unwrap();

        let removed = gc_stale(dir.path(), now);
        assert_eq!(removed.len(), 2);
        let live = live_sessions_in(dir.path(), now);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session.session_id, "keep");
    }

    #[test]
    fn gc_leaves_recent_corrupt_files_and_foreign_names() {
        let dir = temp_dir();
        let now = 100_000;
        fs::write(dir.path().join("p1-00000001.json"), "garbage").unwrap();
        fs::write(dir.path().join("notes.txt"), "not ours").unwrap();

        let removed = gc_stale(dir.path(), now);
        assert!(removed.is_empty());
        assert!(dir.path().join("p1-00000001.json").exists());
        assert!(dir.path().join("notes.txt").exists());
    }

    #[test]
    fn gc_eventually_removes_old_corrupt_files() {
        let dir = temp_dir();
        fs::write(dir.path().join("p1-00000001.json"), "garbage").unwrap();
        // Make the mtime older than the TTL.
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_millis(STALE_TTL_MS + 5_000);
        let ot = filetime::FileTime::from_system_time(old);
        filetime::set_file_times(dir.path().join("p1-00000001.json"), ot, ot).unwrap();

        let removed = gc_stale(dir.path(), now_ms());
        assert!(removed.is_empty()); // corrupt files aren't reported, only removed
        assert!(!dir.path().join("p1-00000001.json").exists());
    }
}
