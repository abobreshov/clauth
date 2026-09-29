# Guest mode

tollgate is a fork of [clauth](https://github.com/uwuclxdy/clauth) and is built to run next to an installed upstream clauth. Both tools would otherwise write the same global files: `~/.claude/.credentials.json`, `~/.claude/settings.json`, `~/.claude.json`, `~/.codex/auth.json`, Claude Code's plugin registry and herdr's `config.toml`. Only one tool may own those at a time, and until the owner's accounts are imported that tool is upstream. While that holds, tollgate runs in **guest mode**: it watches, and runs per-session accounts of its own, but changes none of upstream's state in those files. The one exception is tollgate's own entries in them (its plugin, its MCP server, its herdr blocks), which it adds and removes without touching anything else (see [tollgate's own entries](#tollgates-own-entries)).

## When it is on

Guest mode is on when `~/.clauth` exists and `~/.tollgate/import-journal.json` does not record a completed import. It is decided on every run; there is no flag or setting.

The import that would end it (`tollgate import clauth`) is **not implemented yet**. Until it ships, guest mode ends only when `~/.clauth` is gone. Hand-writing an import journal is not a supported way out: nothing would have moved upstream's accounts, and both tools would then write the same files.

How to tell:

- the TUI header shows a `[ guest ]` pill after the version
- `tollgate usage --json`, `GET /v1/usage` and `GET /v1/health` carry `"guest_mode": true`
- `tollgate status --json` carries `"guest_mode": true` (the one-shot print only; the daemon's `status.json` does not)

## What works

- **`tollgate start <profile>`**: every session runs in its own `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`) under `~/.tollgate/profiles/<name>/`, so it needs none of upstream's files. This is how you use a tollgate profile in guest mode.
- **`tollgate login <name> --base-url … --api-key …`** creates API-key profiles (Ollama Cloud, OpenRouter, DeepSeek, …), and Alibaba console re-logins still run. The first profile is created but never made the active account, since that would link it into `~/.claude`. Subscription logins are refused (below).
- **Monitors, `tollgate usage`, the Usage tab, the local agent API and the MCP `usage` tool** all work unchanged.
- **Upstream's accounts, read only.** tollgate reads upstream's non-secret status feed, `~/.clauth/status.json`, and shows each of its profiles as `upstream:<name>`, labelled `<name> (clauth)`, with its 5h / 7d figures. Nothing else under `~/.clauth` is read: no config, no credentials, no per-profile cache. The feed is as fresh as upstream's daemon keeps it; without one there may be no feed and no upstream rows.
- **`tollgate herdr link` / `unlink`**, which write no herdr config.
- **The Plugin tab's Claude Code plugin install and `mcpServers` wiring, `tollgate herdr install` / `uninstall`, and the Plugin tab's herdr config fix.** Each writes only tollgate's own entries (below).

## tollgate's own entries

Some of the shared files hold keys that only tollgate uses, under its own names. Upstream never reads or writes them, so guest mode lets tollgate add, change and remove them:

| File | tollgate's entries |
|------|--------------------|
| `~/.claude/plugins/installed_plugins.json` | `plugins["tollgate@tollgate"]` |
| `~/.claude/plugins/known_marketplaces.json` | `tollgate` |
| `~/.claude/settings.json` | `enabledPlugins["tollgate@tollgate"]`, `extraKnownMarketplaces.tollgate` (Claude Code declares the marketplace there on a user-scope add) |
| `~/.claude.json` | `mcpServers.tollgate` |
| herdr's plugin registry | the plugin id `tollgate` |
| herdr's `config.toml` | the blocks under `# tollgate herdr plugin` |

Every other key is left alone: `clauth@clauth`, `mcpServers.clauth`, upstream's `# clauth herdr plugin` blocks, `apiKeyHelper`, `env`, `oauthAccount` and the rest. The rules:

- **Nothing else changes.** A direct edit (the `mcpServers` wiring, the herdr config) parses the file before and after, and refuses to write if anything besides tollgate's entries would differ. The plugin install runs `claude plugin`, whose writes tollgate does not control. So tollgate snapshots the three plugin files first, and after the run puts back any upstream key the CLI changed or dropped. It keeps tollgate's rows and anything the CLI only added, and logs the restore.
- **Upstream's writers wait.** Each write holds upstream's state lock, `~/.clauth/.lock`, for the whole read-modify-write (for the plugin install, the whole `claude plugin` run). The lock file is opened read-only and never created. If upstream holds it for more than 5 seconds, the write stops with `upstream clauth is holding ~/.clauth/.lock … nothing was written; retry once it finishes`.
- **Atomic, mode kept.** The file is replaced through a temp file and a rename, with its permission bits kept. A symlink is written through to its target.
- **herdr still checks.** A herdr config edit is still validated with `herdr config check` on a temp copy first. Under the lock, tollgate re-reads the file and writes nothing if it changed since the edit was planned. The default key `prefix+t` is never bound over a key another block already binds, upstream's included. When upstream's block already sets the sidebar's claude row, tollgate tells you what to add rather than editing it.
- **Uninstall removes only tollgate's.** `tollgate herdr uninstall` removes the `tollgate` plugin and tollgate's marked blocks. Nothing in tollgate deletes another tool's entry.

Run the plugin install from a shell outside Claude Code. With `CLAUDE_CONFIG_DIR` set, `claude plugin` would write that session's config, not `~/.claude`, so the install refuses and says why.

Because the plugin now sits in the shared `~/.claude`, upstream's Claude Code sessions load it too. Its hooks (`tollgate self-heal`, `tollgate hook-profile-changed-note`) do nothing in any session whose `CLAUDE_CONFIG_DIR` is not under `~/.tollgate`, so upstream's sessions get no tollgate notes. The plugin's MCP server, and a wired `mcpServers.tollgate`, do show tollgate's tools in those sessions.

## What is refused

These print one line and exit 1:

```
tollgate: upstream clauth manages ~/.claude on this machine (guest mode). Use 'tollgate start <profile>' for a per-session account, or import clauth first (not yet available).
```

| Refused | Why |
|---------|-----|
| every global Claude Code switch: `tollgate switch <name>`, the bare `tollgate <name>`, the TUI's switch, the MCP `switch_profile` tool without `session`, the daemon's REST `POST /api/v1/switch` (answered `409`) | a switch relinks `~/.claude/.credentials.json` and rewrites `settings.json`. A codex switch still runs, but moves only tollgate's own marker and leaves `~/.codex/auth.json` alone. Moving one live session (below) is not a global switch and is allowed |
| `wrap_off` / switch-off-all | clears the same slot |
| `tollgate capture` | adopts the login in `~/.claude/.credentials.json`, which upstream owns |
| `tollgate login <name> --codex`, with or without `--browser` | the capture replaces `~/.codex/auth.json` with a link into tollgate's store; either flow mints a second codex chain beside upstream's |
| the Claude browser OAuth login, `tollgate login <name> --setup-token`, and the TUI's login | each mints a second Claude login for an account upstream already holds |

## Moving a live session

Moving one `tollgate start` session between accounts works in guest mode ([Auto-switch](Auto-Switch#moving-a-live-session-by-hand)): `tollgate switch <sid> <profile>`, `--relaunch`, the TUI's `m` key and the MCP `switch_profile` tool with `session`. An API-key hot swap writes only under `~/.tollgate`: the session's registry row and its key-helper ack in `~/.tollgate/live_sessions/`, the target's liveness marker, and a touch of the session's own runtime `settings.json`. The session's key helper is `tollgate __tollgate-api-key --session <sid>`, written only to that runtime `settings.json`. Settings sync stays off, so the helper line never reaches `~/.claude/settings.json`. A relaunch resumes the conversation from the guest transcript store, and the new start is a guest start too. The version check runs `claude --version` only.

## What is skipped silently

Background work that would write a global file stands down without an error:

- the fallback chain and the scheduler never auto-switch; a chain decision stays put
- the credential detach on TUI exit, the credential snapshot, the `settings.json` apply and the `~/.claude.json` identity strip
- following or detaching `~/.codex/auth.json`
- the Claude Code plugin self-heal, its preflight and the `installed_plugins.json` repoint, when the config dir they would write is an upstream session's. Otherwise they run under the rules above, and the repoint re-points only tollgate's own rows
- renaming, deleting or logging out a profile recorded as active: the profile's own files change, the global legs do not
- every Claude and codex OAuth leg: no refresh-token spend (usage polls use the access token already held and fall back to the cache on a 401), no rolling re-stamp, no codex standby rotation, no adopt from `~/.claude/.credentials.json`, no refresh after an auto-start 401, and no write to the default macOS Keychain item `Claude Code-credentials` (per-session items still work)
- `settings.json` and `~/.claude.json` are not synced: each runtime's copy is seeded from your operator file at start and belongs to its session after that; nothing is written back, and runtime copies do not sync with each other
- a session runtime gets a private copy of `~/.claude/plugins` at start (in both link modes), keeps its transcripts in `~/.tollgate/guest-claude/projects` instead of `~/.claude/projects` (`tollgate resume` seeds the named transcript there), and gets copies instead of links of the `~/.codex` entries it would otherwise share

## What holds whether or not guest mode is on

These keep the two tools apart on any machine where both are installed:

- tollgate's data dir is `~/.tollgate`, its env prefix `TOLLGATE_`, its daemon's default REST port 8453 (upstream: 8443), its herdr plugin id `tollgate`, its Claude Code plugin `tollgate@tollgate`, its completion functions its own.
- tollgate's usage fetcher stands down while upstream's `clauthd.lock`, `clauthd-standby.lock` or `usage-fetch.lock` in `~/.clauth` is held, so the two never rotate a refresh token or auto-switch the same slot at once.
- The `apiKeyHelper` subcommand is `__tollgate-api-key`, and its parser requires the executable to be tollgate, so upstream's helper never loads a tollgate profile's key and the reverse.
- A symlink in `~/.claude/.credentials.json` that points into a store tollgate does not own is never relinked, cleared, captured or detached. Ownership is by location (`~/.tollgate/profiles/`), so upstream's `~/.clauth/profiles/<name>` is never claimed.
- The herdr install and heal pick only `tollgate-v*` release tags and check the manifest id before herdr runs, so upstream's plugin is never registered over.
- `install.sh` runs no post-install `self-heal`.

## Known gaps

- With real links, a guest session still links the other top-level `~/.claude` entries (`CLAUDE.md`, `todos/`, `history.jsonl`, …), so a session can write them; only `plugins/`, `projects/`, the settings and credentials files are kept off.
- An isolated guest session's transcripts are still rescued into `~/.claude/projects` at teardown, and `tollgate sessions` / `resume` list only `~/.claude/projects`, not the guest store.
- A `~/.claude/.credentials.json` or `~/.codex/auth.json` link into a tollgate store made before guest mode is left in place, since removing it would write upstream's tree. With rotation off, tollgate no longer updates the store behind it.
- `tollgate list` and `tollgate status --json` print each profile's `base_url` unredacted. `usage --json` and the local agent API redact endpoints.
- There is no import, and so no supported way to hand the global files from upstream to tollgate.

The migration design (the import transaction, rollback, retiring upstream) is in [the plan](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/multi-provider-redesign-plan.md), section 4.0.
