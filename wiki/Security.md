# Security

This page covers where your logins sit, how they move between accounts, how monitoring keys are kept out of files, and what the local API exposes. The trust model, every host tollgate contacts, and vulnerability reporting live in [SECURITY.md](https://github.com/abobreshov/clauth/blob/feat/tollgate/SECURITY.md).

## Where credentials live

| Path | Holds |
|------|-------|
| `~/.tollgate/profiles/<name>/credentials.json` | that account's OAuth token pair, plus the MCP-server logins described below |
| `~/.tollgate/profiles/<name>/mcp-logins.json` | those MCP-server logins alone, parked whenever the account stores no Claude login of its own and merged back once it regains one |
| `~/.tollgate/profiles/<name>/session-token.json` | a long-lived `claude setup-token` login, when captured |
| `~/.tollgate/profiles/<name>/config.toml` | the endpoint API key, for endpoint accounts |
| `~/.tollgate/profiles/<name>/auth.json` | a codex profile's ChatGPT token chain, in codex's own `auth.json` shape ([Codex](Codex)) |
| `~/.tollgate/profiles/<name>/auth.lkg.json` | the last-known-good copy of that chain, restored only over a store that has read unreadable for 30 seconds with no session live |
| `~/.codex/auth.json` | after `tollgate login <name> --codex`, a symlink onto that profile's `auth.json`, so your own codex and tollgate hold one chain (refused in guest mode) |
| `~/.tollgate/api-token` | the local agent API's bearer token, 64 hex characters, `0600` |
| your environment | the values of the variables `monitors.toml` and `billing_key_env` name; tollgate stores only the names |

Beside a codex store sit two files that hold no credential: `auth.attempt`, a fingerprint of the refresh token tollgate last sent, and `auth.quarantine.json`, the verdict that killed the chain with the judged token's fingerprint. A codex session home links `auth.json` rather than copying it, except on a host without symlinks, where the session's `codex-home/` or `codex-home-isolated/` holds a copy converged with the store at each session start and exit.

The full tree, the quarantine slot and the Keychain row are in [SECURITY.md](https://github.com/abobreshov/clauth/blob/feat/tollgate/SECURITY.md)'s data-at-rest table. On Unix, tollgate creates each file `0600` and each directory `0700` (owner-only from birth, never a later chmod) and re-tightens the whole tree on every launch (stopping at each codex home's `0700` node, the durable store and the session homes, whose contents are codex's own); Windows keeps the user profile's stock ACLs untouched. Each write is temp-file + fsync + rename, and a rotation caught mid-write parks as `credentials.json.pending`, promoted only once durable.

An endpoint account's API key reaches Claude Code through `apiKeyHelper` (`<path to tollgate> __tollgate-api-key <profile>`), so it never lands in `settings.json`. The helper's parser requires the executable to be tollgate, so upstream clauth's helper (`__api-key`) never loads a tollgate key, and the reverse.

tollgate reads one credential it does not own: for a `nous` monitor, Hermes' Nous access token in `<hermes_home>/auth.json`, and only while it is unexpired. The refresh token is never deserialized, the file is never locked or written, and the token is sent to `portal.nousresearch.com` alone ([Providers](Providers#nous-portal-through-hermes)).

## Secrets rules

- **Monitoring keys by name only.** `monitors.toml` and a profile's `billing_key_env` hold the NAME of an environment variable. The value is read from the fetching process's environment at fetch time, used for the one read that needs it, and dropped: never written to disk, logged, printed, or put in argv.
- **Refused where a name goes.** A value shaped like a key (`sk-or-…`, a long mixed string), a secret-shaped key in a `[[monitor]]` table (`api_key`, `token`, …), and process variables (`PATH`, `HOME`, `XDG_*`, `HERDR_*`, `CLAUDE_CONFIG_DIR`, …) are all refused. A `monitors.toml` parse error names the line and column with quoted values and credential-shaped words redacted, so a pasted key never reaches the terminal or `daemon.log`.
- **Scrubbed from children.** Every variable a monitor or a `billing_key_env` names is removed from each `claude` and `codex` session tollgate spawns (`tollgate start`, the MCP `delegate`). The shunt gateway does not inherit the monitoring-only ones either; a monitor's `api_key_env` is left for it, since that is an inference key.
- **Separate slots.** A management key is sent to OpenRouter's `/api/v1/credits` and nowhere else; `apiKeyHelper` never prints a monitoring key.
- **Presets carry no credentials.** A preset's env is limited to five behaviour switches, applied on every load, so a hand-edited preset file cannot put `ANTHROPIC_AUTH_TOKEN` onto an account ([Configuration](Configuration#presets)).
- **Redacted reads.** `tollgate usage --json`, the local agent API and the MCP `usage` tool strip userinfo, the query and key-shaped path segments from every endpoint, and replace token-like words in free text (failure messages, labels, plan names) with `[redacted]`. `tollgate list` and `tollgate status --json` do not yet redact `base_url`.
- **herdr tags carry names and numbers only.** Other herdr clients can read pane metadata, so no id, email or key fragment is published.

## The local API token

The local agent API listens on loopback only (default `127.0.0.1:8454`) and on the unix socket `~/.tollgate/api.sock`. A non-loopback address is refused, since the API has no TLS. Over TCP every request needs `Authorization: Bearer <token>` from `~/.tollgate/api-token`, checked before routing, as a SHA-256 in constant time; the file is re-read per request, so deleting it and running `tollgate api token` rotates it. The socket needs no token: it is `0600` inside the `0700` data dir, so reaching it already proves the caller is your user. Every route is a read, and no response carries a credential. Anything that can read your home directory can read the token; treat it like the rest of `~/.tollgate`.

## Guest mode

While upstream clauth owns `~/.claude` ([Guest mode](Guest-Mode)), tollgate writes none of `~/.claude/.credentials.json`, `~/.claude/settings.json`, `~/.claude.json`, `~/.codex/auth.json`, the Claude Code plugin registry or herdr's `config.toml`. Global switches, `capture`, codex adoption, the plugin install and `herdr install` refuse; background legs that would write those files skip. It reads upstream's `~/.clauth/status.json` for a read-only view and nothing else under `~/.clauth`. `tollgate login` still runs the Claude browser OAuth flow there, minting a second login for that account beside upstream's; that is a known gap.

## What a switch touches

Outside guest mode, a switch rewrites exactly the files in SECURITY.md's switch list: the global credentials link, the `env`/`model`/`apiKeyHelper` parts of `~/.claude/settings.json`, and the stale identity block in `~/.claude.json`, plus the target profile's stored MCP logins so you are not signed out of them. Nothing else moves. Hooks, permissions, status line, projects, plugins and token stats are all left where they are. A switch onto a codex profile rewrites `~/.tollgate/codex-profiles.toml`; when your own `~/.codex/auth.json` is the link tollgate installed into a profile's store, that link moves with the switch, and nothing under `~/.claude` does ([Codex](Codex#switch)).

## MCP-server logins

Claude Code keeps each MCP server's OAuth login in the same file as your Claude login, keyed by the server and its endpoint. Those logins are minted against the server itself, so they belong to no Claude account, and a switch carries them onto the account you switch to. Without that, every switch signed you out of every MCP server.

Two consequences worth knowing:

- The same MCP-server token ends up stored under more than one account. Every copy is `0600` like the rest, and your Claude logins are still never duplicated.
- Signing out of an MCP server in Claude Code propagates on your next switch. A profile you have not switched into since then keeps its old copy until you do.

macOS carries them too, through the Keychain rather than the file. Claude Code keeps your Claude login and your MCP-server logins as sibling keys inside one Keychain item, and a write replaces that whole item, so tollgate reads the item first and carries the MCP logins onto the login it installs. macOS asks you to allow that read the first time it happens: answer **Always Allow** and it should not ask again, since the grant binds to `/usr/bin/security` rather than to tollgate (measured against a stand-in item, not against Claude Code's own, so treat a second prompt as possible rather than a bug). You have up to 10 seconds to answer before tollgate gives up and carries on without the item's contents, and a little less if the same switch already spent part of its 20-second keychain budget. Decline it, miss it, or switch over ssh where the prompt cannot be shown, and the switch still completes. tollgate records what was lost on its event line, which lands in `~/.tollgate/tollgate.log` when you switched by hand and in the daemon's own log when the daemon switched for you, and you re-authenticate the servers that report a signed-out session. If the item ever reads back as anything other than the JSON object Claude Code expects, its raw bytes are preserved under `~/.tollgate/keychain-quarantine/` before the switch replaces or deletes the item, and the event line names the file: the Claude login head usually survives that kind of corruption, so you can slice it back out or re-authenticate the servers that complain. Every Keychain write is read back and verified the same way — one that comes back corrupt fails the switch rather than completing on a broken login, and one that cannot be checked, over ssh or out of time, completes with a note on the event line, since retrying the switch re-runs the write.

## Per-platform behavior

Where Claude Code reads its login from is platform-split, and assuming Linux behavior on a Mac is the trap.

- **Linux.** The plaintext credentials file only, re-read whenever its modification time moves. tollgate stamps that time explicitly on every swap, so a live session follows the new account.
- **macOS.** The Keychain first, the file only on a miss, and Claude Code deletes the file once it migrates tokens into the Keychain. The file swap alone is cosmetic there, so tollgate mirrors each fresh login into the `Claude Code-credentials` Keychain item as well. That item is namespaced per config dir, so a `tollgate start` runtime and a bare `claude` never share one, and a `--with-fallback` session's swaps write its runtime's namespaced item so the session follows its chain. Switching to a profile that stores no Claude login, an api-key or third-party account, signs that item out instead of leaving it, so Claude Code cannot keep spending the account you switched away from.
- **Windows.** No symlinks and no Keychain: the swap is a file copy, read directly.

macOS is why `tollgate login` exists at all. Claude Code's own `/login` under a custom config dir writes only a per-config-dir Keychain item, leaving the profile's credentials file empty.

## Session isolation

`tollgate start <profile>` builds that session its own `CLAUDE_CONFIG_DIR` under the profile directory, so identity, settings, and billing caches never leak between accounts running at once. The tree is torn down when the session ends. `--isolated` goes further and drops your global memory, plugins, and hooks, keeping only the account's auth.

A codex profile gets the same treatment under `CODEX_HOME`: its own home per session with the environment's codex credential variables scrubbed, the store forced to the linked `auth.json` by a `-c` flag, and a start refused where `/etc/codex/managed_config.toml` would outrank that ([Codex](Codex#run)).

## Token rotation

tollgate rotates each account's OAuth pair ahead of expiry, early enough that a running `claude` never reaches its own refresh threshold. Set Config tab `rotation` to `lazy` to refresh only after a request is rejected.

Refresh tokens are single-use. The active account shares one chain with the running `claude`, and whichever side refreshes first revokes the other, so tollgate never bets on winning that race: when Claude Code rotates first, tollgate adopts its fresher pair from the file mirror rather than spending a revoked token. That adoption is identity-guarded, so a login belonging to a different account is never captured unattended. A double-spend costs the loser one rejected request, never the account.

A refresh that fails terminally quarantines the account as `auth broken`. It is then excluded from every chain walk and refused as a switch target, since installing a dead token would sign out every running session. `tollgate login <name>`, or any later successful refresh, clears it.

A codex chain's refresh token is single-use too, and the server answers a replay by killing the chain for good, so tollgate never sends the same token twice: the token it last spent is remembered on disk beside the store, a failed refresh is not retried with it, and a session live inside codex's own five-minute pre-expiry window gets the refresh left to codex. A verdict the server calls final quarantines the chain as `broken`, and only a new login clears it: `tollgate login <name> --codex --browser` ([Codex](Codex#when-a-chain-dies)).

## Account-change detection

If Claude Code logged into a different account while tollgate was closed, the next launch asks before overwriting anything: keep the stored login, capture the live one as a new profile, or discard it. Config tab `on mismatch` picks an answer up front.

## Switching things off

The off-switches are SECURITY.md's table: `TOLLGATE_NO_COMPLETIONS=1`, `TOLLGATE_NO_API=1`, `TOLLGATE_NO_LOCAL_API=1` or `local_api.enabled = false`, an empty `fallback_chain`, `allow extra usage` off, and `auto_start = false` are all default-safe and named there with their effects. Self-update needs no switch: it is compiled out of this build.

Found something exploitable? Report it privately through the fork's [security advisories](https://github.com/abobreshov/clauth/security/advisories/new), not a public issue.
