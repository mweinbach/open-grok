#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
version_file="${repo_root}/OPEN_GROK_VERSION"
dist_dir="${repo_root}/dist"
expected_rg_version="ripgrep 15.0.0"
expected_protoc_version="libprotoc 29.3"

if [[ ! -f "$version_file" ]]; then
    echo "Error: missing $version_file" >&2
    exit 1
fi

version="$(sed -n '1p' "$version_file" | tr -d '\r')"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "Error: invalid Open Grok version '$version' in $version_file" >&2
    exit 1
fi

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "Error: this release builder requires Linux." >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64)
        release_arch="x86_64"
        target_triple="x86_64-unknown-linux-gnu"
        ;;
    aarch64 | arm64)
        release_arch="aarch64"
        target_triple="aarch64-unknown-linux-gnu"
        ;;
    *)
        echo "Error: Linux release builds support only x86_64 and aarch64." >&2
        echo "Detected architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

artifact_name="open-grok-linux-${release_arch}"
artifact_path="${dist_dir}/${artifact_name}"
checksum_path="${artifact_path}.sha256"
release_installer="${dist_dir}/install.sh"
release_license="${dist_dir}/LICENSE"
release_notices="${dist_dir}/THIRD-PARTY-NOTICES"

for command in cargo file git sha256sum strip; do
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

protoc_path="${PROTOC:-}"
if [[ -z "$protoc_path" ]]; then
    protoc_path="$(command -v protoc || true)"
fi
if [[ -z "$protoc_path" || ! -f "$protoc_path" || ! -x "$protoc_path" ]]; then
    echo "Error: protoc is required. Set PROTOC to a verified protoc 29.3 binary." >&2
    exit 1
fi
protoc_path="$(cd "$(dirname "$protoc_path")" && pwd)/$(basename "$protoc_path")"
protoc_version="$($protoc_path --version)"
if [[ "$protoc_version" != "$expected_protoc_version" ]]; then
    echo "Error: release builds require ${expected_protoc_version}." >&2
    echo "Found '${protoc_version}' at $protoc_path" >&2
    exit 1
fi

rg_path="${GROK_TOOLS_BUNDLE_RG_PATH:-}"
if [[ -z "$rg_path" ]]; then
    rg_path="$(command -v rg || true)"
fi
if [[ -z "$rg_path" || ! -f "$rg_path" || ! -x "$rg_path" ]]; then
    echo "Error: a trusted local ripgrep executable is required." >&2
    echo "Set GROK_TOOLS_BUNDLE_RG_PATH to a verified ripgrep 15.0.0 binary." >&2
    exit 1
fi
rg_path="$(cd "$(dirname "$rg_path")" && pwd)/$(basename "$rg_path")"
rg_file_output="$(file "$rg_path")"
case "$release_arch" in
    x86_64)
        if [[ ! "$rg_file_output" =~ ELF\ 64-bit.*x86-64 ]]; then
            echo "Error: the bundled ripgrep executable is not Linux x86_64: $rg_path" >&2
            exit 1
        fi
        ;;
    aarch64)
        if [[ ! "$rg_file_output" =~ ELF\ 64-bit.*(ARM\ aarch64|ARM64) ]]; then
            echo "Error: the bundled ripgrep executable is not Linux aarch64: $rg_path" >&2
            exit 1
        fi
        ;;
esac
rg_version_line="$($rg_path --version | sed -n '1p')"
rg_version="$(printf '%s\n' "$rg_version_line" | awk '{ print $1 " " $2 }')"
if [[ "$rg_version" != "$expected_rg_version" ]]; then
    echo "Error: release builds require ${expected_rg_version}." >&2
    echo "Found '${rg_version_line}' at $rg_path" >&2
    exit 1
fi
rg_checksum="$(sha256sum "$rg_path" | awk '{ print $1 }')"
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

echo "Building Open Grok ${version} (${commit}) for Linux ${release_arch}..." >&2
if [[ "$release_arch" == "aarch64" ]]; then
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=generic -C force-unwind-tables=yes"
fi
GROK_VERSION="$version" \
    GROK_TOOLS_BUNDLE_RG_PATH="$rg_path" \
    PROTOC="$protoc_path" \
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
strip --strip-unneeded "$staged_artifact"

artifact_file_output="$(file "$staged_artifact")"
case "$release_arch" in
    x86_64)
        [[ "$artifact_file_output" =~ ELF\ 64-bit.*x86-64 ]] || {
            echo "Error: release artifact is not a Linux x86_64 ELF binary." >&2
            exit 1
        }
        ;;
    aarch64)
        [[ "$artifact_file_output" =~ ELF\ 64-bit.*(ARM\ aarch64|ARM64) ]] || {
            echo "Error: release artifact is not a Linux aarch64 ELF binary." >&2
            exit 1
        }
        ;;
esac

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

checksum="$(sha256sum "$staged_artifact" | awk '{ print $1 }')"
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
