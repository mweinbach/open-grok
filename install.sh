#!/usr/bin/env bash

set -euo pipefail

readonly REPOSITORY="mweinbach/open-grok"
readonly ARTIFACT_NAME="open-grok-macos-aarch64"

usage() {
    cat >&2 <<'EOF'
Usage: install.sh [VERSION]

Install the latest Open Grok release, or VERSION when supplied. VERSION may
optionally start with "v".

Environment:
  OPEN_GROK_BIN_DIR           Optional PATH-facing symlink directory
  OPENGROK_HOME               Runtime home (default: $HOME/.opengrok)
  OPEN_GROK_RELEASE_BASE_URL  Direct URL containing the release assets
EOF
}

if [[ $# -gt 1 ]]; then
    usage
    exit 2
fi

requested_version="${1:-}"
version="${requested_version#v}"
if [[ -n "$requested_version" ]] &&
    [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "Error: invalid version '$requested_version' (expected X.Y.Z or X.Y.Z-suffix)." >&2
    exit 2
fi

os="$(uname -s)"
arch="$(uname -m)"
if [[ "$os" != "Darwin" ]] || [[ "$arch" != "arm64" && "$arch" != "aarch64" ]]; then
    echo "Error: prebuilt Open Grok releases currently require Apple Silicon macOS." >&2
    echo "Detected: ${os} ${arch}. Build from source on unsupported platforms." >&2
    exit 1
fi

if command -v curl >/dev/null 2>&1; then
    downloader="curl"
elif command -v wget >/dev/null 2>&1; then
    downloader="wget"
else
    echo "Error: curl or wget is required." >&2
    exit 1
fi

download() {
    local url="$1"
    local output="$2"

    if [[ "$downloader" == "curl" ]]; then
        curl -fsSL --retry 3 --retry-delay 1 -o "$output" "$url"
    else
        wget -q -O "$output" "$url"
    fi
}

if [[ -n "${OPEN_GROK_RELEASE_BASE_URL:-}" ]]; then
    release_url="${OPEN_GROK_RELEASE_BASE_URL%/}"
elif [[ -n "$version" ]]; then
    release_url="https://github.com/${REPOSITORY}/releases/download/v${version}"
else
    release_url="https://github.com/${REPOSITORY}/releases/latest/download"
fi

if [[ -z "${OPENGROK_HOME:-}" && -z "${HOME:-}" ]]; then
    echo "Error: HOME or OPENGROK_HOME must be set." >&2
    exit 1
fi
open_grok_home="${OPENGROK_HOME:-$HOME/.opengrok}"
managed_bin_dir="${open_grok_home}/bin"
bin_dir="${OPEN_GROK_BIN_DIR:-$managed_bin_dir}"

for checked_dir in "$managed_bin_dir" "$bin_dir"; do
    case "$checked_dir" in
        /*) ;;
        *)
            echo "Error: the Open Grok bin directory must be an absolute path: $checked_dir" >&2
            exit 1
            ;;
    esac
    if [[ "$checked_dir" == *:* || "$checked_dir" == *$'\n'* || "$checked_dir" == *$'\r'* ]]; then
        echo "Error: the Open Grok bin directory contains unsupported characters." >&2
        exit 1
    fi
done

download_dir="${open_grok_home}/downloads"
mkdir -p "$managed_bin_dir" "$download_dir" "$bin_dir"
managed_bin_dir_resolved="$(cd "$managed_bin_dir" && pwd -P)"
bin_dir_resolved="$(cd "$bin_dir" && pwd -P)"
stage_dir="$(mktemp -d "${download_dir}/.open-grok-install.XXXXXX")"
cleanup() {
    rm -rf "$stage_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

binary_tmp="${stage_dir}/${ARTIFACT_NAME}"
checksum_tmp="${stage_dir}/${ARTIFACT_NAME}.sha256"

echo "Downloading Open Grok ${version:-latest} for Apple Silicon macOS..." >&2
download "${release_url}/${ARTIFACT_NAME}" "$binary_tmp"
download "${release_url}/${ARTIFACT_NAME}.sha256" "$checksum_tmp"

expected_sha="$(awk 'NR == 1 { print $1 }' "$checksum_tmp")"
if [[ ${#expected_sha} -ne 64 || "$expected_sha" == *[!0-9A-Fa-f]* ]]; then
    echo "Error: release checksum is not a valid SHA-256 digest." >&2
    exit 1
fi

if command -v shasum >/dev/null 2>&1; then
    actual_sha="$(shasum -a 256 "$binary_tmp" | awk '{ print $1 }')"
elif command -v sha256sum >/dev/null 2>&1; then
    actual_sha="$(sha256sum "$binary_tmp" | awk '{ print $1 }')"
else
    echo "Error: shasum or sha256sum is required to verify the release." >&2
    exit 1
fi

expected_sha="$(printf '%s' "$expected_sha" | tr '[:upper:]' '[:lower:]')"
actual_sha="$(printf '%s' "$actual_sha" | tr '[:upper:]' '[:lower:]')"
if [[ "$actual_sha" != "$expected_sha" ]]; then
    echo "Error: SHA-256 verification failed; Open Grok was not installed." >&2
    echo "Expected: $expected_sha" >&2
    echo "Actual:   $actual_sha" >&2
    exit 1
fi

chmod 0755 "$binary_tmp"
if ! version_output="$("$binary_tmp" --version 2>&1)"; then
    echo "Error: downloaded Open Grok binary failed its --version smoke test." >&2
    exit 1
fi
reported_version="$(printf '%s\n' "$version_output" | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$/) { print $i; exit } }')"
if [[ ! "$reported_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
    echo "Error: downloaded Open Grok binary did not report a valid version." >&2
    echo "Actual output: $version_output" >&2
    exit 1
fi
if [[ -n "$version" && "$reported_version" != "$version" ]]; then
    echo "Error: downloaded Open Grok binary reported an unexpected version." >&2
    echo "Expected: $version" >&2
    echo "Actual:   $reported_version" >&2
    exit 1
fi

installed_version="$reported_version"

versioned_name="open-grok-${installed_version}-macos-aarch64"
versioned_binary="${download_dir}/${versioned_name}"
if [[ -e "$versioned_binary" || -L "$versioned_binary" ]]; then
    versioned_name="${versioned_name}-reinstall-$$"
    versioned_binary="${download_dir}/${versioned_name}"
fi
mv -f "$binary_tmp" "$versioned_binary"

managed_command="${managed_bin_dir}/open-grok"
if [[ -f "$managed_command" && ! -L "$managed_command" ]]; then
    # Keep the inode of a possibly-running pre-managed installation alive
    # while converting the command into the updater's symlink layout.
    ln "$managed_command" "${download_dir}/.open-grok-previous.$$" 2>/dev/null || {
        echo "Error: could not preserve the currently installed Open Grok binary." >&2
        exit 1
    }
fi
managed_link_tmp="${managed_bin_dir}/.open-grok-link.$$"
rm -f "$managed_link_tmp"
ln -s "../downloads/${versioned_name}" "$managed_link_tmp"
mv -f "$managed_link_tmp" "$managed_command"

if [[ "$bin_dir_resolved" != "$managed_bin_dir_resolved" ]]; then
    exposed_link_tmp="${bin_dir}/.open-grok-link.$$"
    rm -f "$exposed_link_tmp"
    ln -s "$managed_command" "$exposed_link_tmp"
    mv -f "$exposed_link_tmp" "${bin_dir}/open-grok"
fi
echo "Installed Open Grok at ${managed_command}" >&2
if [[ "$bin_dir_resolved" != "$managed_bin_dir_resolved" ]]; then
    echo "Linked ${bin_dir}/open-grok to the managed command." >&2
fi

path_line=""
printf -v quoted_bin_dir '%q' "$bin_dir"
path_line="export PATH=${quoted_bin_dir}:\$PATH"

case ":${PATH:-}:" in
    *":${bin_dir}:"*)
        echo "${bin_dir} is already on PATH." >&2
        ;;
    *)
        profile=""
        shell_path="${SHELL:-}"
        shell_name="${shell_path##*/}"
        if [[ -n "${HOME:-}" ]]; then
            case "$shell_name" in
                zsh)
                    profile="${ZDOTDIR:-$HOME}/.zshrc"
                    ;;
                bash)
                    if [[ "$os" == "Darwin" ]]; then
                        profile="$HOME/.bash_profile"
                    else
                        profile="$HOME/.bashrc"
                    fi
                    ;;
            esac
        fi

        if [[ -n "$profile" ]]; then
            if grep -Fqx "$path_line" "$profile" 2>/dev/null; then
                echo "PATH is already configured in $profile." >&2
            elif { mkdir -p "$(dirname "$profile")" &&
                printf '\n# Open Grok\n%s\n' "$path_line" >> "$profile"; }; then
                echo "Added ${bin_dir} to PATH in $profile." >&2
            else
                echo "Could not update $profile." >&2
                echo "Add this line manually:" >&2
                echo "  $path_line" >&2
            fi
        else
            echo "Add Open Grok to PATH for your shell:" >&2
            echo "  $path_line" >&2
        fi

        echo "Start a new shell, or run this now:" >&2
        echo "  $path_line" >&2
        ;;
esac
