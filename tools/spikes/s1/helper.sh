#!/bin/sh
# S1 spike apiKeyHelper. Prints the current key from <ctl>/key (switchable
# between turns) and appends one line per invocation to <ctl>/counter:
#   <ts> BEGIN pid=<pid> mode=<m> sleep=<s> readat=<r> key_at_start=<key>
#   <ts> <label> pid=<pid>          (label = key printed, or EXIT1 / EMPTY)
# Control files (all optional; ALL read once at the start of the invocation,
# before the BEGIN line, so a later control change never affects a running
# invocation except through the key file when readat=end):
#   mode    ok | exit1 | empty        (default ok)
#   sleep   seconds to sleep before printing (spanning-switch test)
#   readat  start | end               (print the key read at start, or re-read it after the sleep)
# The ctl dir is baked into the apiKeyHelper command string by the driver, so
# the helper never depends on Claude Code passing its env through.
CTL="${1:?ctl dir}"
ts() { date +%s.%3N; }
MODE=$(cat "$CTL/mode" 2>/dev/null || echo ok)
SLEEP=$(cat "$CTL/sleep" 2>/dev/null || echo 0)
READAT=$(cat "$CTL/readat" 2>/dev/null || echo start)
KEY0=$(cat "$CTL/key" 2>/dev/null)
echo "$(ts) BEGIN pid=$$ mode=$MODE sleep=$SLEEP readat=$READAT key_at_start=$KEY0" >> "$CTL/counter"
[ "$SLEEP" != 0 ] && sleep "$SLEEP"
if [ "$READAT" = end ]; then KEY=$(cat "$CTL/key" 2>/dev/null); else KEY=$KEY0; fi
case "$MODE" in
  exit1) echo "$(ts) EXIT1 pid=$$" >> "$CTL/counter"; echo "helper: simulated failure" >&2; exit 1 ;;
  empty) echo "$(ts) EMPTY pid=$$" >> "$CTL/counter"; exit 0 ;;
esac
printf '%s\n' "$KEY"
echo "$(ts) $KEY pid=$$" >> "$CTL/counter"
