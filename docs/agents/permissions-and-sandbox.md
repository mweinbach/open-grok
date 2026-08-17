# Permissions and OS sandbox

Implementation map of the permission pipeline, rule merge, bash classification, folder trust, subagent inheritance, and the process-wide OS sandbox. Paths are relative to the repo root under `crates/codegen/` unless noted.

End-user docs: `xai-grok-pager/docs/user-guide/22-permissions-and-safety.md`, `18-sandbox.md`, `10-hooks.md`, `07-mcp-servers.md`. Plan-mode edit policy details: [editing.md](editing.md). Turn/tool wiring: [agent-runtime.md](agent-runtime.md).

## Architecture snapshot

```text
Model tool_call
  → shell prepare_tool_call
      1. plan_mode_edit_gate          # hard reject non-plan.md edits when Active
      2. PreToolUse hooks             # deny stops; allow does not skip later checks
      3. plan.md auto-approve         # AccessKind::Edit(plan file) only
      4. PermissionHandle::request    # policy → YOLO/auto → grants → prompt
  → WorkspaceOps::call_tool
  → Tool::call  (OS sandbox already applied process-wide if enabled)
```

| Layer | Crate / path |
| --- | --- |
| Prepare order (plan + hooks + request) | `xai-grok-shell/src/session/acp_session_impl/tool_calls.rs` |
| Plan gate | same file → `plan_mode_edit_gate` |
| Permission actor / handle | `xai-grok-workspace/src/permission/manager.rs` |
| Rule DSL + `defaultMode` | `…/permission/rules.rs` |
| Merge (native / Claude / managed) | `…/permission/resolution.rs` |
| Evaluate (deny > ask > allow) | `…/permission/policy.rs` |
| Bash split / wrappers | `…/permission/bash_command_splitting.rs` |
| Shell file-access escalation | `…/permission/shell_access.rs` |
| Remembered grants | `…/permission/state.rs` |
| ACP prompter / MCP naming helpers | `…/permission/prompter.rs` |
| Auto mode classifier | `…/permission/auto_mode.rs` |
| AccessKind / Decision | `…/permission/types.rs` |
| Folder trust (consume gate) | `xai-grok-shell/src/agent/folder_trust.rs` |
| Folder trust (decide / store) | `xai-grok-workspace/src/folder_trust.rs`, `trust.rs` |
| OS sandbox | `xai-grok-sandbox/` |

## Permission pipeline order

Enforced in `SessionActor::prepare_tool_call` **before** dispatch. Nested Code Mode tools re-run this full path.

### 1. Plan-mode edit gate

Hard reject when plan mode is Active and the access is an edit that is not the session plan file.

| Situation | Result |
| --- | --- |
| Active + edit `session_dir/plan.md` | Allow (later plan-file auto-approve) |
| Active + any other `AccessKind::Edit` | Reject (tool result to model; turn continues) |
| Active + `apply_patch` | Always reject (targets unknown until parse) |
| Active + bash / read / MCP / web | **Not** gated here |
| Inactive | No plan gate |

**Invariant:** this gate lives in the **shell**, not only in the permission manager. YOLO / always-approve does **not** bypass it. Do not “fix” plan mode only inside `PermissionHandle`.

### 2. PreToolUse hooks

- A hook **deny** stops the call (failure result to the model; turn continues).
- A hook **allow** does **not** skip later checks.
- Hooks **fail open** (missing registry / errors do not alone authorize against policy deny). They are not a security boundary by themselves — combine with deny rules and sandbox.

### 3. Plan-file auto-approve

When `AccessKind::Edit(path)` and the plan tracker says that path is the session `plan.md`, the permission manager request is skipped for that call.

### 4. `PermissionHandle::request`

Inside the permission actor (`manager.rs`), roughly:

1. **Compiled policy** on the access (direct evaluate).
2. For **Bash**: also `evaluate_bash_command_policy` (per-segment deny/ask) and `evaluate_shell_file_access` (readers/writers/redirects vs Read/Edit rules).
3. Policy **deny** → `Decision::PolicyDeny` (before YOLO).
4. Policy/shell **Ask** blocks YOLO and auto fast-paths (forced prompt).
5. **YOLO / always-approve** → `Allow` (unless shell-forced ask; deny already returned).
6. **Session grants** (remembered bash prefixes, MCP tools/servers, web_fetch domains, session allow-edits) before auto classifier.
7. **Auto mode** (fast-path allowlist / classifier) when enabled and not force-prompted.
8. Bash segment evaluation (safe lists, grants, dangerous list) / MCP pre-decision / static allowlists.
9. **Prompt policy** (`Ask` / `Deny` / auto) → interactive ACP prompt, or cancel/deny in headless.

`PermissionHandle::AllowAll` (tests / special spawns) skips the actor entirely.

## AccessKind

Defined in `permission/types.rs`. Built from `ToolInput` via `From<&ToolInput>`.

| Variant | Typical tools |
| --- | --- |
| `Read(Option<path>)` | `read_file`, `list_dir`, skill/todo/task-control (path often `None`) |
| `Grep { path, glob }` | `grep` |
| `Edit(path_or_label)` | `search_replace`, `write`, hashline edit; `apply_patch` uses label `"apply_patch"` |
| `Bash(command)` | `bash`, `monitor` |
| `MCPTool { name, input }` | MCP / `use_tool` (args kept for classifier + telemetry) |
| `WebFetch(url)` | `web_fetch` |
| `WebSearch(query)` | `web_search` |

Rules filter by `ToolFilter` (`Any`, `Bash`, `Edit`, `Read`, `Grep`, `Mcp`, `WebFetch`, `WebSearch`). **Read rules also govern Grep** at evaluation time (grep reads file contents).

## Decision kinds

| `Decision` | Meaning for the turn |
| --- | --- |
| `Allow` | Proceed to dispatch |
| `Ask` | Policy forced a prompt (internal; becomes prompt path) |
| `Reject(reason)` | User (or session) rejection; typically cancels or stops the tool |
| `PolicyDeny(reason)` | Managed/config deny — **return error to model**; do **not** cancel the turn |
| `Cancelled` | User cancelled (e.g. Cmd+C) or requester gone — `StopReason::Cancelled` |
| `FollowupMessage(text)` | Inject follow-up into chat state |

Distinguish `PolicyDeny` vs `Reject` vs `Cancelled` in callers; substring-matching error text is fragile.

## Rule sources and merge

Evaluation is **order-independent severity**: **deny > ask > allow**. Merge order only affects provenance (`open-grok inspect`, skipped-rule lists).

### Sources (conceptual)

| Source | Location |
| --- | --- |
| System requirements | `/etc/opengrok/requirements.toml` (`is_system`) |
| User requirements | `$OPENGROK_HOME/requirements.toml` |
| Managed settings / managed config | managed-settings JSON + managed_config layers |
| Native global config | `~/.opengrok/config.toml` `[permission]` |
| Native project config | every `.opengrok/config.toml` from git root → cwd |
| Claude settings | `~/.claude/settings*.json`, project `.claude/settings*.json` |
| CLI | `--allow` / `--deny` (session start) |
| Session working dirs | runtime-appended per session via `/add-dir` (see below) |

Compact TOML form (`deny = ["Read(...)"]`) and verbose `[[permission.rules]]` are both accepted (`resolution.rs` / `rules.rs`).

### Session working directories (`/add-dir`)

`x.ai/session/add_working_directory` / `remove_working_directory` ext methods append or remove **runtime-only** allow rules (Read + Edit, absolute `<dir>/**` globs) through `PermissionCommand::AddSessionRules` / `ResetSessionRules`. `CompiledPolicy` tracks the appended count so a reset truncates exactly the runtime slice — configured layers are never touched. The set persists in the session dir (`working_dirs.json`), is restored (without re-disclosure) on resume, and is disclosed to the model as an `<environment-update source="working_set">` system reminder. Subagents inherit the widened scope via the shared `PermissionHandle`.

### Rule string DSL (`rules.rs`)

Prefixes: `Bash(...)`, `Read(...)` / `NotebookRead(...)`, `Edit(...)` / `Write(...)` / `NotebookEdit(...)`, `MCPTool(...)`, `Grep(...)` / `Glob(...)`, `WebFetch(...)`, `WebSearch(...)`, bare tool name (`Bash` = all bash), bare `*` / `Any`.

- Path globs: `*` / `?` do not cross `/`; `**` does.
- Bash: trailing `:*` is a prefix idiom (`Bash(git commit:*)` → prefix `git commit`).
- `WebFetch(domain:example.com)` sets `PatternMode::Domain`.
- Unsupported prefixes (e.g. `EnterWorktree`) are skipped with warn, not fatal.

### Bash allow vs deny/ask matching

| Action | Matched against |
| --- | --- |
| `deny` / `ask` | Every chained segment (+ wrappers peeled, `bash -c` nested), **and** whole string for policy evaluate |
| `allow` | **Whole command string only** |

So `allow Bash(git *)` can auto-approve `git status && rm -rf /` if nothing denies. Pair narrow allows with denies for high-risk patterns.

### `defaultMode` effects (`rules.rs`)

| Mode | Effect |
| --- | --- |
| `default` / `plan` | Normal prompt policy (`plan` is **compat only** — real plan mode is shell plan tracker) |
| `acceptEdits` | Synthetic `Allow Edit` rule |
| `bypassPermissions` | Synthetic catch-all `Allow Any` (unless YOLO pin blocks it) |
| `dontAsk` | `PromptPolicy::Deny` |
| `auto` | `PromptPolicy::Auto` (seeds manager auto flag) |

Unknown mode strings fail safe to `default` and record a skip for inspect.

### Managed pin / YOLO clamp

Enterprise can lock **always-approve** off via requirements:

```toml
[ui]
disable_bypass_permissions_mode = true
# legacy also honored:
# yolo = false
```

When pinned (`yolo_disabled_by_policy()` → `yolo_pin: Some(reason)`):

- Client `set_yolo_mode(true)` is **clamped to false** on the handle Arc **and** re-clamped in the actor (no optimistic true window).
- Persisted `allow_bash_execute` (approve-all bash) is clamped the same way.
- Untrusted **catch-all Allow** rules (Any / freeform Bash/MCP/WebFetch) are dropped; **admin-tier** sources (system requirements, managed settings) keep catch-alls.
- CLI `--allow` catch-alls that substitute for YOLO are dropped at session spawn.

Pin reason constants: `YOLO_PIN_REASON_REQUIREMENTS`, `YOLO_PIN_REASON_LEGACY_YOLO` in `resolution.rs`.

**Not the same as plan mode:** plan edit gate runs first and is independent of this pin.

## Bash command splitting, dangerous list, safe lists

### Splitting (`bash_command_splitting.rs`)

- tree-sitter-bash parse; split on `&&`, `||`, `;`, `|`, newlines for “word-only” sequences.
- Unparseable constructs (subshells, `$(…)`, backticks, bare `&`, control flow, …) → fail closed to prompt / ask when restrictions exist.
- Setup commands skipped for “primary” classification: `cd`, `export`, `sleep`, etc. (`is_setup_command`).
- Wrappers peeled for **deny/ask/grants/safe lists**: `timeout`, `nice`, `ionice`, `chrt`, `stdbuf`, `env` (`unwrap_wrappers`). **`sudo` / `xargs` / `nohup` are not peeled** — rules must name them.
- Nested `bash|sh|dash|zsh|ksh -c '…'` scripts are recursed for policy (depth cap → Ask).

### Segment evaluation (`evaluate_bash_segments`)

Per non-setup segment (after unwrap):

1. Session **disallow** prefix → reject whole script  
2. **Dangerous** → always needs prompt (even if whitelisted)  
3. User grant **or** built-in safe / always-safe → auto-allow segment  
4. Else → needs prompt  

Dangerous commands re-prompt even with remembered grants. Explicit config **allow** rules can still approve them; YOLO approves them unless a deny matched first.

### Dangerous prefixes (`is_dangerous_command_words`)

Word-boundary prefix match: `rm`, `chmod`, `chown`, `chgrp`, `chattr`, `pkill`, `kill`, `killall`, `git push`.

### Safe / always-safe lists

Built-in read-only prefixes (non-exhaustive; see `ALWAYS_SAFE_COMMANDS` / `is_safe_command_words_str`):

- FS: `ls`, `cat`, `pwd`, `date`, `whoami`, `hostname`, `uptime`, `ps`, `head`, `tail`, `wc`, `sort`, `uniq`, `tr`, `cut`
- Git: `git status|branch|log|diff|ls-files|show|rev-parse`
- Search: `grep`, `rg` — **not** `rg --pre` / `rg --pre=…` (`rg_has_pre_flag`)
- Build: `cargo check`
- K8s: `kubectl get|logs|describe`

`tee` is intentionally **not** safe-listed (writes arbitrary files). Matching uses **word boundaries** (CWE-183: `tr` must not match `truncate`, `git` grant must not match `gitleaks` for **whitelist** matching — note Bash **policy allow** patterns are freeform prefix/glob without that boundary unless written carefully).

### Shell file-access gate (`shell_access.rs`)

When any Read/Edit/Any **deny or ask** rule exists, bash is scanned for readers/writers/redirects/path-movers. Escalation only (`Reject` / `Ask`, never auto-Allow). Resolves symlink targets so workspace-relative link dodges fail. Relative paths after `cd` / `env -C` → Ask when unpinnable.

## MCP tool naming for rules

Delimiter constant: `MCP_TOOL_NAME_DELIMITER = "__"` (`xai-grok-workspace-types`).

- Wire / rule name form: **`server__tool`** (no `mcp__` prefix).
- Rule example: `MCPTool(linear__*)` or exact `MCPTool(notion__fetch)`.
- Claude-style `mcp__server__tool` **never matches** Open Grok names.
- Session grants: exact tool in `allowed_mcp_tools`, or server prefix in `allowed_mcp_servers` (everything before first `__`).
- With `remember_tool_approvals`, an existing grant can satisfy a policy `ask` (ask once, then remember); without it, `ask` forces re-prompt.

## Folder trust and project hooks / MCP / LSP gating

Repo-local configs can ship spawnable commands (RCE if auto-started). Trust is a VS Code–style gate **before** project-scoped servers run.

| Piece | Path |
| --- | --- |
| Consume / cache / `project_scope_allowed` | `xai-grok-shell/src/agent/folder_trust.rs` |
| Decide / scan / persist | `xai-grok-workspace/src/folder_trust.rs` |
| Durable store | `~/.opengrok/trusted_folders.toml` via `workspace/trust.rs` |
| CLI | `--trust`; ACP interactive prompt when client advertises folder-trust |

**When untrusted (feature on + repo-local code-exec configs present):**

- Project-scoped **MCP** servers are not loaded/spawned.
- Project **LSP** configs gated similarly.
- Project **hooks** from repo sources are not applied (global hooks still load).

**Notes:**

- Folder trust ≠ plugin trust (`~/.opengrok/trusted-plugins`) — independent stores.
- “No repo configs” allow is **provisional** (not cached) so a later-added `.mcp.json` re-gates.
- `$HOME` / filesystem root keys are unrecordable and treated as trusted by rule (cannot persist deny).
- Feature can be disabled via remote/managed `folder_trust_enabled` / kill-switch; local/dev builds may be inert (`folder_trust_inert`).

## Permission state persistence (per project)

`PermissionState` (`state.rs`) holds session/project remembered choices:

- `edit_policy`, `allow_bash_execute`
- `allowed_bash_commands` / `disallowed_bash_commands` (prefix grants)
- `allowed_web_fetch_domains`
- `allowed_mcp_tools` / `allowed_mcp_servers`

**On disk:** under the sessions cwd directory for the project  
`$OPENGROK_HOME/sessions/<encoded-cwd>/permission.toml`  
(or `permission_<client_id>.toml` when a client identifier is set — per-client file preferred, shared file as fallback).

- Loaded at permission manager spawn; persisted on grant changes.
- Scoped by **project cwd**, not by individual session id.
- Stale files cleaned by `cleanup_stale_permission_state`.
- YOLO does **not** consult remembered grants (short-circuit after policy).
- Dangerous bash segments still prompt despite grants.

## Subagent permission inheritance

| Behavior | Detail |
| --- | --- |
| Permission handle | **Shared** with parent (`inherited_permission_handle`; `owns_permission_manager = false`) |
| Always-approve / YOLO / auto flags | Inherited (same actor + Arcs) |
| Deny-read globs | Inherited via handle for grep excludes |
| Plan tracker | **Fresh Inactive** — parent plan gate does **not** cover children |
| Max depth | 1 (`MAX_SUBAGENT_DEPTH`) |
| Events | Permission events can tag `subagent_session_id` / type / description |

A write-capable subagent can edit files while the parent is in plan mode — intentional isolation. Change only with product design + tests.

## OS sandbox (`xai-grok-sandbox`)

Process-wide kernel enforcement (Landlock on Linux, Seatbelt on macOS) via `nono`. Applied **once at process startup**, not per tool. Covers in-process FS and children; LLM/API network stays open at process level; **child** network may be restricted (Linux seccomp).

### Profiles (`profiles.rs`)

| Profile | FS read | FS write (simplified) | Child net (Linux) |
| --- | --- | --- | --- |
| `off` | unrestricted | unrestricted | open |
| `workspace` | everywhere | CWD + `~/.opengrok` + temp | open |
| `devbox` | everywhere | most top-level dirs except `/data` | open |
| `read-only` | everywhere | home state + temp | restricted |
| `strict` | CWD + system paths | CWD + home state + temp | restricted |
| Custom | from `sandbox.toml` | extends built-in | config |

Config paths: `~/.opengrok/sandbox.toml` (global) + project `.opengrok/sandbox.toml` (**additive** only — project cannot redefine a global custom profile name; conflicts warn and ignore project).

Custom `deny` lists are kernel-enforced (read + write/rename); globs supported with platform caveats (macOS runtime Seatbelt regex; Linux expands at launch). Built-in names cannot be overridden by custom profiles (`devbox` always built-in).

### Process-wide pin and resume invariant

- `SandboxManager::apply` is **irreversible** for the process lifetime; `install()` stores global state (`OnceLock`).
- Profile for a **session** is persisted on the session summary (`sandbox_profile`).
- On **resume**: saved profile is restored; a **different** `--sandbox` is **refused** (cannot widen or silently tighten mid-life of a session). Matching profile or omit flag is OK.
- New session resolution: CLI / env → config → `off`.
- Helpers: `is_active()`, `profile_name()`, `should_restrict_child_network()`, `should_auto_allow_bash()` (optional bash auto-approve when sandbox active + configured), violation log `~/.opengrok/sandbox-events.jsonl`.
- `enforce` feature (default on Unix): without it, helpers still compile but enforcement is a no-op.
- Apply failure usually degrades with warning; **custom profiles** that cannot enforce deny (e.g. missing bwrap on Linux) **refuse to start** rather than run open.

Sandbox is orthogonal to permission rules: permissions can still deny/ask; sandbox blocks FS/net the kernel can see even if YOLO allowed the tool.

## Key paths

### `xai-grok-workspace/src/permission/`

| File | Role |
| --- | --- |
| `mod.rs` | Public exports |
| `types.rs` | `AccessKind`, `Decision`, `PermissionRule`, `PromptPolicy`, events |
| `manager.rs` | Actor, YOLO/auto, bash segments, dangerous/safe, MCP grants |
| `policy.rs` | `CompiledPolicy`, deny>ask>allow, bash segment policy, shell file access entry |
| `rules.rs` | Rule string parse, `defaultMode` |
| `resolution.rs` | Multi-source merge, YOLO pin, MCP allowlists, inspect metadata |
| `claude_settings.rs` | Claude settings load |
| `bash_command_splitting.rs` | tree-sitter split, wrappers, highlights |
| `shell_access.rs` | Bash path escalation vs Read/Edit rules |
| `state.rs` | Persist/load grants under sessions cwd dir |
| `prompter.rs` | ACP permission UI options, MCP display names |
| `auto_mode.rs` | Classifier / fast paths |
| `hub_permission.rs` | Hub / tool-server permission transport |
| `shell_access.rs` | (see above) |

### `xai-grok-sandbox/`

| Path | Role |
| --- | --- |
| `src/lib.rs` | `SandboxManager`, global state, apply/install, metrics |
| `src/profiles.rs` | Built-in + custom profiles, config merge |
| `src/paths.rs` | Essential writable paths, home resolution |
| `src/deny/` | Deny globs / effective deny paths |
| `src/child_net.rs` | Child network restriction helpers |
| `src/logging.rs` | Event log + metrics |
| `src/types.rs` | Events / metrics types |
| `tests/` | Integration + deny path e2e |
| `examples/sandbox_smoke_test.rs` | Manual smoke |

### Shell / startup glue

| Path | Role |
| --- | --- |
| `shell/.../tool_calls.rs` | Pipeline order, plan gate |
| `shell/.../spawn.rs` | Spawn manager vs inherit handle, resolve config, yolo pin |
| `shell/src/config/mod.rs` | Sandbox apply at startup, resume profile |
| `shell/src/session/persistence.rs` | `sandbox_profile` on summary + resume lookup |
| `shell/src/agent/folder_trust.rs` | Project scope gate |

## Tests and focused commands

| Area | Where |
| --- | --- |
| Plan gate | shell `plan_mode_edit_gate_tests`, `acp_session_tests/plan_mode_*` |
| Manager / bash / YOLO pin | `xai-grok-workspace/src/permission/manager.rs` (large `#[cfg(test)]`) |
| Policy / segments | `permission/policy.rs` tests |
| Resolution / merge / pin | `permission/resolution.rs` tests |
| State roundtrip | `permission/state.rs` tests |
| Auto classifier | `permission/auto_mode.rs` tests |
| MCP persistence | shell `tests/test_mcp_permission_persistence.rs` |
| Sandbox | `xai-grok-sandbox/tests/`, profile unit tests in crate |
| Resume profile | shell `persistence.rs` `resumed_sandbox_profile_tests` |
| Folder trust | workspace `folder_trust` + shell consume tests |

```sh
cargo test --locked -p xai-grok-workspace -- permission
cargo test --locked -p xai-grok-shell -- plan_mode
cargo test --locked -p xai-grok-shell -- mcp_permission
cargo test --locked -p xai-grok-sandbox
```

Use an isolated `OPENGROK_HOME` so tests do not pollute real grants / trusted folders.

## Gotchas

| Pitfall | Result |
| --- | --- |
| Teaching only permission manager about plan mode | Plan gate bypass or double-gating bugs |
| Treating hooks allow as full authorization | Policy deny / YOLO still apply after allow |
| Relying on hooks alone | Fail-open; not a security boundary |
| `allow Bash(git *)` without denies | Whole-string allow can cover `git … && rm` |
| Safe-list primary only (pre-split era thinking) | Chains are evaluated **per segment** |
| Whitelisting `rm` | Dangerous list still forces prompt for grants; config allow can still pass |
| `rg --pre` on safe list | Explicitly excluded — never auto-safe |
| MCP rules with `mcp__` prefix | Never match; use `server__tool` |
| Inferring MCP server from substring without `__` | Grants use exact split on first `__` |
| Subagent inherits plan Active | It does **not** — fresh Inactive tracker |
| Changing sandbox on resume | Process refuses mismatched profile |
| Assuming sandbox can be relaxed mid-session | Irreversible process pin |
| Project `sandbox.toml` redefining user custom profile | Ignored; warning at startup |
| Folder trust vs plugin trust | Separate stores; do not unify casually |
| YOLO + remembered deny | YOLO skips grant consultation (policy deny still wins) |
| Headless prompt | Cancels rather than blocks; use `dontAsk` + allow rules for CI |
| Catch-all `--allow` under YOLO pin | Dropped as YOLO substitute |
| `defaultMode: plan` | Does **not** enable plan-mode edit gate |

## Checklist when changing permissions or sandbox

- [ ] Preserve pipeline order (plan → hooks → plan auto-approve → handle)
- [ ] Policy deny still before YOLO; `PolicyDeny` still non-cancelling for the turn
- [ ] Bash: segment deny/ask vs whole-string allow semantics documented/tested
- [ ] Dangerous / `rg --pre` invariants intact
- [ ] MCP names remain `server__tool`
- [ ] Subagent still shares handle + fresh plan tracker
- [ ] YOLO pin clamps handle + actor + catch-all allows
- [ ] Sandbox resume refuses profile mismatch; apply still once at startup
- [ ] Folder-trust gates still fail closed for project MCP/hooks/LSP
- [ ] Tests under `OPENGROK_HOME` isolation
- [ ] User-facing behavior → update user-guide `22` / `18` if needed; agent contract → this file

## See also

- [agent-runtime.md](agent-runtime.md) — turn loop, tools, subagents
- [editing.md](editing.md) — edit tools, plan gate, hunks
- [tui-and-config.md](tui-and-config.md) — settings, hooks UI
- User guide: `22-permissions-and-safety.md`, `18-sandbox.md`, `10-hooks.md`, `19-plan-mode.md`
