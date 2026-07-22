#!/usr/bin/env bash
#
# run-mock.sh - Run the mock AirTouch 5 web UI for local development.
#
# Builds and launches the `airtouch5-webui-mock` binary against the in-memory
# mock controller (no console/hardware needed), pointing the state directory
# at a throwaway /tmp directory so a dev run never touches your real
# ~/.config/airtouch5-webui state.
#
# Usage:
#   ./run-mock.sh                # serve on 127.0.0.1:3000
#   ./run-mock.sh 0.0.0.0:8080   # serve on a custom bind address
#   PORT overrides via the first arg; extra flags are passed straight through:
#   ./run-mock.sh 127.0.0.1:3000 --automation-tick-secs 0

set -euo pipefail

# Resolve the repository root regardless of where the script is run from.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_root"

# First positional arg (if it looks like a bind address) selects the bind;
# anything else is forwarded to the binary untouched.
bind="0.0.0.0:8111"
if [ "$#" -gt 0 ] && [[ "$1" == *:* ]]; then
  bind="$1"
  shift
fi

# Throwaway config directory under /tmp so dev runs stay isolated from the
# real XDG config. Reused across runs (so presets persist within a session)
# but safe to delete at any time.
config_dir="${AIRTOUCH5_MOCK_CONFIG_DIR:-/tmp/airtouch5-webui-mock}"
mkdir -p "$config_dir"

echo "run-mock: bind=$bind config_dir=$config_dir"

exec cargo run --bin airtouch5-webui-mock -- \
  --bind "$bind" \
  --state-dir "$config_dir" \
  "$@"
