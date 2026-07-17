# Agent runtime

Implementation map of sessions, turns, tools, permissions, plan mode, subagents, goals, background work, compaction, and ACP. Paths are relative to the repo root under `crates/codegen/` unless noted.

End-user docs: `xai-grok-pager/docs/user-guide/15-agent-mode.md` through `22-permissions-and-safety.md`.

## Architecture snapshot

```text
ACP Client (pager / IDE / headless)
        │
xai-acp-lib
        │
xai-grok-shell::MvpAgent  (LocalSet, !Send)
   ├── SessionActor (per session, own OS thread + LocalSet)
   │     ├── ChatStateActor
   │     ├── PermissionHandle
   │     ├── ToolBridge → FinalizedToolset → WorkspaceOps.call_tool
   │     ├── PlanModeTracker / GoalTracker
   │     ├── CodeModeRuntime (optional)
   │     ├── HunkTrackerHandle
   │     └── Persistence actor → $OPENGROK_HOME/sessions/...
   ├── SubagentCoordinator
   └── Sampler handle
```

### Core modules

| Concern | Path |
| --- | --- |
| Session main loop | `xai-grok-shell/src/session/acp_session_impl/run_loop.rs` |
| Prompt + agentic loop | `…/acp_session_impl/turn.rs` (`handle_prompt`) |
| Sampling | `…/acp_session_impl/sampler_turn.rs` |
| Tool prepare / execute | `…/acp_session_impl/tool_calls.rs` |
| Dispatch to workspace | `…/acp_session_impl/tool_dispatch.rs` |
| Session actor | `…/session/acp_session.rs` |
| Spawn / thread | `…/acp_session_impl/spawn.rs` |
| Host agent | `…/agent/mvp_agent/` |
| Agent definitions | `xai-grok-agent/src/` |
| Lifecycle contributors | `xai-agent-lifecycle/` |

## Turn loop

1. Client sends `session/prompt` (or a synthetic origin for scheduler / goal / plan-resume).
2. `SessionActor::handle_prompt`:
   - Marks turn active
   - Fires lifecycle `on_turn_start`
   - Reconciles plan mode with prompt mode
   - Handles direct-bash meta and slash commands as applicable
3. **Agentic loop** repeats:
   - Build request from `ChatStateHandle`
   - `run_turn_via_sampler` → stream
   - On tool calls:
     - **Code Mode control** (`exec` / `wait` / CodeModeOnly routing) or
     - **Direct tools** via `execute_tool_calls`
   - Outcomes: permission reject can cancel turn; hook deny continues with failure result; followups inject user messages
   - Preflight overflow → compact and continue
   - Honor `max_turns`
4. Auth recovery uses **`AuthRetrySchedule` of 1s / 2s / 4s** (do not inflate bases — hang regressions).

### Invariants

- `SessionActor` is **`!Send`** — use LocalSet patterns only.
- Persist user-message echoes for synthetic origins; hide in UI via meta (`hideFromScrollback`), do not skip persistence.
- Code Mode nested tools must **not** append top-level function results as ordinary model tool history (`ModelToolResultSink::CodeMode`).
- Concurrent same-file edits serialize via path locks (`file_path` / `path` / `target_file`).
- Lifecycle contributors are **data-only** — they must not own loop control.

## Tools

### Stack

```text
Model tool_call
  → prepare_tool_call (parse, plan gate, hooks, permissions)
  → WorkspaceOps::call_tool
  → ToolBridge / FinalizedToolset
  → xai_tool_runtime::Tool::call  (stream → Terminal)
  → ToolOutput + prompt_text
  → ACP session/update + chat_state tool_result
```

| Piece | Path |
| --- | --- |
| Registry / finalize | `xai-grok-tools/src/registry/` |
| Bridge | `xai-grok-tools/src/bridge.rs` |
| Runtime trait | `crates/common/xai-tool-runtime/` |
| Wire protocol | `crates/common/xai-tool-protocol/` |
| Grok Build pack | `xai-grok-tools/src/implementations/grok_build/` |
| Codex pack | `…/implementations/codex/` |
| OpenCode pack | `…/implementations/opencode/` |
| Hashline pack | `…/implementations/grok_build_hashline/` |

### Built-in grok_build tools (representative)

`bash`, `read_file`, `search_replace`, `list_dir`, `grep`, `todo`, `task` / task wait-kill, `monitor`, `scheduler_*`, `enter_plan_mode` / `exit_plan_mode`, `ask_user_question`, `update_goal`, `web_search`, `web_fetch`, `lsp`, image/video generation, …

Full pack / registry / taxonomy map: [tools.md](tools.md). File mutation details: [editing.md](editing.md).

### Tool invariants

- Implement `xai_tool_runtime::Tool` with typed Args/Output; stream ends with **exactly one** `Terminal` item.
- Respect default output caps (~40KB tool output); truncation is intentional.
- MCP tool names for rules are `server__tool` (no `mcp__` prefix).
- Code Mode nested tools re-run the **full** prepare path (plan + hooks + permissions).

## Permissions and safety

### Order in `prepare_tool_call`

1. **Plan-mode edit gate** — hard reject non-plan-file edits when Active  
2. **PreToolUse hooks** — deny stops; allow does not skip later checks; hooks **fail open**  
3. **Plan-file auto-approve** — `plan.md` only  
4. **Permission manager** — `PermissionHandle::request`

Implementation: `xai-grok-workspace/src/permission/` (`manager`, `policy`, `rules`, `resolution`, `bash_command_splitting`, `auto_mode`, `prompter`, `state`).

OS sandbox: `xai-grok-sandbox/` — process-wide, irreversible for the session life; profile pinned on resume.

### Decision kinds

`Allow` | `Ask` | `Reject` | `PolicyDeny` (return to model; do not cancel turn) | `Cancelled` | `FollowupMessage`

### Gotchas

- Priority across sources: **deny > ask > allow**.
- YOLO short-circuits after policy deny/hooks; shell **ask** rules still apply; remembered grants are not consulted under YOLO.
- Bash: deny/ask per segment; allow matches **whole string** only (`git *` can cover `git status && rm` if allow is whole-string).
- Dangerous commands re-prompt even with remembered grants; `rg --pre` is never safe-listed.
- Subagents share parent permission actor (`owns_permission_manager = false`).
- Plan mode ≠ permission “plan” defaultMode (compat only).

## Plan mode

| Piece | Path |
| --- | --- |
| State machine | `xai-grok-shell/src/session/plan_mode.rs` |
| Edit gate | `tool_calls.rs` → `plan_mode_edit_gate` |
| Tools | `xai-grok-tools/.../enter_plan_mode/`, `exit_plan_mode/` |
| Persistence | `plan_mode.json` + `plan.md` under session dir |
| UI | pager `views/plan_approval_view.rs`, `/plan`, `/view-plan` |

**States:** `Inactive` → `Pending` → `Active` → `ExitPending`.

**Edit policy:**

- Enforced **outside** permission YOLO (gate runs first).
- Only `session_dir/plan.md` is auto-approved for edits when Active.
- Other edits rejected; **`apply_patch` always rejected** in plan mode.
- Bash/read/MCP not gated by plan mode (still subject to permissions).
- Children start with **Inactive** plan trackers.

## Goals

- Model-driven via `update_goal`.
- State: `session/goal_tracker.rs` + planners under `session/goal_*.rs`.
- High-frequency progress is gateway-only (avoid JSONL blowup).
- Synthetic origins: `GoalSummary`, `GoalClassifierNudge`.

Deep map: [memory-and-goals.md](memory-and-goals.md).

## Subagents

Deep implementation map: [subagents.md](subagents.md).

| Piece | Path |
| --- | --- |
| Spawn tool | `xai-grok-tools/.../task/` (`MAX_SUBAGENT_DEPTH = 1`) |
| Request handler | `xai-grok-shell/src/agent/subagent/handle_request.rs` |
| Coordinator | `agent/subagent/`, `mvp_agent/subagent_coordinator.rs` |
| Pure resolution | `xai-grok-subagent-resolution/` |
| Worktrees | `session/worktree.rs`, `xai-grok-workspace`, `xai-fast-worktree` |

**Flow:** model `task` → coordinator drain → resolve agent/persona/capability → optional worktree → child `SessionActor` → wait or return ID.

**Invariants:**

- Depth flat: parent 0, child 1 only.
- Inherit permission handle (including always-approve).
- Fresh plan tracker (Inactive).
- Isolation default: explicit > role > persona > **none** (does not inherit parent worktree by default).
- `resume_from` requires completed source of same type.

## Background tasks, monitors, scheduler

| Feature | Implementation |
| --- | --- |
| Background shell | bash `background: true` + terminal task tracking |
| Wait / kill | `task_output`, `kill_task` tools |
| Monitor | `implementations/grok_build/monitor/` (line → notifications) |
| Scheduler / `/loop` | `implementations/grok_build/scheduler/` + pager slash |

## Sessions, storage, compaction

Deep dive: **[sessions.md](sessions.md)** (identity, file layout, persistence actor, resume/fork/rewind, idle flush/dream, tests).

Under `$OPENGROK_HOME/sessions/<encoded-cwd>/<session-id>/`:

| File | Role |
| --- | --- |
| `updates.jsonl` | Authoritative ACP stream for resume |
| `chat_history.jsonl` | Model-facing messages |
| `summary.json` | Index metadata |
| `plan.json` | TODO tool state |
| `plan.md` / `plan_mode.json` | Plan content + tracker |
| `rewind_points.jsonl` | File snapshots |
| `compaction_checkpoints/` | Compaction state |
| `subagents/` | Child metadata (children are full sessions) |

Modules: `session/storage/`, `session/persistence.rs`, `session/compaction*.rs`, `xai-chat-state/`, `crates/common/xai-grok-compaction/`.

**Notes:**

- Resume identity: `updates.jsonl` is source of truth.
- Sandbox profile pinned for session life.
- Codex remote compaction vs xAI compaction must not leak opaque items across providers.
- Plan mode state is preserved across compact with reminders.

## Code Mode (runtime)

Deep map: **[code-mode.md](code-mode.md)**. Parity contract: [`../code-mode-port.md`](../code-mode-port.md).

| Piece | Path |
| --- | --- |
| Protocol | `xai-grok-code-mode-protocol/` |
| V8 runtime | `xai-grok-code-mode/` |
| Shell adapter | `xai-grok-shell/src/session/code_mode.rs` |
| Turn branches | `acp_session_impl/turn.rs` |
| Nested tools | `tool_calls.rs` → `dispatch_code_mode_nested_tool` |
| Contract | `docs/code-mode-port.md` |

When Code Mode is effective:

- `exec` is provider-compatible: Codex custom/freeform raw JS, xAI function envelope with a `source` string. `wait` is a function tool.
- Mixed Code Mode retains ordinary top-level tools; Code Mode Only leaves ordinary tools only under `tools.*` plus direct-only controls.
- Session-persistent V8; reset on rewind/incompatible route changes and dispose on session end.
- Mark transport with meta (`open-grok/codeModeTransport`); **do not** key UI on tool name alone (MCP might define `exec`).

## ACP surfaces

Full map (transports, extensions, reverse-RPC, meta keys, leader, headless, tests): **[acp.md](acp.md)**.

| Piece | Path |
| --- | --- |
| Channel | `xai-acp-lib/` |
| Agent entry | `shell/src/agent/mvp_agent/acp_agent.rs`, `server.rs`, `relay.rs` |
| Extensions | `shell/src/extensions/*.rs` |
| Pager client | `pager/src/acp/`, `app/acp_handler/`, `app/dispatch/` |

Standard ACP: `initialize`, `session/new|load|prompt`, `session/update`, `session/request_permission`.  
Grok extensions: `x.ai/*` methods and `_x.ai/session/update` notifications.

Reverse requests (permissions, questions, plan approval) use gateway reverse-RPC. Prefer typed failure kinds over substring matching error text.

Headless: permission prompts cancel rather than block.

## Cross-cutting “when editing X also update Y”

| Change | Also update |
| --- | --- |
| Plan edit policy | `plan_mode.rs` + `plan_mode_edit_gate` + pager plan UI tests |
| Subagent depth | Task tool constant + spawn path + docs |
| Permission rules | workspace permission tests + user-guide safety doc if user-facing |
| Tool schema | registry, ACP conversion, scrollback tool block if UI changes |
| Session file format | storage, restore, fork, migration tests |
| Auth stores | isolation tests, never cross `AuthManager` / codex-auth |

## Test index

| Feature | Where |
| --- | --- |
| Turns / auth retry | `acp_session_impl/turn.rs` unit tests |
| Plan mode | `session/plan_mode.rs`, `plan_mode_*_tests`, pager `acp_handler/tests/plan_mode.rs` |
| Permissions | `xai-grok-workspace/src/permission/*`, shell permission tests |
| Subagents | `mvp_agent/tests/subagent_*`, `tests/test_subagent_orphan_reconcile.rs` |
| Compaction | inline auto-compact tests, `xai-grok-compaction` |
| Code Mode | `xai-grok-code-mode` tests |
| Hunks | `xai-hunk-tracker` actor tests |
| Fork / load | `tests/test_fork_session.rs`, session load perf |
| ACP UI | pager `acp_handler/tests/*`, `dispatch/tests/*` |

## See also

- [acp.md](acp.md)
- [subagents.md](subagents.md)
- [tools.md](tools.md)
- [code-mode.md](code-mode.md)
- [editing.md](editing.md)
- [memory-and-goals.md](memory-and-goals.md)
- [hooks-plugins-skills.md](hooks-plugins-skills.md)
- [permissions-and-sandbox.md](permissions-and-sandbox.md)
- [providers.md](providers.md)
- [architecture.md](architecture.md)
