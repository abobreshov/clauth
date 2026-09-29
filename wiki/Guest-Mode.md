# Guest mode

tollgate is a fork of [clauth](https://github.com/uwuclxdy/clauth) and is built to run next to an installed upstream clauth. Both tools would otherwise write the same global files: `~/.claude/.credentials.json`, `~/.claude/settings.json`, `~/.claude.json`, `~/.codex/auth.json`, Claude Code's plugin registry and herdr's `config.toml`. Only one tool may own those at a time, and until the owner's accounts are imported that tool is upstream. While that holds, tollgate runs in **guest mode**: it watches, and runs per-session accounts of its own, but writes none of upstream's files.

## When it is on

Guest mode is on when `~/.clauth` exists and `~/.tollgate/import-journal.json` does not record a completed import. It is decided on every run; there is no flag or setting.

The import that would end it (`tollgate import clauth`) is **not implemented yet**. Until it ships, guest mode ends only when `~/.clauth` is gone. Hand-writing an import journal is not a supported way out: nothing would have moved upstream's accounts, and both tools would then write the same files.

How to tell:

- the TUI header shows a `[ guest ]` pill after the version
- `tollgate usage --json`, `GET /v1/usage` and `GET /v1/health` carry `"guest_mode": true`
- `tollgate status --json` carries `"guest_mode": true` (the one-shot print only; the daemon's `status.json` does not)

## What works

- **`tollgate start <profile>`**: every session runs in its own `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`) under `~/.tollgate/profiles/<name>/`, so it needs none of upstream's files. This is how you use a tollgate profile in guest mode.
- **`tollgate login <name>`** creates profiles: API-key endpoints (Ollama Cloud, OpenRouter, DeepSeek, …) and browser OAuth logins alike. The first profile is created but never made the active account, since that would link it into `~/.claude`.
- **Monitors, `tollgate usage`, the Usage tab, the local agent API and the MCP `usage` tool** all work unchanged.
- **Upstream's accounts, read only.** tollgate reads upstream's non-secret status feed, `~/.clauth/status.json`, and shows each of its profiles as `upstream:<name>`, labelled `<name> (clauth)`, with its 5h / 7d figures. Nothing else under `~/.clauth` is read: no config, no credentials, no per-profile cache. The feed is as fresh as upstream's daemon keeps it; without one there may be no feed and no upstream rows.
- **`tollgate herdr link` / `unlink`**, which write no herdr config.

## What is refused

These print one line and exit 1:

```
tollgate: upstream clauth manages ~/.claude on this machine (guest mode). Use 'tollgate start <profile>' for a per-session account, or import clauth first (not yet available).
```

| Refused | Why |
|---------|-----|
| every Claude Code switch: `tollgate switch <name>`, the bare `tollgate <name>`, the TUI's switch, the MCP `switch_profile` tool, the daemon's REST `POST /api/v1/switch` (answered `409`) | a switch relinks `~/.claude/.credentials.json` and rewrites `settings.json`. A codex switch still runs, but moves only tollgate's own marker and leaves `~/.codex/auth.json` alone |
| `wrap_off` / switch-off-all | clears the same slot |
| `tollgate capture` | adopts the login in `~/.claude/.credentials.json`, which upstream owns |
| `tollgate login <name> --codex` without `--browser` | replaces `~/.codex/auth.json` with a link into tollgate's store |
| the Claude Code plugin install and the `mcpServers` wiring (Plugin tab fixes) | the plugin registry and `~/.claude.json` are upstream's |
| `tollgate herdr install`, the Plugin tab's herdr config fix | herdr's `config.toml` carries upstream's plugin block |

## What is skipped silently

Background work that would write a global file stands down without an error:

- the fallback chain and the scheduler never auto-switch; a chain decision stays put
- the credential detach on TUI exit, the credential snapshot, the `settings.json` apply and the `~/.claude.json` identity strip
- following or detaching `~/.codex/auth.json`
- the Claude Code plugin self-heal, its preflight and the `installed_plugins.json` repoint
- renaming, deleting or logging out a profile recorded as active: the profile's own files change, the global legs do not
- `settings.json` and `~/.claude.json` sync one way: your operator files still seed each tollgate runtime, but nothing is written back to them
- `tollgate herdr uninstall` removes tollgate's plugin and leaves herdr's `config.toml` alone

## What holds whether or not guest mode is on

These keep the two tools apart on any machine where both are installed:

- tollgate's data dir is `~/.tollgate`, its env prefix `TOLLGATE_`, its daemon's default REST port 8453 (upstream: 8443), its herdr plugin id `tollgate`, its Claude Code plugin `tollgate@tollgate`, its completion functions its own.
- tollgate's usage fetcher stands down while upstream's `clauthd.lock`, `clauthd-standby.lock` or `usage-fetch.lock` in `~/.clauth` is held, so the two never rotate a refresh token or auto-switch the same slot at once.
- The `apiKeyHelper` subcommand is `__tollgate-api-key`, and its parser requires the executable to be tollgate, so upstream's helper never loads a tollgate profile's key and the reverse.
- A symlink in `~/.claude/.credentials.json` that points into a store tollgate does not own is never relinked, cleared, captured or detached. Ownership is by location (`~/.tollgate/profiles/`), so upstream's `~/.clauth/profiles/<name>` is never claimed.
- The herdr install and heal pick only `tollgate-v*` release tags and check the manifest id before herdr runs, so upstream's plugin is never registered over.
- `install.sh` runs no post-install `self-heal`.

## Known gaps

- `tollgate login <name>` still runs the Claude browser OAuth flow in guest mode. It writes only the new profile, but it mints a second login for that account alongside upstream's.
- `tollgate list` and `tollgate status --json` print each profile's `base_url` unredacted. `usage --json` and the local agent API redact endpoints.
- There is no import, and so no supported way to hand the global files from upstream to tollgate.

The migration design (the import transaction, rollback, retiring upstream) is in [the plan](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/multi-provider-redesign-plan.md), section 4.0.
