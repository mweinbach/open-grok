# Memory and goals

Implementation map of cross-session memory (`xai-grok-memory`), memory tools / slash commands, and goal-mode orchestration (`GoalTracker` + `update_goal`). Paths are relative to the repo root under `crates/codegen/` unless noted.

End-user docs: `xai-grok-pager/docs/user-guide/13-memory.md` (memory product UX). Goal slash / dashboard surfaces are covered in agent-mode and dashboard user guides; this page is the developer map.

## Architecture snapshot

```text
SessionActor
  ├── SessionMemory (memory_state.rs)
  │     ├── MemoryStorage          → $OPENGROK_HOME/memory/…
  │     ├── MemoryBackendParams    → shared search / embed config
  │     ├── MemoryIndex (sqlite)   → workspace index.sqlite
  │     └── dream / flush config
  ├── Memory tools (ToolBridge Resources)
  │     ├── memory_search
  │     └── memory_get
  ├── GoalTracker (Mutex, pure state machine)
  │     └── GoalOrchestration snapshot → session_dir/goal/…
  └── update_goal tool channel → drain in goal.rs
```

| Concern | Path |
| --- | --- |
| Core engine | `xai-grok-memory/` |
| Shell shim / re-exports | `xai-grok-shell/src/session/memory/` |
| Session memory state | `…/session/memory_state.rs` |
| Dream + tool registration | `…/session/acp_session_impl/memory_dream.rs` |
| Pre-compact flush pure logic | `…/session/helpers/memory_flush.rs` |
| Injection formatting | `…/session/helpers/memory_context.rs` |
| Compaction ↔ memory | `…/session/compaction.rs` |
| Goal state machine | `…/session/goal_tracker.rs` |
| Goal notifications | `…/session/goal_orchestrator.rs` |
| Goal drain / roles | `…/session/acp_session_impl/goal.rs`, `goal_support.rs` |
| Planner / strategist / verifier | `goal_planner.rs`, `goal_strategist.rs`, `goal_classifier.rs`, `goal_summarizer.rs`, … |
| Goal templates | `…/session/templates/goal_*.md` |
| `update_goal` tool | `xai-grok-tools/.../grok_build/update_goal/` |
| Memory tools | `xai-grok-tools/src/implementations/memory/` |

---

## Memory crate (`xai-grok-memory`)

### Enablement

Memory is **off by default**. Enable via (priority high → low):

1. `--no-memory` (always disables)
2. `--experimental-memory`
3. `GROK_MEMORY=1|0`
4. `[memory] enabled` in `$OPENGROK_HOME/config.toml`
5. Default: disabled

Mid-session: `/memory on|off` (session-scoped; does not rewrite config). `/memory` browse modal remains available when backend params exist even if currently toggled off (`memory_configured` gate).

### Storage layout under `$OPENGROK_HOME`

```text
$OPENGROK_HOME/memory/
  ├── MEMORY.md                              # Global curated knowledge
  └── {project-slug}-{hash8}/                # Workspace-scoped (blake3)
        ├── MEMORY.md                        # Project curated knowledge
        ├── index.sqlite                     # Chunks + FTS5 (+ optional vec0)
        ├── dream.lock / consolidation meta  # Dream gates (DreamLock)
        └── sessions/
              └── YYYY-MM-DD-{slug}-{sid8}.md  # Session / flush logs
```

- Workspace directory name: `{slug}-{hash8}` where `slug` is the repo/dir name (max 40 chars) and `hash8` is 8 hex chars from blake3 of a stable workspace identity (`storage::compute_workspace_hash`).
- **Never write under `~/.grok`.** `MemoryStorage` uses `grok_home()` → `$OPENGROK_HOME` / `~/.opengrok`.
- Ephemeral CWDs (temp dirs): workspace writes are silently skipped (`is_ephemeral`).
- Flat storage (`MemoryStorage::new_flat`) exists for project-local agent memory roots already scoped to a single project.

### Sources

| Source label | Content | Temporal decay |
| --- | --- | --- |
| `global` | `$OPENGROK_HOME/memory/MEMORY.md` | Evergreen (no decay) |
| `workspace` | workspace `MEMORY.md` | Evergreen |
| `session` | `sessions/*.md` flush / end-of-session logs | Exponential half-life |

Scaffold / empty `MEMORY.md` stubs are filtered out of search and injection (`search::is_content_free`, `dream::is_scaffold_template`).

### Index, embeddings, search

| Piece | Path / role |
| --- | --- |
| Schema | `schema.rs` — `meta`, `chunks`, `chunks_fts` (FTS5), optional `chunks_vec` (sqlite-vec) |
| Index API | `index.rs` — `MemoryIndex::open_or_create` |
| Chunking | `chunker.rs` |
| Hybrid search | `search.rs` — FTS BM25 + optional vector KNN + temporal decay + source weights + MMR |
| Embeddings | `embedding.rs` + `embed_missing_chunks` in `lib.rs` (batches of 32) |
| Backend for tools | `backend.rs` — `MemoryBackendImpl` / `MemoryBackendParams` |
| File watcher | `watcher.rs` — external edit → reindex on search |
| Query expansion | `query_expansion.rs` |
| Archive | `archive.rs` |

**Search pipeline (summary):** FTS always → vector KNN when available → merge/normalize → drop content-free → temporal decay (session only) → source weights / min_score → optional MMR → `max_results`. Degrades to FTS-only when embeddings or sqlite-vec are missing.

`MemoryBackendParams.search_source` labels telemetry paths:

| Label | When |
| --- | --- |
| `"tool"` | Model `memory_search` |
| `"injection"` | First-turn context injection |
| `"compaction_recovery"` | Post-compaction re-injection |

Share one `MemoryBackendParams` shape for ToolBridge, injection, and recovery so search config cannot silently diverge.

### Flush and dream

#### Flush (pre-compaction + manual)

| Piece | Path |
| --- | --- |
| Gate | `helpers/memory_flush::should_flush` |
| Prompt | `FLUSH_SYSTEM_PROMPT` (same module) |
| Orchestration | Compaction path in `session/compaction.rs`; manual `/flush` via shell slash |
| Write | `MemoryStorage::write_daily_log` (append with timestamped sections) |

Flush runs **below** the compact threshold (`soft_threshold_tokens` headroom) so the model can summarize before context overflows. While flushing, `SessionMemory.is_flushing` suppresses auto-compact. Response quality gates: `NO_REPLY`, markdown headers, semantic near-duplicate check before write. After write: reindex + embed missing chunks.

#### Dream (consolidation)

| Piece | Path |
| --- | --- |
| Gates / prompt | `xai-grok-memory/src/dream.rs` |
| Lock | `dream_lock.rs` |
| Session orchestration | `acp_session_impl/memory_dream.rs` (`maybe_run_dream`) |

Gates (cheap first): `dream.enabled` → hours since last consolidation ≥ `min_hours` → session count ≥ `min_sessions`. On open: lock, sample model with dream system prompt, merge into workspace `MEMORY.md`, reindex. **Subagents skip dream.** Model call is time-bounded (~60s).

#### Slash integration

Shell-advertised ACP slash commands (`session/slash_commands.rs`):

| Command | Gate | Role |
| --- | --- | --- |
| `/flush` | Memory tools registered + enabled | Immediate flush turn |
| `/dream` | same | Manual consolidation |
| `/memory` | `memory_configured` | Browse / toggle |

`/flush` and `/dream` hide when `memory_search` / `memory_get` are not registered; tool name constants are shared (`MEMORY_SEARCH_TOOL_NAME`, `MEMORY_GET_TOOL_NAME`) so a typo cannot silently hide slash without breaking tools (pinned by unit test).

### Memory tools

| Tool | Id | Module |
| --- | --- | --- |
| Search | `memory_search` | `implementations/memory/search_tool.rs` |
| Read file | `memory_get` | `implementations/memory/get_tool.rs` |

- Read-only; require `Arc<dyn MemoryBackend>` in tool Resources.
- If backend missing: text reply that memory is disabled (not a hard tool error).
- Registered at session setup when memory enabled; re-registered on `/memory on` via `register_memory_tools` (dynamic register path).

### Memory ↔ session / compaction

```text
First turn
  → search (injection params, min_score often 0.0)
  → format_memory_reminder → <memory-context> in system prefix
  → conversation_has_memory_context prevents re-search (KV cache stability)

Approaching compact
  → should_flush → flush model turn → write session log → reindex

Compact
  → PreCompact / PostCompact hooks
  → post-compact recovery search (search_source = compaction_recovery)
  → inject recovered memory into compaction context helpers

Session end
  → optional session summary write
  → maybe_run_dream
  → MemorySessionSummary telemetry
```

Do **not** re-score and rewrite an existing memory-context block mid-conversation — it busts the system-prompt prefix cache.

---

## Goals

Goal mode is **model-driven**: the user starts with `/goal <objective>` (feature-gated); the model reports progress via `update_goal`; the shell runs planning, optional strategist, verification (skeptic panel), and synthetic continuation turns.

### State machine (`GoalTracker`)

Pure (no async I/O), owned by `SessionActor` behind `Mutex` — same pattern as plan mode.

| Concept | Values / notes |
| --- | --- |
| `GoalPhase` | `Idle`, `Planning`, `Executing` |
| `GoalStatus` | `Active`, `UserPaused`, `BackOffPaused`, `NoProgressPaused`, `InfraPaused`, `Blocked`, `BudgetLimited`, `Complete` |
| Pause reasons | User / BackOff / NoProgress / Verification / Infra |
| Classifier verdict | `Achieved` \| `NotAchieved` (name keeps `Classifier` for wire stability) |

Unknown wire statuses deserialize to **`UserPaused`** so corrupt / future snapshots never resume as self-driving Active goals. History list capped (`GOAL_HISTORY_MAX = 64`). High-frequency progress (`emit_goal_updated_ephemeral`) is **gateway-only** — not JSONL — to avoid log blowup; durable state uses `PersistenceMsg::GoalModeState` and state-transition `GoalUpdated`s.

### On-disk under session dir

```text
$session_dir/goal/
  ├── plan.md              # Planner output
  ├── plan.baseline.md     # Immutable first plan (verifier diffs against this)
  ├── strategy.md          # Strategist advisory (does NOT replace plan.md)
  └── … classifier details # Moved out of scratch on terminal transitions
```

Scratch for implementer / skeptics lives under a verifier-id scratch root (`goal_scratch_root`), not the durable session tree. Paths are containment-checked before moves.

### Orchestration roles

| Role | Module | Capability gate |
| --- | --- | --- |
| Planner | `goal_planner.rs` | Writes `plan.md` |
| Strategist | `goal_strategist.rs` | Read + search + execute; may grant classifier cap bonus |
| Verifier / skeptic panel | `goal_classifier.rs` | Read + search |
| Summarizer | `goal_summarizer.rs` | Goal summary synthetic turn |
| Stop / next-step helpers | `goal_stop_detector.rs`, `goal_next_step.rs` | — |
| Role tool name resolution | `goal_role_tools.rs` | Inherit vs harness override |

Role spawns use `task` / subagent infrastructure with **fail-open** to parent model + harness when the configured agent type lacks required tools.

The planner must preserve any external system named as the objective's success
bar. For example, when an objective requires CI, a remote pipeline, or a
deployment to pass, that system's fresh verdict remains gating; a local rerun
is supporting evidence only. If the verifier cannot inspect the named system,
it reports the goal as unverifiable rather than silently substituting a local
check.

### `update_goal` tool

**Path:** `xai-grok-tools/.../grok_build/update_goal/`

Inputs: `completed`, `message`, `blocked_reason`.

Flow:

1. Tool sends `UpdateGoalEnvelope` over channel; **blocks on oneshot ack** so the model sees real outcomes (not optimistic success).
2. Mid-turn `completed: true` → `DeferredToTurnEnd` (ack immediately — parking deadlocks the LocalSet actor).
3. Turn-end drain runs verification when enabled.
4. Ack variants cover accept / classifier achieved / fail-open achieved / not achieved / cap / stall / blocked / concurrent / rejected.

`blocked_reason` requires **3 consecutive** blocked attempts before `Blocked` status. Classifier infra failure **fails open** to achieved (documented ack variant). Stall early-exit: identical gap fingerprint `GOAL_CLASSIFIER_STALL_THRESHOLD` times.

### Synthetic prompt origins

| `PromptOrigin` | `prompt_id` prefix | Hide user echo from scrollback |
| --- | --- | --- |
| `GoalSummary` | `goal-summary-` | Yes |
| `GoalClassifierNudge` | `goal-classifier-nudge-` | Yes |

Defined in `session/mod.rs`. Synthetic origins **must still persist** user-message echoes (for resume); UI hides via meta (`hideFromScrollback`). Do not skip persistence to “clean up” the transcript.

Goal setup uses templates (`goal_rules.md`, `goal_task_discipline.md`, `goal_continuation_directive.md`, …) injected as system reminders / synthetic user content with `SyntheticReason` tags where appropriate.

### Goals ↔ compaction / session

- Goal orchestration snapshot persists with session (`goal_mode_state` on persistence messages).
- Resume: in-flight Planning/Executing → Idle; Active becomes UserPaused (subagents do not survive restart).
- Token accounting: flush accurate tokens before status transitions (`goal_tokens`); high-water mark for subagent fold-in.
- Compaction does not clear goal tracker; continuation directives re-surface plan / strategy paths after context loss when goal remains Active.

Subagents inherit parent permission handle but **not** parent plan gate; goal children are orchestrated by the parent goal machinery (verifier/strategist), not nested `/goal` depth.

---

## Test index

| Area | Where |
| --- | --- |
| Memory schema / search unit | `xai-grok-memory` module tests (`schema`, `search`, …) |
| Memory tool id constants | `xai-grok-tools/.../memory/mod.rs` tests |
| Memory config / enablement | `shell/.../acp_session_tests/memory_config_tests.rs` |
| Memory context helpers | `shell/.../helpers/memory_context.rs` tests |
| Flush pure logic | `helpers/memory_flush.rs` (+ compaction flush tests) |
| Goal tracker / orchestrator | `goal_tracker.rs`, `goal_orchestrator.rs` unit tests |
| Goal e2e | `acp_session_tests/goal/*` (backoff, classifier, planner, strategist, summarizer, reminder rules) |
| PromptOrigin | `session/mod.rs` tests |
| Slash memory gates | `session/slash_commands.rs` availability tests |

Focused package checks:

```sh
cargo test --locked -p xai-grok-memory
cargo test --locked -p xai-grok-tools -- memory
cargo test --locked -p xai-grok-shell -- goal
cargo test --locked -p xai-grok-shell -- memory
```

---

## Gotchas

1. **Home isolation** — memory lives under `$OPENGROK_HOME/memory`, never `~/.grok`.
2. **Shared `MemoryBackendParams`** — do not construct ad-hoc backends for injection vs tools with different search configs unless intentional (`search_source` + min_score only).
3. **Do not mutate existing memory-context system prefix** after first injection — cache bust.
4. **`is_flushing` suppresses auto-compact** — always clear the flag in a finally-equivalent path.
5. **Ephemeral CWD** — writes appear “successful” path-wise but skip disk; tests should use real temp dirs with non-ephemeral paths or `with_paths`.
6. **`update_goal` always registered when toolset includes it** — calls outside goal mode reject with structured reason (not channel drop).
7. **Never park `update_goal` ack across turn end** for deferred classifier — deadlock.
8. **Gateway-only goal progress** must not be “fixed” by writing every tick to JSONL.
9. **Classifier fail-open on infra** vs fail-closed product policy — preserve ack variants and telemetry discriminators.
10. **Unknown goal status → UserPaused** — never invent Active from unknown wire.
11. **Dream skips subagents** — do not run consolidation on child sessions.
12. **Tool name constants** gate slash availability — keep `MEMORY_*_TOOL_NAME` in sync with `Tool::id`.

---

## See also

- [agent-runtime.md](agent-runtime.md) — turn loop, permissions, plan mode, session storage
- [providers.md](providers.md) — auxiliary models (memory flush/dream/recap) and provider isolation
- [tui-and-config.md](tui-and-config.md) — config layers, env vars, slash registration
- User guide: `xai-grok-pager/docs/user-guide/13-memory.md`
