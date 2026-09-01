# Clone a repository

`open-grok clone` creates a repository without opening the TUI or signing in
to an AI provider. Git must be available on `PATH`; private repositories use
your existing Git credentials, not Open Grok provider credentials.

```sh
open-grok clone https://github.com/example/project.git
open-grok clone https://github.com/example/project.git work --branch main
open-grok clone https://github.com/example/project.git work --cone src --cone docs
open-grok clone https://github.com/example/project.git work --full-history
```

The destination defaults to the repository basename and must not already
exist. Its parent directory must exist. Clone never replaces an existing
directory or removes a partial destination after an error; inspect it before
retrying. Git's interactive credential prompt is disabled, so configure your
credential helper or SSH authentication first.

## History and sparse checkout

By default, clone fetches only the selected branch at depth 1, without tags,
and requests the `blob:none` partial-clone filter. A server may decline that
filter. `--branch` selects a branch or tag; otherwise Git uses the remote's
default branch. Repeat `--cone` to select repository-relative sparse-checkout
directories. Parent traversal and option-like paths are rejected.

Use `--full-history` to fetch complete history, tags, and all remote branches.
For an existing shallow clone:

```sh
git fetch --deepen=100 origin
git fetch --unshallow origin
git fetch --depth=1 origin other-branch:refs/remotes/origin/other-branch
```

Submodules are not initialized automatically. Git templates and hooks are
disabled for the clone operations; run any project setup explicitly afterward.

## Optional Grove backend

`--grove` selects the optional Grove projected-filesystem backend on macOS or
Linux instead of Git. It requires a separately installed, compatible, running
Grove daemon with cloning enabled in its configuration. Open Grok does not
install or start that daemon and never silently falls back to Git.

```sh
open-grok clone --grove https://github.com/example/project.git work
```

Grove configuration is read from `$XDG_CONFIG_HOME/grove/config.toml` or
`~/.config/grove/config.toml`. Enable `[clone] enabled = true` there, or use
`GROVE_CLONE=1` where supported by your daemon. `GROVE_CONTROL_SOCK` can select
an absolute control-socket path; `GROVE_DATA_DIR` selects its data directory.
The default Git backend does not read this configuration.

The optional backend validates protocol replies, history mode, platform mount
transport, and the final mountpoint. On timeout or an uncertain outcome, it
retains the destination: inspect the daemon and mount state before retrying.
Its wire contract is based on the official 1.0.13 client, but live Grove
mounting has not been verified as part of this migration.
