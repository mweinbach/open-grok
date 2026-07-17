# Code Mode

Developer map of Open Grok’s Codex-compatible **Code Mode**: tool-mode selection, protocol/runtime crates, shell adapter, turn routing, nested tools, history sinks, UI transport hiding, lifecycle, tests, and common breakages.

**Canonical parity contract (do not replace):** [`../code-mode-port.md`](../code-mode-port.md) — pinned Codex baseline, compatibility bullets, and Settings precedence. This page is the **implementation map** for agents editing the code.

Related: [agent-runtime.md](agent-runtime.md) · [editing.md](editing.md) · [providers.md](providers.md) · [architecture.md](architecture.md)

Paths below are under `crates/codegen/` unless noted.

---

## 1. What Code Mode is

Code Mode lets the model drive the agent through a **persistent in-process JavaScript session** instead of (or in addition to) calling ordinary tools as top-level JSON functions.

| Model-facing surface | Kind | Input |
| --- | --- | --- |
| `exec` | Provider-compatible Responses transport | Codex: raw JavaScript custom/freeform input. xAI: function arguments with required string `source`. Both accept the optional `// @exec: {...}` pragma inside the source. |
| `wait` | Ordinary **function** tool | JSON: `cell_id`, optional `yield_time_ms` / `max_tokens` / `terminate` |
| Nested tools | JS `tools.*` | Same Grok tools, via host delegate |
| Direct-only tools | Top-level functions | Human interaction + multi-agent lifecycle (Code Mode Only) |

Cell lifecycle (protocol terms):

1. `exec` starts a **cell** in the session runtime.
2. The cell may **complete**, **yield** (still running; returns a `cell_id`), or keep producing output via `notify()`.
3. `wait` resumes a yielded cell or **terminates** it.
4. Nested `tools.foo(...)` re-enters the normal tool prepare/dispatch path; results return as JSON to V8, not as top-level model tool history.

---

## 2. Tool modes and who wins

### 2.1 `ToolMode` enum

Defined in `xai-grok-sampling-types` (`types.rs`):

| Variant | Wire | Meaning |
| --- | --- | --- |
| `Direct` | `direct` | Default. Ordinary JSON function tools only. |
| `CodeMode` | `code_mode` | `exec` + `wait` available **and** ordinary tools remain top-level. |
| `CodeModeOnly` | `code_mode_only` | Sol-style: top-level is `exec` / `wait` / direct-only (+ hosted search). Ordinary tools only via `tools.*`. |

### 2.2 Effective mode resolution

```text
Codex catalog requires code_mode_only + Responses ──► CodeModeOnly (model requirement)
Codex catalog requires code_mode_only + other API ──► reject before spawn/switch
Non-Responses backend                              ──► Direct
Settings [ui].code_mode enum on Responses          ──► selected mode
Unset Settings preference on Responses             ──► Direct (default)
```

Implementation:

| Piece | Path |
| --- | --- |
| Model metadata lookup | `xai-grok-shell/src/agent/models.rs` → `resolve_model_tool_mode` |
| Settings fallback | `xai-grok-shell/src/agent/config.rs` → `effective_tool_mode` |
| Session spawn | `session/acp_session_impl/spawn.rs` |
| Model switch | `session/acp_session_impl/model_switch.rs` |
| Settings UI | pager `settings/defs.rs` key `code_mode` (SHELL-owned, **restart-required**) |

**Invariants:**

1. **Only an OpenAI Codex Code Mode Only requirement overrides Settings.** A Codex catalog entry with `tool_mode: code_mode_only` (for example GPT-5.6 Sol) cannot be forced off.
2. A Codex requirement on an incompatible backend is rejected during session spawn or model switch, before any prompt is sent.
3. Settings explicitly offers `direct`, mixed `code_mode`, and `code_mode_only`. Mixed Code Mode retains ordinary top-level tools and is the normal xAI choice.
4. A user Code Mode preference on a non-Responses backend fails closed to `Direct`.
5. The setting is process-scoped. Restart Open Grok after changing it; merely starting another session in the already-running process does not reload the shell configuration.
6. Unknown explicit remote `toolMode` values reject that catalog entry instead of silently becoming unspecified.

### 2.3 Code Mode vs Code Mode Only (runtime behavior)

| Behavior | `CodeMode` | `CodeModeOnly` |
| --- | --- | --- |
| Require Responses backend | yes | yes |
| Add provider-compatible `exec` | yes | yes |
| Add function `wait` | yes | yes |
| Top-level ordinary tools | retained | stripped except **direct-only** list |
| Nested `tools.*` registry | yes | yes |
| StructuredOutput function tool (non-native schema path) | allowed | suppressed |

Turn assembly: `session/acp_session_impl/turn.rs` (`process_conversation_turn`).

---

## 3. Crate layout

```text
xai-grok-code-mode-protocol   ← types, tool names, pragma, descriptions, session traits
        ▲
xai-grok-code-mode            ← embedded V8: service, session_runtime, cell_actor, runtime/
        ▲
xai-grok-shell session/{code_mode,tool_surface}.rs ← LocalSet adapter + one effective tool-surface policy
        ▲
acp_session_impl/{turn,tool_calls,run_loop}.rs
        ▲
xai-grok-sampler (provider projection + Responses wire) + pager scrollback (nested cards only)
```

Ported from OpenAI Codex at the pin in [`../code-mode-port.md`](../code-mode-port.md). Open Grok ships the **in-process** V8 provider only — no out-of-process `code-mode-host`.

### 3.1 `xai-grok-code-mode-protocol`

| Module | Role |
| --- | --- |
| `tool_name.rs` | Normalized JS identifiers vs registry keys |
| `description.rs` | `build_exec_tool_description`, `build_wait_tool_description`, `parse_exec_source`, nested-tool filters |
| `runtime.rs` | `ExecuteRequest`, `WaitRequest`, `RuntimeResponse`, nested call types |
| `response.rs` | Content items / image detail |
| `session.rs` | `CodeModeSession`, `CodeModeSessionDelegate`, `CodeModeSessionProvider`, `CellId`, `StartedCell` |

Constants:

- `PUBLIC_TOOL_NAME` = `"exec"`
- `WAIT_TOOL_NAME` = `"wait"`
- Pragma prefix: `// @exec:` (optional first line JSON: `yield_time_ms`, `max_output_tokens`)

### 3.2 `xai-grok-code-mode` (runtime)

| Module | Role |
| --- | --- |
| `service.rs` | `InProcessCodeModeSession` / `InProcessCodeModeSessionProvider` — protocol façade |
| `session_runtime/` | Owns cells, `store()` map, shutdown token, task tracker |
| `cell_actor/` | Per-cell async evaluation, yield/wait/terminate |
| `runtime/` | V8 isolate glue: globals (`tools`, `store`, `notify`, timers), module loader, values |
| `v8_init.rs` | One-time V8 platform init / JIT mode |

**Session persistence inside the isolate:** serializable `store(key, value)` values and running cell handles live for the agent session. Dispose via `CodeModeSession::shutdown` when the shell session ends.

### 3.3 Shell adapter `session/code_mode.rs`

Bridges `Send + Sync` runtime callbacks into the **`!Send` SessionActor LocalSet**:

1. Construct a replaceable `CodeModeRuntimeSlot` once per session (`agent_rebuild` / `spawn`). Its current runtime lazily initializes V8.
2. `start_dispatch_loop(Weak<SessionActor>)` takes the current runtime's unbounded receiver once and `spawn_local`s the dispatcher.
3. Delegate `invoke_tool` / `notify` send `DispatchMessage`s; nested tools may run concurrently; notifications stay FIFO. The V8 delegate retains the runtime weakly so shutdown cannot form an `Arc` cycle.
4. Every dispatcher is generation-bound. Rewind and incompatible provider/mode transitions replace the runtime; stale callbacks and cell IDs fail closed.
5. `wait_for_active_code_mode_turn` polls until a Code Mode turn is active (Codex queues mid-yield callbacks across turn boundaries).
6. `exec` / `wait` parse inputs and call the protocol session; `shutdown` races safely with lazy init.

Also owns:

- Tool list helpers: `create_exec_tool`, `create_wait_tool`, `collect_code_mode_tool_definitions`, `to_code_mode_tool_definition`
- Direct-only / transport predicates
- Hosted-search policy for Code Mode Only (`hosted_tools_for_code_mode`, `nested_tool_definitions_for_provider`)
- Transport meta key: `open-grok/codeModeTransport`

---

## 4. Turn integration

### 4.1 Request shape when Code Mode is effective

`EffectiveToolSurface::build` in `session/tool_surface.rs` is the single source
for turns, compaction, forks, and context/token accounting. In
`process_conversation_turn`:

1. Prepare full tool definitions (including MCP).
2. Fail the turn if backend ≠ **`ApiBackend::Responses`**.
3. If **Code Mode Only**: retain only `is_code_mode_direct_only_tool` names in the function tool list.
4. If any Code Mode mode: push **`wait`** as a function `ToolSpec`.
5. Apply provider hosted-tool filtering for Code Mode.
6. Add **`exec`** with the provider capability from `ProviderProfile.code_mode_transport`: Codex receives a custom/freeform declaration; xAI receives a function schema with required `source`; unsupported providers fail before sampling.

Codex `exec` uses a **Lark freeform grammar**. xAI's envelope is intentionally a
JSON-schema function, but the `source` value is the same raw JavaScript consumed
by the runtime.

Before serialization, `ConversationRequest::project_code_mode_for_provider`
rewrites any persisted Codex-style `exec` declaration/history to xAI function
calls and coalesces notifications plus the terminal result into one ordered
function output. The sampler also rejects unsupported native custom content
before either streaming or non-streaming network I/O.

The same history normalization runs before Chat Completions and Anthropic
Messages conversion, and before both Codex remote-compaction request variants.
That keeps provider switches and compacted histories valid even when earlier
turns used the other provider's `exec` representation. Non-`exec` native custom
content is rejected rather than guessed into a function call.

### 4.2 Routing tool calls after the model responds

```text
for each model tool call:
  if code_mode && (transport-compatible exec | function wait)
       → execute_code_mode_control_call  (V8 path)
  else if code_mode_only && not direct_only
       → also treated as code-mode control path (reject-style / non-ordinary)
  else if custom (unexpected)
       → code-mode control error path
  else
       → direct_tool_calls → execute_tool_calls (ordinary pipeline)
```

`execute_code_mode_control_call` (`turn.rs`):

- Decodes Codex custom input directly or extracts xAI's required JSON `source` field; a mismatched call kind fails closed.
- Records the control result into **chat history** (`custom_tool_output` or ordered function result).
- **Does not** emit ACP tool-call cards for transport tools (`show_transport = false` when `is_code_mode_transport_tool`).

### 4.3 Direct-only tools (Code Mode Only top-level)

From `is_code_mode_direct_only_tool` — must stay model-visible; **excluded** from generated `tools.*`:

| Category | Names |
| --- | --- |
| Human interaction | `ask_user_question`, `request_user_input` |
| Multi-agent / tasks | `task`, `spawn_subagent`, `get_task_output`, `get_command_or_subagent_output`, `wait_tasks`, `wait_commands_or_subagents`, `kill_task`, `kill_command_or_subagent` |

Rationale: ACP questions and collaboration lifecycle cannot safely live inside a JS callback that pauses the model turn. Matches Sol multi-agent-v2 **DirectModelOnly** policy.

### 4.4 Hosted search

- Provider-hosted web search stays **top-level** beside `exec` when backend search is on.
- Nested `tools.web_search` is omitted when hosted web is already advertised.
- Codex Code Mode Only keeps OpenAI web search only (no `x_search` at the provider boundary).

---

## 5. Nested tools (`tools.*`)

### 5.1 Exposure

`collect_code_mode_tool_definitions`:

- Maps each Grok `ToolDefinition` → protocol tool (normalized JS name + original registry `tool_name`).
- Drops direct-only tools and names that fail `is_code_mode_nested_tool`.
- Special case: **`apply_patch`** is nested as **freeform** (raw patch string → dispatcher wraps `{ "patch": input }`).

JS calls `await tools.search_replace(...)` etc. The exec tool description enumerates the namespace.

### 5.2 Full prepare path

`SessionActor::dispatch_code_mode_nested_tool` → `dispatch_code_mode_nested_tool_inner` (`tool_calls.rs`):

1. Reject nested `exec` / `wait` (no re-entrancy of transport tools).
2. Build a synthetic `ToolCallResponse` with a fresh `exec-…` call id.
3. **`prepare_tool_call`** — same as model tools: parse, **plan-mode edit gate**, PreToolUse hooks, permissions, path locks, auth-retry eligibility.
4. Dispatch via workspace / tool bridge (`call_with_auth_retry`).
5. Encode structured result for V8 (`code_mode_result()`); fire PostToolUse when configured.
6. Nested tools **emit ordinary ACP tool cards** (user-visible).

Plan mode, hooks, and permission YOLO therefore apply equally to nested edits — see [editing.md](editing.md) and [agent-runtime.md](agent-runtime.md).

### 5.3 LocalSet channel

V8 threads cannot call SessionActor directly. Flow:

```text
CodeModeSessionDelegate::invoke_tool
  → mpsc DispatchMessage::InvokeTool
  → LocalSet dispatch_loop / spawn_local
  → SessionActor::dispatch_code_mode_nested_tool
  → oneshot result → V8
```

---

## 6. Model history sinks

Nested tools must not look like top-level function rounds to the model.

| Sink | When | Effect |
| --- | --- | --- |
| `ModelToolResultSink::Conversation` | Default | `push_tool_result` into chat state (ordinary tools) |
| `ModelToolResultSink::CodeMode` | Scoped around nested dispatch | **Skips** top-level function-result append (`records_model_tool_results() == false`) |

Task-local enum in `tool_calls.rs`. Control `exec`/`wait` results **are** pushed
(custom/function outputs for the Responses wire). Extra `notify()` text uses
`record_code_mode_notification`; provider projection preserves its order and
coalesces it with the terminal xAI function output.

**Do not** “fix” nested tools by reusing `execute_tool_calls` without the CodeMode sink — that double-counts history and confuses the next sample.

---

## 7. UI transport hiding

Contract: `exec` / `wait` are **transport**, not user tool cards. Users see **nested** tool cards only.

### 7.1 Live turns

`execute_code_mode_control_call` sets `show_transport = !is_code_mode_transport_tool(name)`. For `exec`/`wait`, no `SessionUpdate::ToolCall` / update is sent for the wrapper. Nested dispatch still uses normal tool UI notifications.

### 7.2 Explicit meta (replay / identity)

```text
open-grok/codeModeTransport: true
```

Constant: `CODE_MODE_TRANSPORT_META_KEY` in `session/code_mode.rs`.

| Helper | Use |
| --- | --- |
| `is_code_mode_transport_tool(name)` | Reserved names `exec` / `wait` |
| `is_code_mode_transport_meta(meta)` | Explicit marker on a persisted update |

**Never key the UI only on tool name.** Plugins and MCP may define ordinary tools named `exec` or `wait`.

### 7.3 Persistence and replay

- Model conversation still contains control custom/function items (needed for Responses continuity).
- ACP `updates.jsonl` replay **strips** transport wrappers via `session/storage/mod.rs` (`code_mode_transport_call_ids`, `line_is_code_mode_transport_update`), keeping nested cards.
- Identification layers: explicit meta → known model call ids from chat/compaction → legacy heuristics (Other-kind freeform `exec` with string `rawInput`; `wait` whose `cell_id` belongs to a recognized exec).
- Session load also collects transport ids from chat history (`mvp_agent` `collect_code_mode_transport_ids` / `persisted_code_mode_transport_ids`) so TUI restore stays clean.

### 7.4 TUI implications

| Do | Don’t |
| --- | --- |
| Render nested `tools.*` like normal tools | Show raw JS / wait args as tool cards |
| Filter by meta + id sets on replay | Hide every tool titled `exec`/`wait` |
| Keep transport items in model history | Drop custom_tool outputs from the sample transcript |

---

## 8. Session lifecycle

| Event | Behavior |
| --- | --- |
| Session spawn | `CodeModeRuntimeSlot::new()`; `start_dispatch_loop` after actor is `Arc` |
| First `exec` | Lazy `InProcessCodeModeSessionProvider::create_session` |
| Later `exec` | Same V8 session; `store()` values retained |
| Rewind | Reject during an active turn; replace the runtime before rewinding chat/files |
| Provider or Direct/Code boundary switch | Reject during an active turn; replace the runtime before mutating route state |
| Cold resume | Restore persisted mode/source/transport; V8 starts fresh and persisted yielded cell IDs fail closed |
| Session end / channel close | `code_mode_runtime.shutdown()` in `run_loop.rs` (multiple exit paths) |
| Shutdown vs init race | `shutting_down` flag; failed init; cancel newly created session |

V8 state is **not** a durable on-disk snapshot of the isolate; durability is agent **chat history** + ACP updates under the session dir. Runtime dispose is mandatory so isolates do not leak across sessions on a long-lived process.

---

## 9. Requirements checklist

When Code Mode (either variant) is effective:

| Requirement | Why |
| --- | --- |
| **Responses API** backend | Code Mode transport contract |
| Provider-compatible **`exec`** | Codex custom/freeform raw JS; xAI function envelope with raw JS in `source` |
| Function **`wait`** with pinned schema | Resume / terminate cells |
| Session-persistent V8 | `store`, multi-cell, yield/wait |
| Nested tools full prepare path | Plan mode, hooks, permissions, hunks |
| No nested top-level history for nested tools | ModelToolResultSink::CodeMode |
| Transport hidden in UI | Live skip + meta/replay filters |
| Dispose on session end | Resource / isolation |
| Direct-only exceptions in Code Mode Only | Human + multi-agent tools |

Settings + model metadata rules: §2 and [`../code-mode-port.md`](../code-mode-port.md).

---

## 10. Parity contract (summary only)

Full text: [`../code-mode-port.md`](../code-mode-port.md).

When **Code Mode Only** is effective, the pin requires:

1. Provider-compatible `exec`, function `wait`, direct-only human/multi-agent tools.
2. Codex uses native raw-JS custom input; xAI uses a function envelope whose `source` contains raw JS.
3. Ordinary tools registered but hidden top-level; reachable via `tools.*`.
4. Yield / wait / terminate cell semantics.
5. Structured tool results across the JS boundary.
6. Persistent JS runtime for the agent session; dispose on end.
7. Direct-only collaboration tools stay top-level, out of `tools.*`.
8. `exec`/`wait` stay in model history but not as TUI tool cards; nested tools show normally.

Deliberate Open Grok notes in the port doc: in-process V8 only; UI mirrors Codex split plus stripping transport from legacy replay.

---

## 11. Tests

| Layer | Where | What |
| --- | --- | --- |
| Protocol | `xai-grok-code-mode-protocol` (e.g. `description`, `session_tests`) | Pragma parse, names, descriptions |
| Runtime | `xai-grok-code-mode` `service_tests`, `cell_actor/*_tests`, `session_runtime/tests`, `tests/jit.rs` | Cells, yield/wait, tools, store, shutdown |
| Shell adapter | `session/code_mode.rs` module tests | Transport helpers, exec/wait tool shape, hosted-search policy, direct-only list |
| Tool mode | `agent/config.rs` mixed-fallback/model-first test; `agent/models.rs` resolve tests | Precedence |
| Turn / nested | Shell session tests + `tool_calls` behavior | Sink, prepare path |
| Replay | `session/storage/mod.rs` `replay_hides_code_mode_transport_*` | Nested cards kept; wrappers dropped |
| Settings | pager settings registry / modal tests for `code_mode` | Three enum choices, legacy bool migration, full-restart messaging |
| Sampling types / sampler | Projection unit tests + captured Responses request tests | Codex native wire retained; xAI contains no custom type; invalid custom history fails pre-network |

```sh
cargo test --locked -p xai-grok-code-mode-protocol
cargo test --locked -p xai-grok-code-mode
cargo test --locked -p xai-grok-shell -- code_mode
cargo test --locked -p xai-grok-sampling-types -- tool_mode
```

Also exercise plan-mode nested edits and permission deny paths when changing the nested prepare pipeline ([editing.md](editing.md)).

---

## 12. Common breakages

| Pitfall | Symptom |
| --- | --- |
| Send xAI the Codex native custom declaration | `cli-chat-proxy.grok.com` rejects the request |
| Send Codex a JSON-schema `exec` function | Sol model contract breaks |
| New isolate / process per `exec` | Lost `store()`; broken multi-step scripts |
| Nested `tools.exec` / re-entrant wait | Rejected or deadlocks; protocol forbids |
| Nested tools append top-level function results | Duplicate history; model confuses sinks |
| Skip plan gate / hooks / permissions on nested path | Security / plan-mode bypass |
| Hide UI by tool **name** only | Legitimate MCP `exec`/`wait` disappear |
| Show transport cards live or on replay | Noise; violates parity |
| Expect active Code Mode on Chat Completions / Messages | User preference falls back to Direct; an incompatible Codex hard requirement is rejected before the turn |
| Settings overrides Codex-required `code_mode_only` | Breaks Sol and precedence tests |
| Forget `shutdown` on session end | V8 / task leaks |
| Strong delegate ownership or no reset generation | Leaked isolates or stale callbacks attached to a replacement timeline |
| Hosted web duplicated top-level + nested | Redundant tools; policy mismatch |
| Treat Code Mode Only as “all tools gone” | Dropping direct-only human/task tools |
| Rely on `fs_notify` alone for nested edits | Hunks marked External ([editing.md](editing.md)) |

---

## 13. Where to edit

| Change | Start in |
| --- | --- |
| Pragma / tool descriptions / nested name rules | `xai-grok-code-mode-protocol` |
| V8 cell / store / yield semantics | `xai-grok-code-mode` (`session_runtime`, `cell_actor`, `runtime`) |
| LocalSet bridge, runtime generation, direct-only helpers | `xai-grok-shell/src/session/code_mode.rs` |
| Effective request/compaction/fork surface | `xai-grok-shell/src/session/tool_surface.rs` |
| Request tools + control routing | `acp_session_impl/turn.rs` |
| Nested prepare / history sink | `acp_session_impl/tool_calls.rs` |
| Runtime dispose | `acp_session_impl/run_loop.rs` |
| Replay filtering | `session/storage/mod.rs` |
| Effective mode / settings | `agent/config.rs`, `agent/models.rs`, pager `settings/defs.rs` |
| Provider projection + Responses custom-tool wire | `xai-grok-sampling-types`, `xai-grok-sampler` |
| Parity wording | [`../code-mode-port.md`](../code-mode-port.md) (with tests) |

---

## See also

- [`../code-mode-port.md`](../code-mode-port.md) — canonical parity contract
- [agent-runtime.md](agent-runtime.md) — turn loop, permissions, sessions
- [editing.md](editing.md) — nested edits and hunks
- [providers.md](providers.md) — Codex / Responses context
- [development.md](development.md) — test commands
