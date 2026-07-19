# Subagents

How child sessions are spawned, tracked, isolated, resumed, billed, and healed. Paths are relative to the repo root under `crates/codegen/` unless noted.

End-user product docs: `xai-grok-pager/docs/user-guide/16-subagents.md`.  
Runtime overview (short): [agent-runtime.md](agent-runtime.md#subagents).

## Architecture snapshot

```text
Model `task` tool (parent SessionActor, depth 0)
        │ SubagentBackendResource → ChannelBackend
        ▼
SubagentEvent mpsc  ──►  MvpAgent::start_subagent_coordinator (spawn_local drain)
        │
        ├── Spawn → handle_subagent_request (own spawn_local)
        │              resolve definition / role / persona / capability
        │              optional worktree (xai-fast-worktree)
        │              spawn child SessionActor (depth 1, is_subagent)
        │              await prompt result (or background)
        │              fold usage → parent; write meta; notify
        ├── Query / Cancel / ListActive / Completions / Outstanding
        ├── ValidateType / DescribeType
        └── ClearUsageNotApplied / MarkUsageNotApplied
```

| Piece | Path |
| --- | --- |
| `task` tool | `xai-grok-tools/src/implementations/grok_build/task/` |
| `agent_swarm` tool | `xai-grok-tools/src/implementations/grok_build/agent_swarm/` |
| Input / capability / isolation types | `crates/common/xai-tool-types/src/task.rs` |
| Backend + events | `…/task/backend.rs`, `…/task/types.rs` |
| Coordinator + drain | `xai-grok-shell/src/agent/subagent/`, `mvp_agent/subagent_coordinator.rs` |
| Spawn handler | `agent/subagent/handle_request.rs` |
| Pure resolution | `xai-grok-subagent-resolution/` |
| Agent defs / prompts | `xai-grok-agent/` (`discovery`, `config`, `prompt/subagent_prompts`, templates) |
| Worktrees | `session/worktree.rs` → `xai-grok-workspace/src/worktree/`, `xai-fast-worktree/` |
| Child session spawn | `session/acp_session_impl/spawn.rs` |

## Spawn / task tool flow

1. **Model calls `task`** (UI/hook aliases may show `Task` / `spawn_subagent`; tool id is `task`).
2. **Depth check:** `SubagentDepthCounter` from shared resources; default `0`. If `depth >= MAX_SUBAGENT_DEPTH` (1), reject before spawn.
3. **Sanitize args:** blank/`"null"` `resume_from` and `cwd` cleared; model override ignored when resuming; `cwd` vs `isolation=worktree` mutual exclusion (nonexistent cwd + worktree → clear cwd).
4. **Eager `ValidateType`** via backend (unknown / disabled / not-allowed / coordinator unreachable).
5. **Optional model slug validation** (`TaskModelValidator`) when `model` is set and not resuming.
6. Build `SubagentRequest` → backend:
   - **`run_in_background: true`** — fire-and-forget spawn; tool returns started text + poll instructions.
   - **Foreground (default)** — await spawn result; may return **backgrounded** if await budget expires (still running, poll later).
7. Coordinator drain receives `SubagentEvent::Spawn` and runs `handle_subagent_request`.

Related tools: `get_command_or_subagent_output` / background task action, `kill_task`. Task tool **requires** those kinds in the toolset (`requires_expr`).

Dynamic tool description is **not** the static template on `TaskTool`; it is built in `xai-grok-agent` (`build_task_description` / `builder.rs`) from available agent types.

## Agent swarm flow

`agent_swarm` is a foreground-only orchestration tool for homogeneous, independent work. The shell treats it as an exclusive tool call: if a model batches it with another tool, the batch is rejected before execution.

Input rules:

- `description` is shared by every member; `subagent_type` defaults to `general-purpose` for newly spawned members.
- `items` requires at least two entries unless `resume_agent_ids` is also supplied. Total members are capped at 128.
- When `items` is present, `prompt_template` is required, must contain literal `{{item}}`, and must expand to distinct prompts.
- `resume_agent_ids` is an insertion-ordered object of completed child IDs to continuation prompts. Resume members launch first and preserve the source child profile (type/persona/model) rather than applying the new-member default.

Scheduling and output:

- Validate the full call before spawning anything, including new-member type validation.
- Launch up to five members immediately, then at most one additional member every 700 ms. `OPENGROK_AGENT_SWARM_MAX_CONCURRENCY` can apply a positive active-member cap.
- Launch priority is a suspended same-agent retry, then an explicit resume, then a new member. A retry resumes the live child turn through its scheduler decision lane; the child does not sleep or create a replacement session.
- The first provider 429 enters rate-limit phase. The affected member is requeued at the front with per-member eligibility of 3 s, 6 s, 12 s, 24 s, and so on. If it is already the only unfinished member, the scheduler fails it instead of leaving the swarm suspended indefinitely.
- Rate-limit capacity starts from the number of normal members that reached their first provider request, minus one, with a minimum of one. Later 429s shrink capacity by one at most every two seconds. While rate-limited, a scheduling pass starts at most one retry/resume/new member and requires both the global launch gate and the selected member's eligibility deadline to have elapsed.
- A provider-ready attempt resets the global retry interval to three seconds. If work remains queued and no 429 occurs for three minutes, capacity recovers by one once for that quiet window; another 429 starts a new window.
- Each member has a two-hour default timeout (`OPENGROK_SUBAGENT_TIMEOUT_MS`; `0` disables it).
- Results are collected into fixed input-order slots and returned under `<agent_swarm_result>`, including resumable agent IDs for unfinished work.
- Swarm metadata rides on ordinary `SubagentSpawned` / progress / finish notifications, so coordinator lifecycle, usage fold-back, permissions, resume identity, and worktree handling remain the normal subagent paths.

Swarm mode can be entered manually (`/swarm`, `/swarm on`) or for one turn (`/swarm <task>` / direct `agent_swarm`). Manual mode survives turns and takes precedence over one-shot triggers; task/tool activation auto-exits at the turn boundary.

## SubagentCoordinator lifecycle

`SubagentCoordinator` is a field on `MvpAgent` (`RefCell`). It is **not** itself a long-lived OS thread; the **drain task** is.

### Maps / state

| Map | Meaning |
| --- | --- |
| `pending` | Inserted at start of `handle_subagent_request` (pre-session); cancelled/failed before promote cleanup here |
| `active` | Child `SessionHandle` promoted; progress + cancel tokens live here |
| `completed` | Terminal results for poll / resume / finish re-emit; subject to eviction |
| `pending_completions` | Buffered summaries for idle / completion drain |
| `block_wait_slots` | Foreground `Query(block=true)` waiters |
| `running_gauge` | `pending + active` count (recomputed, not ±1) |
| `is_turn_active` | Parent turn flag for freeze/outstanding logic |
| `subagent_usage_not_applied_prompts` | Sticky incomplete-bill markers per parent prompt |

### Drain task (`start_subagent_coordinator`)

- Takes `subagent_event_rx` **once** (idempotent no-op after).
- Outer `spawn_local` loop on `SubagentEvent`.
- Each **Spawn / Query / ValidateType / DescribeType** gets its **own** `spawn_local` so concurrent children do not block the drain.
- Uses `LocalRef` into `MvpAgent` (LocalSet / `!Send` pattern — do not move across threads).
- Builds `SubagentSpawnContext` from parent session (MCP pool, client hooks, tool snapshot, cwd, auth, models, …).

### Completion path (high level)

1. Child prompt finishes / cancels / errors.
2. Meta flipped to terminal status under parent `subagents/<id>/`.
3. Usage folded into parent (`RecordSubagentUsage`); on failure mark incomplete.
4. Tracker moved to `completed`; optional worktree snapshot + remove.
5. ACP `SubagentFinished` (and related notifications) via gateway + parent cmd channel.
6. Foreground result returned on spawn oneshot / block wait; background surfaces via completions / auto-wake.

## Agent definition, personas, capability modes

### Definition resolution

`resolve_agent_definition(subagent_type, ctx)`:

1. `xai_grok_agent::discovery::by_name_in_cwd_with_plugins` (project / user / plugin / built-in).
2. Else match `cli_agents` on parent config.
3. Apply **session CLI tool/permission overrides** so children cannot skip parent CLI pins.

Built-in types (user-facing): `general-purpose`, `explore`, `plan` (and discovery-defined customs). Toolset flavor can be adjusted by harness / parent file-tool overrides (`resolve_subagent_toolset`).

Gating after resolve: `[subagents.toggle]` (absent key = **enabled**), parent `allowed_subagent_types` allow-list.

### Roles and personas (pure crate)

`xai-grok-subagent-resolution` owns **pure** merge logic (no session/coordinator deps):

| Layer | Source |
| --- | --- |
| Explicit spawn overrides | `SubagentRuntimeOverrides` (model, isolation, capability_mode, persona, …) |
| Role | `SubagentRole` keyed by `subagent_type` (or persona name fallback) |
| Persona | `SubagentPersona` map from config / `.opengrok/personas/` |

**Field precedence** (each field independently): **explicit > role > persona > none**.

- **Persona instructions:** fail-closed on file I/O errors (abort spawn); config “not found” / empty is non-fatal but sets `persona_error`.
- **Role `prompt_file`:** soft degrade (warn, continue without role prompt).
- Injected as conversation `<system-reminder>` for new/forked context (not re-applied on resume the same way; resume re-renders system prompt from current definition).

Model-issued `task` does **not** pass persona today (persona via roles/resolution); harness/goal paths may set persona on `runtime_overrides`.

### Capability modes

`SubagentCapabilityMode` (`xai-tool-types`): `read-only` | `read-write` | `execute` | `all`.

Applied in `handle_subagent_request` via `filter_tool_config` on the agent’s `tool_config` **after** definition resolve, **before** depth strip:

| Mode | Keeps (roughly) | Drops |
| --- | --- | --- |
| `read-only` | Read / search / list / LSP / web / plan enter-exit / task+wait-kill / ask | Edit / write / execute |
| `read-write` | Read + edit/write (+ non-execute set) | Execute (bash) |
| `execute` | Read + execute | Edit/write |
| `all` | No filter | — |

`ToolKind::None` / untyped tools are retained. After filter, orphaned background-task-only tools may be pruned.

### Isolation default (separate axis)

`SubagentIsolationMode`: `none` (default) | `worktree`.

Precedence: **explicit override > role `default_isolation` > persona `default_isolation` > `none`**.

Additionally, if effective isolation is still `none` but `AgentDefinition.isolation == Worktree`, isolation is promoted to worktree.

**Children do not inherit the parent’s worktree by default.**

## Depth limit (`MAX_SUBAGENT_DEPTH = 1`)

```text
Parent session depth 0  →  may call task
Child  session depth 1  →  task + agent_swarm stripped / calls rejected
```

Two complementary guards:

1. **Call-time reject** in `TaskTool::run` when `SubagentDepthCounter >= 1`.
2. **Toolset strip** in `handle_subagent_request`: if `parent_depth + 1 >= MAX_SUBAGENT_DEPTH`, remove `ToolKind::Task` and `ToolKind::AgentSwarm`, then prune orphaned background task tools so the model never sees a nested spawn surface.

Child `tool_context.subagent_depth` and shared `SubagentDepthCounter` are set to `parent_depth + 1` at spawn. Nested depth > 1 is unsupported by design (flat tree).

## Permissions vs plan mode

| Concern | Parent | Child |
| --- | --- | --- |
| Permission actor | Own manager or CLI | **Same `PermissionHandle`** (`inherited_permission_handle`; `owns_permission_manager = false`) |
| Always-approve / YOLO | Session policy | **Inherited** (plus child agent permission mode resolution / policy pins) |
| Hooks | Parent registry + client hooks snapshot | Inherited client hooks; tool prepare still runs PreToolUse |
| Plan tracker | May be Active | **Fresh `PlanModeTracker::new` (Inactive)** — parent plan gate does **not** apply to children |
| Plan file | Parent `session_dir/plan.md` | Child’s own session dir / plan files if it later enters plan mode |

Implications:

- Approvals granted on the parent apply to the child (shared handle).
- A parent stuck in plan mode does **not** force children into plan edit gating.
- Do not “fix” plan isolation only in the permission manager; child trackers are separate by construction at spawn.

## Worktree isolation

When effective isolation is `worktree` (and not resumed without a source worktree):

1. Resolve base dir via `worktree_base_dir_for_source` (fallback temp `grok-subagent-worktrees/<id>`).
2. `xai_fast_worktree::WorktreeBuilder` with `WorktreeKind::Subagent`, creation mode from shell worktree type, optional btrfs delegate.
3. Child `cwd` becomes the worktree path; on failure, **fall back to shared workspace** (log + continue).

On completion (when snapshot dispose enabled):

- `snapshot_subagent_worktree` → persist `snapshot_ref` on meta → `remove_subagent_worktree`.
- Snapshot failures leave the worktree on disk for review/resume.

**Resume:** rehydrate from `snapshot_ref` when the worktree dir is gone; if source had no worktree, ignore `isolation=worktree` override. `cwd` is ignored when `resume_from` is set (inherit source directory).

Shell facade: `session/worktree.rs` re-exports workspace helpers. Low-level create/remove: `xai-fast-worktree`. Workspace orchestration: `xai-grok-workspace/src/worktree/`.

## `resume_from` constraints

Pass prior `subagent_id` (same as `resume_from_hint` on completed output).

| Rule | Behavior |
| --- | --- |
| Source still **running** | Reject — wait for completion first |
| Source missing / wrong parent / evicted | Reject — not found |
| **Type** must match source | Hard fail (`validate_resume_identity`) |
| **Persona** if requested must match | Hard fail when explicitly set |
| **Model** | Soft-ignore caller override; **pin source model**; fail if source model no longer in catalog |
| Transcript / tool state | Inherited (resumed context source) |
| System prompt / prompt context | Freshly rendered from **current** agent definition |
| Worktree / cwd | From source (rehydrate if needed); request `cwd` ignored |

Blank / `"null"` resume ids treated as absent (`is_valid_resume_id`).

## Session directories and `subagents/` meta

Children are **full sessions** under the normal session layout:

```text
$OPENGROK_HOME/sessions/<encoded-cwd>/<child-session-id>/
  updates.jsonl, chat_history.jsonl, summary.json, …
```

Parent-side metadata (not a substitute for the child session):

```text
$OPENGROK_HOME/sessions/<encoded-parent-cwd>/<parent-session-id>/
  subagents/<subagent_id>/
    meta.json          # SubagentMeta: status, type, persona, cwd, worktree, model, …
```

`meta.json` statuses include `running` → terminal (`completed` / `cancelled` / error fields). Used for:

- UI / ACP spawn-finish pairing  
- `resume_from` identity + worktree snapshot_ref  
- **Orphan reconcile** after crash/rewind  

Child session id is typically the same string as `subagent_id` (spawn uses `SessionId::new(subagent_id)`).

## Usage fold-back into parent

On child completion:

1. `chat_state_handle.try_get_session_usage()` → `by_model` + `incomplete` flag.
2. Parent `SessionCommand::RecordSubagentUsage` (ack required).
3. If fold fails / no parent channel: warn, `MarkSubagentUsageNotApplied` for the parent prompt id (sticky incomplete bill / report flag).
4. Coordinator also tracks `subagent_usage_not_applied` for outstanding reply / freeze drain.

Foreground children of a parent prompt are **outstanding** until complete (block turn freeze); **background** children do not block the turn but may mark the prompt report incomplete while live (`background_live_for_prompt`).

Tests: `session/acp_session_tests/subagent_usage_fold_tests.rs`.

## Orphan reconcile

`reconcile_orphaned_subagents` (in `agent/subagent/mod.rs`):

**When:** after session replay / restore so finish events order after spawn.

**Inputs:**

- `unfinished` list of `(subagent_id, child_session_id)` from replayed spawns missing finish  
- On-disk `subagents/*/meta.json` with `status == "running"` for this parent  
- Live coordinator `pending` / `active` (never heal live ids)

**Actions (one finish per id):**

| Situation | Action |
| --- | --- |
| Running meta, not live, no completed result | Flip meta → `cancelled`, error `"interrupted by process restart"`, emit `SubagentFinished` |
| Running meta but coordinator has terminal result | Re-emit real finish (lost meta write) |
| Terminal meta (rewound finish dropped) | Re-emit real outcome from meta |
| No meta but unfinished spawn | Emit cancelled finish for inherited orphan |

Meta write failure on finalize → skip notify so a later reload can re-heal. Idempotent on already-terminal / other-parent metas.

## Key source paths

| Area | Location |
| --- | --- |
| Task tool + depth constant | `xai-grok-tools/.../task/mod.rs` (`MAX_SUBAGENT_DEPTH`) |
| Backend channel | `…/task/backend.rs` |
| Coordinator types | `xai-grok-shell/src/agent/subagent/mod.rs` |
| Lifecycle (pending/active/complete) | `…/coordinator_lifecycle.rs` |
| Query / cancel / list | `…/coordinator_query.rs` |
| Spawn orchestration | `…/handle_request.rs` |
| Drain wiring | `mvp_agent/subagent_coordinator.rs` |
| Resolution | `xai-grok-subagent-resolution/src/{overrides,resume,config,types}.rs` |
| Discovery / defs | `xai-grok-agent/src/{discovery,config,builder}.rs` |
| Subagent system prompt template | `xai-grok-agent/templates/subagent_prompt.md` |
| Session spawn inherit | `session/acp_session_impl/spawn.rs` (`owns_permission_manager`, plan tracker) |
| Worktree helpers | `session/worktree.rs`, `xai-grok-workspace/src/worktree/` |
| Usage fold | `session/acp_session_impl/updates.rs` (`record_subagent_usage`) |
| Pager UI tracking | `xai-grok-pager/src/acp/tracker.rs` (task / spawn titles) |

## Tests

| Area | Where |
| --- | --- |
| Coordinator unit / reconcile | `agent/subagent/tests/` (`mod.rs`, `rest.rs`) |
| Task tool / depth / capability filter | `xai-grok-tools/.../task/mod.rs` `#[cfg(test)]` |
| Resolution / resume identity | `xai-grok-subagent-resolution` unit tests |
| Usage fold | `session/acp_session_tests/subagent_usage_fold_tests.rs` |
| Permission inherit / owns flag | `session/acp_session_tests/permission_auto_mode_tests.rs` |
| Orphan reconcile integration | `xai-grok-shell/tests/test_subagent_orphan_reconcile.rs` |
| Spawn context | `mvp_agent/tests/subagent_*` (see agent-runtime test index) |
| Pager subagent views | `xai-grok-pager/src/app/acp_handler/tests/subagents.rs` |

## Gotchas

| Pitfall | Result |
| --- | --- |
| Raising depth without dual guards | Nested spawn still possible if only strip or only call-time check changes |
| Teaching only permission manager about parent plan mode | Children correctly Inactive — do not couple child edits to parent plan.md |
| Assuming child has separate always-approve | Shared `PermissionHandle` — YOLO/grants leak intentionally |
| Expecting isolation to inherit parent worktree | Default is **none**; explicit/role/persona/definition only |
| Setting both `cwd` and `isolation=worktree` | Hard error if cwd is a real directory |
| `resume_from` while source running | Hard error |
| Changing type/persona on resume | Hard identity fail |
| Passing model on resume | Silently ignored; source model pinned |
| Skipping meta write / finish notify | UI stuck “running”; orphan reconcile must heal |
| Usage fold without ack | Parent bill incomplete (`subagent_usage_not_applied`) |
| Waiting on background children in turn freeze | Background excluded from outstanding; use poll / completion drain |
| Moving coordinator work off LocalSet | `SessionActor` / `MvpAgent` are `!Send` — use `spawn_local` |
| Treating Code Mode / MCP tools named like task specially in pager only by string | Prefer stable meta / tool kind; pager has title matchers for legacy names |

## Cross-cutting “when editing X also update Y”

| Change | Also update |
| --- | --- |
| Max depth | `MAX_SUBAGENT_DEPTH`, task tool tests, strip path in `handle_request`, this doc + AGENTS.md |
| Resume identity | `xai-grok-subagent-resolution/resume.rs` + shell resume lookup tests |
| Isolation defaults | resolution overrides + agent definition isolation field + worktree create/resume paths |
| Permission inherit | `spawn.rs` `owns_permission_manager` + permission tests |
| Meta schema | write/read meta helpers, orphan reconcile, resume source fields |
| Capability modes | `SubagentCapabilityMode` + `filter_tool_config` + task tool tests |
| User-facing behavior | `xai-grok-pager/docs/user-guide/16-subagents.md` |

## See also

- [agent-runtime.md](agent-runtime.md) — turn loop, permissions order, plan mode, session files  
- [editing.md](editing.md) — plan-mode edit gate (parent vs child trackers)  
- [architecture.md](architecture.md) — crate map  
- [providers.md](providers.md) — child provider boundary / export observation on non-xAI models  
- User guide: `xai-grok-pager/docs/user-guide/16-subagents.md`
