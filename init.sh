#!/bin/bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

echo "=== Local Agent Harness Verification ==="

echo "=== ./script/format --check ==="
./script/format --check

echo "=== cargo test -p local_agent_runtime ==="
cargo test -p local_agent_runtime

echo "=== cargo test -p warp_local_agent ==="
cargo test -p warp_local_agent

if [[ "$(uname -s)" == "Darwin" ]] && ! xcrun metal --version >/dev/null 2>&1; then
    echo "ERROR: The Xcode Metal Toolchain is required for Warp library tests." >&2
    echo "Install it with: xcodebuild -downloadComponent MetalToolchain" >&2
    echo "Then re-run ./init.sh." >&2
    exit 1
fi

echo "=== cargo test -p warp -- convert_from agent_viz local_agent ==="
cargo test -p warp --lib -- convert_from agent_viz local_agent --skip local_agent_task_sync_model

echo "=== Verification Complete ==="
echo ""
echo "Next steps:"
echo "1. Read feature_list.json and locate roadmap.active_feature"
echo "2. Read progress.md and session-handoff.md"
echo "3. Work on exactly one in-progress feature"
echo "4. Re-run ./init.sh before claiming done"
