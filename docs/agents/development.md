# Development workflow

Build, test, release, and contribution practices for Open Grok.

Also see: root [`README.md`](../../README.md), [`CONTRIBUTING.md`](../../CONTRIBUTING.md), [`SECURITY.md`](../../SECURITY.md).

## Prerequisites

- Rust pin: `rust-toolchain.toml` (currently **1.92.0** + rustfmt, clippy)
- Optional Hawk analysis: `cargo-hawk` **0.1.9** + Rust **1.97.1**
- `protoc` via `./bin/protoc` (Dotslash) or `PROTOC` / `PATH`
- Xcode CLT for signed macOS release artifacts (Apple Silicon release path)

## Setup and run

```sh
./bin/setup-dev
./bin/open-grok-dev --version
./bin/open-grok-dev                 # interactive TUI from source

# Focused build
cargo build --locked -p xai-grok-pager-bin --bin open-grok
./target/debug/open-grok --version
```

`./bin/open-grok-dev` runs the workspace binary without overwriting an installed release under `$OPENGROK_HOME/bin/`.

### Useful modes

```sh
./bin/open-grok-dev -p "hello"              # headless
./bin/open-grok-dev agent stdio             # ACP
./bin/open-grok-dev login --codex           # Codex OAuth
OPENGROK_HOME=/tmp/og-test ./bin/open-grok-dev   # isolated state
```

## Workspace rules

1. **Root `Cargo.toml` is generated / read-only.** Edit per-crate `Cargo.toml` files.
2. Prefer **`--locked`** on cargo commands so `Cargo.lock` stays consistent.
3. Prefer **package-scoped** builds/tests (`-p xai-grok-shell`) over full workspace unless you need cross-crate validation.
4. Features of note on `xai-grok-pager-bin`: `jemalloc`, `sandbox-enforce`; release uses profile **`release-dist`** + feature `release-dist`.

## Formatting and lint

```sh
cargo fmt --all -- --check
cargo clippy --locked -p <crate> --all-targets
# Wider (slow):
cargo clippy --locked --workspace --all-targets
```

Workspace clippy config: `clippy.toml`. Format: `rustfmt.toml`.

### Hawk public-API analysis

[Hawk](https://github.com/astral-sh/hawk) checks whether public Rust APIs are
needed by the shipped `open-grok` binary. It is experimental and tied to the
compiler version used to build it, so its Rust 1.97.1 toolchain is kept
separate from the workspace's pinned Rust 1.92.0 toolchain.

Install the compatible toolchain and Hawk release:

```sh
rustup toolchain install 1.97.1
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/astral-sh/hawk/releases/download/0.1.9/cargo-hawk-installer.sh | sh
```

Run the workspace configuration in warning-only mode:

```sh
./bin/hawk-check
```

Use `./bin/hawk-check -D warnings` to enforce all default Hawk findings,
`./bin/hawk-check --only dead-public` to focus on deletion candidates, or
`./bin/hawk-check --fix` to apply machine-applicable visibility reductions.
Review fixes before committing; dead declarations remain report-only.

## Testing strategy

### Layers

| Layer | Where | Use for |
| --- | --- | --- |
| Unit | `src/**` module tests | Pure logic, parsers, gates |
| Crate integration | `crates/.../tests/` | Auth contracts, routing e2e |
| Sampler | `xai-grok-sampler/tests/` | Wire shapes, adapters |
| Shell session | `xai-grok-shell/src/session/acp_session_tests/` | Turn loop, plan, permissions |
| PTY e2e | `xai-grok-pager/tests/pty_e2e/`, harness crate | Full TUI + mock inference |
| Shared mocks | `xai-grok-test-support/` | Mock inference, SSE helpers, hermetic env |

### Commands

```sh
# Focused
cargo test --locked -p xai-grok-sampler --test test_actor
cargo test --locked -p xai-grok-sampling-types
cargo test --locked -p xai-grok-shell --test codex_auth_contract
cargo test --locked -p xai-grok-shell -- plan_mode
cargo test --locked -p xai-grok-tools -- search_replace
cargo test --locked -p xai-hunk-tracker
cargo test --locked -p xai-grok-code-mode
cargo test --locked -p xai-grok-workspace

# Pager / PTY (heavier)
cargo test --locked -p xai-grok-pager --test pty_e2e_smoke
cargo test --locked -p xai-grok-pager-pty-harness

# Single filter
cargo test --locked -p xai-grok-shell --test codex_auth_contract -- <filter>
```

### Hermetic tests

- Always isolate **`OPENGROK_HOME`** (and `HOME` / `USERPROFILE`) under temp dirs.
- Prefer `xai-grok-test-support` helpers (`EnvVarGuard` patterns, mock servers).
- Never point tests at a developer’s real `~/.opengrok`.

### PTY harness notes

- Stack: PTY → screen (alacritty) → mock content server → YAML scenarios.
- Seed fake OAuth / env via harness helpers (`flows::seed_fake_oauth`, `env_for_pager`).
- Prefer scripted responses / SSE builders for wire assertions.

## How to work on a change

### 1. Orient

- Read [`../../AGENTS.md`](../../AGENTS.md) non-negotiables.
- Open the matching doc under `docs/agents/`.
- Grep for the feature and open nearest existing tests.

### 2. Implement at the right layer

| Kind of change | Layer |
| --- | --- |
| Pixels / keys / modals | pager `dispatch` + `views` + `scrollback` |
| Agent behavior / turns | shell session / agent |
| Tool semantics | `xai-grok-tools` |
| Wire format / retries | sampler + sampling-types |
| Permissions / FS policy | workspace |
| Auth / catalogs | shell auth + `*_models.rs` |
| Provider policy | sampling-types + sampler adapter |

### 3. Verify

- Unit tests for the pure core.
- Integration test if ACP, auth, or multi-crate wiring changes.
- Manual `./bin/open-grok-dev` only when TUI feel matters; still add automated coverage if possible.

### 4. Document

- User-visible product behavior → update `user-guide/` when needed.
- Agent/contributor contracts → update `AGENTS.md` and/or `docs/agents/`.
- Provider / Code Mode parity → update the matching file under `docs/`.

## Release and versioning

| Item | Detail |
| --- | --- |
| Canonical version | `OPEN_GROK_VERSION` (e.g. `0.1.220-open-grok.9`) |
| Embedded version | Build injects `GROK_VERSION` → `xai-grok-version` |
| Public command | `open-grok` only |
| Managed install path | `$OPENGROK_HOME/bin/open-grok` |
| Update source | GitHub `mweinbach/open-grok` releases; SHA-256 verified |
| CLI | `open-grok update --check` / `open-grok update` |
| Disable auto | `[cli] auto_update = false`, `--no-auto-update`, `OPENGROK_DISABLE_AUTOUPDATER=1` |
| Release notes | `docs/releases/` |

### macOS release build (Apple Silicon)

```sh
# Clean worktree required; ripgrep 15.0.0 arm64 on PATH or GROK_TOOLS_BUNDLE_RG_PATH
./scripts/build-macos-release.sh
```

Produces under `dist/`:

- `open-grok-macos-aarch64` + `.sha256`
- `install.sh`, `LICENSE`, `THIRD-PARTY-NOTICES`

Binaries are stripped and ad-hoc signed, **not** notarized.

## Contribution hygiene

From `CONTRIBUTING.md` and fork practice:

- Open an issue for large designs first.
- Keep PRs scoped; describe user-visible behavior.
- Add or update tests.
- Do not commit credentials, generated release artifacts, or unrelated formatting sweeps.
- Security: follow `SECURITY.md` — no public issues for vulnerabilities.
- License: Apache-2.0 for first-party contributions; preserve third-party notices for ported code.

## Upstream relationship

- Remote `upstream` may point at `xai-org/grok-build`; this fork’s public product is **Open Grok**.
- Baseline snapshot and Codex pins are recorded in README / `docs/*-port.md`.
- Upstream-only bugs should be reported upstream; fork-specific behavior stays here.
- Do not reintroduce `~/.grok` fallbacks or shared credential stores with upstream installs.

## Checklist before requesting review

- [ ] Change scoped to the right crate(s)
- [ ] Tests added/updated for the changed path
- [ ] `cargo fmt` / targeted `clippy` / targeted `test` clean
- [ ] No secrets or personal `OPENGROK_HOME` data
- [ ] Docs updated if agent contracts or user-visible behavior changed
- [ ] Provider isolation preserved (if auth/tools/sampling touched)
- [ ] Plan mode / hunk / Code Mode invariants preserved (if tools/edits touched)

## See also

- [architecture.md](architecture.md)
- [agent-runtime.md](agent-runtime.md)
- [editing.md](editing.md)
- [tui-and-config.md](tui-and-config.md)
- [providers.md](providers.md)
