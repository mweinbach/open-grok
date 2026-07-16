#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
version_file="${repo_root}/OPEN_GROK_VERSION"
dist_dir="${repo_root}/dist"
artifact_name="open-grok-macos-aarch64"
target_triple="aarch64-apple-darwin"
expected_rg_version="ripgrep 15.0.0"
artifact_path="${dist_dir}/${artifact_name}"
checksum_path="${artifact_path}.sha256"
release_installer="${dist_dir}/install.sh"
release_license="${dist_dir}/LICENSE"
release_notices="${dist_dir}/THIRD-PARTY-NOTICES"

if [[ ! -f "$version_file" ]]; then
    echo "Error: missing $version_file" >&2
    exit 1
fi

version="$(sed -n '1p' "$version_file" | tr -d '\r')"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "Error: invalid Open Grok version '$version' in $version_file" >&2
    exit 1
fi

if [[ "$(uname -s)" != "Darwin" ]] ||
    [[ "$(uname -m)" != "arm64" && "$(uname -m)" != "aarch64" ]]; then
    echo "Error: this release builder requires Apple Silicon macOS." >&2
    exit 1
fi

for command in cargo git xcrun codesign shasum file; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Error: required command not found: $command" >&2
        exit 1
    fi
done

if [[ -n "$(git -C "$repo_root" status --porcelain --untracked-files=normal)" ]]; then
    echo "Error: release builds require a clean git worktree." >&2
    echo "Commit or remove all tracked and untracked changes, then retry." >&2
    exit 1
fi
commit="$(git -C "$repo_root" rev-parse --short HEAD)"

rg_path="${GROK_TOOLS_BUNDLE_RG_PATH:-}"
if [[ -z "$rg_path" ]]; then
    rg_path="$(command -v rg || true)"
fi
if [[ -z "$rg_path" || ! -f "$rg_path" || ! -x "$rg_path" ]]; then
    echo "Error: a trusted local ripgrep executable is required." >&2
    echo "Install rg or set GROK_TOOLS_BUNDLE_RG_PATH to a verified arm64 binary." >&2
    exit 1
fi
rg_path="$(cd "$(dirname "$rg_path")" && pwd)/$(basename "$rg_path")"
if ! file "$rg_path" | grep -q 'arm64'; then
    echo "Error: the bundled ripgrep executable is not arm64: $rg_path" >&2
    exit 1
fi
rg_version_line="$("$rg_path" --version | sed -n '1p')"
rg_version="$(printf '%s\n' "$rg_version_line" | awk '{ print $1 " " $2 }')"
if [[ "$rg_version" != "$expected_rg_version" ]]; then
    echo "Error: release builds require ${expected_rg_version}." >&2
    echo "Found '${rg_version_line}' at $rg_path" >&2
    echo "Set GROK_TOOLS_BUNDLE_RG_PATH to a verified ripgrep 15.0.0 arm64 binary." >&2
    exit 1
fi
rg_checksum="$(shasum -a 256 "$rg_path" | awk '{ print $1 }')"
echo "Bundling trusted local ${rg_version} (${rg_checksum}) from ${rg_path}" >&2

mkdir -p "$dist_dir"
staged_artifact="${dist_dir}/.${artifact_name}.tmp.$$"
staged_checksum="${dist_dir}/.${artifact_name}.sha256.tmp.$$"
staged_installer="${dist_dir}/.install.sh.tmp.$$"
staged_license="${dist_dir}/.LICENSE.tmp.$$"
staged_notices="${dist_dir}/.THIRD-PARTY-NOTICES.tmp.$$"
cleanup() {
    rm -f \
        "$staged_artifact" \
        "$staged_checksum" \
        "$staged_installer" \
        "$staged_license" \
        "$staged_notices"
}
trap cleanup EXIT

echo "Refreshing version/commit build metadata..." >&2
cd "$repo_root"
cargo clean \
    --quiet \
    --profile release-dist \
    --target "$target_triple" \
    -p xai-grok-pager-bin \
    -p xai-grok-pager \
    -p xai-grok-tools

echo "Building Open Grok ${version} (${commit})..." >&2
GROK_VERSION="$version" \
    GROK_TOOLS_BUNDLE_RG_PATH="$rg_path" \
    CARGO_INCREMENTAL=0 \
    cargo build \
    --locked \
    --profile release-dist \
    --features release-dist \
    --target "$target_triple" \
    -p xai-grok-pager-bin \
    --bin open-grok

source_binary="${repo_root}/target/${target_triple}/release-dist/open-grok"
if [[ ! -x "$source_binary" ]]; then
    echo "Error: Cargo did not produce $source_binary" >&2
    exit 1
fi

cp "$source_binary" "$staged_artifact"
chmod 0755 "$staged_artifact"
xcrun strip -x -S "$staged_artifact"
codesign --force --sign - --timestamp=none "$staged_artifact"
codesign --verify --strict --verbose=2 "$staged_artifact"

if ! file "$staged_artifact" | grep -q 'arm64'; then
    echo "Error: release artifact is not an arm64 Mach-O binary." >&2
    exit 1
fi

version_output="$($staged_artifact --version)"
if [[ "$version_output" != *"$version"* ]]; then
    echo "Error: release version verification failed." >&2
    echo "Expected output to contain: $version" >&2
    echo "Actual output: $version_output" >&2
    exit 1
fi
if [[ "$version_output" != *"$commit"* ]]; then
    echo "Error: release commit verification failed." >&2
    echo "Expected output to contain: $commit" >&2
    echo "Actual output: $version_output" >&2
    exit 1
fi

checksum="$(shasum -a 256 "$staged_artifact" | awk '{ print $1 }')"
printf '%s  %s\n' "$checksum" "$artifact_name" > "$staged_checksum"
cp "${repo_root}/install.sh" "$staged_installer"
chmod 0755 "$staged_installer"
cp "${repo_root}/LICENSE" "$staged_license"
cp "${repo_root}/THIRD-PARTY-NOTICES" "$staged_notices"

mv -f "$staged_artifact" "$artifact_path"
mv -f "$staged_checksum" "$checksum_path"
mv -f "$staged_installer" "$release_installer"
mv -f "$staged_license" "$release_license"
mv -f "$staged_notices" "$release_notices"
trap - EXIT

echo "Release assets:" >&2
echo "  $artifact_path" >&2
echo "  $checksum_path" >&2
echo "  $release_installer" >&2
echo "  $release_license" >&2
echo "  $release_notices" >&2
