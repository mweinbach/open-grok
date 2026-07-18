# Tool system

Developer map of tool packs, registry/finalize, taxonomy, major implementations, output caps, Computer Hub backends, and how to add a tool.

Paths are relative to the repo root under `crates/codegen/` unless noted. Platform traits live under `crates/common/`.

Related: [agent-runtime.md](agent-runtime.md) (turn prepare/dispatch), [editing.md](editing.md) (mutation tools + hunks), [tui-and-config.md](tui-and-config.md) (MCP / skills UX), [providers.md](providers.md) (provider tool modes).

End-user docs: `xai-grok-pager/docs/user-guide/` (permissions, MCP, skills, agent mode).

## Architecture snapshot

```text
Model tool_call
  → shell prepare_tool_call   (plan gate → hooks → permissions)
  → WorkspaceOps::call_tool
  → ToolBridge / FinalizedToolset
  → xai_computer_hub_sdk LocalRegistry  (in-process dispatch)
  → xai_tool_runtime::Tool::execute/run
       stream: Progress* → Terminal(Result)
  → ToolOutput + prompt_text (+ reminders)
  → ACP session/update + chat_state tool_result
```

| Layer | Path | Role |
| --- | --- | --- |
| Product tools | `xai-grok-tools/` | Packs, registry, computer backends, implementations |
| Runtime trait | `crates/common/xai-tool-runtime/` | `Tool`, streams, errors, dispatch helpers |
| Wire / protocol | `crates/common/xai-tool-protocol/` | IDs, capabilities, frames |
| Shared types | `crates/common/xai-tool-types/` | Descriptions, task types |
| Computer Hub | `crates/common/xai-computer-hub-*` | Local/remote tool routing, MCP adapter, SDK |
| Session bridge | `xai-grok-tools/src/bridge.rs` | `ToolBridge` session-facing API |
| Agent presets | `xai-grok-agent/src/config.rs` | Named toolset presets (`grok-build`, `codex`, …) |
| Shell config | `xai-grok-shell/src/tools/` | Bash / web_fetch / hashline shell-side config |
| Workspace resolve | `xai-grok-workspace/src/session/tool_config.rs` | Merge MCP/hub + capability filter + finalize |
| Permission kinds | `xai-grok-workspace/src/permission/` | `AccessKind` for bash/edit/MCP/… |

## Registry, finalize, bridge

### `ToolRegistryBuilder`

**Path:** `xai-grok-tools/src/registry/types.rs`

- `ToolRegistryBuilder::new()` registers **every** built-in pack tool (and any process-global external packs via `register_tool_pack`).
- Registration key is fully qualified: `"GrokBuild:read_file"`, `"Codex:apply_patch"`, `"OpenCode:edit"`, …
- `register::<T>()` / `register_with_params::<T, P>()` require `T: xai_tool_runtime::Tool + ToolMetadata + Default`.
- Out-of-tree packs: call `register_tool_pack` **before** the first `ToolRegistryBuilder::new()` in the process.

### `ToolServerConfig` / `ToolConfig`

What a session **enables**, not what is compiled in:

| Field | Meaning |
| --- | --- |
| `tools: Vec<ToolConfig>` | Enabled tools by fully-qualified `id` |
| `name_override` | Client-facing / model-facing name |
| `params` / `params_name_overrides` | Config params + schema renames |
| `description_override` | Replace description template |
| `behavior_version` / `behavior_preset` | Version-managed schema/behavior (see `versions.rs`) |
| `kind` | Auto-filled for built-ins; used by capability-mode filters |

`ToolConfig::for_tool::<T>()` builds a config from the type (namespace + id + kind).

### `SessionContext`

Public finalize inputs (concrete types only — callers never touch `Resources` directly):

- `backend` (`TerminalBackend`), `fs` (`AsyncFileSystem`)
- `cwd`, `session_folder`, `session_env`, `owner_session_id`
- `notification_handle`, `skills`, `state_path`
- Optional: `memory_backend`, `web_search_config`, `web_fetch_config`, `lsp`, `image_gen_config`, `video_gen_config`, `api_key_provider`, `auth_provider`, …

### `FinalizedToolset`

Produced by `ToolRegistryBuilder::finalize(config, ctx)`:

1. Validate enabled IDs and requirements
2. Merge defaults + client params; build client-facing `ToolDefinition`s
3. Populate `LocalRegistry` for in-process dispatch
4. Inject shared `Resources` (terminal, FS, renderer, scheduler, skills, …)
5. Install cross-cutting reminders (LSP diagnostics, task completion, skill discovery)

Dispatch (`call` / `call_raw`):

- Look up by client-facing name
- Reverse-remap params → canonical schema
- Run through hub local registry → `Tool::execute`
- Convert typed output → `ToolOutput`
- Append system reminders → `prompt_text`
- Persist `Resources` state when configured

Tools list is `RwLock`-guarded so MCP tools can register/unregister without blocking every lookup across `.await`.

### `ToolBridge`

**Path:** `xai-grok-tools/src/bridge.rs`

Session-facing wrapper over `Arc<FinalizedToolset>`:

- `finalize_builder` → owns registry + separate `terminal` handle for cancel (kill without holding registry lock)
- `tool_definitions` / `tool_definitions_builtins_only`
- `tool_for_kind` / `tool_kind` (kind ↔ client name)
- `register_mcp_tools` / `unregister_tools_by_prefix`
- `call_new_tool` → `ToolBridgeResult { output, prompt_text }`

### `xai_tool_runtime::Tool`

**Path:** `crates/common/xai-tool-runtime/src/tool.rs`

```text
trait Tool {
  type Args;    // Deserialize + JsonSchema
  type Output;  // Serialize + ToolOutput
  fn id(&self) -> ToolId;
  fn description(&self, ctx) -> ToolDescription;
  fn execute(...) -> ToolStream<Output>;  // canonical entry
  fn run(...) -> Result<Output, ToolError>; // convenience; default stream wraps run
}
```

**Stream invariant:** zero or more `Progress`, then **exactly one** `Terminal(Result<…>)`. Helpers: `terminal_only`, `with_progress`.

Product tools also implement **`ToolMetadata`** (`xai-grok-tools/src/types/tool_metadata.rs`):

| Method | Role |
| --- | --- |
| `kind()` | `ToolKind` (read/edit/execute/…) |
| `tool_namespace()` | `ToolNamespace` pack identity |
| `description_template()` | MiniJinja template (`${{ tools.by_kind.* }}`) |
| `is_read_only()` | Default from kind; override when needed |
| `requires_expr()` | Finalize-time requirements |
| `versioned_definition()` | Params-aware schema/description overrides |

## Taxonomy

**Path:** `xai-grok-tools/src/tool_taxonomy.rs`, kinds/namespaces in `types/tool.rs`.

### Namespaces (`ToolNamespace`)

| Wire (serde) | Display id prefix | Pack dir |
| --- | --- | --- |
| `grok_build` | `GrokBuild:` | `implementations/grok_build/` |
| `grok_build_concise` | `GrokBuildConcise:` | `implementations/grok_build_concise/` |
| `grok_build_hashline` | `GrokBuildHashline:` | `implementations/grok_build_hashline/` |
| `codex` | `Codex:` | `implementations/codex/` |
| `opencode` | `OpenCode:` | `implementations/opencode/` |
| `mcp` | MCP / dynamic | Registered at runtime |

Closed enum — new toolsets require a namespace update (strict typed `_meta` consumers fail closed).

### `ToolKind` + canonical `_meta`

ACP / consumers attach identity under `x.ai/tool` (`TOOL_META_KEY`):

```json
{
  "version": 1,
  "name": "read_file",
  "kind": "read",
  "namespace": "grok_build",
  "label": "Read",
  "read_only": true,
  "input": { "path": "…" }
}
```

- **`label`** — cross-harness display/grouping (`presentation_name()`)
- **`kind`** — finer discriminator; prefer `label` to join across packs
- **`input`** — canonical projection only (`tool_taxonomy::field::*`); bulky payloads stay in `raw_input`
- Schema: `xai-grok-tools/schema/tool_meta.schema.json`

Normalization (harness keys → canonical): `xai-grok-tools/src/normalization.rs`.

## How tools are selected

Registration ≠ exposure. Selection pipeline:

```text
Agent / preset ToolServerConfig          (which built-ins)
  → optional FileToolset hashline swap   (shell toolset config)
  → merge MCP tools                      (workspace tool_config)
  → merge hub tools
  → capability_mode filter               (drop kinds not allowed)
  → finalize → FinalizedToolset
  → Code Mode / ToolMode may hide top-level tools
  → per-turn should_list / provider hosted tools
```

### Named presets (`xai-grok-agent/src/config.rs`)

| Preset | Intent |
| --- | --- |
| `grok-build` | Default + plan/ask/web/media/memory/lsp (workspace full set via `workspace_grok_build_toolset`) |
| `grok-build-concise` | Concise bash/read/edit variants + common utilities |
| `grok-build-plan` | Grok build + enter/exit plan + ask user |
| `codex` | Codex read/list/grep + `apply_patch` + shared orchestration tools |
| `explore` | Read-only: `read_file`, `list_dir`, `grep` (no bash) |
| `plan` | Read-only + todo (no shell/edit) |
| `grok-computer` | Sandbox-oriented shell/FS subset |
| OpenCode / orchestrator / ask-user | Additional builders in the same module |

Hashline is not a static preset name alone: shell `FileToolset::Hashline` + `grok_build_hashline_toolset(hashline_tools)` swaps file tools for `hashline_read` / `hashline_edit` / `hashline_grep` with scheme params.

### Capability mode

Workspace `capability_mode` filters by `ToolKind`. Tools with `kind: None` (typical MCP/custom) are preserved on baseline but **dropped** under restrictive modes when they are MCP/hub-origin. Known built-in IDs get kinds backfilled before filter.

### Code Mode

When Code Mode is effective (a Codex Code Mode Only model requirement wins over Settings — see [agent-runtime.md](agent-runtime.md) and `docs/code-mode-port.md`):

| Surface | Tools |
| --- | --- |
| Mixed | Provider-compatible `exec`, `wait`, and ordinary top-level tools |
| Only | Provider-compatible `exec`, `wait`, plus direct-only (human interaction / multi-agent) |
| Nested | Ordinary tools via JS `tools.*` in either Code Mode variant — still full prepare (plan + hooks + permissions) |
| UI | Transport tools hidden; nested `tools.*` are the cards users see |

Nested results must not append as ordinary model tool history (`ModelToolResultSink::CodeMode`).

### Provider / toolset

Provider profile and agent definition choose tool packs (e.g. Codex sessions use Codex read/`apply_patch`). Do **not** infer pack from model slug. Hosted web search may be provider-side (sampler) in addition to the client `web_search` tool. The client tool can use either the legacy Responses-backed helper or the opt-in Perplexity raw Search API backend; the latter is exposed only when the active provider profile lacks native web search.

## Tool packs

All packs are registered in `ToolRegistryBuilder::new()`; presets select which IDs finalize into a session.

### `grok_build` (default)

**Path:** `xai-grok-tools/src/implementations/grok_build/`

Primary Open Grok toolset: bash, FS, search, plan, task, scheduler, monitor, web, media, LSP, goals, etc.

### `grok_build_concise`

**Path:** `…/grok_build_concise/`

Concise output variants of bash / read_file / search_replace (shared helpers with grok_build). System reminders (e.g. skill discovery) may be suppressed under concise mode.

### `grok_build_hashline`

**Path:** `…/grok_build_hashline/`

Anchor-based `hashline_read` / `hashline_edit` / `hashline_grep`. Scheme config: shell `[toolset.hashline]` (`chunk` / `content_only`, `hash_len`, `chunk_size`). See [editing.md](editing.md).

### `codex`

**Path:** `…/codex/`

| Tool | Role |
| --- | --- |
| `apply_patch` | Freeform multi-file patch; in plan mode, only plan-file updates may pass |
| Codex `read_file` / `list_dir` / `grep_files` | Codex-shaped FS tools |

Shared grok_build tools (bash, todo, task, scheduler, plan enter/exit, ask user, …) are mixed into the `codex` preset.

### `opencode`

**Path:** `…/opencode/`

Compatibility shapes: `bash`, `read`, `edit`, `write`, `grep`, `glob`, `todowrite`, `skill`. Prefer not to drift semantics from primary Grok tools without an explicit compat goal.

## Major tool groups

Paths under `xai-grok-tools/src/implementations/` unless noted.

| Group | Client names (typical) | Path | Notes |
| --- | --- | --- | --- |
| Shell / terminal | `bash` / `run_terminal_cmd` | `grok_build/bash/` | Background tasks; shadows find/grep via local terminal |
| Read | `read_file` | `grok_build/read_file/`, shared `read_file/` (image/pdf/pptx) | Multi-format; cursor-rules-on-read reminders |
| Edit | `search_replace` | `grok_build/search_replace/` | **Details: [editing.md](editing.md)** |
| Patch (Codex) | `apply_patch` | `codex/apply_patch/` | Freeform; plan mode permits only exact plan-file add/update hunks |
| List / search FS | `list_dir`, `grep` | `grok_build/list_dir/`, `grok_build/grep/` | Versioned list_dir schemas |
| Web | `web_search`, `web_fetch` | `web_search/`, `grok_build/web_fetch/` | Search supports Responses and Perplexity raw-result backends; fetch applies SSRF checks |

### Perplexity web-search fallback

- Public schema remains `web_search(query, allowed_domains?)`.
- Enable with `[toolset.perplexity_web_search].enabled = true` or **Settings → Models → Perplexity web search**.
- Save the key through **Perplexity API key**. It is stored only in owner-protected `auth.json` under `perplexity::api_key`; it never enters `config.toml`, session persistence, provider credentials, or diagnostics.
- The backend calls Perplexity `POST /search`, forwards `allowed_domains` as `search_domain_filter`, requests ten ranked results, and returns title, URL, date, snippet, and deduplicated URL citations for the active model to synthesize.
- Enabling without a key is valid configuration but leaves the tool unavailable and displays **API key required**.
- `--disable-web-search` remains the top-level kill switch for both native declarations and this fallback.
- Settings changes apply live. The pager pauses new Kimi sends and queue draining until persistence and every resident-session rebuild are confirmed; failures restore durable state and reconcile the runtime before releasing the queue.
| Subagents | `task`, wait/kill helpers | `grok_build/task/`, `task_output/`, `kill_task/` | `MAX_SUBAGENT_DEPTH = 1` |
| Background I/O | `get_task_output` / wait / kill | `task_output/`, `kill_task/` | Terminal + subagent tasks |
| Monitor | `monitor` | `grok_build/monitor/` | Line → notifications; rate limited |
| Scheduler / loop | `scheduler_*` | `grok_build/scheduler/` | Parent handle shared with subagents when set |
| Plan | `enter_plan_mode`, `exit_plan_mode` | `grok_build/enter_plan_mode/`, `exit_plan_mode/` | Gate in shell, not only permissions |
| Ask user | `ask_user_question` | `grok_build/ask_user_question/` | Reverse RPC to UI; timeout config |
| Goals | `update_goal` | `grok_build/update_goal/` | Goal tracker in shell session |
| Todo | `todo` / OpenCode `todowrite` | `grok_build/todo/`, `opencode/todowrite/` | Persisted plan state |
| Image / video | `image_gen`, `image_edit`, image/ref → video | `grok_build/image_*`, `video_gen/` | Imagine APIs; config + API key provider |
| LSP | `lsp` | `grok_build/lsp/` + `implementations/lsp/` | Shared handle from shell |
| Memory | `memory_search`, `memory_get` | `memory/` | Needs `memory_backend` in session context |
| Skills | skill invoke + discovery | `skills/`, `opencode/skill/` | Discovery reminder; types in `skills/types.rs` |
| MCP meta | `search_tool`, `use_tool` | `search_tool/`, `use_tool/` | BM25 discover + dispatch; stable top-level set |
| Deploy | `deploy_app` | `grok_build/deploy_app_stub.rs` | Service-gated stub |

### MCP tools (`search_tool` / `use_tool`)

- MCP tools are registered dynamically with qualified names **`server__tool`** (double underscore). **No `mcp__` prefix** for permission rules.
- `search_tool` indexes MCP tools (BM25) so the model-facing tool list stays stable across turns (KV-cache friendly).
- `use_tool` dispatches to a discovered tool via `InnerDispatch` → `call_raw` (avoids bridge deadlock; post-processing once on outer call).
- Workspace merge: `xai-grok-workspace/src/mcp.rs`, `session/tool_config.rs`. Name collisions with built-ins are skipped.
- MCP result size: `MCP_MAX_OUTPUT_BYTES` / env overrides in `lib.rs` + `util/mcp_truncate.rs`.

### Skills

Discovery and skill metadata: `implementations/skills/`. OpenCode exposes a skill tool; Grok Build may surface skills via system reminders, slash, and agent preload. Canonical message envelope: `skills/skill.rs` (`build_skill_message`).

## Output caps and streaming

| Constant | Value | Role |
| --- | --- | --- |
| `DEFAULT_TOOL_OUTPUT_BYTES` | 40_000 | Default tool result budget (~10k tokens) |
| `DEFAULT_TOOL_OUTPUT_CHARS` | 20_000 | Bash/terminal char budget |
| `MCP_MAX_OUTPUT_BYTES` | (see `util/mcp_truncate`) | MCP inline result cap |

Truncation is intentional — do not “fix” by removing caps without product review. Helpers: `util/truncate.rs`, `util/mcp_truncate.rs`.

### Terminal stream invariant

Every tool call stream must end with **exactly one** `Terminal` item. Missing terminal → `stream_no_terminal` error from the registry. Prefer `run()` for simple tools; use `execute()` only when you need `Progress` (bash stdout chunks, long-running media, …).

Bash streams output via terminal backend notifications (`BashOutputChunk` ~100ms). Full output is also written to an on-disk `output_file` so background tasks remain recoverable after in-memory truncation.

## Computer hub and local backends

### In-process computer

**Path:** `xai-grok-tools/src/computer/`

| Piece | Role |
| --- | --- |
| `types::TerminalBackend` | Run/kill/list shell tasks |
| `types::AsyncFileSystem` | Read/write/delete for tools |
| `local/terminal.rs` | `LocalTerminalBackend` |
| `local/file_system.rs` | `LocalFs` |
| `local/mock_fs.rs` | Tests |
| `local/shell_state.rs` | Persistent shell state (unix) |
| `local/embedded_search_tools.rs` | Optional find→bfs / grep→ugrep shadows |
| `local/cgroup.rs` | Memory cgroup limits (where available) |

`SessionContext` injects `backend` + `fs` into `Resources` as `Terminal` / FS handles. Subagents may share a parent terminal backend (and thus search-shadow config).

### Computer Hub (common)

| Crate | Role |
| --- | --- |
| `xai-computer-hub-core` | Local/remote resolver, registry traits |
| `xai-computer-hub-sdk` | `LocalRegistry`, connection, server helpers |
| `xai-computer-hub-mcp-adapter` | MCP bridge into hub |

Local sessions: tools execute in-process through `LocalRegistry` owned by `FinalizedToolset`. Remote/proxy modes can execute tools on a workspace/hub server (`workspace_grok_build_toolset` is the full hub registration set). Prefer extending existing hub patterns rather than inventing a second dispatch path.

## Path locks (parallel tools)

Concurrent tool calls in one model turn:

- Shell serializes same-file work using `lock_path_for_args` in `xai-grok-shell/.../tool_dispatch.rs`
- Path keys (priority): `file_path`, `path`, `target_file`
- Directory-only keys (`target_directory`) are **not** locked
- Additional FS lock manager: `implementations/editor_infra/file_operation_lock.rs` (per-path + exclusive)

## How to add a new tool (checklist)

1. **Choose pack / namespace** — almost always `GrokBuild` under `implementations/grok_build/<name>/`.
2. **Implement** `xai_tool_runtime::Tool` + `ToolMetadata` (`kind`, `namespace`, `description_template`). Prefer typed `Args`/`Output` + `run()`.
3. **Map output** — `Into<ToolOutput>` for ACP/UI; implement `ToolOutput` for model-facing blocks if needed.
4. **Map input** — `Into<ToolInput>` if prepare/UI need structured classification (edit vs read).
5. **Register** in `ToolRegistryBuilder::new()` (`registry/types.rs`).
6. **Enable** in the right agent preset(s) in `xai-grok-agent/src/config.rs` (`default_grok_build_toolset` / `workspace_grok_build_toolset` / specialized presets).
7. **Permissions** — ensure `AccessKind` / permission rules cover the new kind if it mutates or egresses; add tests under `xai-grok-workspace/src/permission/`.
8. **Plan mode** — if edit-like, confirm `plan_mode_edit_gate` classification; never special-case plan only inside the tool.
9. **Hunks** — file writes must go through `record_agent_write` (or the shared write path that does). See [editing.md](editing.md).
10. **Caps** — respect output size limits; stream with a single `Terminal`.
11. **Taxonomy** — if you add a new `ToolKind`, update `presentation_name`, `is_read_only`, schema tests, and capability tables.
12. **Code Mode** — nested `tools.*` reuses the same tool; no separate implementation. Mark transport-only tools correctly if any.
13. **UI** — if the pager needs a custom card, extend scrollback blocks + ACP meta; otherwise generic tool card is fine.
14. **Tests** — unit tests next to the tool; registry/finalize if wiring is non-trivial; shell prepare tests if permissions/plan interact.
15. **Docs** — update this file + [agent-runtime.md](agent-runtime.md) if the turn contract changes.

Optional: config-gated tools follow the web/image pattern — `SessionContext` flag + graceful error or omit from preset when disabled.

External packs without editing `xai-grok-tools`: `register_tool_pack` at process startup (before first builder).

## Tests locations

| Area | Where |
| --- | --- |
| Individual tools | `implementations/**` module `#[cfg(test)]` (search_replace, use_tool, hashline, opencode, …) |
| Registry / finalize / dispatch | `registry/types.rs` tests, bridge tests |
| Taxonomy / schema | `tool_taxonomy.rs` (`tool_meta_schema_is_up_to_date`) |
| Versions | `versions.rs` |
| Truncation / MCP caps | `util/truncate.rs`, `util/mcp_truncate.rs`, `tests/web_citation_counter.rs` |
| Computer / cgroup | `tests/cgroup_memory_test.rs`, computer local tests |
| Path suggestions | `tests/path_suggestions_production.rs` |
| Runtime trait | `crates/common/xai-tool-runtime/tests/` |
| Protocol | `crates/common/xai-tool-protocol/tests/` |
| Hub | `crates/common/xai-computer-hub-core/tests/` |
| Workspace tool resolve / MCP | `xai-grok-workspace/src/session/tool_config.rs`, `mcp.rs` |
| Permissions | `xai-grok-workspace/src/permission/*` |
| Plan gate / tool prepare | `xai-grok-shell/src/session/acp_session_impl/tool_calls.rs`, plan_mode tests |
| Presets | `xai-grok-agent/src/config.rs` tests |
| Code Mode nested | `xai-grok-code-mode` + shell `code_mode` / nested dispatch |

```sh
cargo test --locked -p xai-grok-tools -- <filter>
cargo test --locked -p xai-tool-runtime -- <filter>
cargo test --locked -p xai-grok-workspace -- permission
cargo test --locked -p xai-grok-shell -- plan_mode
```

## Gotchas

| Pitfall | Result |
| --- | --- |
| MCP rule uses `mcp__server__tool` | Rule never matches — use `server__tool` |
| Enabling a tool only in registry, not preset | Compiled but never finalized for sessions |
| Inferring toolset from model id | Wrong pack when catalogs/providers change |
| Parallel edits without path keys | Same-file races; ensure args expose `file_path` / `path` / `target_file` |
| Stream ends without `Terminal` | Hard dispatch failure |
| Treating Code Mode `exec` as an ordinary tool card | UI/contract break; nested `tools.*` are user-visible |
| Nested Code Mode tools skipping prepare | Plan/hooks/permissions bypass |
| Plan mode only in permission manager | Gate bypass under YOLO; gate is in shell prepare |
| Skipping `record_agent_write` | Hunks marked External |
| Sharing credentials via tool HTTP clients | Use session `api_key_provider` / auth providers; no baked long-lived secrets |
| Mutating root workspace `Cargo.toml` for a new dep | Edit `xai-grok-tools/Cargo.toml` (or the relevant crate) only |
| Registering external pack after first builder | Pack silently missing from that process’s builders |
| `use_tool` calling through outer bridge mutex | Deadlock risk — keep `InnerDispatch` / `call_raw` pattern |
| New `ToolKind` without capability / presentation updates | Restrictive modes drop the tool or UI label is wrong |

## See also

- [agent-runtime.md](agent-runtime.md) — turn loop, prepare order, subagents, plan
- [editing.md](editing.md) — search_replace, apply_patch, hunks, plan-mode edits
- [permissions-and-sandbox.md](permissions-and-sandbox.md) — AccessKind, bash policy, sandbox
- [architecture.md](architecture.md) — crate layering
- [tui-and-config.md](tui-and-config.md) — MCP/skills settings surfaces
- [providers.md](providers.md) — provider-specific tools and isolation
- [development.md](development.md) — build/test commands
- `docs/code-mode-port.md` — Code Mode contract
- `docs/codex-provider-port.md` — Codex tool parity
