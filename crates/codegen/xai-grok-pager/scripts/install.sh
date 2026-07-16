#!/usr/bin/env bash

set -euo pipefail

# Keep one tracked installer implementation in the repository. Release builds
# copy the root install.sh into dist; this source-tree entry point delegates to
# that same root file so tests and developer workflows exercise the published
# contract.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd "${script_dir}/../../../.." && pwd)"
installer="${repository_root}/install.sh"
if [[ ! -x "$installer" ]]; then
    echo "Error: canonical Open Grok installer not found at $installer" >&2
    exit 1
fi
exec "$installer" "$@"
