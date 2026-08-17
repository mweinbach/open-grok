# Session bus — machine-local cross-session collaboration

**Audience:** agents/contributors changing `xai-grok-shell/src/session_bus/`, the session-collaboration tools, or anything that touches cross-process session identity.

The session bus lets **live Open Grok sessions on one machine discover and message each other** — across projects, terminals, and processes. It is the infrastructure behind the `list_sessions` / `read_session` / `message_session` tools. There is deliberately **no hub process**: the presence directory is the rendezvous and every process dials its peers directly.

## 1. Topology

```text
$OPENGROK_HOME/session-bus/
  p<pid>-<rand8>.json     one presence file per Open Grok process (≥1 hosted session)
  p<pid>-<rand8>.sock     one IPC socket per process (UDS on Unix; named pipe on Windows)
```

- Every process hosting a root session binds a listener (`session_bus::host::SessionBusHost`, `!Send`, lives on the `MvpAgent` LocalSet) and publishes a presence file.
- A process with **zero hosted sessions is invisible** — the presence file is removed when the last session unregisters.
- Discovery is a read-only directory scan (`presence::live_sessions_in`); delivery is a direct dial to the owning process's socket. A crashed process only leaves a stale file, garbage-collected by heartbeat age and PID liveness (`gc_stale`, every 4th heartbeat).

## 2. Presence format

`p<pid>-<rand8>.json` — written atomically (tmp + rename), rewritten on every change and refreshed by a **5 s heartbeat** (stale TTL is 20 s):

```jsonc
{
  "instance_id": "p123-ab12cd34",
  "pid": 123,
  "socket_path": "…/session-bus/p123-ab12cd34.sock",
  "protocol_version": 1,
  "heartbeat_at_ms": 1760000000000,
  "started_at_ms": 1759990000000,
  "sessions": [
    {
      "session_id": "019f…",     // ACP session id (root sessions only)
      "cwd": "/Users/x/repo",
      "project_name": "repo",    // final cwd component
      "model_id": "grok-4",
      "title": null,
      "status": "busy",          // "busy" | "idle"
      "updated_at_ms": 1760000000000
    }
  ]
}
```

- **Subagent sessions are never announced** — peers address root sessions, which own their children.
- Busy/idle is **probed** from `AgentActivity::live_session_states()` by the heartbeat (self-healing: sessions whose actor exited without an unregister are reaped from presence there too).
- The same session id announced by two processes (a session resumed twice) resolves to the freshest heartbeat; the listing marks it `conflict: true`.

## 3. Wire protocol (v1)

JSON lines, **one request per connection**:

```text
client → server:  {"Message":{"v":1,"message_id":"<uuidv7>","target_session":"…",
                                "source_session":"…","source_project":"…","body":"…"}}
                   (or {"Ping":{}})
server → client:  {"Ack":{"message_id":"…","status":"Accepted"|"UnknownSession"|"Rejected"|"InboxFull"}}
                   (or {"Pong":{}})
```

- `PROTOCOL_VERSION = 1`; a version mismatch acks `Rejected`.
- Body cap 32 KiB (`MAX_MESSAGE_BODY_BYTES`); frames capped at `MAX_FRAME_LINE_BYTES`.
- `InboxFull` is reserved on the wire but not emitted in v1 (peer messages consume at turn boundaries like user interjections, which are likewise unbounded).

## 4. Delivery semantics (recipient side)

Inbound messages arrive as `SessionCommand::PeerSessionMessage` on the recipient session actor — the **same mailbox as flat-team agent mail**:

1. `MvpAgent`'s router (`agent/mvp_agent/session_bus_host.rs::route_peer_message`) resolves `resident_handle(target)` and sends the command; a closed channel means the actor exited → `UnknownSession`.
2. The run loop (`session/acp_session_impl/run_loop.rs`, `PeerSessionMessage` arm):
   - **Turn running** → queued into `pending_interjections` (consumed at the next turn boundary, like a user interjection).
   - **Idle** → synthetic wake-up prompt (`peer-message-<id>`, `PromptOrigin::PeerSessionMessage`) fed to the model in an `<agent_message kind="peer_session_message">` envelope.
3. A persisted, auditable update (`SessionUpdate::PeerSessionMessage`, status `delivered_interjection` / `delivered_wake`) is written to `updates.jsonl` **before** delivery, so resume/rewind keep the timeline; the pager renders it as a bounded-preview card.

**Trust model:** peer bodies are model-authored untrusted input. They never carry user consent, permissions, or YOLO semantics; the envelope tells the recipient model this explicitly and points it at `message_session` for replies.

## 5. Tools

`xai-grok-tools/src/implementations/grok_build/session_collaboration/` — `ToolKind::AgentCollaboration`, registered unconditionally, direct-only (top-level) in Code Mode Only:

| Tool | Scope | Behavior |
| --- | --- | --- |
| `list_sessions` | read | Live sessions across processes (bus presence), with `is_self` markers |
| `read_session` | read | Last N persisted user/agent/peer entries from the target's `updates.jsonl` (target must be live; presence cwd locates the session dir) |
| `message_session` | write | Sends over the bus; reports `accepted` / `unknown_session` / `rejected` |

The shell implements `SessionCollaborationBackend` (`session_bus/collaboration.rs::ShellSessionCollaboration`) over the bus client + session storage, and injects it per session via `SessionBusResource` in `agent_rebuild.rs` (after `initialize()` has started the bus, so the client handle is live). Subagent sessions are injected bus-less in v1 — cross-session identity belongs to root sessions.

## 6. Lifecycle and config

- **Start:** `MvpAgent::initialize()` calls `start_session_bus()` (idempotent) — alongside `start_subagent_coordinator()`. Fail-open: a failed bind logs a warning and the process runs bus-less (`SessionBusClient::disabled()`), with tools that explain the state.
- **Register:** end of `spawn_and_register_session` (root sessions only). **Unregister:** `remove_session`; the heartbeat probe reaps anything missed.
- **Shutdown:** dropping the host (agent teardown) cancels the accept/heartbeat tasks and removes this process's presence file and socket.
- **Config:** `[session_bus] enabled` in config.toml, default `true` (`Config::session_bus_enabled`).

## 7. Testing

- Unit/host tests: `session_bus/host.rs` (register/list/self-dial/ping/bad-version/unregister/disabled/probe) and `session_bus/presence.rs` (naming, GC, conflicts, staleness); `two_hosts_exchange_messages_over_shared_bus_dir` is the cross-process stand-in (two hosts, one bus dir, real socket dials both ways).
- `session_bus/collaboration.rs` tests the `updates.jsonl` extraction used by `read_session`.
- Isolate `OPENGROK_HOME` in any test that could start a real bus.

## 8. Non-goals / future work

- No message history or inbox persistence beyond the persisted update on the recipient.
- No cross-machine networking — the machine boundary is the trust boundary (same user, same OS user account).
- Titles in presence start `null`; a generated-title → `update_session_meta` hook is future polish.
- Subagents are bus-less in v1 (see §5).
