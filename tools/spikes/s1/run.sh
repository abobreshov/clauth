#!/bin/sh
# S1 spike launcher: runs driver.py (stubs + the real claude) inside bubblewrap.
#   ./run.sh [scenario ...]      (no args = all scenarios)
# Isolation: / read-only, tmpfs over /tmp and over the real $HOME (so the real
# ~/.claude, ~/.clauth, ~/.codex, ~/.hermes are invisible), only this spike
# directory bound read-write, the claude install dir bound read-only (it lives
# under $HOME), a private network namespace (the stubs on 127.0.0.1 are the
# only reachable endpoints), and a cleared environment (no ANTHROPIC_*,
# CLAUDE_CODE_OAUTH_TOKEN or CLAUDE_CONFIG_DIR from outside).
set -eu
SPIKE=$(cd "$(dirname "$0")" && pwd)
CLAUDE_BIN=$(readlink -f "$(command -v claude)")
CLAUDE_DIR=$(dirname "$CLAUDE_BIN")
command -v bwrap >/dev/null || { echo "bubblewrap not installed" >&2; exit 2; }
mkdir -p "$SPIKE/runs"
exec bwrap \
  --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
  --tmpfs "$HOME" \
  --bind "$SPIKE" "$SPIKE" \
  --ro-bind "$CLAUDE_DIR" "$CLAUDE_DIR" \
  --unshare-net --die-with-parent \
  --clearenv \
  --setenv PATH /usr/bin:/bin \
  --setenv HOME "$SPIKE/home" \
  --setenv S1_CLAUDE "$CLAUDE_BIN" \
  --chdir "$SPIKE" \
  -- /usr/bin/python3 "$SPIKE/driver.py" "$@"
