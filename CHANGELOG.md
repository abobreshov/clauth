# Changelog

## Unreleased

### Native monitors and secret store

- Added read-only Grok, Antigravity (Linux Secret Service), and native Codex monitors. Borrowed tokens are never refreshed; expired or undated tokens make no network call. Managed Codex store symlinks are refused.
- Added monitor presets, offline `monitor detect --explain`, and private `monitor refresh --capture DIR` structure dumps. Detection never executes a CLI or reads a keyring secret.
- Added `secret set|list|rm` with hidden input, an atomic private store, environment-first lookup (`--prefer-store` reverses it), and stored-name child environment scrubbing.

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

- **No import yet.** `tollgate import clauth` is designed but not implemented, so guest mode cannot end while clauth is installed.
- **Old credential links survive guest mode.** A `~/.claude/.credentials.json` or `~/.codex/auth.json` link into a tollgate store made before `~/.clauth` appeared is left in place, since removing it would write upstream's tree. With rotation off, tollgate no longer updates the store behind it.
- **Guest sessions still link the operator's read-mostly `~/.claude` content.** `CLAUDE.md`, `commands/`, `agents/`, `skills/`, `hooks/`, `output-styles/` and `keybindings.json` stay linked under real links, so an edit the session is asked to make there (a `#` memory note, `/agents`) lands in `~/.claude`. A passthrough `--continue` is not seeded into the guest store.
- **macOS guest Keychain guard not built on macOS yet.** The default-item refusal is tested on Linux through its predicate; `keychain.rs` itself has not been compiled for macOS since the change. Nor have the macOS- and Windows-only helper spawns that now scrub monitoring keys; their command builders are compiled and tested on Linux.
- **`list` and `status --json` show `base_url` unredacted.** `usage --json` and the agent API redact it.
- **`tollgate api serve` leaves `api.sock` behind on SIGTERM.** The next start replaces the stale socket.
- **herdr `--display-agent` scope unverified.** Whether the `border label` knob's label is scoped to one pane is not verified against herdr 0.9.
- **Self-update is disabled.** There is no fork-signed release or updater yet; upgrade by reinstalling from source. `tollgate herdr install` has no release to fetch until one is published.
