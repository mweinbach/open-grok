# AGENTS.md — Open Grok

Instructions for AI coding agents (and humans) working in this repository.

**Product:** Open Grok (`open-grok`) — community fork of [Grok Build](https://github.com/xai-org/grok-build) with ChatGPT Codex, multi-provider support, and Code Mode.  
**Version file:** [`OPEN_GROK_VERSION`](OPEN_GROK_VERSION)  
**Deeper docs:** [`docs/agents/`](docs/agents/) · fork contracts: [`docs/`](docs/) · repo skills: [`.agents/skills/`](.agents/skills/) · end-user guide: [`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)

---

## 1. What this repo is

| Concept | Value |
| --- | --- |
| Public binary | `open-grok` (not `grok`) |
| User state | `$OPENGROK_HOME` or `~/.opengrok` — **never** `~/.grok` |
| Project config | `.opengrok/` in the repo |
| Language | Rust (edition 2024), toolchain pin in `rust-toolchain.toml` |
| Root `Cargo.toml` | **Generated / read-only** — edit per-crate `Cargo.toml` only |

Open Grok is **not** affiliated with xAI or OpenAI. Credentials, sessions, skills, plugins, and caches are **fully isolated** from upstream Grok Build installs.

---

## 2. Start here (orientation)

```text
User / IDE / headless client
        │ ACP (stdio, WS, leader, or in-process)
        ▼
xai-grok-pager          ← TUI, slash commands, settings, scrollback
        │ ACP client
        ▼
xai-grok-shell          ← sessions, turns, auth, providers, subagents
        │
        ├── xai-grok-sampler     (xAI / Codex / Kimi adapters)
        ├── xai-grok-tools       (bash, edit, plan, web, …)
        ├── xai-grok-code-mode   (persistent V8 exec/wait when Code Mode is active)
        ├── xai-grok-workspace   (permissions, sandbox FS, worktrees)
        └── xai-chat-state       (conversation / tokens)
```

| If you need to change… | Primary crate / path |
| --- | --- |
| Binary entry / CLI | `crates/codegen/xai-grok-pager-bin` |
| TUI, slash, settings | `crates/codegen/xai-grok-pager` |
| Agent turns, sessions, auth | `crates/codegen/xai-grok-shell` |
| Tool implementations | `crates/codegen/xai-grok-tools` |
| HTTP / provider wire | `crates/codegen/xai-grok-sampler` |
| Provider types / profiles | `crates/codegen/xai-grok-sampling-types` |
| Permissions / worktrees | `crates/codegen/xai-grok-workspace` |
| Code Mode V8 | `crates/codegen/xai-grok-code-mode*` |
| Config / home paths | `crates/codegen/xai-grok-config` |
| Prompts / AGENTS.md load | `crates/codegen/xai-grok-agent` |

Full crate map: [`docs/agents/architecture.md`](docs/agents/architecture.md).

---

## 3. Non-negotiable rules

1. **Do not fall back to `~/.grok`.** Open Grok state is `$OPENGROK_HOME` / `~/.opengrok` / project `.opengrok/`.
2. **Do not edit root `Cargo.toml`.** Change the relevant crate manifest.
3. **Keep providers isolated.** xAI, Codex, Kimi Platform, and Kimi Code credentials, catalogs, caches, hosted tools, and opaque history must not cross. See [`docs/provider-architecture.md`](docs/provider-architecture.md).
4. **Provider identity comes from model metadata**, never from a model slug or URL alone.
5. **API backend ≠ credentials.** Selecting Responses does not select Codex OAuth; an explicit model API key wins over OAuth.
6. **Plan mode is not permission YOLO.** Edit gating for plan mode runs in the shell tool path (`plan_mode_edit_gate`), before hooks/permissions. Do not “fix” plan mode only inside the permission manager.
7. **Hunk attribution:** agent file writes must call `record_agent_write` (or the existing write path that does). Relying on `fs_notify` alone marks hunks **External**.
8. **Code Mode transport is hidden in the UI.** Do not treat `exec`/`wait` as ordinary tool cards; nested `tools.*` calls are what users see.
9. **SessionActor is `!Send` (LocalSet).** Use existing `spawn_local` / handle patterns; do not move session work across threads.
10. **No secrets in commits.** No real API keys, OAuth tokens, release binaries, or user session dumps.
11. **Scoped changes.** Prefer the smallest crate/module set that implements the behavior; add tests next to the behavior.
12. **Hooks fail open.** They are not a security boundary alone — combine with deny rules and sandbox.
13. **Preserve concurrent work.** This checkout may be shared by multiple tasks. Check `git status` before and after long builds, format only owned files when unrelated edits are present, and stage explicit paths.
14. **Commit tested units as they land.** Do not wait until the end of a large task. Each coherent commit should state the behavior or invariant it adds and include its focused verification.
15. **A release is the published artifact.** Source push, tag push, and local build are intermediate states; release work ends only after public asset and installer verification.

---

## 4. How features work (short map)

Detailed behavior: [`docs/agents/agent-runtime.md`](docs/agents/agent-runtime.md), edits: [`docs/agents/editing.md`](docs/agents/editing.md).

### 4.1 Turn loop

1. Client sends `session/prompt` (ACP).
2. `SessionActor::handle_prompt` (`shell/.../acp_session_impl/turn.rs`).
3. Sample via `xai-grok-sampler` → stream tokens/tool calls.
4. For each tool call: **plan edit gate → PreToolUse hooks → permissions → dispatch**.
5. Tool results return to chat state; loop until stop / max turns / cancel.
6. Persist `updates.jsonl` + chat history under the session dir.

### 4.2 File edits

| Path | When used | Implementation |
| --- | --- | --- |
| `search_replace` | Default Grok Build toolset | `xai-grok-tools/.../grok_build/search_replace/` |
| `apply_patch` | Codex toolset / freeform patches | `.../implementations/codex/apply_patch/` |
| Hashline edit | Alternate edit scheme | `.../grok_build_hashline/` |
| Nested tools | Code Mode / Code Mode Only | JS `tools.*` → same prepare/dispatch path |

**Plan mode:** only the session plan file (`session_dir/plan.md`) may be edited; other `AccessKind::Edit` tools (including `apply_patch`) are rejected. Non-edit tools (bash, read, MCP) still go through normal permissions/YOLO.

### 4.3 Permissions (order)

1. Plan-mode edit gate (hard reject when active)  
2. PreToolUse hooks (deny stops; allow does **not** skip later checks)  
3. Plan-file auto-approve (plan.md only)  
4. Permission manager (`xai-grok-workspace` permission actor)

Subagents **inherit** the parent `PermissionHandle` (including always-approve). They get a **fresh Inactive** plan tracker (parent plan gate does not cover children).

### 4.4 Subagents

- Max depth **1** (`MAX_SUBAGENT_DEPTH = 1`).
- Spawn via `task` / spawn_subagent tool → `SubagentCoordinator`.
- Optional worktree isolation (`xai-fast-worktree` + workspace worktree).
- Children are full sessions; usage folds back into parent.
- Deep map: [`docs/agents/subagents.md`](docs/agents/subagents.md).

### 4.5 Code Mode

When Code Mode is effective (an OpenAI Codex Code Mode Only model requirement wins over Settings):

- Codex exposes native custom/freeform `exec`; xAI exposes function `exec` with raw JavaScript in `source`. Both expose `wait`.
- Mixed Code Mode keeps ordinary tools top-level. Code Mode Only keeps them under `tools.*`, with direct-only human/multi-agent controls top-level.
- Persistent V8 session for a compatible timeline; reset on rewind/incompatible route changes and disposed on session end.
- Deep map: [`docs/agents/code-mode.md`](docs/agents/code-mode.md).
- Parity contract: [`docs/code-mode-port.md`](docs/code-mode-port.md).

### 4.6 Multi-provider

Three independent axes: **`ApiBackend`** × **`ProviderProfile`** × **`AuthScheme`/`BearerResolver`**.

| Provider | Auth store | Notes |
| --- | --- | --- |
| xAI | `$OPENGROK_HOME/auth.json` | Default `login` / `logout` |
| Codex | `$OPENGROK_HOME/codex-auth.json` | `login --codex`; separate model cache |
| Kimi Platform | `auth.json` scope `kimi::api_key` | Isolated from Kimi Code |
| Kimi Code | `auth.json` scope `kimi_code::api_key` | Isolated from Platform |

After any non-xAI profile that denies xAI services, the session export boundary closes monotonically (compatibility field still named `ever_used_codex`).

---

## 5. How we should work in this repo

### 5.1 Before coding

1. Run `git status --short --branch`; identify unrelated or concurrent edits before formatting, testing, or staging.
2. Identify the layer: **pager (UI)** vs **shell (agent)** vs **tools** vs **sampler** vs **workspace**.
3. Read the nearest module docs / existing tests.
4. For provider, Code Mode, auth, release, session-forensics, or upstream-sync work, load the matching repo skill below.
5. Prefer extending existing patterns over inventing parallel ones.

| Repeated task | Repo skill |
| --- | --- |
| Build or change this checkout | [`develop-open-grok`](.agents/skills/develop-open-grok/SKILL.md) |
| Add/change provider, model, auth, or wire behavior | [`change-open-grok-provider`](.agents/skills/change-open-grok-provider/SKILL.md) |
| Prove compaction, resume, or subagent persistence | [`verify-open-grok-session`](.agents/skills/verify-open-grok-session/SKILL.md) |
| Build and publish a macOS release | [`release-open-grok`](.agents/skills/release-open-grok/SKILL.md) |
| Compare or replay an upstream snapshot | [`sync-open-grok-upstream`](.agents/skills/sync-open-grok-upstream/SKILL.md) |

### 5.2 While coding

- Keep **dispatch pure** in the pager: `Action` → state + `Effect`; I/O only in `effects/` / ACP handlers.
- Keep **provider adapters credential-free** (`xai-grok-sampler/src/provider.rs`).
- When adding a tool: implement `xai_tool_runtime::Tool`, register in the correct pack, emit proper ACP tool meta, respect output caps.
- When changing permissions: update rule docs + unit tests under `xai-grok-workspace/src/permission/`.
- When changing plan mode: update `plan_mode.rs` **and** `plan_mode_edit_gate` tests.
- Brand user-facing strings as **Open Grok**; crate names remain `xai-grok-*` (upstream heritage).
- For live provider/settings mutations, preserve FIFO dispatch order, hold sends fail-closed while state is unresolved, and cover send-now, queued, interject, and subagent paths.
- For auxiliary work (recap, memory, title generation, embeddings), treat the selected provider/model as an explicit route with its own credentials and export policy; do not inherit the active chat provider accidentally.

### 5.3 After coding

```sh
# Focused checks (prefer package-scoped)
cargo fmt --all -- --check
cargo clippy --locked -p <crate> --all-targets
cargo test --locked -p <crate> -- <filter>

# Dev binary without installing over a release
./bin/open-grok-dev --version
```

See [`docs/agents/development.md`](docs/agents/development.md) for full build/test/release commands.

Interpret failures before broadening the patch: the Rust workspace has long compile/link phases and some suites share global state. Re-run a failing test alone to distinguish a deterministic regression from suite interference, but do not hide repeatable failures. Run `bash -n` on changed shell scripts. Use an isolated `OPENGROK_HOME` for tests and installer smokes.

### 5.4 History-backed completion contracts

| Task | Complete only when… |
| --- | --- |
| Local setup | The target package compiles/tests, the repo-local runner reports the expected version, and CLI smoke (such as completions) works without replacing an installed release. |
| Provider/model/auth | Registry/request/stream coverage passes; credentials, caches, hosted tools, opaque history, 401 refresh, logout, auxiliary routing, and export boundaries stay provider-local; live model/provider switches rebind before sends resume. |
| Code Mode | Model metadata controls effective mode; raw-JS `exec`, persistent V8, nested tool dispatch, permissions, and TUI suppression/rendering all stay aligned. |
| Session/compaction | The exact session id is found on disk; `summary.json`, `updates.jsonl`/`events.jsonl`, child `subagents/`, and referenced checkpoints agree. For auto-compaction, prove `auto_compact_started` → checkpoint persistence → `auto_compact_completed`. |
| Release | A clean exact commit produces a version/commit-verified arm64 binary; signature and checksum pass; the canonical five assets are published; GitHub digests match; latest/tag-specific installer smoke and the managed installed binary report the expected version. |
| Upstream sync | Merge-base state is checked first; imports are replayed in reviewable batches; fork naming, paths, updater, provider/auth, Code Mode, and security contracts survive; baseline and focused tests are recorded. |

### 5.5 PR / change hygiene

- Scoped diffs; explain user-visible behavior.
- Add or update tests for the changed path.
- Stage explicit owned paths and commit each tested unit with a descriptive subject; never sweep concurrent work into the commit.
- Do not commit `target/`, release artifacts under `dist/` (except intentional release workflow), or credentials.
- Upstream-only bugs: report upstream; fork-specific issues belong here.
- License: Apache-2.0 first-party; preserve third-party notices for ported Codex code.

---

## 6. Build & run (cheat sheet)

```sh
./bin/setup-dev
./bin/open-grok-dev                 # TUI from source
cargo build --locked -p xai-grok-pager-bin --bin open-grok

# Headless one-shot
./bin/open-grok-dev -p "say hi"

# Agent / ACP
./bin/open-grok-dev agent stdio
```

Release (Apple Silicon, clean tree): `./scripts/build-macos-release.sh` reads `OPEN_GROK_VERSION`. The builder requires the pinned arm64 `ripgrep 15.0.0` through `GROK_TOOLS_BUNDLE_RG_PATH`; do not substitute a newer Homebrew binary. Use [`release-open-grok`](.agents/skills/release-open-grok/SKILL.md) for the publication and installer-verification sequence.

---

## 7. Where user docs live

End-user product docs (install, slash commands, MCP, skills, sandbox, etc.):

`crates/codegen/xai-grok-pager/docs/user-guide/`

Do **not** duplicate long user tutorials in this file. Link them. Keep **AGENTS.md** and **`docs/agents/`** developer- and agent-oriented: architecture, invariants, edit paths, and test locations.

Project rules for **user projects** (not this repo’s own guide) are documented in user-guide `12-project-rules.md` (AGENTS.md / Claude.md discovery).

---

## 8. Documentation index for agents

| Doc | Contents |
| --- | --- |
| [`docs/agents/README.md`](docs/agents/README.md) | Index of agent developer docs |
| [`docs/agents/architecture.md`](docs/agents/architecture.md) | Crate map, layering, request flow |
| [`docs/agents/agent-runtime.md`](docs/agents/agent-runtime.md) | Turns, tools, sessions, subagents, plan, permissions |
| [`docs/agents/acp.md`](docs/agents/acp.md) | ACP transports, methods, extensions, reverse-RPC, meta keys, leader |
| [`docs/agents/sessions.md`](docs/agents/sessions.md) | Session dirs, persistence, resume/fork/rewind, compaction host, idle flush |
| [`docs/agents/subagents.md`](docs/agents/subagents.md) | Spawn, coordinator, depth, worktrees, resume, usage, orphans |
| [`docs/agents/editing.md`](docs/agents/editing.md) | File edits, hunks, plan-mode edits, Code Mode nested edits |
| [`docs/agents/code-mode.md`](docs/agents/code-mode.md) | Code Mode / Only, V8, exec/wait, nested tools, transport UI |
| [`docs/agents/tools.md`](docs/agents/tools.md) | Tool packs, registry, taxonomy, major tools, caps, how to add a tool |
| [`docs/agents/permissions-and-sandbox.md`](docs/agents/permissions-and-sandbox.md) | Permission pipeline, rules, bash policy, folder trust, OS sandbox |
| [`docs/agents/memory-and-goals.md`](docs/agents/memory-and-goals.md) | Memory storage/search, flush/dream, goals / `update_goal` |
| [`docs/agents/hooks-plugins-skills.md`](docs/agents/hooks-plugins-skills.md) | Hooks, plugins/marketplace, skills session load |
| [`docs/agents/tui-and-config.md`](docs/agents/tui-and-config.md) | Pager, config, slash, hooks, plugins, skills, MCP |
| [`docs/agents/providers.md`](docs/agents/providers.md) | Multi-provider, auth isolation, compaction |
| [`docs/agents/development.md`](docs/agents/development.md) | Build, test, release, contribution workflow |
| [`docs/provider-architecture.md`](docs/provider-architecture.md) | Extension contract (canonical) |
| [`docs/codex-provider-port.md`](docs/codex-provider-port.md) | Codex parity notes |
| [`docs/code-mode-port.md`](docs/code-mode-port.md) | Code Mode parity notes |

---

## 9. Common pitfalls

| Pitfall | Result |
| --- | --- |
| Writing to `~/.grok` | Breaks isolation; wrong install |
| Using xAI `AuthManager` for Codex tokens | Wrong logout/refresh; credential bleed |
| Inferring provider from `gpt-*` / model id | Wrong dialect/tools when catalogs change |
| Treating Code Mode `exec` as JSON function tool | Breaks Sol / Codex contract |
| New JS process per `exec` | Breaks session-persistent Code Mode |
| Skipping `record_agent_write` | Hunks show as external edits |
| Teaching only permission manager about plan mode | Plan gate bypass or double-gating bugs |
| Nested subagents (depth > 1) | Unsupported; task tool stripped at max depth |
| Replaying Codex compaction items to xAI | Opaque history / export boundary violation |
| Editing root workspace `Cargo.toml` only | Lost / inconsistent workspace generation |
| Tests without isolated `OPENGROK_HOME` | Pollutes real user state |

---

## 10. Quick “where do I edit?” 

| Task | Start in |
| --- | --- |
| New slash command | `xai-grok-pager/src/slash/commands/` + register in `mod.rs` |
| New setting | `xai-grok-shared` `UiConfig` (if persisted) → pager `settings/defs.rs` → `dispatch/settings/setters.rs` |
| New tool | `xai-grok-tools/src/implementations/…` + pack registration |
| New permission rule shape | `xai-grok-workspace/src/permission/` |
| New provider | `sampling-types` profile → sampler adapter → shell auth/catalog → tests (see providers doc) |
| Session persistence bug | `xai-grok-shell/src/session/storage/` + `persistence.rs` |
| Scrollback / tool card UI | `xai-grok-pager/src/scrollback/blocks/` |
| Prompt / system instructions | `xai-grok-agent` templates + `prompt/` |
| Auto-update | `xai-grok-update` + `OPEN_GROK_VERSION` |
