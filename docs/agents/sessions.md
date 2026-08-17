# Sessions, storage, and resume

Implementation map of **session identity**, on-disk layout under `$OPENGROK_HOME`, the persistence actor, load/fork/rewind flows, compaction host wiring, idle memory flush / dream, and headless create/resume flags.

Paths are relative to the repo root under `crates/codegen/` unless noted.

End-user product docs: `xai-grok-pager/docs/user-guide/` (session resume UX). Runtime overview: [agent-runtime.md](agent-runtime.md). Provider / compaction contracts: [providers.md](providers.md), [`../codex-provider-port.md`](../codex-provider-port.md).

---

## Architecture snapshot

```text
ACP Client (pager / IDE / headless)
        │ session/new | session/load | session/prompt
        ▼
MvpAgent  (LocalSet)
   └── SessionActor  (!Send, own OS thread + LocalSet)
         ├── ChatStateActor  ── ChatPersistence ──┐
         ├── FileStateTracker (rewind snapshots)  │
         ├── PlanModeTracker / GoalTracker         │
         └── PersistenceHandle ──► SessionPersistence actor
                                      │
                                      ▼
                           StorageAdapter (JsonlStorageAdapter)
                                      │
                    $OPENGROK_HOME/sessions/<encoded-cwd>/<session-id>/
```

| Concern | Primary path |
| --- | --- |
| Session actor | `xai-grok-shell/src/session/acp_session.rs` |
| Spawn / thread | `…/acp_session_impl/spawn.rs` |
| Run loop (idle flush / dream timers) | `…/acp_session_impl/run_loop.rs` |
| Persistence actor + `Summary` | `…/session/persistence.rs` |
| Storage trait + JSONL adapter | `…/session/storage/` (`mod.rs`, `jsonl/`) |
| Chat → persistence bridge | `…/session/chat_persistence.rs` |
| Fork | `…/session/fork.rs` |
| Rewind | `…/acp_session_impl/rewind.rs` |
| Cross-compaction replay | `…/helpers/replay.rs` |
| Compaction host | `…/session/compaction.rs`, `compaction_config.rs`, `compaction_segments.rs` |
| Plan mode files | `…/session/plan_mode.rs` |
| Memory flush / dream | `…/helpers/memory_flush.rs`, `…/acp_session_impl/memory_dream.rs` |
| Path helpers | `xai-grok-shared/src/session/mod.rs` (`session_dir`), `xai-grok-tools` / `xai-grok-shell-base` `util/grok_home` |
| ACP entry | `…/agent/mvp_agent/acp_agent.rs` (`new_session`, `load_session`) |

---

## Session identity

### Components

| Field | Meaning |
| --- | --- |
| **Session ID** | Opaque string (typically UUIDv7). New forks also use plain UUIDv7 — no source-id embedding. |
| **CWD** | Working directory of the session (`Info.cwd`). Determines the on-disk parent directory. |
| **`Info`** | `{ id: SessionId, cwd: String }` — key for all storage path resolution. |

### On-disk root

```text
$OPENGROK_HOME/sessions/<url-encoded-cwd>/<session-id>/
```

- Home: `$OPENGROK_HOME` or `~/.opengrok` via `xai_grok_config` / `grok_home()` — **never** `~/.grok`.
- CWD encoding: `encode_cwd_dirname` / `decode_cwd_from_dirname` (shared grok_home helpers). Encoding is stable so path characters and separators do not break the tree.
- Path construction:
  - Normal sessions: `JsonlStorageAdapter::session_dir` → `{root}/sessions/{encode(cwd)}/{id}/`
  - Shared helper: `xai_grok_shared::session::session_dir(&Info)`
  - Subagent children: `JsonlStorageAdapter::with_explicit_session_dir` → `{parent_session_dir}/subagents/{subagent_id}/` (bypasses cwd encoding)

### Resume existence rules

A directory is a **resumable** session only if it contains `summary.json`.

| Helper | Behavior |
| --- | --- |
| `session_exists_for_cwd(id, cwd)` | True only under **that** encoded cwd + `summary.json` present. Correct check for `-r` / same-cwd resume. |
| `resolve_local_session(id, cwd)` | Exact under cwd, else previously restored child (`parent_session_id == remote_id`). |
| `resolve_local_session_any_cwd(id)` | Scan all encoded-cwd dirs (pager `--resume` across worktrees). |
| `resolve_local_session_for_repo` | Ordered candidate cwds (exact first) for worktree resume. |
| `find_session_dir_by_id` | Path lookup across all cwds; dir-only (non-resume uses). |
| `find_local_child_for_remote` | Deterministic pick among restored children: newest `updated_at`, then dir mtime, then lex id. |

**Invariant:** an `images/`-only stub (no `summary.json`) must not hijack `--resume`.

---

## File layout

Under `$OPENGROK_HOME/sessions/<encoded-cwd>/<session-id>/`:

### Core (resume-critical)

| Path | Role |
| --- | --- |
| `summary.json` | Index + sticky metadata (`Summary`). Written under `summary.json.lock`. |
| `updates.jsonl` | **Authoritative ACP stream for resume / rewind / cross-compaction replay.** Envelope: `{ timestamp, method, params }` for ACP `session/update` or `_x.ai/session/update`. |
| `chat_history.jsonl` | Model-facing `ConversationItem` lines (format version in summary; `CHAT_FORMAT_VERSION = 1`). Compaction rewrites this file wholesale via `ReplaceChatHistory`. |

### Plan / goals / tools

| Path | Role |
| --- | --- |
| `plan.json` | TODO tool state (`TodoState`) |
| `plan.md` | Plan-mode plan document (edit-gated path when plan Active) |
| `plan_mode.json` | `PlanModeSnapshot` lifecycle |
| `goal/state.json` | Goal orchestration state |
| `tool_state.json` | Persisted tool state (e.g. todo bridge); optional copy on fork |
| `signals.json` | Session signals snapshot |
| `announcement_state.json` | MCP / skill announcement dedup |

### Rewind / compaction

| Path | Role |
| --- | --- |
| `rewind_points.jsonl` | File snapshots for rewind (`RewindPoint` from workspace file-state) |
| `compaction_checkpoints/` | Checkpoint JSON files referenced from updates stream |
| `compaction/` | Segment archive (`segment_*.md`, `INDEX.md`) — pre-compaction transcripts; optional on fork |
| `compaction_requests/` | Offline prompt-iteration artifacts (request + summary/error) |
| `recap_requests/` | Same pattern for recap |

### Memory / debug / ancillary

| Path | Role |
| --- | --- |
| `system_prompt.txt` | Exact rendered system prompt (load/rebuild) |
| `prompt_context.json` | Structured prompt context |
| `prompts/prompt_{n}.txt` | Per-prompt debug dumps |
| `feedback.jsonl` | Local feedback entries (`LocalFeedbackEntry`) |
| `btw_history.jsonl` | `/btw` side questions |
| `subagents/<subagent_id>/` | Child session trees (full file set via explicit-dir adapter) |
| `images/` | Image assets; alone does **not** make a session resumable |

### `summary.json` sticky fields (selected)

See `Summary` in `persistence.rs`. Agents editing resume or export must respect:

| Field | Invariant |
| --- | --- |
| `current_model_id` / `agent_name` | Resume harness without re-deriving from mutable catalog |
| `previous_turn_model` | Compaction contract from last **started** turn (Codex `comp_hash` after resume) |
| `ever_used_codex` | **Monotonic** export boundary (compat name; any non-xAI denied profile can set it) |
| `chat_format_version` | 0 = legacy ChatRequestMessage; 1 = ConversationItem |
| `sandbox_profile` | Pinned for session life on resume |
| `parent_session_id` / `forked_at` / `session_kind` | Fork / subagent / worktree lineage |
| `inherited_prefix_len` | Compaction preserves fork inherited prefix |
| `cache_affinity_id` | Sticky prompt-cache identity (session id, or inherited parent id on verbatim forks). Restored onto `StartupHints` on `session/load` so resume still *tries* the warmed cache; a server TTL miss after idle is acceptable |
| `prompt_display_cwd` | Display path for worktree forks (not real worktree path) |
| `source_workspace_dir` | Group worktree sessions under origin workspace |
| `last_active_at` | Advanced only by local content append (not remote metadata writes) |
| `generated_title` / `title_is_manual` | Auto title vs `/rename` |
| `hidden` | Default: hide `session_kind` starting with `subagent` |

---

## Persistence actor vs storage modules

### Layering

```text
SessionActor / ChatState
        │ PersistenceMsg (unbounded mpsc)
        ▼
SessionPersistence::run   (tokio task; sequential writes)
        │ Arc<dyn StorageAdapter>
        ▼
JsonlStorageAdapter       (filesystem; optional torn-tail heal on append)
```

| Piece | Path | Responsibility |
| --- | --- | --- |
| `PersistenceMsg` | `persistence.rs` | All write intents: updates, chat, replace history, model, plan, rewind, checkpoints, flush, copy, titles, … |
| `PersistenceHandle` | `persistence.rs` | `tx` + `ProviderBoundary` + optional `noop` |
| `SessionPersistence` | `persistence.rs` | Merge consecutive text chunks, append, remote/relay queue, title generator routing |
| `StorageAdapter` trait | `storage/mod.rs` | Init/load/append/replace/plan/rewind/checkpoint/list/copy |
| `JsonlStorageAdapter` | `storage/jsonl/` | Concrete layout, locks, heal, list-by-mtime |
| `ChannelChatPersistence` | `chat_persistence.rs` | `ChatPersistence` → `PersistenceMsg::{Chat,ReplaceChatHistory,Flush}` |
| `summary_write` / search | `storage/summary_write.rs`, `search*.rs` | Locked summary mutations; FTS / remote search helpers |

### `PersistenceMsg` categories (not exhaustive)

| Category | Variants |
| --- | --- |
| Stream | `Update`, `ContentChunk` |
| Chat | `Chat`, `ReplaceChatHistory` |
| Model / provider | `CurrentModel`, `PreviousTurnModel`, `ObserveProvider`, `RefreshCodexSummaryAuth` |
| Plan / goal | `PlanState`, `PlanModeState`, `GoalModeState` |
| Rewind | `RewindPoint`, `TruncateRewindPoints`, `MergeRewindPointsFrom` |
| Compaction artifacts | `CompactionCheckpoint`, `CompactionRequest`, `CompactionSegment`, `RecapRequest` |
| Meta | `Signals`, `AnnouncementState`, `GitHead`, `CollectionId`, `NextTraceTurn`, `GeneratedTitle`, `Feedback`, `Btw` |
| Barriers | `Flush` (fire-and-forget), `FlushAndAck` (oneshot after disk), `CopyFile` (flush + in-memory snapshot) |

### Actor behaviors that matter when editing

1. **Text chunk merge** — consecutive ACP agent message/thought text chunks without meta are coalesced before write (reduces JSONL size). Empty chunks are dropped.
2. **Fail-open append** — append errors are logged and the actor continues. Torn tails (kill mid-write) are healed by prepending `\n` so the next record does not merge into a corrupt line; lenient readers skip bad lines.
3. **Provider boundary** — once `ever_used_codex` (non-xAI export denied) is observed, remote_sync / relay / registry title sync are dropped and not re-opened.
4. **Subagent variants**:
   - `persistence::new` — full root path, optional remote/relay/gateway
   - `persistence::new_with_explicit_dir` — parent `subagents/{id}/`, no remote/relay/gateway, default `session_kind = "subagent"`
   - `PersistenceHandle::noop()` — discard all messages (results via coordinator oneshot only)
5. **Title generation** — background LLM on first content chunk; result returns as `GeneratedTitle` so storage stays sequential; must not overwrite manual `/rename`.
6. **Worktree GC touch** — debounced touch while the actor receives messages so long-lived sessions stay out of worktree GC.

### Load helpers on the persistence module

| Function | Returns |
| --- | --- |
| `persistence::new` | Fresh session + handle |
| `persistence::load` | Full `PersistedInfo` (includes all updates) + handle |
| `persistence::load_light` (pattern) | `PersistedInfoLight` — summary/chat/plan paths; **stream** `updates.jsonl`; defer rewind points for `FileStateTracker` |

`load_session` vs `load_session_without_updates` on the adapter: full vs light for memory-efficient resume.

---

## Create / load / fork / rewind

### ACP entry points

| ACP method | Shell path | Disk effect |
| --- | --- | --- |
| `session/new` | `mvp_agent/acp_agent.rs` → spawn `SessionActor` | `init_session` creates dir + `summary.json` |
| `session/load` | same + restore path | `load_session` / light load; stream replay updates to client |
| Extensions | `x.ai/*` (fork, rewind, history, …) | See shell `extensions/` + session commands |

**Invariants:**

- `SessionActor` is **`!Send`** — LocalSet / `spawn_local` only.
- Resume identity for UI/transcript: **`updates.jsonl` is source of truth** for the ACP stream; `chat_history.jsonl` is the model conversation (post-compaction).
- Sandbox profile from summary is restored for the session life (do not silently fall back to config default).
- `agent_name` + `current_model_id` drive harness rebuild without catalog re-inference.
- `cache_affinity_id` is restored onto `StartupHints` and stamped on every turn / side-call / remote-compact request. Resume must keep the same key so the provider can still serve a warmed prefix; a miss after server TTL is acceptable.

### Fork

**Module:** `session/fork.rs` → `JsonlStorageAdapter::copy_session_data(_sync)`.

```text
ForkSessionRequest
  source_session_id, source_cwd, new_cwd
  optional new_session_id, new_model_id, target_prompt_index
  session_kind (default "fork"; worktree uses "worktree")
  source_workspace_dir
        │
        ▼
copy_session_data (spawn_blocking for true parallelism off LocalSet)
  → new summary with parent_session_id, forked_at, copied chat/updates/…
  → optional compaction/ segment archive (forks enable copy_compaction_segments)
        │
        ▼
background backend upsert if allows_xai_export (telemetry; not required for local success)
```

`CopySessionOptions` flags control plan/signals/tool_state/announcement copy, cwd transform, strip reasoning, fork filter, inherited prefix, etc. Defaults copy plan/signals/tool state; **do not** copy compaction segments unless enabled (size).

Worktree resume/fork also lives in `session/worktree.rs` / workspace worktree APIs.

### Rewind

**Module:** `acp_session_impl/rewind.rs` + workspace `FileStateTracker`.

| Mode | Conversation | Files |
| --- | --- | --- |
| `All` | Truncate / replay to target | Revert to snapshots at target |
| `ConversationOnly` | Truncate / replay | Leave disk files; may merge rewind points on disk |
| `FilesOnly` | Leave chat | Revert files only (prompt index validation exempt when conversation is server-side) |

Semantics: restore state **before** prompt N ran (keep prompts `0..N-1`).

**Cross-compaction:** after any compaction, in-memory user-message counts diverge from `prompt_index`. Always use `helpers/replay.rs` → `replay_to_prompt(updates.jsonl, session_dir, target)` which understands `CompactionCheckpoint` and `RewindMarker`. Do not rely on `truncate_to_prompt_index` alone post-compaction.

Persistence messages after rewind:

- `TruncateRewindPoints { from_index }` after full file+conversation rewind
- `MergeRewindPointsFrom { target_index }` after ConversationOnly (disk authoritative)

Markers in `updates.jsonl` (wire tags) keep prompt extraction and timeline correct after rewinds.

### Remote restore

`restore_stub.rs` defines progress/phases; **full remote restore may be unavailable** in some builds (`"Remote session restore is not available in this build"`). Local resolution still prefers same-cwd child of a remote id to avoid duplicate restores.

---

## Compaction host wiring

Full provider contracts: [providers.md](providers.md) (compaction table), [`../codex-provider-port.md`](../codex-provider-port.md), [`../provider-architecture.md`](../provider-architecture.md). Engine: `crates/common/xai-grok-compaction/`. Chat-state helpers: `xai-chat-state` compaction utils.

| | xAI | Codex |
| --- | --- | --- |
| Host module | `session/compaction.rs` | same host; different builders |
| Default path | Local / two-pass summary compaction | Remote Compaction V2 over streaming `/responses` |
| Legacy | — | unary `/responses/compact` if flag off |
| History builders | `build_compacted_history`, two-pass helpers | `build_codex_remote_compaction_v2_history` |
| Preflight | overflow → compact and continue | tool-output rewrite to fit window; avoid discarding ordinary messages |

**Session-level invariants (do not re-implement in UI):**

1. **Never** replay Codex opaque compaction carriers into xAI requests after provider switch — plaintext fallback only.
2. `ever_used_codex` / export boundary closes xAI-only remote persistence for the session tree (including subagent observation of parent).
3. `ReplaceChatHistory` rewrites `chat_history.jsonl`; checkpoints + updates stream preserve pre-compaction material for rewind.
4. Plan mode state survives compact with reminders; do not clear `plan_mode.json` on compact.
5. Prefire pass-1 caches are invalidated by prefix fingerprint (edit/rewind/branch).
6. Memory flush can run **before** compact (`helpers/memory_flush.rs`); `is_flushing` suppresses auto-compact during flush.
7. After compaction, reset `last_idle_flush_conversation_len` so idle flush does not skip incorrectly.
8. **Environment updates are append-only.** The first-turn `<user_info>` prefix and the spawn-time AGENTS.md `ProjectInstructions` item stay frozen for prompt-cache stability. Date rollover, cwd relocation, live `user_info` identity changes (os/shell/workspace), and AGENTS.md / rules file edits are injected as later hidden `<system-reminder>` / `<environment-update>` items — never by rewriting the cached prefix. Git status in the prefix is still a startup snapshot.

---

## Idle flush and dream

Wired in `acp_session_impl/run_loop.rs` timers; config from memory settings at spawn (`idle_flush_timeout`, `dream_check_timeout`).

### Idle memory flush

| Item | Detail |
| --- | --- |
| When | `idle_flush_timeout` elapsed, memory enabled, not already flushing |
| Gate | Conversation length **grew** since `last_idle_flush_conversation_len` |
| Action | `run_memory_flush("interval", …)` (spawn_local) |
| Reset | On `ConversationReset` events (compaction / rewind) |
| Related | Pre-compact flush via `helpers/memory_flush.rs` (`should_flush`, quality gates, semantic dedup) |

### Dream

| Item | Detail |
| --- | --- |
| Module | `acp_session_impl/memory_dream.rs` + `session/memory/dream*` |
| When | Periodic `dream_check_timeout`; also session-end path after summary |
| Skip | Subagent sessions (`startup_hints.is_subagent`) |
| Gates | `check_dream_gates` + workspace `DreamLock` + sessions dir inventory |
| Slash | `/dream` and `/flush` gated on memory enabled (`slash_commands.rs`) |

Do not treat dream as a security boundary; it is best-effort consolidation into memory storage.

### Idle prompt (notifications)

Separate from memory flush: `acp_session_impl/extensions/idle_prompt.rs` debounces an `idle_prompt` notification (~60s default) for hooks after sustained inactivity. Synthetic turns only defer the timer.

---

## Headless / CLI create vs resume

CLI definitions: `xai-grok-pager/src/app/cli.rs` (`PagerArgs`). Binary wiring: `xai-grok-pager-bin`.

| Flag | Effect |
| --- | --- |
| (default) | `session/new` — new UUID under current cwd |
| `-s` / `--session-id <UUID>` | **New** conversation with client-chosen id (must not already exist). With resume only valid together with `--fork-session`. |
| `-r` / `--resume [SESSION_ID]` | Load by id; omit id → most recent. Empty default-missing value means “most recent.” |
| `--load <SESSION_ID>` | Hidden alias for `--resume` |
| `-c` / `--continue` | Most recent session for **current cwd** (conflicts with resume/load) |
| `--fork-session` | On resume/continue, copy to a **new** session id (optional `-s` names the fork) |
| `-w` / `--worktree` | New git worktree session |
| `--worktree-ref` | Base ref for worktree |
| `--restore-code` | With `--resume`, restore code at original commit (worktree/restore path) |
| Headless `-p` / prompt | One-shot or agent mode still uses same session create/load under the hood |

**Resolution order for resume (implementation intent):**

1. Exact session under requested cwd with `summary.json`
2. Previously restored local child of a remote id under that cwd
3. Same-repo different cwd / any-cwd scan (worktree / pager)
4. Remote pull/restore if build supports it and local miss

**Headless notes:**

- Permission prompts cancel rather than block (see agent-runtime ACP section).
- Tests must set isolated `OPENGROK_HOME` (never pollute real `~/.opengrok`).
- Leader reconnect replays `session/load` (not `session/new`) from cached params; unconfirmed `session/new` must not poison ownership caches.

---

## Key source map

```text
xai-grok-shell/src/session/
  mod.rs                 # re-exports fork, persistence resolve helpers
  acp_session.rs         # SessionActor fields, prompt_context / system_prompt IO
  acp_session_impl/
    spawn.rs             # construct actor, idle/dream timeouts
    run_loop.rs          # select! command/events + idle flush + dream
    turn.rs              # handle_prompt
    rewind.rs            # rewind points + handle_rewind
    memory_dream.rs      # flush + dream orchestration on actor
    extensions/idle_prompt.rs
  persistence.rs         # PersistenceMsg, Summary, actor, new/load, resolve_*
  chat_persistence.rs    # ChatState → PersistenceMsg
  storage/
    mod.rs               # StorageAdapter, CopySessionOptions, envelopes
    jsonl/mod.rs         # layout, append heal, copy_session_data, list
    jsonl/tests.rs       # storage unit tests
    summary_write.rs
    search*.rs
  fork.rs
  compaction.rs          # SessionActor compaction methods
  compaction_config.rs
  compaction_segments.rs
  plan_mode.rs
  helpers/
    replay.rs            # cross-compaction rewind replay
    memory_flush.rs
    session_compact.rs
    full_replace_compaction.rs
  restore_stub.rs        # remote restore types / stub
  worktree.rs            # worktree resume/fork session glue
  summary.rs             # title generation lifecycle
  memory/                # dream, index, embedding, storage
```

Related outside `session/`:

- `agent/mvp_agent/acp_agent.rs` — ACP `new_session` / `load_session`
- `agent/subagent/` — child dirs under `subagents/`, resume identity
- `xai-chat-state` — conversation mutation, compaction history builders
- `xai-grok-workspace` — `FileStateTracker`, rewind snapshots, permissions
- `crates/common/xai-grok-compaction/` — summarization engine

---

## Test locations

| Area | Where |
| --- | --- |
| JSONL storage / copy / torn tail / light load | `session/storage/jsonl/tests.rs` |
| Persistence resolve / images stub / cwd | `session/persistence.rs` unit tests |
| Fork end-to-end | `xai-grok-shell/tests/test_fork_session.rs` |
| Rewind / cross-compaction | `session/acp_session_tests/rewind_*_tests.rs`, `helpers/replay` tests |
| Idle flush / resume | `session/acp_session_tests/idle_resume_tests.rs` |
| Inline auto-compact | `session/acp_session_tests/inline_auto_compact_flow_tests.rs` |
| Plan mode persistence / resume | `plan_mode_*_tests.rs`, `plan_approval_resume_tests.rs` |
| Prompt context / system prompt | `prompt_context_persistence_tests.rs` |
| Subagent session files / kinds | `agent/subagent/tests/` |
| Leader reconnect `session/load` | `tests/test_leader_*`, pager-bin main tests |
| Headless lifecycle | `tests/test_built_binary_e2e.rs` |
| Compaction engine | `crates/common/xai-grok-compaction/`, chat-state compaction utils |
| Summary fields | `tests/test_summary_reasoning_effort.rs` |

Focused commands:

```sh
cargo test --locked -p xai-grok-shell --test test_fork_session
cargo test --locked -p xai-grok-shell -- rewind
cargo test --locked -p xai-grok-shell -- storage
cargo test --locked -p xai-grok-shell -- idle
```

Always export a temp `OPENGROK_HOME` in tests.

---

## Gotchas for agents editing this code

| Pitfall | Why it breaks |
| --- | --- |
| Writing under `~/.grok` or hardcoding home | Breaks Open Grok isolation |
| Treating `chat_history.jsonl` as sole resume truth for UI | Post-compaction chat is collapsed; **updates.jsonl** owns the ACP timeline |
| Using `truncate_to_prompt_index` after compaction without replay | Wrong cut point; must use `replay_to_prompt` |
| Clearing `ever_used_codex` or reopening xAI export after Codex | Export boundary is monotonic |
| Inferring provider from model slug on load | Use summary `agent_name` + model metadata |
| Forgetting `summary.json` existence check | `images/` stubs steal resume IDs |
| Using `session_exists_by_id` for `-r` when cwd is known | Wrong cwd match; use `session_exists_for_cwd` / `resolve_local_session` |
| Editing root workspace `Cargo.toml` for session crates | Generated; edit crate manifests only |
| Blocking the LocalSet with sync fork copies | Use `spawn_blocking` for `copy_session_data_sync` (fork already does) |
| Skipping `FlushAndAck` before archive/upload | Partial files in tar/GCS |
| Making append failures fatal | Actor is intentionally fail-open; heal + skip corrupt lines |
| Subagent remote sync | Explicit-dir children skip remote/relay; parent tree may still mark export boundary via `ObserveProvider` |
| Plan mode only in permission manager | Gate is in tool prepare; plan files live in session dir |
| Copying compaction segments by default for every fork | Large; enable only when child needs archive |
| Assuming remote restore always works | May be stubbed per build |
| Tests without isolated `OPENGROK_HOME` | Pollutes real user state |
| Moving `SessionActor` work across threads | `!Send` — hang or compile failure |

---

## Cross-links

| Change | Also update |
| --- | --- |
| Session file format / new sticky summary field | storage load/copy tests, fork, light load, migration notes here |
| Resume CLI semantics | `pager` `cli.rs` + this doc + user-guide if user-visible |
| Compaction host / provider | [providers.md](providers.md), codex port docs, compaction tests |
| Rewind modes | `rewind.rs`, replay tests, workspace file_state |
| Plan files | [agent-runtime.md](agent-runtime.md), [editing.md](editing.md) |
| Subagent disk layout | subagent coordinator + `new_with_explicit_dir` |

## See also

- [agent-runtime.md](agent-runtime.md) — turns, tools, permissions, plan, subagents
- [providers.md](providers.md) — multi-provider + compaction axes
- [editing.md](editing.md) — plan.md edit gate
- [architecture.md](architecture.md) — crate map
- [development.md](development.md) — build/test workflow
