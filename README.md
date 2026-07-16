<div align="center">

# Open Grok (`open-grok`)

**Grok Build with ChatGPT Codex optimizations**

[Install](#install) · [Sign in](#sign-in-and-use-both-providers) ·
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
- **Codex automatic compaction.** Codex conversations use OpenAI's
  `/responses/compact` endpoint, replay its opaque replacement history exactly,
  and follow the live model catalog's compaction limit and compatibility hash.
  xAI conversations keep Grok Build's existing compaction path.
- **ChatGPT Codex OAuth.** `open-grok login --codex` uses the Codex OAuth flow and
  stores those credentials separately in `~/.opengrok/codex-auth.json`.
- **xAI and Codex side by side.** xAI sign-in remains available, `/model` can
  switch an existing conversation between provider-backed models by rebuilding
  the correct harness in place, and `/usage` reports xAI billing and Codex quota
  windows independently.
- **Provider-aware hosted search.** xAI models receive xAI web/X search tools;
  Codex models receive OpenAI `web_search`. Open Grok does not pass one
  provider's credentials or provider-only tools to the other.
- **One harness across providers.** Codex keeps the same subagent, scheduler,
  monitor, goal, plan, and user-question features as Grok while using Codex's
  file tools, prompt, transport, and model metadata.

The implementation and compatibility contract are documented in
[`docs/code-mode-port.md`](docs/code-mode-port.md) and
[`docs/codex-provider-port.md`](docs/codex-provider-port.md).

## Install

The initial binary release supports **Apple Silicon macOS only** (`arm64` /
`aarch64`). Linux, Intel macOS, and Windows users should build from source for
now.

```sh
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash
open-grok --version
```

The installer downloads the raw `open-grok-macos-aarch64` artifact and its
`.sha256` file, verifies SHA-256 before replacing anything, and atomically
installs only `open-grok`. It does not create `grok` or `agent` aliases.

By default, the destination is
`${OPENGROK_HOME:-$HOME/.opengrok}/bin`. Override it with
`OPEN_GROK_BIN_DIR`. When piping the installer, export the override first so it
is available to `bash`:

```sh
export OPEN_GROK_BIN_DIR="$HOME/.local/bin"
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash
```

Install a specific version by passing it to the script (with or without a
leading `v`):

```sh
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh \
  | bash -s -- v0.1.220-open-grok.2
```

For local installer testing, `OPEN_GROK_RELEASE_BASE_URL` may point directly to
an asset base URL such as `http://127.0.0.1:8000` or `file:///tmp/release`
containing `open-grok-macos-aarch64` and its `.sha256` file.

Release binaries are stripped and ad-hoc signed, but they are not Apple
Developer ID signed or notarized. macOS may show an unidentified-developer or
Gatekeeper warning. If your security policy requires notarization, build from
source and sign the result with your own identity.

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

## Codex Code Mode

There are two related behaviors:

- **Code Mode** can be enabled in Settings for newly started sessions whose
  Responses-backed model does not declare an explicit tool mode.
- **Code Mode Only** is selected by model metadata and takes precedence over
  the preference. GPT-5.6 Sol always uses this mode.

In Code Mode Only, the model calls the native freeform `exec` tool with raw
JavaScript. Local tools are invoked through `tools.*`, and `wait` resumes or
terminates a yielded JavaScript cell. The runtime persists for the session and
is disposed when the session ends. The TUI and restored transcripts hide the
outer `exec`/`wait` transport—including raw JavaScript and wrapper JSON—and show
only the decoded inner tool calls and their normal structured results. Hosted
search remains available directly through the selected provider. Changing the
setting requires starting a new session.

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
move all user-level Open Grok state, or `OPEN_GROK_BIN_DIR` to change only the
installed command location. Codex OAuth is stored at
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
| `docs/` | Fork compatibility and implementation notes |
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
