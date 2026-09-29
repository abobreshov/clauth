# Real-machine test campaign: tollgate on the owner's workstation

Draft, 2026-09-29, for `feat/tollgate` after lane 4 (`docs/specs/providers-lane4.md`). Run the steps in order;
each step has a go/no-go.

**Accounts under test:** 3 Claude Max logins managed by upstream clauth (`leadtone`, `personal`, `scifoo`), Grok CLI
1.0.44, agy 1.2.13, codex 0.157.1 (`~/.codex/auth.json` a regular file), and keys for OpenRouter, Nous, OpenAI and Google AI.

**Roles**
- **OWNER** (his own terminal): every step that types a secret, first sends a real credential through a tollgate path,
  answers a confirmation, backs up / imports / rolls back, or needs someone watching for a browser.
- **AGENT**: read-only steps only: tollgate caches and outputs (`usage`, API, MCP), metadata snapshots
  (`stat`/`find -printf`), and evidence files the owner wrote.

**Hard rules:** the agent never opens `~/.claude`, `~/.clauth`, `~/.codex/auth.json`, `~/.grok/auth.json`, `~/.hermes`,
`~/.tollgate/secrets.env` or the keyring. No secret goes in argv (`--api-key`) or chat: use the echo-off prompts. No `git push`.

**Evidence:** `E=~/tollgate-campaign/$(date +%F)` (`mkdir -m 700 -p "$E"`), files `T<n>-<what>.txt`; the owner reviews
every capture file before the agent reads it. Agent snapshot helper (metadata only):
```sh
snap() { find ~/.clauth ~/.claude/.credentials.json ~/.claude/settings.json ~/.claude.json ~/.codex/auth.json \
    ~/.grok/auth.json ~/.grok/auth.json.lock ~/.hermes ~/.tollgate ~/.config/herdr/config.toml -maxdepth 3 \
    -printf '%p\t%y\t%i\t%n\t%m\t%s\t%T@\t%l\n' 2>/dev/null | sort > "$1"; }
```
**Upstream noise** (expected in diffs; upstream's daemon writes them): `~/.clauth/{status.json,status_cache.json,clauth.log,usage-fetch.lock}`,
`~/.clauth/profiles/*/usage_{cache.json,history.jsonl}`, `~/.clauth/conversations/`.

**Build prerequisites per step** (a step whose lane has not merged is recorded **BLOCKED**, not skipped silently)

| Step | Needs merged |
|---|---|
| T1, T2 | lane 4, slices 1 and 2 |
| T3, T4 | import R3a–R3d (`docs/specs/import-clauth.md`) |
| T5 | P6a, plus S1 PASS for the installed Claude Code (2.1.283 today) |
| T6 | H-1a and H-1b (Hermes harness) |
| T7 | herdr H1/H2 (shipped), plus T4 done (guest mode off) |

## T0 Install the new build (OWNER; AGENT pre-checks)
1. **AGENT (hermetic):** `cd ~/Work/clauth && git status --short && cargo nextest run --workspace && cargo clippy --all-targets --all-features -- -D warnings`. Expected: green, no untracked credential-shaped files.
2. **OWNER:** `cargo install --path ~/Work/clauth --locked --force && tollgate --version | tee "$E/T0-version.txt"`.
3. **AGENT:** `snap "$E/T0-snap.tsv"`. This is the baseline for every later diff.

**Abort** if tests are red or the version does not match the branch HEAD. Rollback: reinstall the previous commit the same way.

## T1 Read-only checks on the real HOME (guest mode on)
1. **AGENT:** record what upstream exposes.
   ```sh
   tollgate usage --plain | tee "$E/T1-usage.txt"
   tollgate usage --json | jq '{guest_mode, ids:[.accounts[].id]}' | tee "$E/T1-ids.json"
   ```
   Expected: `guest_mode: true`, with `upstream:leadtone`, `upstream:personal` and `upstream:scifoo` labelled `(clauth)`, and figures matching what the OWNER sees in upstream clauth's own TUI.
2. **AGENT:** the API on the unix socket.
   ```sh
   pgrep -f 'tollgate daemon' || (tollgate api serve & echo $! > "$E/api.pid"; sleep 1)
   curl -s --unix-socket ~/.tollgate/api.sock http://t/v1/health | tee "$E/T1-health.json"
   curl -s --unix-socket ~/.tollgate/api.sock http://t/v1/usage | jq '[.accounts[].id]' > "$E/T1-api-ids.json"
   ```
   Expected: `guest_mode: true`, and the same ids as step 1. Afterwards, `kill $(cat "$E/api.pid")` if the agent started the server.
3. **AGENT:** MCP, over stdio.
   ```sh
   printf '%s\n' \
     '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
     '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
     '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"usage","arguments":{}}}' \
     | timeout 10 tollgate mcp | tee "$E/T1-mcp.jsonl"
   ```
   Expected: the three upstream accounts appear, and no error.
4. **OWNER:** refresh each CLI's token by using it: open `grok` and quit; open `agy` and quit (codex is already in use). Then run `tollgate monitor detect --explain | tee "$E/T1-detect.txt"`.
   Expected: proposals `grok`, `agy`, `codex-native`; `nous: skipped: Hermes has no Nous login`; shapes only
   (`expires_at: rfc3339`, `agy blob: nested`), never a value.
5. **OWNER:** add the three native monitors and refresh them with capture.
   ```sh
   tollgate monitor add grok; tollgate monitor add agy; tollgate monitor add codex-native
   tollgate monitor refresh --capture "$E/cap1"
   ```
   This is the first time a borrowed token is sent. The owner opens the shape files to review them before the agent reads them.
6. **AGENT:**
   ```sh
   tollgate monitor list | tee "$E/T1-monitors.txt"
   for p in grok antigravity codex; do tollgate usage --provider $p --plain; done
   snap "$E/T1-snap.tsv"; diff "$E/T0-snap.tsv" "$E/T1-snap.tsv"
   ```
   Expected: **grok** one shared weekly or monthly window and **no money**; **agy** Gemini and 3p pools, 5h and 7d;
   **codex-native** 5h and 7d with plan.
   - **Snapshot diff:** only `~/.tollgate/*` and the allowlist changed. `~/.grok/auth.json.lock` and `~/.codex/auth.json` keep the same inode and mtime, unless the CLI itself rotated them (compare against the CLI's own last use).
   - `grep -c kick ~/.tollgate/tollgate.log` gives 0.
7. **OWNER (gates):**
   - **GROK-UNITS:** compare `$E/cap1/grok-*.shape.json` `prepaidBalance.val` against the balance on grok.com. Note the unit (cents or dollars) in `$E/T1-gates.txt`.
   - **AGY-CLI** (optional, supervised). With the desktop in view, run `agy -p "/help" --output-format json > "$E/agy-help.json"`, then `agy -p "/usage" --output-format json > "$E/agy-usage.json"`. Note whether any browser or sign-in appeared. Review the files before sharing them.

**Abort** if any of these happens: a browser opens; grok, agy or codex asks for a login after a tollgate refresh; a CLI store changes because of tollgate; any secret-looking string appears in any output. Rollback: `tollgate monitor remove <id>`, then re-login the affected CLI, and file the defect against lane 4.

## T2 Secrets and API-key monitors
1. **OWNER:** store each key at the echo-off prompt, one per line. Paste the key only at the prompt.
   ```sh
   for n in OPENROUTER_API_KEY NOUS_API_KEY OPENAI_API_KEY GEMINI_API_KEY; do tollgate secret set $n; done
   ```
   Optional: `OPENAI_ADMIN_KEY` (costs), `OPENROUTER_MGMT_KEY` (wallet). Each prints `stored <NAME> (<len> chars, <prefix>…)`.
2. **AGENT:**
   `stat -c '%a %U %h' ~/.tollgate/secrets.env; tollgate secret list | tee "$E/T2-secrets.txt"`. Expected: `600 abobreshov 1`,
   names only. The agent never opens the file.
3. **OWNER:** add the key monitors, start the daemon, and refresh with capture.
   ```sh
   tollgate monitor add openrouter [--billing-key-env OPENROUTER_MGMT_KEY]
   tollgate monitor add nous-key --probe
   tollgate monitor add openai [--admin-key-env OPENAI_ADMIN_KEY]
   tollgate monitor add google-ai
   tollgate daemon --replace &    # in a spare pane
   tollgate monitor refresh --capture "$E/cap2"
   ```
4. **AGENT:** `tollgate usage --json | jq '.accounts[]|select(.origin=="monitor")|{id,key_health,note,windows:[.windows[].id],money:[.money[].meter_id],failure}' | tee "$E/T2-keys.json"`.
   Expected:

   | Monitor | Expected |
   |---|---|
   | openrouter | key spend and limit (wallet when the management key is set) |
   | nous-key | `valid`, plus credit meters if headers came back (gate **NOUS-SK-PORTAL**, headers half) |
   | openai | `valid`; rpm/tpm windows only if headers came back (gate **OPENAI-HEADERS**); `spend.monthly` with the admin key |
   | google-ai | `valid`, plus the note "spend and quota not available for API keys…" |

   Record the gates in `$E/T2-gates.txt`. If the owner has a project key, also record **OPENAI-COSTS-403**: set `--admin-key-env` to it once, then remove it.
5. **AGENT (leak checks, names only):**
   ```sh
   for p in $(pgrep -f 'tollgate daemon'); do tr '\0' '\n' </proc/$p/environ | cut -d= -f1; done \
     | grep -cxE 'OPENROUTER_API_KEY|NOUS_API_KEY|OPENAI_API_KEY|OPENAI_ADMIN_KEY|GEMINI_API_KEY|OPENROUTER_MGMT_KEY'
   grep -cE 'sk-or-v1-|sk-proj-|sk-admin-|AIza' ~/.tollgate/tollgate.log ~/.tollgate/monitors/*.json
   ```
   Expected: 0 names, unless the owner exported one in his shell on purpose, and 0 key prefixes. Repeat the first check inside a `tollgate start` session using `env | cut -d= -f1`. Expected 0 there too.
6. **OWNER (drill):**
   `tollgate secret rm GEMINI_API_KEY` (within 10 s `monitor list` shows `$GEMINI_API_KEY MISSING`), then
   `tollgate secret set GEMINI_API_KEY` (recovers within 10 s).

**Abort** if a key value or a key prefix shows up anywhere. Rollback: `tollgate secret rm <NAME>`, then **revoke that key at the provider**.

## T3 `tollgate import clauth --dry-run`: proven read-only, then reviewed
1. **AGENT (sandbox proof).**
   ```sh
   cargo nextest run -E 'test(/^import_/)'
   H=$(mktemp -d)
   ```
   Build a fake upstream tree in `$H` with dummy bytes and the real layout (`~/.clauth/profiles/{leadtone,personal,scifoo}/…`, `profiles.toml`, and a `.claude/.credentials.json` symlink). Then:
   ```sh
   (export HOME=$H; snap "$E/T3-sbx-pre.tsv"
    bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp tollgate import clauth --dry-run --json > "$E/T3-sbx.json"; echo "exit=$?"
    snap "$E/T3-sbx-post.tsv")
   diff "$E/T3-sbx-pre.tsv" "$E/T3-sbx-post.tsv"
   ```
   Expected: exit 0 or 3, no `Read-only file system` error, and an empty diff.
   Also rehearse the rollback here: `HOME=$H tollgate import clauth --yes && HOME=$H tollgate import rollback --yes`. After rollback, the snapshot must equal the pre-import one, apart from the journal and backup entries the spec lists.
2. **AGENT:** `snap "$E/T3-real-pre.tsv"`.
3. **OWNER (real HOME, read-only mount):**
   ```sh
   bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
     tollgate import clauth --dry-run 2>&1 | tee "$E/T3-dryrun.txt"; echo "exit=${PIPESTATUS[0]}" >> "$E/T3-dryrun.txt"
   ```
   Then repeat with `--json > "$E/T3-dryrun.json"`.
4. **AGENT:** `snap "$E/T3-real-post.tsv"; diff "$E/T3-real-pre.tsv" "$E/T3-real-post.tsv"`. Expected: only allowlisted upstream noise changed. Then `jq '{roster, slots, blockers, edits}' "$E/T3-dryrun.json"`.
   Expected: 3 claude and 0 codex profiles; the live slot as a symlink to one profile or a regular file;
   `~/.codex/auth.json` `untouched`; edits G1–G4 listed; blockers only running `claude`/`clauth` processes.
5. **OWNER:** review the report and write `GO` or `NO-GO <reason>` to `$E/T3-decision.txt`.

**Abort** on any `Read-only file system` line, any non-noise diff, a `refuse` entry the owner does not understand, or an account missing from the roster.

## T4 The real import (OWNER only, with explicit GO; the agent's session must be closed)
**Preconditions:** T3 `GO`; a 30-minute window; every Claude Code session closed, the agent's and the herdr claude
panes included (`pgrep -a -x claude; pgrep -a clauth` both empty).

1. **OWNER (backup, 0700, dated; the agent never reads it):**
   ```sh
   B=~/tollgate-backup/$(date +%Y%m%d-%H%M%S); mkdir -m 700 -p "$B"
   cp -a ~/.clauth "$B/clauth"
   cp -P ~/.claude/.credentials.json "$B/credentials.json.link"
   cp -L ~/.claude/.credentials.json "$B/credentials.json.data"
   cp -a ~/.claude/settings.json ~/.claude.json "$B/"
   cp -a ~/.codex/auth.json "$B/codex-auth.json"
   chmod -R go-rwx "$B"
   (cd "$B" && find . -type f -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
   ls -la "$B" > "$E/T4-backup-ls.txt"
   ```
2. **OWNER:** `tollgate import clauth 2>&1 | tee "$E/T4-import.txt"`. Answer `y` only if the listed edits match T3.
   Expected: `imported 3 claude and 0 codex profiles from ~/.clauth; guest mode is off. Next: tollgate import retire`.
3. **OWNER:** open a new terminal and run `tollgate start personal -p "say ok"`. Repeat for `leadtone` and `scifoo`; each should print `ok`. Then check that a plain `claude -p "say ok"` works, and resume the agent session.
4. **AGENT (post-import checks):**
   ```sh
   tollgate usage --json | jq '{guest_mode, ids:[.accounts[].id]}'   # guest_mode false; claude:{leadtone,personal,scifoo}; no upstream:*
   jq -r .state ~/.tollgate/import-journal.json                     # complete
   readlink ~/.claude/.credentials.json                             # into ~/.tollgate/profiles/<active>/
   pgrep -a clauth                                                  # empty
   tollgate list
   ```
   Also `stat ~/.codex/auth.json`: its inode must equal the T0 value, since the import leaves an independent login alone.
5. **OWNER:** switch accounts: `tollgate scifoo`, then `claude -p "say ok"`; `tollgate personal`, then `claude -p "say ok"`. The AGENT checks `readlink` after each switch.
6. **OWNER:** `tollgate import retire` (R1 CC plugin, R2 `tollgate@tollgate`, R3 herdr, R4 completions). The owner then runs `jq '.enabledPlugins' ~/.claude/settings.json` himself and pastes only that key's output into the evidence.
7. **Soak for 24 h.** The AGENT runs `tollgate usage` the next day: all three accounts are fresh, and `tollgate.log` has no refresh errors.

**Rollback triggers** (any one): an account fails to authenticate twice; `guest_mode` still true after commit; the journal is not `complete`; an account is missing; upstream re-adopts the live slot; Claude Code will not start.

**Rollback drill** (already rehearsed in T3.1): close all claude processes, then run `tollgate import rollback 2>&1 | tee "$E/T4-rollback.txt"`.
Expected: `clauth list` shows 3 accounts again, and `tollgate usage --json` is back to `guest_mode: true`.

**Last resort (owner):** `sha256sum -c "$B/SHA256SUMS"`, then restore `~/.clauth` and each file from `$B` with `cp -a`. For the live slot, restore `credentials.json.link` if the slot was a symlink, else `.data`.

## T5 Same-provider hot swap: two OpenRouter keys, one account
1. **OWNER:** create two keys on openrouter.ai, each with a $1 limit. Add them as two profiles with the key read echo-off (never `--api-key`):
   `for p in or-a or-b; do tollgate login $p --base-url https://openrouter.ai/api; done`
2. **OWNER:** in a pane, run `tollgate start or-a` and send `say A`.
3. **AGENT:** `tollgate sessions --json | jq '.[]|{sid,profile,executor,served,committed}'`. Expected: executor `b`, served `or-a`.
4. **OWNER:** `tollgate switch <sid> or-b --wait` (exit 0 within 65 s), then send `say B` in the same session.
   **AGENT:** confirms `served: or-b`. After `tollgate monitor refresh`, key B's usage should grow and key A's should not; the owner cross-checks on openrouter.ai/activity. This is also the owner half of S1(g), the real endpoint accepting the header pair.
5. **Mid-stream:** ask for a long answer and switch back to `or-a` while it streams. The answer should complete; the next turn goes on `or-a`.
6. **Negative case:** `tollgate switch <sid> personal` is refused as a different class, naming `--relaunch`. Then `tollgate switch <sid> personal --relaunch --yes` resumes the same conversation on `personal`.

**Abort** on two consecutive 401 turns: switch back, or `/exit` and restart. Afterwards delete both test keys at OpenRouter.

## T6 Hermes profile with an OpenRouter or Nous key (BLOCKED until H-1a/H-1b)
1. **AGENT:** `snap "$E/T6-pre.tsv"; readlink ~/.claude/.credentials.json > "$E/T6-slot-pre.txt"`.
2. **OWNER:** create the profile, add the key at the echo-off prompt, and launch it.
   `tollgate hermes new or-hermes; tollgate hermes auth or-hermes; tollgate start or-hermes`, send `say ok`, and from
   another terminal record env names only:
   `tr '\0' '\n' </proc/$(pgrep -n -f hermes)/environ | cut -d= -f1 | sort > "$E/T6-env-names.txt"`. Then `/exit`.
3. **AGENT:** `snap "$E/T6-post.tsv"` and diff it against the pre snapshot.
   Expected: `~/.hermes` unchanged; the live slot's `readlink` and inode unchanged (the anthropic hazard); the home under
   `~/.local/share/tollgate/hermes/or-hermes`; env names include `HERMES_HOME` and no `ANTHROPIC_*`,
   `CLAUDE_CODE_OAUTH_TOKEN` or T2 stored name.
4. **OWNER:** `tollgate start or-hermes -- --provider anthropic` must be refused.

**Abort** if `~/.hermes` or the live slot changed. Rollback: remove the Hermes home with `tollgate hermes` removal per the H-1b spec, and restore the slot with `tollgate personal`.

## T7 herdr: plugin, pane tags, `tollgate.usage`
1. **OWNER:** `cp -a ~/.config/herdr/config.toml "$E/T7-herdr-config.pre.toml"`. Then `tollgate herdr install` (skip this if retire R3 already did it), and answer herdr's own preview.
2. **OWNER:** open one pane each: `tollgate start personal`, plain `claude`, `codex`, `grok`, `agy`, and the T6 Hermes profile if T6 ran.
3. **AGENT:**
   ```sh
   for a in grok agy codex; do tollgate herdr tag --agent $a; done
   tollgate herdr tag --agent claude personal
   ```
   Expected: each prints `<label> <lead metric>[ ⏸][ ⚠|‼]` and a severity line. The agy pane shows `⏸` once its token has gone stale.
4. **OWNER:** take a screenshot of the sidebar tags into `$E/T7-sidebar.png`, then press the dashboard key: the TUI should open on the Usage tab (`tollgate.usage`).

**Abort** if herdr rejects the config or panes lose their tags. Rollback: `tollgate herdr uninstall`, then `cp "$E/T7-herdr-config.pre.toml" ~/.config/herdr/config.toml`.

## Close-out
- **AGENT:** copy the gate results (GROK-UNITS, AGY-CLI, OPENAI-HEADERS, NOUS-SK-PORTAL, OPENAI-COSTS-403, grok expiry format, agy blob shape) into `docs/specs/providers-lane4.md` §9.
- **AGENT:** write `$E/SUMMARY.md` with one row per step: PASS / FAIL / BLOCKED, plus the evidence file names.
