<div align="center">

# Open Grok (`open-grok`)

**Grok Build with ChatGPT Codex optimizations**

[Install](#install) · [Updates](#updates) · [Sign in](#sign-in-and-use-both-providers) ·
[Code Mode](#codex-code-mode) · [Build from source](#build-from-source) ·
[Provenance](#upstream-provenance-and-license)

</div>

Open Grok is a community fork of [Grok Build](https://github.com/xai-org/grok-build),
the terminal coding agent published by SpaceXAI. It retains the Grok Build TUI,
headless agent, tools, and Agent Client Protocol support while adding an
OpenAI Codex provider path and Codex-compatible execution behavior.

The public command is `open-grok`. It can be installed beside the upstream
`grok` command without replacing it.

## What this fork adds

- **Codex Code Mode and Code Mode Only.** Open Grok includes the persistent
  JavaScript `exec`/`wait` runtime used by Codex. Ordinary tools remain available
  through the generated `tools.*` namespace; direct-only interaction and
  multi-agent controls stay top-level.
- **Live Codex model catalog.** When ChatGPT Codex is connected, Open Grok calls
  the same authenticated `/models?client_version=…` endpoint as codex-rs. Live
  model names, context windows, reasoning levels, search support, and tool modes
  override the embedded Sol/Terra/Luna fallback catalog when OpenAI changes it.
- **Codex Max and Ultra parity.** Max and Ultra remain distinct in the TUI while
  both use Codex's `max` wire effort. For models whose live catalog metadata
  declares multi-agent v2, Ultra also enables codex-rs's proactive request
  policy.
- **Current Codex automatic compaction.** By default, Codex conversations use
  Remote Compaction V2 over the normal streaming `/responses` endpoint, retain
  the newest bounded real-user tail, and replay the opaque encrypted compaction
  item exactly. The legacy unary `/responses/compact` protocol remains an
  explicit compatibility option. Both paths follow the live model catalog's
  compaction limit and compatibility hash; xAI keeps Grok Build's compaction.
- **Codex response continuity.** A stable per-session prompt-cache key,
  turn-scoped `x-codex-turn-state`, durable `response.output_item.done` items,
  encrypted reasoning, and catalog-selected reasoning summaries are preserved
  across normal turns, tool loops, retries, compaction, and client rebuilds.
  Full-input HTTP turns deliberately do not send `previous_response_id`,
  matching codex-rs; codex-rs reserves ID-based prefix reuse for its validated
  WebSocket path.
- **ChatGPT Codex OAuth.** `open-grok login --codex` uses the Codex OAuth flow and
  stores those credentials separately in `~/.opengrok/codex-auth.json`.
- **xAI and Codex side by side.** xAI sign-in remains available, `/model` can
  switch an existing conversation between provider-backed models by rebuilding
  the correct harness in place, and `/usage` reports xAI billing and Codex quota
  windows independently.
- **Kimi Platform and Kimi Code.** Settings and `/login kimi` can select either
  Kimi service while keeping their endpoints, credentials, and model catalogs
  isolated. Platform supports live discovery; Code provides the membership
  coding models through its dedicated API.
- **Provider-aware hosted search.** xAI models receive xAI web/X search tools;
  Codex models receive OpenAI `web_search`. Open Grok does not pass one
  provider's credentials or provider-only tools to the other.
- **Optional Kimi web-search fallback.** Settings can enable Perplexity's raw
  Search API for Kimi Platform and Kimi Code while keeping the public
  `web_search(query, allowed_domains?)` schema. The isolated key lives only in
  owner-protected `auth.json`; xAI and Codex continue using native search.
- **One harness across providers.** Codex keeps the same subagent, scheduler,
  monitor, goal, plan, and user-question features as Grok while using Codex's
  file tools, prompt, transport, and model metadata.
- **Verified release updates.** Release builds check the public Open Grok
  GitHub release feed, verify version and SHA-256 before activation, and keep a
  canonical versioned binary under `OPENGROK_HOME`.

The implementation and compatibility contract are documented in
[`docs/code-mode-port.md`](docs/code-mode-port.md) and
[`docs/codex-provider-port.md`](docs/codex-provider-port.md). The extension
contract for adding providers, wire formats, and API-key or OAuth credentials
is in [`docs/provider-architecture.md`](docs/provider-architecture.md).

For AI coding agents and contributors working in this tree, start with
[`AGENTS.md`](AGENTS.md) and the deep dives under
[`docs/agents/`](docs/agents/) (architecture, agent runtime, file edits, TUI
and config, providers, and development workflow).

## Install

The initial binary release supports **Apple Silicon macOS only** (`arm64` /
`aarch64`). Linux, Intel macOS, and Windows users should build from source for
now.

```sh
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash
open-grok --version
```

The installer downloads the raw `open-grok-macos-aarch64` artifact and its
`.sha256` file, verifies SHA-256, runs a version smoke test, and atomically
activates only `open-grok`. The managed command always lives at
`${OPENGROK_HOME:-$HOME/.opengrok}/bin/open-grok`, which keeps manual installs
and in-app updates on the same path. It does not create `grok` or `agent`
aliases.

By default, that managed `bin` directory is also added to `PATH`. Set
`OPEN_GROK_BIN_DIR` to expose a symlink from another absolute directory while
keeping the canonical managed binary under `OPENGROK_HOME`. When piping the
installer, export the override first so it is available to `bash`:

```sh
export OPEN_GROK_BIN_DIR="$HOME/.local/bin"
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash
```

Install a specific version by passing it to the script (with or without a
leading `v`):

```sh
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh \
  | bash -s -- v0.1.220-open-grok.16
```

For local installer testing, `OPEN_GROK_RELEASE_BASE_URL` may point directly to
an asset base URL such as `http://127.0.0.1:8000` or `file:///tmp/release`
containing `open-grok-macos-aarch64` and its `.sha256` file.

Release binaries are stripped and ad-hoc signed, but they are not Apple
Developer ID signed or notarized. macOS may show an unidentified-developer or
Gatekeeper warning. If your security policy requires notarization, build from
source and sign the result with your own identity.

## Updates

Release builds check for a newer full GitHub Release at launch. When automatic
updates are enabled, the TUI downloads and verifies the release in the
background, atomically advances `${OPENGROK_HOME:-$HOME/.opengrok}/bin/open-grok`,
and offers to restart onto it. The running process is never overwritten in
place.

Check or update explicitly:

```sh
open-grok update --check
open-grok update --check --json
open-grok update
```

Automatic updates default on and are configurable in Settings or in
`$OPENGROK_HOME/config.toml`:

```toml
[cli]
auto_update = false
```

For one launch, pass `--no-auto-update`. For managed environments, set
`OPENGROK_DISABLE_AUTOUPDATER=1`; the legacy `GROK_DISABLE_AUTOUPDATER` name is
accepted only for compatibility. Explicit `open-grok update` remains available
when background checks are disabled.

## Sign in and use both providers

Sign in to either provider, or both:

```sh
open-grok login                         # xAI browser sign-in
open-grok login --codex                 # ChatGPT Codex OAuth
open-grok login --codex --device-auth   # headless/remote Codex flow
```

Then launch the TUI and select a model:

```sh
open-grok
```

On first launch, the TUI offers ChatGPT Codex and xAI Grok as separate sign-in
choices. Inside the TUI, use `/model gpt-5.6-sol` for GPT-5.6 Sol, `/model` (or
the model picker) to switch providers, and `/usage` to view both providers'
available usage. The picker labels each model with its provider and live context
window. Bare `login`/`logout` continue to operate on xAI credentials; use
`logout --codex` for Codex or `logout --all` for both.

Codex credentials are isolated from the primary xAI credential store. A
Codex-selected session can run without xAI authentication, and a provider
failure in `/usage` does not hide the other provider's result.

Codex catalog data is cached for five minutes in the separate
`$OPENGROK_HOME/codex_models_cache.json`. The cache is scoped to the Open Grok
version, endpoint, and Codex account identity; it never shares xAI's
`models_cache.json`. Authenticated Codex sessions can use the embedded catalog as
a fallback when the cache or network is unavailable.

Recap and memory work can use independent models from either provider. In
Settings, choose **Recap model** and **Memory model**, or leave them on
**Automatic**: Codex chats use GPT-5.6 Terra at medium reasoning while xAI chats
reuse their active Grok model at low reasoning. Explicit choices can mix
providers—for example, GPT chat with Grok memory—using each provider's isolated
credentials and endpoint. Codex reasoning summaries default to detailed when
the selected model supports them.

## Code Mode

Settings exposes three explicit tool presentations for Responses-backed
models:

- **Direct** exposes ordinary function tools only.
- **Code Mode** adds `exec` and `wait` alongside the ordinary tools.
- **Code Mode Only** keeps ordinary tools behind JavaScript `tools.*`, leaving
  `exec`, `wait`, and direct-only interaction/collaboration controls top-level.

OpenAI Codex model metadata may require Code Mode Only and overrides the user
preference; GPT-5.6 Sol does so. xAI Responses models use the explicit Settings
choice.

Codex carries `exec` as its native freeform custom tool with raw JavaScript;
xAI carries the same runtime control as an ordinary function whose `source`
field contains the JavaScript, because xAI Responses does not accept OpenAI's
native custom-tool type. `wait` resumes or terminates a yielded cell in both
routes. The V8 runtime persists for the active session, resets on timeline or
incompatible provider changes, and is disposed when the session ends. The TUI
and restored transcripts hide the outer `exec`/`wait` transport and show only
decoded nested calls and their normal structured results. Hosted search remains
provider-local. Changing this restart-scoped setting requires restarting Open
Grok; existing persisted sessions retain their resolved mode when resumed.

## Build from source

Requirements:

- Rust, pinned by [`rust-toolchain.toml`](rust-toolchain.toml)
- `protoc`, resolved through [`bin/protoc`](bin/protoc) and its
  [Dotslash](https://dotslash-cli.com) launcher, or provided through `PATH` /
  `PROTOC`
- Xcode Command Line Tools when producing the signed macOS release artifacts

Prepare a checkout and run it without touching an installed release:

```sh
./bin/setup-dev
./bin/open-grok-dev --version
./bin/open-grok-dev
```

For a focused local build:

```sh
cargo build --locked -p xai-grok-pager-bin --bin open-grok
./target/debug/open-grok --version
```

On Apple Silicon macOS, build the complete release asset set with:

```sh
./scripts/build-macos-release.sh
```

The script reads [`OPEN_GROK_VERSION`](OPEN_GROK_VERSION), injects it through
`GROK_VERSION`, builds the hardened `release-dist` profile, strips and ad-hoc
signs the binary, verifies its version and source commit, and writes these
ignored local artifacts. Release builds require a clean worktree and embed a
trusted local arm64 `rg` selected through `GROK_TOOLS_BUNDLE_RG_PATH` or `PATH`.
The required version is ripgrep 15.0.0, matching the embedded-tool metadata.

```text
dist/open-grok-macos-aarch64
dist/open-grok-macos-aarch64.sha256
dist/install.sh
dist/LICENSE
dist/THIRD-PARTY-NOTICES
```

## Compatibility with Grok Build

Open Grok uses `~/.opengrok` for user-level runtime state and project-local
`.opengrok` directories for repository configuration. Set `OPENGROK_HOME` to
move all user-level Open Grok state, or `OPEN_GROK_BIN_DIR` to add a
PATH-facing symlink to the canonical managed command. Codex OAuth is stored at
`~/.opengrok/codex-auth.json` by default.

The fork does not fall back to upstream Grok Build state. Credentials, settings,
sessions, caches, skills, plugins, and project configuration are isolated from
an upstream installation.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/codegen/xai-grok-pager-bin` | Composition-root package for `open-grok` |
| `crates/codegen/xai-grok-pager` | TUI, commands, settings, and rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime, sessions, provider routing, and headless modes |
| `crates/codegen/xai-grok-code-mode*` | Codex-compatible Code Mode protocol and runtime |
| `crates/codegen/xai-grok-tools` | Terminal, file, search, and other tool implementations |
| `AGENTS.md` | Instructions for coding agents working in this repo |
| `docs/` | Fork compatibility notes, release notes, and agent developer docs |
| `docs/agents/` | Architecture, runtime, editing, TUI/config, providers, development |
| `scripts/` | Release packaging helpers |

> [!IMPORTANT]
> The root `Cargo.toml` is generated. Treat it as read-only and make dependency
> changes in the per-crate manifests.

## Upstream provenance and license

Open Grok is maintained at
[`mweinbach/open-grok`](https://github.com/mweinbach/open-grok). It is derived
from the Grok Build local source snapshot
`c1b5909ec707c069f1d21a93917af044e71da0d7` dated 2026-07-15. That exact
snapshot is the fork baseline because the upstream repository currently has
replacement, unrelated history. Codex behavior is ported from a pinned revision of
[`openai/codex`](https://github.com/openai/codex). The pinned commit and
deliberate compatibility differences are recorded in the documents linked above.

This community fork is not affiliated with, sponsored by, or endorsed by xAI,
SpaceXAI, or OpenAI. Grok, ChatGPT, Codex, and related marks belong to their
respective owners.

First-party source is licensed under the **Apache License, Version 2.0**; see
[`LICENSE`](LICENSE). Ported and vendored code retains its original license and
attribution. See [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES),
[`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md),
and [`third_party/NOTICE`](third_party/NOTICE).

Every binary GitHub Release must upload `LICENSE` and `THIRD-PARTY-NOTICES`
beside the core binary, checksum, and installer assets. Publish the release as a
full GitHub Release, not a GitHub prerelease, so the documented
`/releases/latest/download` installer URL resolves.
