# Development workflow

Build, test, release, and contribution practices for Open Grok.

Also see: root [`README.md`](../../README.md), [`CONTRIBUTING.md`](../../CONTRIBUTING.md), [`SECURITY.md`](../../SECURITY.md).

## Prerequisites

- Rust pin: `rust-toolchain.toml` (currently **1.94.0** + rustfmt, clippy)
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
3. Default routine validation to **package-scoped `cargo check`** (`cargo check --locked -p xai-grok-shell`). It skips final linking and avoids creating executable churn. Use `cargo build`, Clippy, or tests only when the change requires them.
4. Cargo uses its machine-aware default job count (one job per logical CPU). Do not hard-code a repository-wide `-j` limit; use a temporary override only when diagnosing contention.
5. Features of note on `xai-grok-pager-bin`: `jemalloc`, `sandbox-enforce`; release uses profile **`release-dist`** + feature `release-dist`.

## Formatting and lint

```sh
cargo fmt --all -- --check
cargo clippy --locked -p <crate> --all-targets
# Wider (slow):
cargo clippy --locked --workspace --all-targets
```

Workspace clippy config: `clippy.toml`. Format: `rustfmt.toml`.

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
| Canonical version | `OPEN_GROK_VERSION` (e.g. `1.0.0-open-grok.59`) |
| Embedded version | Build injects `GROK_VERSION` → `xai-grok-version` |
| Public command | `open-grok` only |
| Managed install path | `$OPENGROK_HOME/bin/open-grok` |
| Update source | GitHub `mweinbach/open-grok` releases; SHA-256 verified |
| CLI | `open-grok update --check` / `open-grok update` |
| Disable auto | `[cli] auto_update = false`, `--no-auto-update`, `OPENGROK_DISABLE_AUTOUPDATER=1` |
| Release notes | `docs/releases/` |

The public version follows the upstream Grok Build release line and keeps
Open Grok's monotonic fork serial as SemVer prerelease metadata. For example,
the first fork release based on upstream `1.0.0` is
`1.0.0-open-grok.59`, following `0.1.220-open-grok.58`.

### macOS release build (Apple Silicon)

```sh
# Clean worktree required; ripgrep 15.0.0 arm64 on PATH or GROK_TOOLS_BUNDLE_RG_PATH
./scripts/build-macos-release.sh
```

Produces under `dist/`:

- `open-grok-macos-aarch64` + `.sha256`
- `install.sh`, `LICENSE`, `THIRD-PARTY-NOTICES`

Binaries are stripped and ad-hoc signed, **not** notarized.

### Linux release build (x86_64 / arm64)

```sh
# Clean worktree required; ripgrep 15.0.0 for the host arch on PATH or
# GROK_TOOLS_BUNDLE_RG_PATH
./scripts/build-linux-release.sh
```

Produces under `dist/`:

- `open-grok-linux-x86_64` + `.sha256` (on x86_64 hosts)
- `open-grok-linux-aarch64` + `.sha256` (on arm64 hosts)
- `install.sh`, `LICENSE`, `THIRD-PARTY-NOTICES`

Binaries are stripped ELF executables, **not** signed.

### Windows release build (x86_64)

```powershell
.\scripts\build-windows-release.ps1
```

Produces under `dist/`:

- `open-grok-windows-x86_64.exe` + `.sha256`
- `install.ps1`, `LICENSE`, `THIRD-PARTY-NOTICES`

The builder prefers an explicit `PROTOC` or `protoc.exe` from `PATH`. When
neither is available it downloads the pinned official protoc 29.3 Windows
archive, verifies the archive digest, and caches it under
`target/release-tools/`. Windows artifacts are not currently code signed.

### Full GitHub publication

After tests pass, commit the version/release note and create the matching tag
on that exact commit. Dispatch [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
with the existing tag. It checks out the tagged source independently on
Windows x86_64, Apple Silicon macOS, and Linux x86_64, builds and verifies
all three asset sets, publishes one full GitHub Release, re-downloads the
public bytes, and verifies the tag is Latest.

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
