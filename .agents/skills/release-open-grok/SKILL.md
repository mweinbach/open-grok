---
name: release-open-grok
description: Build, publish, and verify an Open Grok macOS arm64, Linux x86_64/arm64, and Windows x86_64 release end to end. Use when the user says push the build, publish the release, update an existing release, bump `OPEN_GROK_VERSION`, upload release assets, verify latest, or smoke-test an installer or managed installation.
---

# Release Open Grok

Treat source push, tag creation, local artifact build, GitHub publication, and installed-binary verification as distinct gates. “The release” means all gates pass.

## Prepare an exact clean source

1. Read `docs/agents/development.md`, the matching `docs/releases/` note,
   `OPEN_GROK_VERSION`, and the platform builders under `scripts/`.
2. Run the focused/full tests required by the changes and commit them before building.
3. Require a clean worktree and record the full/short HEAD. Recheck status after the long link in case concurrent edits appeared.
4. Verify a trusted arm64 `ripgrep 15.0.0`; set `GROK_TOOLS_BUNDLE_RG_PATH` to its explicit path. Never substitute a newer Homebrew `rg`.

## Build and verify local assets

On Apple Silicon macOS, run `./scripts/build-macos-release.sh`. It must produce:

- `dist/open-grok-macos-aarch64`
- `dist/open-grok-macos-aarch64.sha256`
- `dist/install.sh`
- `dist/LICENSE`
- `dist/THIRD-PARTY-NOTICES`

Independently verify arm64 Mach-O type, strict ad-hoc signature, embedded version and commit, SHA-256, and the bundled `rg`. Exercise `dist/install.sh` against `OPEN_GROK_RELEASE_BASE_URL=file://<absolute-dist-dir>` with explicit temporary `OPENGROK_HOME` and `OPEN_GROK_BIN_DIR` paths.

On Linux x86_64 or arm64, run `./scripts/build-linux-release.sh`. It must produce:

- `dist/open-grok-linux-x86_64` + `.sha256` (x86_64 hosts)
- `dist/open-grok-linux-aarch64` + `.sha256` (arm64 hosts)
- `dist/install.sh`
- `dist/LICENSE`
- `dist/THIRD-PARTY-NOTICES`

Independently verify the host-arch ELF type, embedded version and commit,
SHA-256, and
the bundled `rg`. Exercise `dist/install.sh` the same isolated way.

On Windows x86_64, run `.\scripts\build-windows-release.ps1`. It must produce:

- `dist/open-grok-windows-x86_64.exe`
- `dist/open-grok-windows-x86_64.exe.sha256`
- `dist/install.ps1`
- `dist/LICENSE`
- `dist/THIRD-PARTY-NOTICES`

Independently verify the PE executable's embedded version and commit, SHA-256,
PowerShell syntax, and an isolated `install.ps1` smoke. Set
`OPEN_GROK_NO_PATH_UPDATE=1` during automation so the real User `PATH` stays
unchanged.

## Publish exact bytes

1. Push the exact source commit and tag.
2. Check GitHub CLI auth without inherited overrides: `env -u GH_TOKEN -u GITHUB_TOKEN gh auth status`.
3. Dispatch `.github/workflows/release.yml` with the existing version tag. The
   workflow checks out the tag, builds all four supported platforms, smokes the
   macOS and Linux installers, publishes all twelve unique assets, re-downloads
   them, and verifies that the release is Latest.
4. Do not use browser upload as the primary path when local file access is blocked.

If another publisher races this release, compare tag peel, asset size, and digest before replacing anything. Never assume an existing same-version asset was built from the current head.

## Verify public and managed paths

- Re-download all public assets to a fresh directory and compare GitHub/local digests and sizes.
- Run both tag-specific and `/releases/latest/download` installer smokes for
  the platform available locally, in isolated homes.
- Verify each downloaded/installed binary reports the expected version and
  commit and passes platform-appropriate signature/checksum checks.
- Upgrade the managed install only when requested or already part of the release task, then verify `open-grok --version` and updater latest-state behavior.

Report the release URL, tag/commit, artifact digest, tests, public installer result, managed-install result, and any missing attestation.
