//! End-to-end measurement of why resuming a large session is slow — the time
//! spent before the client can render anything.
//!
//! The pager resumes via `session/load` and blocks on the response. The shell
//! answers by (1) `load_light` (chat history; rewind points now load lazily) and
//! (2) `replay_session_updates` — reading `updates.jsonl`, filtering it, typed-
//! parsing every line, and forwarding each as a `session/update`. All of that
//! happens while the client waits; both tests drive the real production code.
//!
//! * [`phase_breakdown_real_functions`] drives the exact load-path functions
//!   (`load_session_without_updates`, `load_updates_for_replay_at`) and attributes
//!   wall-clock to rewind load, chat+summary load, and updates read+parse+filter,
//!   then prints a per-`sessionUpdate`-kind byte breakdown of `updates.jsonl`.
//! * [`full_session_load_e2e`] stands up a real `MvpAgent` over in-process ACP
//!   pipes; times `session/load` end-to-end, counts replayed notifications, and
//!   dumps the shell's own per-phase `instrumentation_timer!` events.
//!
//! Session data (both tests): a synthetic session mirroring the pathological real
//! one (redundant `available_commands_update` + big rewind snapshots; size knobs
//! via env, see [`GenOpts::from_env`]), or a real session dir via
//! `GROK_PERF_SESSION_SRC=/path/to/<session-dir>`.
//!
//! Run:
//!   cargo test -p xai-grok-shell --test session_load_perf -- --nocapture
//!   cargo test -p xai-grok-shell --test session_load_perf full_session_load_e2e -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_client_protocol::{self as acp};
use tempfile::TempDir;

use xai_grok_shell::session::info::Info;
use xai_grok_shell::session::storage::{
    JsonlStorageAdapter, StorageAdapter, load_updates_for_replay_at,
};
use xai_grok_workspace::session::file_state::{FileSnapshot, FlexiblePath, RewindPoint};

// ───────────────────────── size knobs ─────────────────────────

/// Generation parameters. Defaults produce a session large enough that the
/// per-phase costs are clearly measurable (tens of MB) while still finishing
/// in a few seconds. Scale up via env to approach a real heavy session.
struct GenOpts {
    turns: usize,
    /// `available_commands_update`s persisted per turn. The real session had
    /// ~12.5 of these per turn — the slash-command catalog re-advertised on
    /// every skill discovery / subagent boundary.
    acu_per_turn: usize,
    catalog_commands: usize,
    catalog_desc_len: usize,
    agent_chunks_per_turn: usize,
    agent_chunk_len: usize,
    rewind_points: usize,
    files_per_rewind: usize,
    file_content_len: usize,
}

impl GenOpts {
    fn from_env() -> Self {
        fn g(key: &str, default: usize) -> usize {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        // A single multiplier for quick scaling of the dominant contributors.
        let scale = g("GROK_PERF_SCALE", 1).max(1);
        Self {
            turns: g("GROK_PERF_TURNS", 80) * scale,
            acu_per_turn: g("GROK_PERF_ACU_PER_TURN", 15),
            catalog_commands: g("GROK_PERF_CATALOG_COMMANDS", 64),
            catalog_desc_len: g("GROK_PERF_CATALOG_DESC_LEN", 320),
            agent_chunks_per_turn: g("GROK_PERF_AGENT_CHUNKS_PER_TURN", 8),
            agent_chunk_len: g("GROK_PERF_AGENT_CHUNK_LEN", 2000),
            rewind_points: g("GROK_PERF_REWIND_POINTS", 60) * scale,
            files_per_rewind: g("GROK_PERF_FILES_PER_REWIND", 40),
            file_content_len: g("GROK_PERF_FILE_CONTENT_LEN", 8000),
        }
    }
}

// ───────────────────────── filler ─────────────────────────

/// Deterministic, non-trivially-compressible-ish filler of `n` bytes. Uses a
/// rotating word list so serde has real strings to allocate (not one repeated
/// byte), matching the cost profile of real prose/code content.
fn filler(n: usize) -> String {
    const WORDS: &[&str] = &[
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
    ];
    let mut s = String::with_capacity(n + 8);
    let mut i = 0usize;
    while s.len() < n {
        s.push_str(WORDS[i % WORDS.len()]);
        s.push(' ');
        i += 1;
    }
    s.truncate(n);
    s
}

// ───────────────────────── update synthesis ─────────────────────────

fn sid(session_id: &str) -> acp::SessionId {
    acp::SessionId::new(session_id.to_string())
}

fn text_chunk(text: String) -> acp::ContentChunk {
    acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)))
}

/// Build one large `AvailableCommandsUpdate` — the redundant catalog that the
/// real session re-persisted thousands of times.
fn available_commands_update(opts: &GenOpts) -> acp::SessionUpdate {
    let desc = filler(opts.catalog_desc_len);
    let commands: Vec<acp::AvailableCommand> = (0..opts.catalog_commands)
        .map(|i| {
            acp::AvailableCommand::new(format!("command-number-{i:03}"), desc.clone()).input(Some(
                acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                    "[optional arguments here]".to_string(),
                )),
            ))
        })
        .collect();
    acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(commands))
}

/// Serialize one notification into the exact on-disk `updates.jsonl` envelope:
/// `{"timestamp":..,"method":"session/update","params":<SessionNotification>}`.
///
/// Params are plain JSON (not the typed `acp::SessionNotification`) so generation
/// doesn't depend on the acp crate's `_meta` field type; the production replay
/// still parses it back into a typed notification — the cost we're measuring.
fn envelope_line(session_id: &str, update: acp::SessionUpdate) -> String {
    let update_val = serde_json::to_value(&update).expect("serialize update");
    let params = serde_json::json!({
        "sessionId": session_id,
        "update": update_val,
    });
    let envelope = serde_json::json!({
        "timestamp": 0u64,
        "method": "session/update",
        "params": params,
    });
    serde_json::to_string(&envelope).expect("serialize envelope")
}

/// Per-kind statistics for the generated/loaded updates file.
#[derive(Default)]
struct KindStats {
    count: BTreeMap<String, u64>,
    bytes: BTreeMap<String, u64>,
}

fn generate_updates_jsonl(path: &Path, session_id: &str, opts: &GenOpts) {
    let mut out = String::new();
    for turn in 0..opts.turns {
        out.push_str(&envelope_line(
            session_id,
            acp::SessionUpdate::UserMessageChunk(text_chunk(format!(
                "user prompt for turn {turn}"
            ))),
        ));
        out.push('\n');
        for _ in 0..opts.acu_per_turn {
            out.push_str(&envelope_line(session_id, available_commands_update(opts)));
            out.push('\n');
        }
        for _ in 0..opts.agent_chunks_per_turn {
            out.push_str(&envelope_line(
                session_id,
                acp::SessionUpdate::AgentMessageChunk(text_chunk(filler(opts.agent_chunk_len))),
            ));
            out.push('\n');
        }
    }
    std::fs::write(path, out).expect("write updates.jsonl");
}

fn generate_rewind_jsonl(path: &Path, opts: &GenOpts) {
    let mut out = String::new();
    for p in 0..opts.rewind_points {
        let mut rp = RewindPoint::new(p);
        for f in 0..opts.files_per_rewind {
            let fp =
                FlexiblePath::Absolute(PathBuf::from(format!("/repo/src/module_{p}/file_{f}.rs")));
            rp.add_snapshot(FileSnapshot::new_flexible(
                fp.clone(),
                Some(filler(opts.file_content_len)),
            ));
            rp.set_after_snapshot(FileSnapshot::new_flexible(
                fp,
                Some(filler(opts.file_content_len + 64)),
            ));
        }
        out.push_str(&serde_json::to_string(&rp).expect("serialize rewind point"));
        out.push('\n');
    }
    std::fs::write(path, out).expect("write rewind_points.jsonl");
}

// ───────────────────────── session setup ─────────────────────────

/// Find `<root>/sessions/<enc-cwd>/<id>` without depending on the (internal)
/// cwd encoder: scan the one level of cwd dirs for a child named `<id>`.
fn locate_session_dir(root: &Path, id: &str) -> PathBuf {
    let sessions = root.join("sessions");
    for entry in std::fs::read_dir(&sessions)
        .expect("read sessions dir")
        .flatten()
    {
        let candidate = entry.path().join(id);
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!(
        "could not locate session dir for {id} under {}",
        sessions.display()
    );
}

/// Recursively copy a directory tree.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// Prepare a session on disk under `root` for working dir `cwd`. Returns the
/// `Info` and the session directory path. Uses `GROK_PERF_SESSION_SRC` if set
/// (copies a real session), otherwise synthesizes one via the production
/// storage adapter (summary) + raw envelope writes (updates/rewind).
async fn prepare_session(root: &Path, cwd: &Path, opts: &GenOpts) -> (Info, PathBuf) {
    let adapter = JsonlStorageAdapter::with_root(root.to_path_buf());

    if let Ok(src) = std::env::var("GROK_PERF_SESSION_SRC") {
        // Real session: create a registered session shell to get the encoded
        // cwd dir + a valid summary, then overlay the real files on top.
        let id = uuid::Uuid::new_v4().to_string();
        let info = Info {
            id: sid(&id),
            cwd: cwd.to_string_lossy().to_string(),
        };
        adapter
            .init_session(&info, acp::ModelId::new("test-model"))
            .await
            .expect("init_session");
        let dir = locate_session_dir(root, &id);
        // Copy real session files (updates/rewind/chat/etc.) over the stub,
        // but keep our freshly-written summary.json (correct id + cwd + model).
        for name in ["updates.jsonl", "rewind_points.jsonl", "chat_history.jsonl"] {
            let from = Path::new(&src).join(name);
            if from.exists() {
                std::fs::copy(&from, dir.join(name)).unwrap();
            }
        }
        // Compaction checkpoints may be referenced by replay; copy if present.
        let ckpt = Path::new(&src).join("compaction_checkpoints");
        if ckpt.is_dir() {
            copy_tree(&ckpt, &dir.join("compaction_checkpoints"));
        }
        eprintln!("[perf] using REAL session copied from {src}");
        return (info, dir);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let info = Info {
        id: sid(&id),
        cwd: cwd.to_string_lossy().to_string(),
    };
    adapter
        .init_session(&info, acp::ModelId::new("test-model"))
        .await
        .expect("init_session");
    let dir = locate_session_dir(root, &id);

    let t = Instant::now();
    generate_updates_jsonl(&dir.join("updates.jsonl"), &id, opts);
    generate_rewind_jsonl(&dir.join("rewind_points.jsonl"), opts);
    eprintln!(
        "[perf] generated synthetic session in {} ms (turns={}, acu/turn={})",
        t.elapsed().as_millis(),
        opts.turns,
        opts.acu_per_turn
    );
    (info, dir)
}

fn file_size_mb(path: &Path) -> f64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6
}

/// `(len, content_hash)` fingerprint of a file, for asserting it is byte-for-byte
/// unchanged across an operation (zero-data-loss guard). Missing file → `(0, 0)`.
fn file_fingerprint(path: &Path) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let Ok(bytes) = std::fs::read(path) else {
        return (0, 0);
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len() as u64, hasher.finish())
}

/// Per-`sessionUpdate`-kind byte + count breakdown of an `updates.jsonl`.
fn updates_kind_breakdown(path: &Path) -> KindStats {
    let mut stats = KindStats::default();
    let Ok(contents) = std::fs::read_to_string(path) else {
        return stats;
    };
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let len = line.len() as u64 + 1;
        let kind = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| {
                v.get("params")
                    .and_then(|p| p.get("update"))
                    .and_then(|u| u.get("sessionUpdate"))
                    .and_then(|s| s.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "<unparsed>".to_string());
        *stats.count.entry(kind.clone()).or_default() += 1;
        *stats.bytes.entry(kind).or_default() += len;
    }
    stats
}

fn print_kind_breakdown(label: &str, stats: &KindStats) {
    let total: u64 = stats.bytes.values().sum();
    eprintln!(
        "\n[perf] {label}: updates.jsonl composition ({:.1} MB total):",
        total as f64 / 1e6
    );
    eprintln!(
        "  {:<32} {:>8} {:>10} {:>7}",
        "sessionUpdate kind", "count", "MB", "%"
    );
    let mut rows: Vec<(&String, &u64)> = stats.bytes.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, bytes) in rows {
        let count = stats.count.get(kind).copied().unwrap_or(0);
        let pct = if total > 0 {
            *bytes as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "  {:<32} {:>8} {:>10.1} {:>6.1}%",
            kind,
            count,
            *bytes as f64 / 1e6,
            pct
        );
    }
}

// ───────────────────────── TEST 1: phase breakdown ─────────────────────────

/// Attribute the pre-render load cost to its real phases using the exact
/// production functions, isolating rewind-point load from everything else.
///
/// `#[ignore]`: this is a measurement tool (generates tens of MB, ~3 s), and its
/// only correctness assertion is covered by the unit tests. Run explicitly with
/// `--ignored` (optionally `GROK_PERF_SESSION_SRC=...`) to get the numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf measurement tool; run with --ignored"]
async fn phase_breakdown_real_functions() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let opts = GenOpts::from_env();

    let (info, dir) = prepare_session(root.path(), cwd.path(), &opts).await;

    let updates_path = dir.join("updates.jsonl");
    let rewind_path = dir.join("rewind_points.jsonl");
    eprintln!(
        "\n[perf] session dir: {}\n[perf]   updates.jsonl       = {:.1} MB\n[perf]   rewind_points.jsonl = {:.1} MB",
        dir.display(),
        file_size_mb(&updates_path),
        file_size_mb(&rewind_path),
    );

    let adapter = JsonlStorageAdapter::with_root(root.path().to_path_buf());

    // Phase A: load_light core (summary + chat_history) — what mvp_agent's
    // `load_light` blocks on before replay.
    let t = Instant::now();
    let light = adapter
        .load_session_without_updates(&info)
        .await
        .expect("load_session_without_updates");
    let full_load_light = t.elapsed();
    // load_light no longer reads rewind_points.jsonl (deferred/lazy), so 0 by
    // construction — `PersistedDataLight` has no rewind field.
    let light_rewind_in_load = 0usize;
    drop(light);

    // Lazy rewind path (T2): the deferred cost moved here. The picker only needs
    // a cheap metadata scan; an actual rewind triggers the full content load.
    // Both read the same file that `load_light` no longer touches.
    use xai_grok_workspace::session::file_state::FileStateTracker;
    let t = Instant::now();
    let lazy_metas = FileStateTracker::with_lazy_source(rewind_path.clone())
        .get_rewind_point_metas()
        .await;
    let lazy_metas_scan = t.elapsed();
    let t = Instant::now();
    let lazy_points = FileStateTracker::with_lazy_source(rewind_path.clone())
        .get_rewind_points()
        .await;
    let lazy_full_load = t.elapsed();
    let num_rewind = lazy_points.len();
    assert_eq!(
        lazy_metas.len(),
        num_rewind,
        "picker metadata scan must see every rewind point"
    );

    // Phase A': isolate rewind cost — delete rewind file and re-measure. The
    // delta is the rewind-point deserialization (full file-content snapshots).
    std::fs::remove_file(&rewind_path).ok();
    let t = Instant::now();
    let _light2 = adapter
        .load_session_without_updates(&info)
        .await
        .expect("load_session_without_updates (no rewind)");
    let load_light_no_rewind = t.elapsed();
    // restore for downstream/manual reruns
    generate_or_restore_rewind(&rewind_path, &opts);

    let rewind_cost = full_load_light.saturating_sub(load_light_no_rewind);

    // Phase B: updates replay parse — production `load_updates_for_replay_at`
    // reads the whole file, typed-parses every line, applies rewind filtering.
    let t = Instant::now();
    let replayed = load_updates_for_replay_at(info.id.0.as_ref(), root.path())
        .expect("load_updates_for_replay_at")
        .unwrap_or_default();
    let updates_parse = t.elapsed();

    let stats = updates_kind_breakdown(&updates_path);
    print_kind_breakdown("phase_breakdown", &stats);

    eprintln!("\n[perf] ===== PRE-RENDER LOAD PHASE BREAKDOWN (real production fns) =====");
    eprintln!("  rewind_points (on disk)      : {num_rewind}");
    eprintln!("  rewind_points loaded in load : {light_rewind_in_load} (deferred → lazy)");
    eprintln!("  updates replayed (acp)       : {}", replayed.len());
    eprintln!("  ----------------------------------------------------------------");
    eprintln!(
        "  load_light (summary+chat)        : {:>8.1} ms",
        full_load_light.as_secs_f64() * 1e3
    );
    eprintln!(
        "    └─ rewind in load_light (now)  : {:>8.1} ms",
        rewind_cost.as_secs_f64() * 1e3
    );
    eprintln!(
        "    └─ summary + chat only         : {:>8.1} ms",
        load_light_no_rewind.as_secs_f64() * 1e3
    );
    eprintln!(
        "  lazy rewind: picker metas scan   : {:>8.1} ms (on /rewind open)",
        lazy_metas_scan.as_secs_f64() * 1e3
    );
    eprintln!(
        "  lazy rewind: full content load   : {:>8.1} ms (on rewind execute)",
        lazy_full_load.as_secs_f64() * 1e3
    );
    eprintln!(
        "  updates read+parse+filter        : {:>8.1} ms",
        updates_parse.as_secs_f64() * 1e3
    );
    eprintln!("  ----------------------------------------------------------------");
    eprintln!(
        "  TOTAL pre-render parse work      : {:>8.1} ms",
        (full_load_light + updates_parse).as_secs_f64() * 1e3
    );
    eprintln!("================================================================\n");

    assert!(!stats.bytes.is_empty(), "expected a non-empty updates file");
}

/// Re-create the rewind file after the isolation step deletes it (synthetic
/// case). For a real session copy we cannot regenerate; leave it absent.
fn generate_or_restore_rewind(path: &Path, opts: &GenOpts) {
    if std::env::var("GROK_PERF_SESSION_SRC").is_ok() {
        return;
    }
    generate_rewind_jsonl(path, opts);
}

// ───────────────────────── TEST 2: true e2e ─────────────────────────

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::mvp_agent::MvpAgent;

const DUPLEX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// Counts replayed notifications and records first/last receipt timestamps so
/// we can see how long the client streams history before `load` returns.
#[derive(Default)]
struct LoadCounters {
    count: u64,
    /// `available_commands_update` notifications forwarded during the load. T1
    /// skips the (thousands of) historical ones, so this must stay tiny.
    acu_count: u64,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
}

struct CountingClient {
    counters: Rc<RefCell<LoadCounters>>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for CountingClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let outcome = args
            .options
            .first()
            .map(|o| {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    o.option_id.clone(),
                ))
            })
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let mut c = self.counters.borrow_mut();
        let now = Instant::now();
        c.count += 1;
        if matches!(args.update, acp::SessionUpdate::AvailableCommandsUpdate(_)) {
            c.acu_count += 1;
        }
        c.first_at.get_or_insert(now);
        c.last_at = Some(now);
        Ok(())
    }
}

/// Parse the production instrumentation JSON log into `(name -> elapsed_ms)`.
fn parse_instrumentation_log(path: &Path) -> Vec<(String, f64)> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let fields = v.get("fields").unwrap_or(&v);
        if fields.get("event").and_then(|e| e.as_str()) != Some("timing") {
            continue;
        }
        let Some(name) = fields.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let us = fields
            .get("elapsed_us")
            .and_then(|u| u.as_u64())
            .or_else(|| {
                fields
                    .get("elapsed_ms")
                    .and_then(|m| m.as_u64())
                    .map(|m| m * 1000)
            })
            .unwrap_or(0);
        out.push((name.to_string(), us as f64 / 1000.0));
    }
    out
}

/// True end-to-end: real `MvpAgent` over real ACP pipes. Times `session/load`
/// (what the pager blocks on), counts replayed notifications, and prints the
/// shell's own per-phase instrumentation.
///
/// `#[ignore]` by default because it stands up the full agent; run with
/// `--ignored --nocapture`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "heavy: builds a full MvpAgent and replays a large session; run with --ignored"]
async fn full_session_load_e2e() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();

    let grok_home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let opts = GenOpts::from_env();
    let instr_log = grok_home.path().join("instr.jsonl");

    // SAFETY: single-threaded current-thread runtime; set before any agent code
    // reads these process-globals (grok_home()/instrumentation mode are OnceLock).
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_INSTRUMENTATION", "log");
        std::env::set_var("GROK_INSTRUMENTATION_LOG", &instr_log);
        std::env::set_var("GROK_CLI_CHAT_PROXY_BASE_URL", server.url());
        std::env::set_var("GROK_XAI_API_BASE_URL", server.url());
        std::env::set_var("XAI_API_KEY", "test-key-for-ci");
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
    }

    // Install the production instrumentation layer so `instrumentation_timer!`
    // events are written to our temp log file.
    use tracing_subscriber::Registry;
    use tracing_subscriber::prelude::*;
    let _ = tracing_subscriber::registry()
        .with(xai_grok_shell::instrumentation::layer::<Registry>())
        .try_init();

    let (info, dir) = prepare_session(grok_home.path(), cwd.path(), &opts).await;
    let updates_path = dir.join("updates.jsonl");
    let rewind_path = dir.join("rewind_points.jsonl");
    eprintln!(
        "\n[perf] e2e session: updates={:.1} MB rewind={:.1} MB",
        file_size_mb(&updates_path),
        file_size_mb(&rewind_path)
    );
    let stats = updates_kind_breakdown(&updates_path);
    print_kind_breakdown("e2e", &stats);

    // Zero-data-loss guard (C1): a pure load must never rewrite rewind_points.jsonl
    // (T2 reads it lazily, never on the load path). Captured here, asserted after.
    let rewind_path_guard = rewind_path.clone();
    let rewind_fp_before = file_fingerprint(&rewind_path_guard);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let agent_config = AgentConfig::default();
            let auth_manager = Arc::new(agent_config.create_auth_manager());
            let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let gateway = GatewaySender::new(gw_tx);
            let agent =
                MvpAgent::new(gateway, &agent_config, auth_manager, None).expect("valid config");

            let (c2a_a, c2a_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
            let (a2c_a, a2c_b) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);

            // Agent side.
            let agent_incoming = LineBufferedRead::spawn_local(c2a_b.compat());
            let (agent_conn, agent_io) =
                acp::AgentSideConnection::new(agent, a2c_a.compat_write(), agent_incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });
            tokio::task::spawn_local(
                GatewayReceiver::new(gw_rx, agent_conn)
                    .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
                    .run(),
            );
            tokio::task::spawn_local(agent_io);

            // Client side.
            let counters = Rc::new(RefCell::new(LoadCounters::default()));
            let client = CountingClient {
                counters: counters.clone(),
            };
            let client_incoming = LineBufferedRead::spawn_local(a2c_b.compat());
            let (client_conn, client_io) =
                acp::ClientSideConnection::new(client, c2a_a.compat_write(), client_incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });
            tokio::task::spawn_local(client_io);

            use acp::Agent as _;

            // initialize + authenticate (api-key, like the pager does).
            let init = tokio::time::timeout(
                Duration::from_secs(60),
                client_conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(acp::ClientCapabilities::new().fs(acp::FileSystemCapabilities::new()).terminal(false)).meta(serde_json::json!({
                        "startupHints": { "nonInteractive": true, "skipGitStatus": true, "skipProjectLayout": true },
                        "clientType": "perf-test",
                        "clientVersion": "0.0-test",
                    }).as_object().cloned())),
            )
            .await
            .expect("initialize timed out")
            .expect("initialize failed");

            if let Some(method) = init.auth_methods.iter().find(|m| &*m.id().0 == "xai.api_key") {
                let _ = client_conn
                    .authenticate(acp::AuthenticateRequest::new(method.id().clone()).meta(serde_json::json!({ "headless": true }).as_object().cloned()))
                    .await;
            }

            // The measurement: time the full session/load round-trip.
            let load_started = Instant::now();
            let resp = tokio::time::timeout(
                Duration::from_secs(180),
                client_conn.load_session(acp::LoadSessionRequest::new(info.id.clone(), cwd.path().to_path_buf())),
            )
            .await
            .expect("session/load timed out (>180s)")
            .expect("session/load failed");
            let load_elapsed = load_started.elapsed();
            let _ = resp;

            // Snapshot replay results immediately — BEFORE the post-load
            // AdvertiseCommands re-advertise can arrive — so `acu_replayed` is the
            // count of ACUs forwarded during history replay (the T1 skip count).
            let (replay_count, acu_replayed, ttfn, ttln) = {
                let c = counters.borrow();
                (
                    c.count,
                    c.acu_count,
                    c.first_at
                        .map(|t| t.duration_since(load_started).as_secs_f64() * 1e3)
                        .unwrap_or(0.0),
                    c.last_at
                        .map(|t| t.duration_since(load_started).as_secs_f64() * 1e3)
                        .unwrap_or(0.0),
                )
            };

            // The post-load `AdvertiseCommands` re-advertise (the safety basis for
            // dropping historical ACUs on replay) must reach the client. It's
            // enqueued at the end of `load_session` and forwarded async, so poll.
            // Replay forwards 0 ACUs, so any received ACU is the re-advertise.
            let readvertised = tokio::time::timeout(Duration::from_secs(10), async {
                while counters.borrow().acu_count == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok();

            // Flush the instrumentation writer and read the per-phase log.
            let _ = xai_grok_shell::instrumentation::finalize();
            std::thread::sleep(Duration::from_millis(150));
            let mut phases = parse_instrumentation_log(&instr_log);
            phases.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // T1 guard: the historical available_commands_update copies (3197 in
            // the pathological real session, hundreds in the synthetic one) must
            // NOT be replayed.
            let acu_persisted = stats.count.get("available_commands_update").copied().unwrap_or(0);

            eprintln!("\n[perf] ===== END-TO-END session/load (what the pager waits on) =====");
            eprintln!("  total session/load round-trip : {:>9.1} ms", load_elapsed.as_secs_f64() * 1e3);
            eprintln!("  notifications replayed         : {:>9}", replay_count);
            eprintln!("  available_commands_update      : {acu_replayed:>9} replayed / {acu_persisted} on disk");
            eprintln!("  post-load re-advertise reached : {readvertised:>9}");
            eprintln!("  time-to-first notification     : {ttfn:>9.1} ms");
            eprintln!("  time-to-last notification      : {ttln:>9.1} ms");
            eprintln!("  ---- shell-side per-phase instrumentation (elapsed) ----");
            if phases.is_empty() {
                eprintln!("  (no instrumentation events captured)");
            } else {
                for (name, ms) in &phases {
                    eprintln!("  {name:<40} {ms:>9.1} ms");
                }
            }
            eprintln!("================================================================\n");

            assert!(replay_count > 0, "expected replayed notifications during load");
            // C1: the lazy rewind file must be byte-for-byte unchanged by a load.
            assert_eq!(
                file_fingerprint(&rewind_path_guard),
                rewind_fp_before,
                "rewind_points.jsonl must be unchanged after a load (zero data loss)"
            );
            // The thousands of persisted ACUs must be skipped on replay (T1)...
            assert!(
                acu_persisted > 100,
                "fixture should have many persisted ACUs to exercise the skip"
            );
            assert!(
                acu_replayed < 100,
                "historical available_commands_update must be skipped on replay \
                 (replayed {acu_replayed} of {acu_persisted} persisted)"
            );
            // ...but the catalog IS re-advertised to the client after load.
            assert!(
                readvertised,
                "post-load available_commands_update re-advertise must reach the client"
            );
        })
        .await;
}
