# Changelog

## Unreleased

### Import

- `tollgate import clauth` moves an upstream clauth 0.16.0 install into `~/.tollgate` and ends guest mode. `--dry-run` shows everything it would do and changes nothing, not even a lock file; the real run asks once (`--yes` skips it; a non-interactive stdin without `--yes` exits 2) and then runs one journaled transaction. [Importing clauth](wiki/Import.md).
- Refresh chains move by rename, never by copy, so each keeps one inode; a move across filesystems is refused. Live slots are repointed or captured in the same step.
- The global edits are journaled with their prior values: upstream's plugin off in `settings.json` (G1), upstream's own `clauth herdr uninstall --yes` with herdr's config backed up (G2), upstream's `apiKeyHelper` and `permissions.allow` MCP tool names rewritten to tollgate's (G3), and `installed_plugins.json` paths under `~/.clauth/profiles/` repointed (G4). No value from `settings.json`'s `env`, a credential or a `config.toml` reaches the journal, a backup or the report.
- The import refuses while any Claude Code session, clauth process, other tollgate process (Hermes sessions included) or live session marker exists, and re-checks after the prompt. It holds upstream's and tollgate's leases, rotation locks and state locks for the whole transaction, and nothing inside that hold prompts, spawns a process or uses the network.
- Any failure before the commit reverses every step automatically: exit 3 when nothing had moved yet, exit 1 after a store moved. An interrupted import is named on every command's stderr; `--resume` continues it and `tollgate import rollback` undoes it (stores before the upstream binary; upstream's herdr plugin reinstalled at its recorded commit only after the locks are released). `tollgate import status` reads the journal. Exit 4 means the journal needs you.
- `tollgate import retire` is the checklist after a committed import: upstream's plugin wiring out (r1), `tollgate@tollgate` in (r2), tollgate's herdr plugin in (r3), and the `.bashrc` completion line swapped (r4). Each step is journaled, and a rollback undoes them first.
- `tollgate plugin install` and `tollgate plugin uninstall`. The uninstall removes only `tollgate@tollgate` and `mcpServers.tollgate`, under the owned-keys guard in every mode.
- `GET /v1/health` and `GET /v1/status` carry `import: {state, completed_at}`; the TUI's guest-mode footer and the guest refusal name `tollgate import clauth --dry-run`.
- The name check spans the claude, codex and Hermes rosters and names the one that holds the name; an unreadable `hermes-profiles.toml` blocks the import like the other two rosters. Hermes homes are never read, moved or rolled back.
- Review hardening (credentials): `--adopt-live` keeps the store it supersedes in the profile's `quarantine/` and a rollback restores the regular slot and the store byte for byte; a slot holding another profile's login (`live_is_other_profile`) or an older login than the store (`live_older_than_store`) is refused even with `--adopt-live`, at import and at rollback. A session-token profile's discarded live copy is quarantined too, and a crash between the store move and the slot rename no longer turns the live file into a link. A `clauth` on `PATH` that resolves to tollgate itself is never retired (`upstream_binary_is_tollgate`). A crashed copy's own `.<name>.tmp.*` no longer blocks the rollback as `pending_rotation`; its revert sweeps it. Hermes children lose an inherited `CODEX_HOME`, and an anthropic alias as a Hermes profile's `--model` is refused.

### Moving a live session: API-key hot swap and relaunch in place

- `tollgate switch <sid> <profile>` now moves a running API-key session onto another account without a restart, when the target has the same endpoint, model routing and custom env and stores no OAuth login. The session gets a per-session key helper (`__tollgate-api-key --session <sid>`), and a switch changes only the key that helper prints. The commit touches the session's runtime `settings.json`, so Claude Code runs the helper before its next request (the S1 spike showed any change to that file drops its cached key); a 30 s helper TTL is the backstop.
- Every surface separates requested, committed and served. Until the helper has printed the new key the session is **swapping…**, and pane attribution, the TUI's live count, the herdr tag and `tollgate which` all name the account whose key it still sends. A helper failure shows as `stalled` with its code; an idle session never warns.
- `switch` prints `committed to '<p>' (api-key hot swap, key generation <N>)`; `--wait` blocks until the new key is served (exit 3 if it is not within 65 s, or the helper failed). A target of another class, and a session that cannot hot-swap, are refused with the reason and the relaunch command.
- Hot swap is on only for the Claude Code versions the S1 spike passed on, which are listed in the gate block appended to `docs/spikes/s1-apikeyhelper.md` (2.1.283 today). Other versions, isolated sessions, delegates, hosts that copy runtime trees, local or daemon endpoints, cloud-provider env and `forceLogin*` policies register relaunch-only. `TOLLGATE_HOT_SWAP=off` turns it off.
- `tollgate switch <sid> <profile> --relaunch [--yes] [--conversation <id>]` stops the session gracefully and resumes the same conversation under the profile, in the same terminal. It asks first on a terminal. The supervisor restores the terminal state, and if the new start fails it falls back to the original profile. The relaunch request carries no claude arguments, and a relaunched session honours its hand-off variables only with a matching one-time nonce.
- New surfaces: the TUI's Overview `m` key (move a live session onto the selected account) and a `…` live-cell marker for a session mid swap; the MCP `switch_profile` tool's `session` argument (`"self"` or a session id; it never relaunches); `live_sessions` on the local API's `/v1/accounts` and `/v1/accounts/{id}`; `state` on each `GET /api/v1/panes` session; `tollgate herdr tag --session <sid>`, which the reporter passes, and a `<served> → <committed> swapping…` tag while a swap is in flight.
- All of it works in guest mode: it writes only under `~/.tollgate`.
- Moving a session (or `resume --profile`) onto a Hermes profile name is refused with the Hermes relaunch hint instead of "profile not found".
- Review hardening (concurrency): the key helper re-checks the member it serves against the session's launch class, so a member whose endpoint was edited after the commit fails `class_differs:*` instead of handing its key to the launch endpoint. A new session never reuses a sid the registry already holds (a foreign row or a dead namesake's sidecars). Two `--relaunch` requests for one session are arbitrated (exclusive publish, per-request id on the answer), the CLI reports the profile that actually registered, and the answer survives the relaunching session's teardown. The cwd scan refuses when another live session shares the working directory. Asking again for a member the session refused answers `refused` again, and switching back to the current member withdraws a standing intent (the TUI `m` modal goes through the same core). The import fence retries a momentarily busy lock before `lock_held`.

### Hermes profiles (foundation)

- Hermes Agent is a third harness. `tollgate hermes new <name> [--provider nous|openrouter|ollama-cloud] [--model <id>] [--pool] [--env-key] [--stdin] [--no-key]` creates a profile, and each profile is a whole Hermes home: `~/.tollgate/profiles/<name>/hermes-home` (`HERMES_HOME`). `tollgate start <name> [-- <hermes args>]` launches it with `--provider` and `-m` from the roster. `hermes key`, `hermes auth <name> add|remove|reset`, `hermes list` and `hermes delete` manage it.
- A Hermes child runs with its own `HOME`, `profiles/<name>/child-home`, which holds only links to `~/.gitconfig`, `~/.config/git` and `~/.ssh`. Hermes therefore cannot find `~/.claude/.credentials.json`, `~/.claude.json`, `~/.codex`, `~/.config/gh` or `~/.hermes`. This closes the implicit Anthropic route through Hermes' auxiliary auto chain and its 402 fallback. Spike S7(f) (`docs/spikes/s7f-hermes-home.md`) verified it against the real Hermes 0.19.0, and a start refuses on a Hermes series the spike has not passed.
- Every launch audits the home first. The checks cover its shape, `active_profile`, sub-profiles, `-p` / `--provider` / anthropic `-m` in the pass-through, `$HSP/.env`, a live session or gateway, and the version. Then Hermes' own interpreter projects `config.yaml`, `.env`, `.op.env` and the managed scope into names and hosts only, and tollgate refuses every route to anthropic (config, `auth.json`, the env layers, `.anthropic_oauth.json`), any unpinned auxiliary provider, a bulk secrets source and an overriding managed scope. The projector fails closed against a strict schema.
- `new` pins all 15 auxiliary providers to the profile's provider through `hermes config set`. The env-mode key is written as one managed line of the home `.env` (prompted hidden or read from stdin, never argv), and every other line stays the user's. The roster holds only Hermes' own fingerprint of the key.
- The entrypoint is resolved at every launch: `[settings] bin`, then the mise install glob (`mise where` only as a fallback, run from `~/.tollgate`), then pipx, then PATH. A candidate must be a Python entrypoint of its own venv. The Omarchy shim is rejected by its first line and never run.
- Names are unique across the claude, codex and Hermes rosters. The bare `tollgate delete` resolves claude, then codex, then Hermes. `tollgate <hermes-name>` is a usage error that names the relaunch, and `tollgate switch <sid> …` refuses a Hermes session. Guest mode allows every Hermes verb, and no Hermes verb writes an upstream-owned file.

### Hermes profiles (surfaces)

- Each Hermes home's own usage ledger becomes a `hermes:<name>` account in `usage`, the agent API, the MCP `usage` tool, the TUI's Usage tab and herdr tags, with the new origin `hermes_profile`. tollgate reads `state.db` with `sqlite3 -readonly -json`, never with Hermes or its interpreter, and caches the month's rows in `profiles/<name>/hermes_usage_cache.json`. The estimate is the UTC month to date: billed cost where Hermes knows it, else Hermes' own estimate, summed exactly. A Nous cooldown in `rate_limits/nous.json` reads as rate-limited, a missing `sqlite3` or an unknown schema as unavailable, and a failed read keeps the last good figures for 7 days. The daemon re-reads each home at most once a minute, and a `tollgate start` refreshes it at teardown.
- `tollgate hermes show <name> [--json] [--check]` prints the home, the binding, the month's spend, the credential pool and the five latest sessions (to feed `-- --resume <id>`). The pool view reads `auth.json` through a whitelist that has no field for a token or key, shows each entry's label, type, source, status, request count, priority and the last four digits of its fingerprint, and marks the entry that is the key tollgate bound. `--check` resolves the install and runs every launch guard, printing each verdict, and exits 1 when one refuses. `hermes list` shows the month's spend too.
- `tollgate hermes pool <name> strategy fill_first|round_robin|random|least_used` sets a pool home's strategy through `hermes config set`, on an idle home only, holding the home's marker and no lock across the child. tollgate never writes `auth.json` or `config.yaml` itself.
- `tollgate which` answers inside a tollgate Hermes session (`HERMES_HOME`), and a tollgate `HERMES_HOME` is scrubbed from every other spawn.
- herdr: a `tollgate start <hermes-profile>` pane is tagged through its live session like a Claude Code pane, and a bare `hermes` pane through its `HERMES_HOME`. The reporter reads only that one variable from the pane's `/proc/<pid>/environ`. A tollgate Hermes profile is never the native match for the operator's own `~/.hermes` pane. Outside guest mode, `hermes new` runs `herdr integration install hermes` for the new home. In guest mode it prints the command instead.
- The TUI's `c` filter cycles all → claude → codex → hermes. Hermes rows show the provider, the mode, a live dot and the month's spend, and are read-only: Enter and `s` on one say it switches by relaunch.
- `status.json` gains `hermes_profiles[]` (`name`, `provider`, `model`, `mode`, `live`), `tollgate list` prints a Hermes section, and the bash, zsh and fish completions cover the `hermes` verbs and offer Hermes names after `start` and `delete`. `hermes` is listed in `--help`.
- The MCP `switch_profile` tool refuses a Hermes name with the relaunch hint.
- CI runs the `needs sqlite3` tests in a job that installs it, and a `hermes_projector_real` job runs the real projector against the PyYAML and python-dotenv versions Hermes 0.19.0 pins. The fixtures include anchors, `<<:` merge keys hiding a route, and every dotenv form.

### Guest mode

- The Plugin tab's Claude Code plugin install and `mcpServers` wiring, `tollgate herdr install` / `uninstall` and the Plugin tab's herdr config fix now run in guest mode instead of refusing. They add, change or remove only tollgate's own entries: `tollgate@tollgate` in the plugin registry and `enabledPlugins`, the `tollgate` marketplace declaration in `settings.json`'s `extraKnownMarketplaces`, `mcpServers.tollgate` in `~/.claude.json`, the herdr plugin `tollgate` and the herdr config blocks under tollgate's marker (the keybinding conflict check still applies).
- Each of those writes holds upstream's `~/.clauth/.lock` (opened read-only, never created, a bounded 5 s wait, then a clear error), and is atomic with the file's mode kept. A direct edit is refused when any other key or line would change. A `claude plugin` run is snapshotted first, and any upstream key it changed or dropped is put back afterwards. herdr's config is still validated with `herdr config check`, and re-read under the lock so a file that moved since the plan is not overwritten.
- The plugin install refuses to run under a `CLAUDE_CONFIG_DIR` (it would land in that session's config, not `~/.claude`). The plugin self-heal, its preflight and the `installed_plugins.json` repoint run again in guest mode. They write only `~/.claude` or a tollgate runtime, never an upstream session's config dir, and the repoint re-points only tollgate's own rows.
- Upstream's sessions load the shared plugin too, so the `self-heal` and `hook-profile-changed-note` hooks do nothing in a session whose `CLAUDE_CONFIG_DIR` is not under `~/.tollgate`.

## 0.1.0 — 2026-09-29

First release of tollgate, a hard fork of [clauth](https://github.com/uwuclxdy/clauth) (MIT) at upstream `b7d7cb02`, turned into a multi-provider subscription and spend monitor. Everything clauth did for Claude Code and codex accounts is kept; what changed or is new is below.

### Identity

- Package, binary and plugin names are `tollgate`, version reset to 0.1.0. `LICENSE` keeps clauth's copyright notice and adds the fork's.
- Data dir `~/.tollgate`, env prefix `TOLLGATE_`, daemon lock `tollgated.lock`, log `tollgate.log`.
- The daemon's value-less `--listen` binds `0.0.0.0:8453` (upstream keeps 8443), so both can listen.
- The herdr plugin id is `tollgate` (actions `tollgate.*`, tokens `$tollgate`, `$tollgate_severity`, `$tollgate_delegate`); the Claude Code plugin is `tollgate@tollgate` (tools `mcp__plugin_tollgate_tollgate__*`); the `apiKeyHelper` subcommand is `__tollgate-api-key`, and its parser requires the executable to be tollgate.
- Self-update is compiled out: no release check, no download, no self-replace, no network reinstall of the herdr plugin. The Config tab's `auto-update` row renders off. `install.sh` installs with cargo from the fork's `feat/tollgate` branch and runs no post-install `self-heal`.

### Guest mode

- While `~/.clauth` exists and no import has completed, tollgate writes none of `~/.claude/.credentials.json`, `~/.claude/settings.json`, `~/.claude.json`, `~/.codex/auth.json`, the Claude Code plugin registry or herdr's `config.toml`.
- Claude Code switches (CLI, TUI, MCP `switch_profile`, REST `POST /api/v1/switch` with a 409), switch-off, `capture`, codex adoption, the plugin install, `mcpServers` wiring and `herdr install` refuse with one line; auto-switch, the credential detach and snapshot, settings apply, the identity strip and the plugin self-heal skip silently. Settings sync is off: each runtime's copy is seeded from the operator file at start, and nothing is written back.
- `tollgate start`, API-key logins (never auto-activated), monitors, `usage` and the agent API keep working. Claude OAuth and `--setup-token` logins, both codex logins and the TUI login refuse.
- No Claude or codex OAuth leg runs: no refresh-token spend (polls use the held access token), no rolling re-stamp, no codex standby rotation, no live-rotation adopt, and no write to the default macOS Keychain item.
- Session runtimes get a private copy of `~/.claude/plugins` (its registry's absolute paths repointed at the copy on every start, so plugin loads and marketplace updates stay inside it; a copy that cannot be isolated leaves the session with no plugins and a warning), keep transcripts in `~/.tollgate/guest-claude/projects`, and copy rather than link the shared `~/.codex` entries.
- With real links, only the read-mostly `~/.claude` content (`CLAUDE.md`, `commands/`, `agents/`, `skills/`, `hooks/`, `output-styles/`, `keybindings.json`) still links into a guest session. `history.jsonl`, `todos/`, `shell-snapshots/`, the caches and any entry tollgate does not know link into `~/.tollgate/guest-claude` instead.
- An isolated guest session's transcripts and sidecars are rescued into `~/.tollgate/guest-claude`. `sessions`, `resume`, `info` and the sessions API list and find the guest store, and `tollgate start <p> -- --resume <id>` seeds the transcript into it as `tollgate resume` does.
- Upstream's accounts appear read-only as `upstream:<name>`, projected from `~/.clauth/status.json` alone.
- The TUI shows a `[ guest ]` pill; `usage --json`, the agent API and `status --json` carry `guest_mode`.
- Independently of guest mode, tollgate's usage fetcher stands down while upstream's daemon, standby or fetch lock is held, and nothing claims a credentials symlink into a store tollgate does not own.

### Observation model

- One `AccountObservation` per account across every source, with stable ids `claude:`, `codex:`, `monitor:`, `upstream:`: quota windows (unclamped `used_pct`, reset time, scope, `chain_eligible`, per-model breakdown), money meters as exact signed decimal strings, freshness (`fresh`, `stale`, `not_fetched`) kept apart from typed failures, times as RFC 3339.
- `tollgate usage --json` prints the stable envelope `{schema_version: 1, generated_at, guest_mode, accounts}`, redacted like the agent API.
- Shared severity (`ok`, `mid`, `HIGH` / `LOW`, `CRITICAL`), pace and truncating countdowns, pinned by fixture tables, so the CLI, TUI, API, herdr tags and alerts agree.

### Providers

- Ollama Cloud as a typed provider: `GET ollama.com/api/usage`, legacy 5h / 7d windows (chain-eligible) or the new monthly pool (display only), 4-week spend kept exact, per-model request counts, typed 401 / quota-429 / 429 / parse failures. The local Ollama daemon is recognised and never probed. New `Ollama-Cloud` preset.
- OpenRouter v2: `/api/v1/key` first as the auth probe, then `/api/v1/credits` degrading per meter; wallet, daily / weekly / monthly / lifetime spend, key cap, BYOK and free-model requests parsed from raw JSON into exact decimals. The `OpenRouter` preset pins the Claude tiers to `~anthropic/claude-*-latest` and sets `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` instead of defaulting to `openrouter/auto`.
- `billing_key_env` in a profile's `config.toml`: a monitoring-only key (an OpenRouter management key) by environment variable name, sent to `/api/v1/credits` alone and scrubbed from every child session.
- Presets may carry env from a five-key allowlist, applied on every load and only where the account has no value.

### Monitors

- `~/.tollgate/monitors.toml` and `tollgate monitor [list | add | remove | refresh]` for accounts tollgate watches without launching: `nous`, `ollama_cloud`, `openrouter`, and `provider` (DeepSeek, Z.ai, MiniMax, OpenRouter, Ollama Cloud).
- Keys by environment variable NAME only. Secret-shaped keys, key-shaped values and process variables are refused; parse errors redact quoted values.
- Nous Portal through Hermes: reads only `providers.nous.access_token` from Hermes' `auth.json` while unexpired, never refreshes, locks or writes it; monthly credits window and subscription / top-up / rollover / total-usable balances.
- Per-monitor cache with a 90 s default interval, a 5-minute minimum hold after a 429, 7-day stale retention and single-flight refresh; the daemon polls due monitors each tick.
- Monthly budgets (`budget_usd_month`) graded like a cap, and one-shot desktop notifications on a HIGH / CRITICAL crossing or `alert_pct`.

### Look and feel

- Palette engine: `palette = "auto" | "omarchy" | "catppuccin"`, default `auto`. Omarchy colours come from the running theme's `colors.toml` with nearest xterm-256 indices and reload live; Catppuccin Mocha is kept verbatim as the fallback.
- `tollgate usage` prints metric cards grouped by provider (countdown with local time, bar with elapsed marker, pace glyph and severity word), coloured on a terminal and plain under `NO_COLOR`, off a terminal or with `--plain`; `--watch N` repeats.
- `tollgate usage --waybar` prints `{text, tooltip, class, percentage}` for a Waybar custom module.
- The TUI's Usage tab lists monitors and upstream accounts below the profiles, rendered as the same metric cards.

### Local agent API

- A read-only HTTP/1.1 JSON API on loopback (default `127.0.0.1:8454`, bearer from `~/.tollgate/api-token`, 64 hex characters, 0600) and on the unix socket `~/.tollgate/api.sock` (no token). Non-loopback binds are refused.
- Routes `GET /v1/health`, `/v1/accounts`, `/v1/accounts/{id}`, `/v1/usage`, `/v1/providers`, `/v1/status`, `/v1/openapi.json`; endpoints and free text redacted; size, time and connection caps; no CORS.
- Hosted by `tollgate daemon` unless `local_api.enabled = false` or `TOLLGATE_NO_LOCAL_API=1`; `tollgate api serve | token | url` for everything else.
- MCP: a read-only `usage` tool returning the same envelope, beside the kept `profiles`, `switch_profile`, `delegate` and `monitor`.

### herdr

- The `$tollgate` pane tag carries the account's lead figure (`42%`, `23%w`, `64% mo`, `$13.67`, `$9.00 left`, `$4.08/mo`) with stale / HIGH / CRITICAL marks, capped at herdr's token limit and carrying no ids; `$tollgate_severity` publishes the class.
- Native `hermes`, `grok` and `agy` panes are tagged when exactly one enabled account belongs to their harness. Only `hermes` can be today, through a Nous monitor that reads Hermes' login; no Grok or Antigravity reader ships yet.
- `tollgate.usage` action opens the dashboard on the Usage tab.
- `tollgate herdr link [--path]` / `unlink` for a local checkout. `install` and the heal pick only `tollgate-v*` release tags and check the manifest id before herdr runs; a key another binding owns is never bound twice.

### Fixes from the 0.1.0 review

The pre-release review and how each finding was settled: [docs/tollgate-code-review-0.1.0.md](docs/tollgate-code-review-0.1.0.md). Beyond the guest-mode changes above:

- Helper processes (`notify-send`, the browser opener, herdr, git, the terminal spawn, plugin probes, and the macOS / Windows-only `/usr/bin/security`, `ps`, `tasklist`, `taskkill` and `powershell`) start with monitoring and billing keys scrubbed from their environment.
- Key-bearing provider GETs follow no redirect, cap bodies at 2 MiB and have a 20 s end-to-end deadline; the shared usage agent has the same deadline.
- An OpenRouter wallet read with a management key no longer decides the inference account's availability, and a `/credits` 429 holds that wallet for its `Retry-After` (at least 5 minutes). The hold is kept in `~/.tollgate/holds/`, so every process honours it: the daemon, profile fetches and a forced `tollgate monitor refresh`, which still re-reads `/key`.
- Credential redaction also catches keys embedded in punctuation, such as JSON strings.
- The local agent API's GETs no longer repair anything on disk, `/v1/status` is always parsed and fully redacted, TCP requests must carry a loopback `Host` (421 otherwise), and the socket's directory is checked and tightened before bind.
- The herdr plugin scripts no longer pass a pane id into `sed` or session paths through `xargs`.
- A guest-mode `delegate` resume copies the operator's transcript into the guest store first, as `tollgate resume` does, so Claude Code finds the conversation.

### Known gaps

- **No import yet.** `tollgate import clauth` is designed but not implemented, so guest mode cannot end while clauth is installed. (Closed in Unreleased: see Import above.)
- **Old credential links survive guest mode.** A `~/.claude/.credentials.json` or `~/.codex/auth.json` link into a tollgate store made before `~/.clauth` appeared is left in place, since removing it would write upstream's tree. With rotation off, tollgate no longer updates the store behind it.
- **Guest sessions still link the operator's read-mostly `~/.claude` content.** `CLAUDE.md`, `commands/`, `agents/`, `skills/`, `hooks/`, `output-styles/` and `keybindings.json` stay linked under real links, so an edit the session is asked to make there (a `#` memory note, `/agents`) lands in `~/.claude`. A passthrough `--continue` is not seeded into the guest store.
- **macOS guest Keychain guard not built on macOS yet.** The default-item refusal is tested on Linux through its predicate; `keychain.rs` itself has not been compiled for macOS since the change. Nor have the macOS- and Windows-only helper spawns that now scrub monitoring keys; their command builders are compiled and tested on Linux.
- **`list` and `status --json` show `base_url` unredacted.** `usage --json` and the agent API redact it.
- **`tollgate api serve` leaves `api.sock` behind on SIGTERM.** The next start replaces the stale socket.
- **herdr `--display-agent` scope unverified.** Whether the `border label` knob's label is scoped to one pane is not verified against herdr 0.9.
- **Self-update is disabled.** There is no fork-signed release or updater yet; upgrade by reinstalling from source. `tollgate herdr install` has no release to fetch until one is published.
