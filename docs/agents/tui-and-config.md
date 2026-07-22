# TUI, configuration, and extensions

Developer guide for the pager, config layers, slash commands, settings, hooks, plugins, skills, and MCP.

End-user docs: `crates/codegen/xai-grok-pager/docs/user-guide/` (especially 03–10).

## TUI architecture

| Crate | Role |
| --- | --- |
| `xai-grok-pager-bin` | Composition root; binary `open-grok` |
| `xai-grok-pager` | Full TUI, slash, settings, ACP client, headless orchestration |
| `xai-grok-pager-render` | Themes, `pager.toml` appearance, terminal probes |
| `xai-grok-pager-minimal` | Scrollback-native minimal mode |
| `xai-grok-shell` | Agent runtime (ACP server side) |

### Elm-style loop

Documented in `xai-grok-pager/src/app/mod.rs` and `event_loop.rs`:

```text
IO (terminal / ACP / ticks / config watcher)
  → AppView input routing
  → Action
  → dispatch/   (sync pure state + Vec<Effect>)
  → effects/    (async ACP, disk, network)
  → TaskResult → more Actions
```

| Module | Responsibility |
| --- | --- |
| `app/event_loop.rs` | Thin `tokio::select!`; delegates to AppView |
| `app/app_view.rs` | Root: welcome, roster, global config, trust |
| `app/agent_view/` | Per-session VM: prompt, scrollback, modals, panes |
| `app/dispatch/` | Pure `Action` handling — **no I/O** |
| `app/actions.rs` | `Action` / `Effect` / `TaskResult` enums |
| `app/effects/` | Async side effects |
| `app/acp_handler/` | Inbound ACP notifications |
| `app/modals.rs` | Modal key/mouse/draw routing |
| `scrollback/` | Transcript blocks (user, agent, tools, mermaid, …) |
| `views/` | Widgets and modals |
| `slash/` | Builtin slash commands |
| `settings/` | Settings registry metadata |

### Screens worth knowing

| Area | Path |
| --- | --- |
| Welcome | `views/welcome/` |
| Chat | `views/agent.rs`, `agent_view/` |
| Settings modal | `views/settings_modal/` |
| Extensions modal | `views/extensions_modal.rs` |
| Permission / plan / question | `views/permission_view.rs`, `plan_approval_view.rs`, `question_view.rs` |
| Dashboard | `views/dashboard/` |
| Scrollback tool cards | `scrollback/blocks/tool/` |

### Minimal vs fullscreen

- Preference: `[ui] screen_mode` in `config.toml` (`minimal` | `fullscreen`).
- Legacy: `pager.toml` `[terminal] minimal`.
- Re-exec: `/minimal` / `/fullscreen` via `app/screen_mode_relaunch.rs`.
- Minimal mode: **no theming** (`/theme` gated off).

### Gotchas for UI work

1. **Dispatch purity** — never perform I/O inside `dispatch/`; emit `Effect`.
2. **Settings ownership** — `SettingOwner::{Pager, Shell, Shared}` controls persist path and hot cache.
3. **Minimal mode** — gate with `available_in_minimal()` / `hidden_in_minimal`.
4. **Mermaid** — lazy out-of-process render (`mermaid_worker`); do not layout on UI thread.
5. **Theme preview** — live preview mutates theme without disk; cancel must restore.
6. **Folder trust** — one switch for project hooks + MCP + LSP.
7. **Brand** — user-facing Open Grok strings; keep ACP meta keys stable.

## Configuration

### Home isolation (fork-critical)

| Concept | Default | Override |
| --- | --- | --- |
| Runtime home | `~/.opengrok` | `OPENGROK_HOME` |
| Managed binary | `$OPENGROK_HOME/bin/open-grok` | installer / `OPEN_GROK_BIN_DIR` PATH symlink |
| Project dir | `.opengrok/` in repo | — |
| System managed | `/etc/opengrok/` (Unix) | — |

Implementation: `xai-grok-config/src/paths.rs` (`grok_home()`, `default_grok_home()`, …).

**Never use `~/.grok`.** The fork does not fall back to upstream Grok Build state.

### Config merge (disk, low → high)

From `xai-grok-config`:

1. `/etc/opengrok/managed_config.toml`
2. `$OPENGROK_HOME/managed_config.toml`
3. `$OPENGROK_HOME/config.toml`
4. `$OPENGROK_HOME/requirements.toml` (signed cloud cache when key embedded)
5. `/etc/opengrok/requirements.toml`
6. macOS MDM (`ai.x.opengrok`) where applicable

User-facing effective precedence also includes CLI flags and environment variables (see user-guide `05-configuration.md`).

### Two TOML surfaces

| File | Purpose |
| --- | --- |
| `$OPENGROK_HOME/config.toml` | Models, auth, MCP, skills, plugins, `[ui]`, tools, sandbox, … |
| `$OPENGROK_HOME/pager.toml` | Appearance / animation / scrollback chrome; **hot-reloaded** |

`UiConfig` serde shape: `xai-grok-shared/src/ui_config.rs`.

### Swarm controls

```toml
[ui]
swarm_mode = false
```

- **Settings:** the Swarm mode row persists this default and updates the active shell session immediately.
- **Slash:** `/swarm` toggles manual mode, `/swarm on` and `/swarm off` set it explicitly, and `/swarm <task>` enables a one-turn swarm prompt that auto-exits afterward. If manual mode was already active, it remains active.
- **Live UI:** an active session shows a bold `swarm` footer badge. Swarm children render in one foldable scrollback card with fixed input-order slots, running/queued/completed/failed/cancelled counts, elapsed time, tool/turn counts, and context usage. Ordinary child tracking remains available through the tasks pane and framed transcript view.
- **Dispatch contract:** pager one-shot submission uses one ordered effect (`swarm_mode_changed` before `session/prompt`). If either send fails before the prompt is accepted, the optimistic turn is rolled back and the draft is restored.

### Antigravity subagents

```toml
[ui]
antigravity_subagents = false   # Settings row: "Antigravity subagents"

[antigravity]                   # optional operator knobs (no Settings UI)
binary = "agy"                  # name or absolute path of the Antigravity CLI
skip_permissions = true         # default: full access (see below)
```

- **What it does:** when enabled, the Antigravity CLI's models become subagent
  models for the `task`, `agent_swarm`, and `workflow` tools as
  `antigravity:<model>` slugs (e.g. `antigravity:gemini-3.1-pro`), queried live
  from `agy models`. The child runs out-of-process via `agy --print` — its own
  model, login, and tool loop; no `SamplingClient` — with the workspace granted
  via `--add-dir` and the conversation id captured from `--log-file` so
  `resume_from` continues the same CLI conversation (`--conversation`).
- **Gating:** the Settings row is hidden unless the binary resolves
  (`xai-grok-pager` seeds `ANTIGRAVITY_CLI_PRESENT` at startup, honoring
  `[antigravity].binary`). Being signed out of `agy` fails spawns with a
  "run `agy` to sign in" error; the roster/validator caches probe results
  (`agent/antigravity.rs`, 5m TTL signed-in / 30s signed-out). Restart
  required after toggling.
- **Permissions:** headless `agy` auto-denies mutating tools without its
  auto-approve flag, which surfaced as constant permission errors for
  worker subagents. Open Grok therefore passes the flag **by default**
  (`skip_permissions` unset ⇒ true): antigravity members get workspace
  writes and command execution. Set `[antigravity] skip_permissions =
  false` to force read-only researchers, and spawns whose capability mode
  is pinned read-only never get the flag regardless.
- **Anchors:** CLI mechanics `xai-grok-shell/src/agent/antigravity.rs`;
  dispatch branch `agent/subagent/handle_request.rs` (routes `antigravity:*`
  to `agent/subagent/antigravity_runner.rs`); roster/validator injection
  `session/agent_rebuild.rs`; conversation id persisted in subagent
  `meta.json` (`antigravity_conversation_id`).

### Notable env vars

| Var | Role |
| --- | --- |
| `OPENGROK_HOME` | State root |
| `OPEN_GROK_BIN_DIR` | PATH-facing symlink directory |
| `XAI_API_KEY` | Default API key |
| `GROK_*` feature toggles | Memory, subagents, web fetch, sandbox, MCP timeouts, … |
| `GROK_FOLDER_TRUST=0` | Disable folder trust (ungates project hooks/MCP/LSP) |
| `OPENGROK_DISABLE_AUTOUPDATER=1` | Disable background update checks |
| `COLORTERM` / `NO_COLOR` | Color capability |

Tests that touch config must set **`HOME` and `OPENGROK_HOME`** (and `USERPROFILE` on Windows) under a temp directory.

## How to add a slash command

1. Create `xai-grok-pager/src/slash/commands/<name>.rs`.
2. Implement `SlashCommand` (`slash/command.rs`):
   - `name`, `aliases`, `description`, `usage`
   - `takes_args` / `args_required` / `suggest_args`
   - `run` → prefer `CommandResult::Action(Action::…)` so effects stay in dispatch
3. Register in `slash/commands/mod.rs` → `builtin_commands()`.
4. Optional: `available_in_minimal()`, preview hooks, `required_tools()`, visibility.
5. ACP-advertised commands can replace by name via `CommandRegistry::set_acp_commands()` (skills / shell).
6. Document in user-guide `04-slash-commands.md` if user-facing.

Reference: `slash/commands/theme.rs`.

## How to add a setting

1. If shell-persisted: field on `UiConfig` (`xai-grok-shared`) + shell config setter as needed.
2. Add `SettingMeta` in `settings/defs.rs` → `default_settings()`.
3. Add `Action::Set…` in `app/actions.rs`.
4. Implement setter in `app/dispatch/settings/setters.rs` (often `Effect::PersistSetting`).
5. Wire modal current/default values in `settings/registry.rs` if not auto-derived.
6. Add tests that defaults match `UiConfig::default()` where applicable.

Owners:

- **Pager** — session-only
- **Shell** — `config.toml` only
- **Shared** — `config.toml` + hot render cache

## How to add a UI modal / screen

1. State: `ActiveModal` in `views/modal.rs` (or a pane on `AgentView` / `AppView`).
2. Open via slash → `Action` → dispatch, or keybinding.
3. Input: `app/modals.rs` handlers.
4. Draw: `views/<feature>.rs`; reuse `modal_window` / picker chrome.
5. Side effects only via `Action` / `Effect`.

Extensions modal already covers Hooks | Plugins | Marketplace | Skills | MCP.

## Hooks

| Item | Path |
| --- | --- |
| Crate | `xai-grok-hooks/` |
| Discovery / runner | `discovery.rs`, `dispatcher.rs`, `runner/` |
| Examples | `xai-grok-hooks/examples/` |
| User docs | user-guide `10-hooks.md` |

Locations: `$OPENGROK_HOME/hooks/*.json`, project `.opengrok/hooks/`, vendor compat, plugin bundles.

**Trust:** project hooks require folder trust (`trusted_folders.toml`).  
**Security:** only **PreToolUse** can block; hooks fail open — not a sole security boundary.

Deep map: [hooks-plugins-skills.md](hooks-plugins-skills.md).

## Plugins and marketplace

| Item | Path |
| --- | --- |
| Marketplace | `xai-grok-plugin-marketplace/` |
| CLI | `pager/src/plugin_cmd.rs` → `open-grok plugin …` |
| User doc | user-guide `09-plugins.md` |

Plugin layout: `skills/`, `commands/`, `agents/`, `hooks/hooks.json`, `.mcp.json`, `.lsp.json`, optional `plugin.json`.

Scopes: user `$OPENGROK_HOME/plugins/`, project `.opengrok/plugins/`, CLI `--plugin-dir`, session meta.

Deep map: [hooks-plugins-skills.md](hooks-plugins-skills.md).

## Skills

| Item | Path |
| --- | --- |
| Discovery | `xai-grok-tools/src/implementations/skills/` |
| Built-ins | `xai-grok-shell/skills/*/SKILL.md` |
| User doc | user-guide `08-skills.md` |

Priority (high → low): cwd / repo `.opengrok/skills` & `commands`, `.agents/`, vendor dirs (compat toggles), user `$OPENGROK_HOME/skills`.  
Flat `commands/*.md` become slash commands.  
Skill roots are **not** filtered by `.gitignore` — use `[skills] ignore` / `disabled`.

Deep map: [hooks-plugins-skills.md](hooks-plugins-skills.md).

## MCP

| Item | Path |
| --- | --- |
| Crate | `xai-grok-mcp/` |
| CLI | `pager/src/mcp_cmd.rs` |
| Config | `[mcp_servers.<name>]` in user or project config |
| Credentials | `$OPENGROK_HOME/mcp_credentials.json` |
| User doc | user-guide `07-mcp-servers.md` |

Transports: stdio, HTTP, SSE / streamable HTTP. Project MCP is folder-trust gated. Permission rule names use `server__tool`.

## Theming, markdown, mermaid

| Layer | Path |
| --- | --- |
| Themes | `xai-grok-pager-render/src/theme/` |
| Appearance | `pager-render/src/appearance/` |
| Streaming MD | `xai-grok-markdown` |
| Mermaid engine | `xai-grok-mermaid` |
| Mermaid UI | `pager/src/scrollback/blocks/mermaid_content.rs` |
| Worker | `pager/src/app/mermaid_worker.rs` |

Setting: `[ui] render_mermaid = auto|on|off`. Truecolor themes may hide without truecolor support.

## Headless and agent modes

| Mode | Entry |
| --- | --- |
| Headless prompt | `open-grok -p` / `--prompt-json` / `--prompt-file` → `pager/src/headless.rs` |
| ACP stdio | `open-grok agent stdio` |
| Leader | `open-grok agent leader` |
| Serve | `open-grok agent serve` |

User docs: `14-headless-mode.md`, `15-agent-mode.md`.

## User-guide inventory

Index: `xai-grok-pager/docs/user-guide/README.md` (01–24). Prefer that set for product UX over older sibling notes in `pager/docs/hooks-and-plugins.md` when they disagree.

## See also

- [architecture.md](architecture.md)
- [agent-runtime.md](agent-runtime.md)
- [development.md](development.md)
