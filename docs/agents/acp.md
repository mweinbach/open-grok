# Agent Client Protocol (ACP)

Developer map of Open Grok’s ACP surfaces: transports, channel primitives, standard methods, Grok/Open Grok extensions, reverse-requests, pager/shell roles, meta keys, leader multi-client routing, headless differences, tests, and gotchas.

Paths are under `crates/codegen/` unless noted.

**End-user product docs:** [`xai-grok-pager/docs/user-guide/15-agent-mode.md`](../../crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md)  
**Runtime context:** [agent-runtime.md](agent-runtime.md) · **Architecture:** [architecture.md](architecture.md)

---

## Roles

```text
ACP Client                         ACP Agent
(pager TUI / headless / IDE)       (xai-grok-shell MvpAgent + SessionActor)
        │                                    │
        │     xai-acp-lib (channels /        │
        │      gateway reverse-RPC)          │
        └────────── transports ──────────────┘
```

| Side | Owns | Primary paths |
| --- | --- | --- |
| **Client** | `initialize`, `session/*` requests, answering reverse-RPC, rendering `session/update` | `xai-grok-pager/src/acp/`, `app/acp_handler/`, `app/dispatch/`, `app/effects/` |
| **Agent** | Sessions, turns, tools, extensions, reverse-RPC *issuance* | `xai-grok-shell/src/agent/mvp_agent/`, `session/acp_session*`, `extensions/` |
| **Channel** | Typed message envelopes, gateway, in-process pipes | `xai-acp-lib/` |

Open Grok is **not** affiliated with xAI/OpenAI; state is `$OPENGROK_HOME` / `~/.opengrok` only. Wire method prefixes (`x.ai/*`, `_x.ai/*`) are inherited from upstream Grok Build and stay stable for client compatibility.

---

## Transports

CLI entry: `open-grok agent <mode>` (`xai-grok-pager/src/app/cli.rs` → shell agent entry points). Shared agent flags (`--model`, `--always-approve` / `--yolo`, `--reauth`, `--agent-profile`, `--plugin-dir`, `--leader` / `--no-leader`) apply before the mode name.

| Transport | Command / path | When used |
| --- | --- | --- |
| **In-process** | Default interactive TUI (`open-grok` without agent subcommand) | Pager spawns `MvpAgent` on a dedicated OS thread + `LocalSet` (`pager/src/acp/spawn.rs`). No JSON-RPC process boundary — `acp_channels()` + direct gateway dispatch. |
| **stdio** | `open-grok agent stdio` | IDE / external ACP clients (Zed, Neovim, Emacs, custom). JSON-RPC lines on stdin/stdout. |
| **Leader (IPC)** | `open-grok agent leader` · clients via `--leader` / `[cli] use_leader` | One long-lived agent process per machine; multiple clients attach over a Unix domain socket under `$OPENGROK_HOME` (`shell/src/leader/`). |
| **Serve / WebSocket** | `open-grok agent serve --bind 127.0.0.1:2419 --secret <token>` | Remote TUI/clients; auth via `Authorization: Bearer` or `?server-key=`. Single `MvpAgent` survives reconnects (`shell/src/agent/server.rs`). Secret: `--secret` or `GROK_AGENT_SECRET` (auto-generated if omitted). |
| **Relay (outbound WS)** | `open-grok agent headless --grok-ws-url …` · leader may open relay eagerly or `--relay-on-demand` | Agent dials out to a relay (first-party grok.com session bearer only). Used for remote/web UIs that cannot spawn local processes (`shell/src/agent/relay.rs`). BYOK / non-xAI OIDC → relay off; leader still serves local IPC. |

### In-process details

- `spawn_grok_shell` builds `AuthManager`, bootstraps config/models, creates linked `AcpClientChannel` / `AcpAgentChannel`, runs `AcpGatewayReceiver` on the agent thread.
- Pager keeps a clone of the same `AuthManager` for voice/other authenticated side channels.
- Prefer this path for interactive Open Grok; it avoids leader version skew and socket lifecycle.

### Leader details

- Socket/lock paths: `$OPENGROK_HOME/leader.sock` (and WS-URL-suffixed variants when relay-scoped). See `shell/src/leader/lock.rs`.
- `connect_or_spawn`: client adopts an existing leader or spawns one; newer client may **evict** a strictly older leader version (never thrash newer→older).
- Leader flags: `--no-exit-on-disconnect`, `--relay-on-demand` (interactive auto-spawn uses on-demand; bare/systemd leaders connect relay eagerly), `--no-auto-update`.
- Pager bridge: `pager/src/acp/leader_bridge.rs` → typed ACP channels + `ConnectionStatus` watch.

### Serve / relay notes

- Serve: first WS connection creates the agent thread; later reconnects rebind gateway senders so in-flight turns continue streaming.
- Relay: keepalive + read-liveness timeout; 401 triggers auth recovery then reconnect backoff. Half-open TCP without liveness bricks sessions until process kill.

---

## `xai-acp-lib` role

Crate: `xai-acp-lib/`.

| Piece | Role |
| --- | --- |
| `message` | Typed `AcpAgentMessage` / `AcpClientMessage`, `AcpArgs` (request + oneshot response), method name mapping onto `agent_client_protocol` types |
| `channel` | Linked unbounded pairs (`acp_channels`), `acp_send` helper |
| `gateway` | `AcpGatewaySender` / `AcpGatewayReceiver` — fan-in channel → connection; reverse-RPC from agent→client uses the same envelope with oneshots |
| `line_reader` / `stdin_reader` | Stdio line framing |
| `common` | `AcpResult`, channel failure kinds (`SendFailed` / `RecvFailed`), compact JSON debug helpers |

**Gateway reverse-RPC:** agent calls into the client (`session/request_permission`, `x.ai/ask_user_question`, `x.ai/exit_plan_mode`, etc.) by sending a request-shaped message and awaiting `response_tx`. Failures are structured (`AcpChannelFailure`, JSON-RPC errors, typed tool enums) — **do not** parse error *text* with substrings.

Ext methods/notifications on the wire may appear **wrapped** by the SDK:

```text
direct:   {"method":"x.ai/foo", "params":{…}}
wrapped:  {"method":"_x.ai/foo","params":{"method":"x.ai/foo","params":{…}}}
```

Leader routing uses `method_of` / `interaction_inner_params` (`shell/src/leader/server.rs`) so both forms classify correctly. Anything that keys off method names must do the same.

---

## Standard ACP methods

Lifecycle (client → agent unless noted):

| Method | Direction | Purpose |
| --- | --- | --- |
| `initialize` | C→A | Protocol version, client capabilities/meta; agent returns models, auth methods, feature flags, available commands |
| `authenticate` | C→A | Login / credential establish when required |
| `session/new` | C→A | Create session (cwd, MCP servers, `_meta` overrides) |
| `session/load` | C→A | Resume; may stream historical `session/update` with `_meta.isReplay` |
| `session/prompt` | C→A | User turn; agent runs agentic loop |
| `session/cancel` | C→A | Cancel in-flight turn |
| `session/update` | A→C | Streaming notifications (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, …) |
| `session/request_permission` | A→C | **Reverse-RPC** tool permission modal |

Implementation:

- Agent trait: `shell/src/agent/mvp_agent/acp_agent.rs` (`impl acp::Agent for MvpAgent`)
- Session turn: `session/acp_session_impl/turn.rs` (`handle_prompt`)
- Persistence of updates: `session/storage/`, `updates.jsonl`

### `initialize` meta (representative)

Agent stamps response meta with model state, auth methods, command catalog, feature ads (e.g. `sessionRecap`, hooks, fs_notify). Client may send:

| Client `_meta` key | Meaning |
| --- | --- |
| `clientType` | Pager / headless / IDE identity (`pager/src/client_identity.rs`) |
| `clientIdentifier` | Stable client instance id |

### `session/new` `_meta` (representative)

| Key | Meaning |
| --- | --- |
| `rules` / `systemPromptOverride` / `agentProfile` | Prompt composition (see user-guide) |
| `x.ai/mcp/servers` | In-process SDK MCP server list |
| `x.ai/persist` | Persist preference |
| `x.ai/leaderClientId` | Leader ownership / routing hint |
| `x.ai/restore_code` | Headless restore-code path |
| `x.ai/skip_envrc` / `x.ai/display_cwd` | Env / UI cwd |

Non-exhaustive — discover live methods from `initialize` and extension dispatch in `acp_agent.rs`.

---

## Grok / Open Grok extensions

### Method prefix `x.ai/*` (client → agent)

Handlers live under `shell/src/extensions/` and are dispatched from `MvpAgent`’s `ext_method` match (`mvp_agent/acp_agent.rs`). Representative categories:

| Category | Prefix / methods | Module |
| --- | --- | --- |
| Filesystem | `x.ai/fs/*` | `extensions/fs.rs` |
| Git / worktrees | `x.ai/git/*`, `x.ai/git/worktree/*` | `git.rs`, `worktree.rs` |
| Search | `x.ai/search/*` | `search.rs` |
| Terminal / PTY | `x.ai/terminal/*` | `terminal.rs` |
| Session admin | `x.ai/session/*` (info, list, fork, rename, delete, updates, load_history, repair, add/remove_working_directory, …) | `session_admin.rs`, `session_updates.rs`, `working_dirs.rs`, … |
| History / rewind / compact | `x.ai/prompt_history`, `x.ai/rewind*`, `x.ai/compact_conversation*` | `prompt_history.rs`, `rewind.rs` |
| Auth | `x.ai/auth/*`, `x.ai/getApiKey`, `x.ai/setApiKey` | `auth.rs` |
| Hooks / plugins / marketplace | `x.ai/hooks/*`, `x.ai/plugins/*`, `x.ai/marketplace/*` | `hooks.rs`, `plugins.rs`, `marketplace.rs` |
| MCP | `x.ai/mcp/*` (+ reverse `x.ai/mcp/sdk_call`) | `mcp.rs`, `xai-grok-mcp` |
| Tasks / subagents / scheduler | `x.ai/task/*`, `x.ai/subagent/*`, `x.ai/scheduler/*` | `task.rs` |
| Hunk tracker | `x.ai/hunk-tracker/*` | `hunk_tracker.rs` |
| Plan mode toggle | `x.ai/toggle_plan_mode` | session handlers |
| Queue | `x.ai/queue/*` | session queue |
| Skills / code nav / suggest | `x.ai/skills/*`, `x.ai/code/*`, `x.ai/suggest*` | `skills.rs`, `code_nav.rs`, `suggest/` |
| Feedback / telemetry / billing / share | various `x.ai/*` | matching modules |

**Open Grok–specific** methods (fork branding, same dispatch table):

| Method | Purpose |
| --- | --- |
| `open-grok/codex/models/refresh` | Refresh Codex model catalog after OAuth |
| `open-grok/codex/models/clear` | Clear Codex model cache |
| `open-grok/kimi/models/query` | Query Kimi models |
| `open-grok/kimi/models/clear` | Clear Kimi model cache |
| `open-grok/kimi/endpoint/apply` | Apply Kimi Platform vs Code endpoint |

Extension responses often use `ExtMethodResult<T>` (`session/result.rs`): `{ result, error? }` — prefer structured `ExtMethodError { code, message, data? }` when adding failures.

### Notifications agent → client

| Wire method | Role |
| --- | --- |
| `session/update` | Standard ACP stream (also persisted envelope method in `updates.jsonl`) |
| `_x.ai/session/update` | **Persisted** Grok-specific session updates (turn completed, compaction checkpoints, rewind markers, …). Storage/replay key. |
| `x.ai/session_notification` | **Live** fire-and-forget session events (pending interaction, queue, retry, diff review, …). Often not the same rail as durable history. |
| `x.ai/session/update` | Live extension path used by some clients/handlers (pager acp_handler comments treat it alongside `session_notification`) |
| `x.ai/session/prompt_complete` | Fire-and-forget turn terminal for live UIs; durable twin is `TurnCompleted` on `_x.ai/session/update` |
| `x.ai/fs_notify`, `x.ai/fs/index*`, `x.ai/search/fuzzy/status`, `x.ai/git/worktree/status`, `x.ai/mcp/*`, `x.ai/task_backgrounded`, `x.ai/task_completed`, `x.ai/monitor_event`, … | Domain push notifications |

**Persistence rule:** `updates.jsonl` lines are envelopes with `method` + `params`. Clients reading bulk history (`x.ai/session/updates`) must branch on `method`:

- `"session/update"` → standard ACP notification params  
- `"_x.ai/session/update"` → Grok `SessionNotification` (`extensions/notification.rs`)

See `extensions/session_updates.rs` and `session/turn_completion.rs`.

### Reverse-requests (agent → client)

Blocking reverse-RPC parks the tool loop on a oneshot and registers in `PendingInteractions` (`session/pending_interaction.rs`). Kinds:

| Kind | Wire | User outcome vs infra failure |
| --- | --- | --- |
| **Permission** | `session/request_permission` | Selected option / `Cancelled` (user cancel is not a transport error) |
| **Question** | `x.ai/ask_user_question` | `UserQuestionResponse::{Accepted, ChatAboutThis, SkipInterview, Cancelled}` vs `UserQuestionError::{TransportError, MalformedResponse}` |
| **Plan approval** | `x.ai/exit_plan_mode` | Approve / reject / cancel; may re-park after resume with persisted `awaiting_plan_approval` |

Coordinator spawn for questions: `session/acp_session_impl/spawn.rs` (maps ACP JSON → typed `UserQuestionResponse` / `UserQuestionError`). Tool code: `xai-grok-tools/.../ask_user_question/`.

**Rules:**

1. Treat **user cancel / skip / chat** as successful user paths (`Ok` at the tool boundary), not failures.
2. Treat **channel drop, timeout, bad JSON** as typed infrastructure errors (`Failed` tool status) — never substring-match error messages.
3. Reverse-requests are **not persisted**; roster learns via live `pending_interaction` / `interaction_resolved` on `x.ai/session_notification`.
4. Keyed by `tool_call_id` so multi-client first-answer-wins and replay-on-attach work.

Pager handlers: `app/acp_handler/permissions.rs`, `interactions.rs`. Views: `views/permission_view.rs`, `question_view.rs`, `plan_approval_view.rs`.

---

## Pager as ACP client

```text
terminal / ticks
  → event_loop
  → Action → dispatch/ (pure)
  → Effect → effects/ (ACP send, disk, network)
  → TaskResult → Action

inbound ACP
  → AcpClientMessage
  → app/acp_handler/  (route by session_id)
  → tracker / AgentView / scrollback
```

| Module | Responsibility |
| --- | --- |
| `pager/src/acp/mod.rs` | Connect (in-process or leader), `AcpConnection`, model/auth bootstrap |
| `acp/spawn.rs` | In-process agent thread |
| `acp/leader_bridge.rs` | IPC ↔ typed channels, reconnect status |
| `acp/meta.rs` | Typed `_meta` parse (`NotificationMeta`, user prompt / chunk keys) |
| `acp/tracker.rs` | Apply streaming updates to scrollback / tool cards |
| `app/acp_handler/` | Permissions, questions, plan approval, session notifications, MCP, queue, background tasks |
| `app/dispatch/` + `app/effects/` | Outbound ACP methods (prompt, ext_method, settings) |

**Invariants:**

- Dispatch stays pure — no ACP I/O inside `dispatch/`.
- Route by `session_id` (including subagent views); never assume the active tab owns every reverse-request.
- YOLO / always-approve is per owning agent, including background turns.

---

## Shell as ACP agent

```text
MvpAgent (LocalSet, !Send)
  ├── initialize / ext_method / session/*  (acp_agent.rs)
  ├── SessionActor per session (own thread + LocalSet)
  │     ├── handle_prompt → sampler → tools
  │     ├── reverse-RPC via GatewaySender
  │     └── persist updates.jsonl
  ├── SubagentCoordinator
  └── stdio | serve | leader | relay front-ends
```

| Piece | Path |
| --- | --- |
| Host agent | `shell/src/agent/mvp_agent/` |
| ACP trait impl | `mvp_agent/acp_agent.rs` |
| Session actor | `session/acp_session.rs`, `acp_session_impl/*` |
| Stdio / entry | `agent/app.rs`, bin wiring from pager |
| WebSocket serve | `agent/server.rs` |
| Relay | `agent/relay.rs` |
| Leader IPC | `shell/src/leader/` |
| Extensions | `shell/src/extensions/` |

`SessionActor` is **`!Send`** — use `spawn_local` / existing handle patterns only (see [agent-runtime.md](agent-runtime.md)).

---

## Meta keys agents commonly break

### Notification / chunk meta

| Key | Where | Contract |
| --- | --- | --- |
| `x.ai/tool` | Tool call `_meta` | Canonical tool stamp (`TOOL_META_KEY`): `version`, `name`, `kind`, `namespace`, `label`, `read_only`, `input` projection. Schema: `xai-grok-tools/schema/tool_meta.schema.json`. Prefer **`label`** for UI grouping across harnesses; do not assume `kind` is cross-harness stable. |
| `open-grok/codeModeTransport` | Tool call / update `_meta` | Marks Code Mode `exec`/`wait` **transport** wrappers. **Never** hide UI by tool name alone — MCP may define `exec`. See `session/code_mode.rs` (`CODE_MODE_TRANSPORT_META_KEY`). |
| `hideFromScrollback` | `UserMessageChunk` / content `_meta` | Suppress scrollback echo for synthetic origins while **still persisting**. Constant: `user_message_chunk_meta::HIDE_FROM_SCROLLBACK`. |
| `promptId` | Notification `_meta` | Client-supplied id echoed on every update for a prompt; drop chunks from cancelled/rewound turns. |
| `eventId` / seq | Notification `_meta` | `"{sessionId}-{counter}"` for reconnect cursor + dedup. Keep full string; counter alone is ambiguous across resumes. |
| `isReplay` | Notification `_meta` | Historical `session/load` stream; do not re-fire bells/toasts. |
| `totalTokens`, `agentTimestampMs`, `streamStartMs`, `turnStartMs` | Notification `_meta` | UI timing / token chrome (`acp/meta.rs`). |
| `displayText`, `displayAsSkill`, `displayAsCron`, `skillTokenRanges` | User prompt content `_meta` | Display overrides vs wire text (`user_prompt_meta`). |
| `promptIndex` | User message chunk `_meta` | Rewind / attribution. |

### Code Mode + tools

- Nested `tools.*` from Code Mode emit **ordinary** tool cards; transport wrappers stay model-visible but should be hidden or de-emphasized in scrollback via `open-grok/codeModeTransport`.
- Direct-only tools (`ask_user_question`, task/subagent controls, …) stay top-level even in Code Mode Only — a JS callback must not own blocking reverse-RPC.

### Hunks

Agent file writes must go through paths that call `record_agent_write`. Relying only on `fs_notify` marks hunks **External** ([editing.md](editing.md)).

---

## Headless behavior differences

Surfaces: `open-grok -p "…"`, headless agent/relay modes (`pager/src/headless.rs`).

| Behavior | Interactive pager | Headless |
| --- | --- | --- |
| `session/request_permission` | Modal / YOLO | **Cancelled** unless `--yolo` / `--always-approve` (then AllowOnce/Always if offered) |
| `ask_user_question` / plan approval | Modal | No interactive UI — design for cancel/auto or avoid tools that need answers |
| `WaitForTerminalExit` | Not supported (poll fallback) | Same — `METHOD_NOT_FOUND` |
| Usage projection | ACP `PromptUsage` (full input tokens + cost ticks) | Reduced headless shape (uncached input tokens, float USD) — see `notification.rs` wire table |
| Auth errors | Login UX | Exit with device-code / API key messaging |
| Leader reconnect | Unlimited while TUI alive | Bounded attempts (`RECONNECT_MAX_ATTEMPTS_BOUNDED`) |

Permission prompts must **never block** headless forever — cancel is the fail-open for non-YOLO.

---

## Leader multi-client notes

Source of truth: `shell/src/leader/server.rs`, `leader/mod.rs`, pager `leader_bridge` / `leader_cluster`.

| Topic | Rule |
| --- | --- |
| **Session driver** | One client is the *driver* for a session (runs prompts). Others are *subscribers*. |
| **Broadcast stream** | `session/update` (and most `sessionId`-bearing notifications) fan out to every subscriber so all clients render the same transcript. |
| **Shared reverse-RPC** | `session/request_permission`, `x.ai/ask_user_question`, `x.ai/exit_plan_mode` are **broadcast** to every subscriber; **first answer wins**; leader caches by `tool_call_id` for replay-on-attach; `interaction_resolved` evicts. |
| **Driver-only** | `x.ai/scheduled_task_inject_prompt` (and similar “enqueue + drive” signals) go to the **single** driver only — otherwise phantom queue entries / competing turns. |
| **Subagents** | Child sessions inherit routing; driverless descendants can adopt parent driver. |
| **Disconnect** | Driver transfer to another subscriber when possible; parked plan-approval can keep session resident (`has_parked_plan_approval`) so idle-unload does not drop the oneshot. |
| **Version skew** | Newer client may replace older leader; unparseable versions are left alone. |
| **Wrapped methods** | Always normalize with `method_of` before classifying. |

In-process multi-client tests: `pager/src/app/leader_cluster/`.

---

## Test locations

| Area | Where |
| --- | --- |
| ACP channel / send failures | `xai-acp-lib` unit tests (`channel.rs`) |
| Session turn / permissions / plan | `shell/src/session/acp_session_tests/`, `plan_mode*`, `acp_session_impl/turn.rs` |
| `_x.ai/session/update` persistence | `shell/tests/test_xai_session_update.rs` |
| Session load / updates bulk | `shell/tests/session_load_perf.rs`, `extensions/session_updates.rs` |
| Leader stdio multi-client | `shell/tests/test_leader_stdio_integration.rs` |
| Leader soak / death / version | `test_leader_soak.rs`, `test_leader_death_repro.rs`, `test_leader_version_skew.rs` |
| Fork / resume | `shell/tests/test_fork_session.rs` |
| Ask-user typed paths | `xai-grok-tools/.../ask_user_question/` tests |
| Pager ACP UI | `pager/src/app/acp_handler/tests/*` (permissions, interactions, plan_mode, reconnect, queue, …) |
| Pager dispatch / effects | `pager/src/app/dispatch/tests/`, `effects/tests.rs` |
| Meta parse | `pager/src/acp/meta.rs` tests |
| Tracker hide / transport | `pager/src/acp/tracker.rs` tests |
| Headless permissions / notifications | `pager/src/headless.rs` tests |
| Leader cluster (in-process) | `pager/src/app/leader_cluster/` |
| Built binary e2e | `shell/tests/test_built_binary_e2e.rs` |
| Shared leader harness | `xai-grok-test-support/src/leader.rs` |

Focused commands: see [development.md](development.md).

---

## Gotchas

1. **`_x.ai/session/update` vs `x.ai/session_notification`** — Durable history vs live fire-and-forget. Reattach/replay depends on the `_x.ai` rail for terminals like `TurnCompleted`; live-only signals will not appear after cold load.
2. **Wrapped `_x.ai/*` methods** — Leader and custom routers that only read top-level `method` miss reverse-RPC and notifications.
3. **Code Mode transport meta** — Hiding every tool named `exec`/`wait` breaks legitimate MCP tools; use `open-grok/codeModeTransport`.
4. **Tool stamp** — UI and analytics should read `x.ai/tool.label` (and version), not invent parallel naming.
5. **`hideFromScrollback` ≠ skip persist** — Synthetic prompt echoes must hit `updates.jsonl` or rewind/resume loses them.
6. **Typed reverse-RPC failures** — Match `UserQuestionError` / permission outcomes / channel failure enums; never `err.contains("disconnect")`.
7. **Headless permissions cancel** — Tests and agents that expect AllowOnce without YOLO will see cancelled tools.
8. **Shared modals, single driver** — Answering a permission from any client is fine; injecting a scheduled prompt from every client is not.
9. **`SessionActor` `!Send`** — Gateway default spawner is `spawn_local`; do not move session work across threads.
10. **Home isolation** — Leader socket, sessions, auth all under `$OPENGROK_HOME` / `~/.opengrok` — never `~/.grok`.
11. **Provider isolation** — ACP meta must not smuggle Codex opaque history into xAI sessions (export boundary / `ever_used_codex`); see [providers.md](providers.md).
12. **`eventId` highwater** — Clients must keep the full id string for `session/load` cursors; numeric suffix alone collides across resumes.
13. **Plan approval re-park** — Resume can re-issue `x.ai/exit_plan_mode` without a running turn; idle-unload policy is special-cased.
14. **Extension result envelope** — Prefer `ExtMethodResult` / `ExtMethodError` so pager effects deserialize failures without scraping free text.

---

## See also

- [agent-runtime.md](agent-runtime.md) — turns, tools, plan gate, subagents  
- [editing.md](editing.md) — edit tools + plan-mode ACP interactions  
- [tui-and-config.md](tui-and-config.md) — pager Action/Effect, hooks, MCP  
- [providers.md](providers.md) — multi-provider isolation  
- [architecture.md](architecture.md) — crate map and request flow  
- User guide: [15-agent-mode.md](../../crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md), [19-plan-mode.md](../../crates/codegen/xai-grok-pager/docs/user-guide/19-plan-mode.md)
