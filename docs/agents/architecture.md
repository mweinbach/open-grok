# Architecture

Open Grok is a Rust monorepo. The product surface is the **`open-grok`** binary, composed in `xai-grok-pager-bin`, which wires the **pager TUI** (`xai-grok-pager`) to the **agent runtime** (`xai-grok-shell`) over ACP.

## Top-level layout

| Path | Role |
| --- | --- |
| `crates/codegen/` | Product crates (TUI, shell, tools, sampling, auth, workspace, Code Mode, …) |
| `crates/common/` | Shared platform (tool protocol/runtime, Computer Hub, compaction engine, tracing) |
| `crates/build/` | Build helpers (e.g. protobuf via `xai-proto-build`) |
| `docs/` | Fork contracts, release notes, agent developer docs |
| `bin/` | `open-grok-dev`, `setup-dev`, Dotslash `protoc` |
| `scripts/` | Release packaging (`build-macos-release.sh`) |
| `prod/mc/` | Deployment / managed-config types |
| `third_party/` | Vendored Mermaid layout stack |
| `dist/` | Local/CI release artifacts (not source of truth for version — use `OPEN_GROK_VERSION`) |

**Root `Cargo.toml` is generated and read-only.** Edit per-crate manifests only.

## Layering

```text
xai-grok-pager-bin          composition root (binary only)
    ├── xai-grok-pager      TUI / CLI surfaces / headless orchestration
    └── xai-grok-shell      sessions, auth, turns, leader, ACP agent

Mid (owned by shell + tools path)
    sampler, tools, agent definitions, chat-state, workspace,
    memory, mcp, hooks, code-mode, hunk-tracker, compaction

Leaf / common
    config, paths, env, http, auth traits, sampling-types,
    tool-types / tool-protocol / tool-runtime, telemetry, version
```

**Rules of thumb:**

- `crates/common/` must not depend on product TUI/shell.
- `xai-grok-mcp` quarantines MCP stack dependencies (rmcp / older reqwest) from the rest of the workspace.
- `xai-grok-pager-bin` exists so the pager library does not cycle with `pager-minimal`.
- Provider policy lives in **types + sampler adapters**; credentials live in **shell auth stores** — never mixed into adapters.

## Crate map (when to edit)

### Entry / UI

| Crate | Role | Edit when… |
| --- | --- | --- |
| `xai-grok-pager-bin` | Binary `open-grok`, CLI dispatch | Startup, allocator, crash/update wiring |
| `xai-grok-pager` | Full TUI, slash, settings, ACP client | Interactive UX |
| `xai-grok-pager-minimal` | Minimal scrollback mode | Minimal-mode rendering |
| `xai-grok-pager-render` | Themes / appearance / low-level render | Visual chrome, `pager.toml` appearance |
| `xai-grok-pager-pty-harness` | PTY e2e harness | Terminal integration tests |
| `xai-grok-markdown*` | Streaming markdown | Transcript rendering |
| `xai-grok-mermaid` | Mermaid → image | Diagram blocks |

### Agent runtime

| Crate | Role | Edit when… |
| --- | --- | --- |
| `xai-grok-shell` | Sessions, turns, auth, providers, headless, leader | Core agent behavior |
| `xai-grok-shell-base` | Shared shell foundation | Env / home re-exports |
| `xai-grok-agent` | Agent builder, prompts, AGENTS.md discovery | System prompts, agent defs |
| `xai-agent-lifecycle` | Host-agnostic lifecycle contributors | Data-only turn/session hooks |
| `xai-chat-state` | Conversation actor, usage | History mutation, tokens |
| `xai-prompt-queue` | Queue wire types | Prompt queue protocol |

### Sampling / tools / execution

| Crate | Role | Edit when… |
| --- | --- | --- |
| `xai-grok-sampler` | HTTP streaming + provider adapters | Wire format, retries |
| `xai-grok-sampling-types` | `ApiBackend`, profiles, conversation types | Shared type contracts |
| `xai-grok-tools` | Tool implementations (Grok / Codex / OpenCode packs) | New tools, edit engines |
| `xai-tool-*` (common) | Tool trait, protocol, types | Platform tool contract |
| `xai-grok-code-mode*` | V8 Code Mode runtime + protocol | `exec` / `wait` |
| `xai-grok-sandbox` | OS sandbox profiles | Isolation enforcement |
| `xai-hunk-tracker` | Agent vs external hunk attribution | Diff accept/reject UX |
| `xai-grok-compaction` (common) | Compaction engine | Summaries / compression |

### Config / auth / workspace / extensions

| Crate | Role | Edit when… |
| --- | --- | --- |
| `xai-grok-config` | Load/merge, `OPENGROK_HOME` | Paths, managed config |
| `xai-grok-auth` | HTTP auth DI seam | Credential provider trait |
| `xai-grok-workspace*` | Permissions, FS, worktrees, hub | Trust, isolation, VCS |
| `xai-fast-worktree` | Fast CoW worktrees | Subagent isolation perf |
| `xai-grok-mcp` | MCP transports + OAuth | External tools |
| `xai-grok-hooks` | File/HTTP hooks | Pre/post tool events |
| `xai-grok-plugin-marketplace` | Plugin marketplace | Install sources |
| `xai-grok-memory` | Cross-session memory | Search / storage |
| `xai-grok-update` | GitHub release updates | Auto-update |
| `xai-acp-lib` | ACP channel primitives | Transport |

## Entry points

### Binary

```text
./bin/open-grok-dev  →  cargo run -p xai-grok-pager-bin --bin open-grok
```

Composition root: `crates/codegen/xai-grok-pager-bin/src/main.rs`  
CLI surface: `crates/codegen/xai-grok-pager/src/app/cli.rs`

Common commands:

| Command | Meaning |
| --- | --- |
| `open-grok` | Interactive TUI |
| `open-grok -p "…"` | Headless single turn |
| `open-grok agent stdio` | ACP over stdio (IDE) |
| `open-grok agent leader` | Long-lived multi-client leader |
| `open-grok agent serve` | WebSocket agent server |
| `open-grok login [--codex]` | Provider sign-in |
| `open-grok update` | Install verified GitHub release |

### Request flow (interactive)

```text
open-grok
  → pager event_loop (app/event_loop.rs)
  → AppView / AgentView  (Action → dispatch → Effect)
  → ACP client (pager/src/acp/)
  → shell SessionActor (session/acp_session_impl/)
  → sampler (ProviderAdapter)
  → tools / Code Mode
  → ACP session/update → scrollback
```

### Important module index

| Concern | Path |
| --- | --- |
| TUI event loop | `xai-grok-pager/src/app/event_loop.rs` |
| TUI dispatch | `xai-grok-pager/src/app/dispatch/` |
| Slash commands | `xai-grok-pager/src/slash/` |
| Session turn | `xai-grok-shell/src/session/acp_session_impl/turn.rs` |
| Tool dispatch | `…/tool_dispatch.rs`, `…/tool_calls.rs` |
| Codex auth | `xai-grok-shell/src/codex_auth.rs` |
| Codex models | `xai-grok-shell/src/codex_models.rs` |
| Kimi models | `xai-grok-shell/src/kimi_models.rs` |
| Sampler providers | `xai-grok-sampler/src/provider.rs` |
| Tools | `xai-grok-tools/src/implementations/` |
| Config / home | `xai-grok-config/src/paths.rs` |
| Permissions | `xai-grok-workspace/src/permission/` |

## Fork-specific surfaces

| Area | Care |
| --- | --- |
| Home | `OPENGROK_HOME` / `~/.opengrok` only |
| Binary name | `open-grok` |
| Codex OAuth + catalog | Separate files from xAI |
| Code Mode | Ported contract; see `docs/code-mode-port.md` |
| Auto-update | GitHub Open Grok releases + SHA-256 |
| Branding | User-facing “Open Grok”; crates still `xai-grok-*` |

## See also

- [agent-runtime.md](agent-runtime.md)
- [tui-and-config.md](tui-and-config.md)
- [providers.md](providers.md)
- [development.md](development.md)
