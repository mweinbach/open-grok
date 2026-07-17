# Open Grok

Open Grok is Grok Build with ChatGPT Codex optimizations. It keeps its command,
configuration, sessions, and credentials isolated from an upstream Grok install.

**[Repository](https://github.com/mweinbach/open-grok)**

## Install

Install the signed macOS release:

```bash
curl -fsSL https://github.com/mweinbach/open-grok/releases/latest/download/install.sh | bash
```

The installer exposes only the `open-grok` command and places runtime state in
`${OPENGROK_HOME:-$HOME/.opengrok}`. It does not create or replace `grok` or
`agent` commands.

## npm Packaging Status

This directory contains the fork-owned `@mweinbach/open-grok` packaging for a
future npm distribution. The `v0.1.220-open-grok.9` release does **not** publish
npm packages; use the GitHub release installer above.

## Get Started

```bash
# Launch the interactive TUI
open-grok

# Run a single task
open-grok -p "Explain this codebase"
```

On first launch, choose xAI or OpenAI Codex sign-in. The provider credentials
are stored separately under the Open Grok home. `XAI_API_KEY` remains available
for xAI API-key authentication.

## Update

Re-run the GitHub release installer to update.

## Supported Platforms

The GitHub release currently provides an Apple Silicon macOS binary. The source
npm package metadata supports macOS, Linux, and Windows for a future npm
release.

## Feedback

Run `/feedback` inside Open Grok to report an issue.
