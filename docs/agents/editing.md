# File editing, hunks, and plan-mode edits

How mutation tools work in Open Grok, what must stay true when changing them, and how plan mode / Code Mode / Codex toolsets interact.

## Edit tool families

| Family | Primary tools | Path |
| --- | --- | --- |
| Grok Build default | `search_replace`, `read_file`, `list_dir`, `grep`, `bash` | `xai-grok-tools/src/implementations/grok_build/` |
| Codex | `apply_patch`, Codex `read_file` / `list_dir` / `grep_files` | `…/implementations/codex/` |
| Hashline | Hashline-anchored edit / read / grep | `…/implementations/grok_build_hashline/` |
| OpenCode-compat | `edit`, `write`, `read`, … | `…/implementations/opencode/` |
| Code Mode nested | Same tools via JS `tools.*` | Shell `session/code_mode.rs` + nested dispatch |

Which set is visible to the model depends on **session tool mode / provider / Code Mode**, not on “whatever is compiled in.” Tools may still be registered for nested use while hidden top-level.

## `search_replace` (default Grok edits)

**Path:** `xai-grok-tools/src/implementations/grok_build/search_replace/`

Behavior highlights:

- Exact string replace; empty `old_string` creates / writes content (create path).
- Matching helpers normalize common whitespace/edge cases (see `helpers.rs`).
- Versioned variants under `versions/` (e.g. `legacy-0.4.10`) preserve old schemas for resume compatibility.
- Successful writes emit file-written style notifications for UI / hunk tracking.
- Concurrent edits to the same path are serialized at the session layer.

**When changing `search_replace`:**

1. Keep versioned compatibility if older sessions can still call the legacy schema.
2. Ensure the write path still records agent hunks (see below).
3. Do not bypass plan-mode gate by inventing a parallel write API.
4. Add unit tests next to helpers and any version module you touch.

## `apply_patch` (Codex freeform patches)

**Path:** `xai-grok-tools/src/implementations/codex/apply_patch/`

| File | Role |
| --- | --- |
| `parser.rs` | Parse freeform patch text |
| `apply.rs` | Apply to filesystem |
| `seek_sequence.rs` | Locate context |
| `tool.rs` | Tool wrapper + error mapping |
| `errors.rs` | Structured failures |

Access kind is edit-like (`AccessKind::Edit("apply_patch")`). **Always rejected in plan mode** because target files are not known until parse — plan mode only auto-approves the session plan file path.

**When changing `apply_patch`:**

- Preserve pure parser/apply separation so tests can cover without full shell.
- Do not special-case plan mode inside the tool; the gate is in shell `prepare_tool_call`.
- Keep Codex wire compatibility unless `docs/codex-provider-port.md` is updated deliberately.

## Hashline and OpenCode packs

- **Hashline** (`grok_build_hashline/`): alternate anchoring for edits; has its own apply/range policy. Treat as a separate schema with its own tests.
- **OpenCode** (`opencode/`): compatibility shapes (`edit`, `write`, …). Prefer not to drift semantics from the primary Grok tools without an explicit compat goal.

## Write pipeline (all edit tools)

```text
prepare_tool_call
  1. plan_mode_edit_gate          # reject non-plan.md edits when Active
  2. PreToolUse hooks
  3. plan.md auto-approve skip
  4. PermissionHandle::request    # YOLO / rules / prompt
dispatch_tool → WorkspaceOps → Tool::call
  → write bytes
  → record_agent_write (hunk tracker)
  → ACP tool updates + FileWritten notifications
```

### Plan-mode edit gate (critical)

Implemented in shell `session/acp_session_impl/tool_calls.rs` (`plan_mode_edit_gate`), **not** only in the workspace permission manager.

| Situation | Result |
| --- | --- |
| Plan Active + edit `session_dir/plan.md` | Allow (auto-approve path) |
| Plan Active + any other edit | Reject with plan-file-only guidance |
| Plan Active + `apply_patch` | Always reject |
| Plan Active + bash / read / MCP | Not gated here (normal permissions) |
| Plan Inactive | No plan gate |

Subagents start with **Inactive** plan trackers: a write-capable child can edit files while the parent is planning. That is intentional isolation, not a bug — change it only with explicit product design + tests.

## Hunk tracker

**Crate:** `xai-hunk-tracker`  
**Shell extension:** `xai-grok-shell/src/extensions/hunk_tracker.rs`  
**FS external changes:** `session/fs_watch.rs`

Behavior:

- Actor message-passing (no shared mutable locks inside actor state).
- **Agent** edits: tools / write path call `record_agent_write(path, content, prompt_index)`.
- **External** edits: filesystem notify → `handle_file_change`.
- Accept/reject per hunk / file / turn / all; LOC telemetry sink.

### Invariant

> If an agent write only hits the disk and only `fs_notify` observes it, the hunk is **External**. Regression tests in `xai-hunk-tracker` document this.

When adding a new mutation tool or write helper:

1. Reuse existing write helpers that already call `record_agent_write`, or
2. Call it yourself with the correct prompt index, and
3. Add a regression test if the path is new.

## Code Mode nested edits

In Code Mode Only, the model does not call `search_replace` top-level. It runs JavaScript via `exec` and calls nested tools (`tools.search_replace`, etc.).

Nested dispatch (`dispatch_code_mode_nested_tool`):

- Reuses **full** prepare path (plan gate + hooks + permissions).
- Returns structured JSON to the V8 cell, not a top-level function-result history item.
- TUI shows nested tool cards; transport `exec`/`wait` stay hidden (meta flag, not name alone).

Do **not**:

- Start a new V8 isolate per `exec`
- Allow `exec` to nest another `exec`/`wait` from inside JS
- Expose freeform `exec` as a JSON-schema function tool

Contract: [`../code-mode-port.md`](../code-mode-port.md).

## Permissions that affect edits

Workspace permission rules can allow/deny/ask on `Edit` / path globs. Order of authority with plan mode:

1. Plan gate can hard-reject before permission YOLO would allow.
2. If plan gate passes, permission manager still runs (unless plan-file auto-approve short-circuits the prompt).
3. Hooks can deny even when permissions would allow; hooks cannot alone authorize when policy denies.

User-facing safety: `user-guide/22-permissions-and-safety.md`.

## Rewind and file state

Rewind snapshots and file-state restoration live in workspace session file state + shell rewind paths (see session `rewind_points.jsonl`). Edits that bypass the tool layer may not rewind correctly — keep mutations on the tool / workspace write paths.

## Testing edits

| Layer | Tests |
| --- | --- |
| `search_replace` helpers / versions | Module tests under `search_replace/` |
| `apply_patch` parser/apply | `codex/apply_patch/` unit tests |
| Plan gate | shell `plan_mode_edit_gate_tests`, mid-turn plan tests |
| Hunks | `xai-hunk-tracker/src/actor/tests.rs` |
| Scrollback diff UI | pager `scrollback/blocks/tool/edit` snapshots |
| PTY e2e | `xai-grok-pager/tests/pty_e2e/` when UX must be proven |

### Suggested focused commands

```sh
cargo test --locked -p xai-grok-tools -- search_replace
cargo test --locked -p xai-grok-tools -- apply_patch
cargo test --locked -p xai-hunk-tracker
cargo test --locked -p xai-grok-shell -- plan_mode
```

## Checklist for any new edit capability

- [ ] Registered in the correct tool pack / Code Mode nested namespace
- [ ] Correct `AccessKind` so plan + permission rules apply
- [ ] Plan-mode behavior defined and tested
- [ ] `record_agent_write` (or shared write path) invoked
- [ ] Output size / binary safety considered
- [ ] ACP tool meta stamped for TUI
- [ ] Scrollback block behavior acceptable (or new block type)
- [ ] Docs updated (`AGENTS.md` short map + this file if contract changes)

## See also

- [agent-runtime.md](agent-runtime.md)
- [providers.md](providers.md) (Codex toolset selection)
- User guide: plan mode, permissions, sessions
