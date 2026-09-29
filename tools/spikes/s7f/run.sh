#!/bin/sh
# S7(f) spike launcher: the child-HOME redirect for Hermes (hermes-harness spec §8).
#   tools/spikes/s7f/run.sh <run dir>      (the run dir must be outside the repo)
#
# Isolation, same shape as S1 (tools/spikes/s1/run.sh):
# - / read-only, tmpfs over /tmp, /run and over the REAL $HOME, so the real
#   ~/.claude, ~/.clauth, ~/.codex, ~/.hermes and ~/.config/herdr are invisible;
# - only the run dir is bound read-write; the Hermes 0.19.0 venv and the uv
#   CPython it links to are bound read-only at their own paths (they live under
#   the real $HOME) — the Omarchy shim ~/.local/bin/hermes is NOT bound and is
#   never run;
# - --unshare-net: the stub on 127.0.0.1 is the only reachable endpoint;
# - --clearenv: no ANTHROPIC_*, CLAUDE_*, NOUS_*, OPENROUTER_*, XDG_* from outside.
#
# Phases (each its own bwrap): setup (build the fake outer home and the
# tollgate home layout), main (the redirect scenarios, inotify armed on the
# sentinel), control (HOME = the outer home, no redirect: the sentinel MUST be
# seen, proving the instruments work). inotifywait runs OUTSIDE the sandbox on
# the host path of the bound run dir, so it sees opens made inside it.
set -eu
[ $# -eq 1 ] || { echo "usage: $0 <run dir>" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
RUN=$(cd "$1" && pwd)
case "$RUN" in "$HOME"/Work/*) echo "run dir must be outside the repo" >&2; exit 2;; esac
MISE_INSTALLS="${MISE_DATA_DIR:-$HOME/.local/share/mise}/installs/pipx-hermes-agent"
VENV=$(readlink -f "$MISE_INSTALLS/0.19.0/hermes-agent")
PYHOME=$(dirname "$(dirname "$(readlink -f "$VENV/bin/python")")")
command -v bwrap >/dev/null || { echo "bubblewrap not installed" >&2; exit 2; }
command -v inotifywait >/dev/null || { echo "inotify-tools not installed" >&2; exit 2; }
rm -rf "$RUN/home" "$RUN/out"
mkdir -p "$RUN/home" "$RUN/out"
cp "$HERE/driver.py" "$HERE/stub.py" "$HERE/probe.py" "$HERE/auditwrap.py" "$RUN/"

sandbox() {
  bwrap \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
    --tmpfs "$HOME" \
    --bind "$RUN" "$RUN" \
    --ro-bind "$VENV" "$VENV" \
    --ro-bind "$PYHOME" "$PYHOME" \
    --unshare-all --die-with-parent \
    --clearenv \
    --setenv PATH /usr/bin:/bin \
    --setenv HOME "$RUN/home" \
    --setenv LANG C.UTF-8 \
    --setenv S7F_REAL_HOME "$HOME" \
    --setenv S7F_MISE_INSTALLS "$MISE_INSTALLS" \
    --chdir "$RUN" \
    -- /usr/bin/python3 "$RUN/driver.py" "$@"
}

sandbox setup

listing() {  # outside the sandbox and outside any inotify window
  find "$RUN/home" -path "$RUN/home/.tollgate/profiles/s7f/hermes-home" -prune \
    -o -printf '%y %m %P %l\n' | sort >"$RUN/out/$1"
}

watch() {  # $1 = log name
  inotifywait -m -q --timefmt '%s' --format '%T %e %w%f' \
    -e open -e access -e modify -e attrib -e close_write -e moved_from -e delete_self \
    "$RUN/home/.claude/.credentials.json" "$RUN/home/.claude.json" "$RUN/home/.claude" \
    >"$RUN/out/$1" 2>&1 &
  WATCH=$!
  sleep 1
}

listing outer-before.txt
watch inotify-main.log
sandbox main || echo "main phase exited $?" >&2
sleep 1; kill "$WATCH" 2>/dev/null || true; wait "$WATCH" 2>/dev/null || true
listing outer-after-main.txt

watch inotify-control.log
sandbox control || echo "control phase exited $?" >&2
sleep 1; kill "$WATCH" 2>/dev/null || true; wait "$WATCH" 2>/dev/null || true

echo "== outer home before vs after the redirect scenarios (hermes-home excluded):"
diff "$RUN/out/outer-before.txt" "$RUN/out/outer-after-main.txt" && echo "(identical)"
echo "== inotify during the redirect scenarios (expect nothing):"
cat "$RUN/out/inotify-main.log"
echo "== inotify during the control (expect OPEN/ACCESS on the sentinel):"
cat "$RUN/out/inotify-control.log"
