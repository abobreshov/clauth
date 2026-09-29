# Security

tollgate keeps live Claude Code and codex OAuth tokens and third-party API keys on disk, reads provider billing endpoints, and serves a local API. That is worth scrutiny, so this doc spells out what it stores, what it talks to, what can touch your account, and how to switch each thing off.

tollgate is a fork of [clauth](https://github.com/uwuclxdy/clauth). Report issues in tollgate to this fork, not to upstream.

## Reporting a vulnerability

Report privately through the fork's GitHub security advisories: <https://github.com/abobreshov/clauth/security/advisories/new> (the repo's **Security** tab, **Report a vulnerability**). If private reporting is not available on the repo, open an issue at <https://github.com/abobreshov/clauth/issues> that says only that you have a security report and asks for a private channel; do not put details, logs or credentials in it. A description, the affected commit or version, and repro steps are enough to get started.

## Scope

In scope: anything in this repository, the `tollgate` binary built from it, the herdr plugin under `herdr-plugin/`, and the Claude Code plugin under `plugins/`. That includes credential handling, the guest-mode boundary with an installed upstream clauth, the local agent API, the TLS REST API, and what monitors read.

Out of scope: upstream clauth itself (report to [uwuclxdy/clauth](https://github.com/uwuclxdy/clauth)), the providers' own APIs, Claude Code, codex, Hermes and herdr.

## Supported versions

Only the newest commit on the `feat/tollgate` branch (0.1.0 at the time of writing). There are no signed releases yet and self-update is compiled out, so update by reinstalling from source.

## Secrets handling, in short

- **At rest.** Credentials live under `~/.tollgate`, every file `0600` and every directory `0700` on Unix, written atomically. API keys for launchable profiles are in each profile's `config.toml`; OAuth chains in its `credentials.json` / `auth.json`.
- **Monitoring keys by name only.** `monitors.toml` and a profile's `billing_key_env` hold environment variable NAMES. Values are read from the fetching process's environment, used for one read, and never written, logged, printed or put in argv. Secret-shaped keys, key-shaped values and process variables (`PATH`, `HOME`, …) are refused where a name goes; parse errors redact quoted values.
- **Scrubbed from children.** Every named variable is removed from each `claude` / `codex` session tollgate spawns; the shunt gateway does not inherit the monitoring-only ones.
- **One credential slot per purpose.** Inference keys reach Claude Code only through `apiKeyHelper`; a management key goes only to OpenRouter's `/api/v1/credits`; presets can carry only five non-secret env switches.
- **Borrowed, never refreshed.** For a `nous` monitor tollgate reads Hermes' Nous access token while it is unexpired and nothing else from that file; it never refreshes, locks or writes it.
- **Redacted reads.** `tollgate usage --json`, the local agent API and the MCP `usage` tool strip endpoint userinfo, queries and key-shaped path segments, and replace token-like words in free text. `tollgate list` and `tollgate status --json` do not yet redact `base_url`.
- **The API token.** `~/.tollgate/api-token` (64 hex characters from the OS random source, `0600`) guards the loopback API; the unix socket relies on its `0600` mode inside the `0700` data dir.
- **Guest mode.** While upstream clauth is installed and not imported, tollgate writes none of upstream's global files ([wiki/Guest-Mode.md](wiki/Guest-Mode.md)).

## Data at rest

Per-profile state lives under `~/.tollgate/`. Nothing under upstream's `~/.clauth/` is written; in guest mode its `status.json` is read, and nothing else there. On Unix that whole tree is owner-only: every tollgate-owned file is `0600`, every directory `0700`; the one exception is the inside of a codex home (`profiles/<name>/codex-home*`: the durable store and every session home, each itself `0700`), where codex's own files keep the modes codex gives them. The only credential copy outside it is the macOS Keychain item below. tollgate's one other tree of its own is `~/.local/share/tollgate/`, which holds the bundled plugin and no credentials; what a switch writes into Claude Code's own `~/.claude/` is listed at the end of this section.

| Path | Contents | Unix mode |
|------|----------|-----------|
| `~/.tollgate/profiles/<name>/credentials.json` | OAuth token snapshot | file `0600`, dirs `0700` |
| `~/.tollgate/profiles/<name>/mcp-logins.json` | Claude Code's MCP-server OAuth logins, parked here whenever the profile stores no Claude login of its own and merged back once it regains one. Live bearer credentials, minted against each server and belonging to no Claude account | file `0600`, dirs `0700` |
| `~/.tollgate/profiles/<name>/session-token.json` | long-lived `claude setup-token` login, if captured (sessions run on this; no refresh token) | file `0600`, dirs `0700` |
| `~/.tollgate/profiles/<name>/session-token.static.json` | the `claude setup-token` mint a rolling token superseded, kept so `tollgate static-token <p>` (or a dead chain) can restore it | file `0600`, dirs `0700` |
| `~/.tollgate/profiles/<name>/quarantine/<ts>-<seq>.<basename>` | credential files moved aside before repair, kept as evidence — a mis-filled sidecar (`….session-token.json`, which by definition carries a refresh token: that is what made it a mis-fill) or a backup slot whose content was not a mint (`….session-token.static.json`). Evidence can hold live credentials, which is why the dir is `0700`, why nothing prunes it automatically, and why it lives under the profile so `tollgate delete` removes it with everything else that account owns | file `0600`, dir `0700` |
| `~/.tollgate/profiles/<name>/config.toml` | base URL, API key (endpoint profiles), env block | `0600` |
| `~/.tollgate/codex-profiles.toml` | the codex roster: profile names, the active codex slot, the codex fallback chain and its settings; no credentials | `0600` |
| `~/.tollgate/profiles/<name>/auth.json` | a codex profile's ChatGPT token chain in codex's own `auth.json` shape (access, refresh and id tokens, account id); the one file codex sessions on that profile read and refresh through a link | `0600` |
| `~/.tollgate/profiles/<name>/auth.attempt`, `auth.lkg.json`, `auth.quarantine.json` | beside a codex store: a fingerprint of the refresh token last sent (never the token), the last-known-good copy of the chain, and the terminal verdict on a dead chain (the server's, or tollgate's own when a rotation the server accepted never reached the store) | `0600` |
| `~/.tollgate/profiles/<name>/codex-home/` | a codex profile's durable store: its sqlite state, `history.jsonl` and the rollout roots (`sessions/`, `archived_sessions/`) that every shared session links into | `0700` |
| `~/.tollgate/profiles/<name>/codex-home-<sid>/` | one live codex session's `CODEX_HOME`: links to the profile's `auth.json`, sqlite state and rollout roots, a copy of your `~/.codex/config.toml` with the store and lockfile keys stripped, and per-session scratch | `0700` |
| `~/.tollgate/profiles/<name>/usage_cache.json` | last-known utilization and plan | `0600` |
| `~/.tollgate/profiles/<name>/runtime-<sid>/settings.json` | one live session's Claude Code settings. An endpoint profile's key is **not** in it: the file carries an `apiKeyHelper` line naming `<path to tollgate> __tollgate-api-key <profile>`, which Claude Code runs per request to print the key. The parser accepts only a tollgate executable, so upstream clauth's `__api-key` helper never reads a tollgate profile | `0600` |
| `~/.tollgate/devices.json` | the devices that may call the daemon's REST API: each one's name, tier (`view` or `control`), when and how it joined, and a SHA-256 of its bearer token, plus a SHA-256 of the last token imported from before pairing, so a revoked one is not imported again. Never the token, which is shown once, when it is minted. Only if you have paired or added a device, or run `tollgate daemon --listen` over a token from before pairing | `0600` |
| `~/.tollgate/pairing.json` | the one pairing code waiting while `tollgate devices pair` runs: a SHA-256 of the code, never the code, with the device name and tier it mints, its expiry, and the tries left. A code that pairs, runs out of tries, or is withdrawn is deleted at once; an expired one by its wait, or by the next redemption that finds it | `0600` |
| `~/.tollgate/tls.json` | which directory holds the REST API's lego certificate; written with the platform default the first time `tollgate daemon --listen` starts. Not a secret — a path, no key material | `0600` |
| `~/.tollgate/monitors.toml` | monitoring-only accounts: ids, kinds, labels, budgets, and the NAMES of the env vars holding their keys. Never a key | `0600` |
| `~/.tollgate/monitors/<id>.json` | one monitor's last reading, its 429 hold, its sent-alert keys and a non-secret fingerprint of its target. Never a credential | file `0600`, dir `0700` |
| `~/.tollgate/profiles/<name>/config.toml` `billing_key_env` | the NAME of an env var holding a monitoring-only key; the value is never stored | `0600` |
| `~/.tollgate/api-token` | the local agent API's bearer token, 64 hex characters | `0600` |
| `~/.tollgate/api.sock` | the local agent API's unix socket; no token needed, so its mode is the access control | `0600` |
| `~/.tollgate/jobs/<id>.json` | backgrounded `delegate` prompt + result | file `0600`, dir `0700` |
| `~/.tollgate/live_sessions/<sid>.json`, `~/.tollgate/live_bare/<pid>` | liveness markers for running sessions: pid, profile name, working directory, flags. No credentials | file `0600`, dir `0700` |
| `~/.tollgate/keychain-quarantine/<UTC>-<pid>-<service>.json` | macOS only: raw bytes of a Keychain item that read back as anything other than the JSON object Claude Code expects, preserved before the write or sign-out that would otherwise destroy them — live credential bytes, which is why nothing prunes the dir | file `0600`, dir `0700` |
| usage and price caches, session history, logs, lock files (`~/.tollgate/`) | last-known usage, third-party state, burn samples, event log, advisory locks | file `0600`, dir `0700` |
| `~/.local/share/tollgate/` (macOS `~/Library/Application Support/tollgate/`, Windows `%APPDATA%\tollgate\`) | the bundled Claude Code plugin, laid down where `claude plugin` can register it: `plugin.json`, the `hooks/` dir, a generated `marketplace.json`, and an install marker under `markers/`. Content-keyed under `versions/`, with a `current@claude` pointer at the live one. No credentials | your umask; not re-tightened |

On macOS a second copy of the active login lives outside this tree, in the login Keychain, because that is where Claude Code reads it from:

| Item | Contents | Written by |
|------|----------|------------|
| generic password `Claude Code-credentials`, account `$USER` | the OAuth pair for whichever profile is linked | `/usr/bin/security`, one item per `CLAUDE_CONFIG_DIR` |

tollgate reads that item before every write (to carry your MCP-server logins across) and reads it back after every write to verify it landed: a write that reads back corrupt fails the switch, and one that cannot be checked completes with a note on its event line. The write goes to `security -i` over stdin while the command line fits 4096 bytes, keeping the token out of the process table; a larger item rides the tool's argument vector instead — visible to processes of your own user for the life of the call, and disclosed on the event line — and one past 512 KiB is refused outright, naming the size and how to shrink the item. A failed write reports the tool's exit code and the size of its stderr, never the stderr itself, since the tool can echo the value being written. Access is whatever the login Keychain grants, not `0600`.

- Writes are atomic. The temp file gets mode `0600` at creation, not a chmod afterward, so a loose umask never leaves a readable window; it's fsynced, then renamed into place. A rotation caught mid-write lands as `credentials.json.pending` and is promoted only once it's durable.
- Modes are enforced on Unix two ways: each writer creates its file owner-only, and every launch re-tightens the whole `~/.tollgate` tree, repairing a store from an older build or a loose umask; the sweep stops at each codex home's `0700` node (the durable store and the session homes) and leaves codex's own files inside it alone.
  - The repair never follows a symlink out of the tree, so it can't touch a file tollgate doesn't own. On Windows, access falls to the default user-profile ACLs, which tollgate does not loosen.
- Outside guest mode, a switch rewrites three files: `~/.claude/.credentials.json`, parts of `~/.claude/settings.json` (the `env` block, the top-level `model` key, `apiKeyHelper`), and `~/.claude.json`, whose stale account-identity block is dropped so Claude Code re-derives identity from the new token.
  - The rest of `~/.claude/` is left alone. On macOS the switch writes the Keychain item above as well, because Claude Code reads the Keychain before the file there.
- `tollgate login <name> --codex` (refused in guest mode) is the one write into codex's own tree: it replaces `~/.codex/auth.json` with a symlink onto that profile's store (staged beside it and renamed over), so your codex and every tollgate session share one chain; deleting the profile removes that link and says so. On a host without symlinks the file is copied instead and your codex keeps a second live copy of the chain, which tollgate tells you at capture time.

## Network activity

Every request tollgate makes, and what rides along with it:

| Endpoint | When | Carries |
|----------|------|---------|
| `platform.claude.com/v1/oauth/token` | token refresh ahead of expiry, on a rejected request, and on `t` force-rotate | your stored refresh token |
| `claude.com/cai/oauth/authorize` | `tollgate login` interactive sign-in, opened in your browser or shown as a link for you to open anywhere | no credentials; a PKCE challenge + random `state` |
| `platform.claude.com/v1/oauth/token` | `tollgate login` authorization-code exchange; a pasted code sends Claude Code's hosted redirect URI and the same `state` | the one-time auth code + PKCE verifier (mints a fresh token pair) |
| `api.anthropic.com/api/oauth/usage` | usage poll on the refresh interval | access token (Bearer) |
| `api.anthropic.com/api/oauth/profile` | plan-tier detection, and reading which account a token belongs to (so a live re-login can be told apart) | access token |
| `api.anthropic.com/v1/messages` | auto-start kick (opt-in, off by default) | access token; a 1-token Haiku request |
| `status.claude.com/api/v2/incidents.json` | Status tab and background poll | no credentials |
| `raw.githubusercontent.com/uwuclxdy/ai-pricelog/...` | model price table for the Tokens tab cost lens, fetched and disk-cached | no credentials |
| `api.deepseek.com/user/balance` | profiles whose base URL is DeepSeek, and DeepSeek `provider` monitors | that provider's API key |
| `api.z.ai/api/monitor/usage/...` | profiles whose base URL is Z.ai, and Z.ai `provider` monitors | that provider's API key |
| `github.com/abobreshov/clauth.git` (`git ls-remote`, a shallow `git fetch`) | `tollgate herdr install` only: find the newest `tollgate-v*` tag and check the plugin manifest's id before herdr runs | no credentials |
| `openrouter.ai/api/v1/key`, then `/api/v1/credits` | profiles whose base URL is OpenRouter, and `openrouter` monitors | the inference key for `/key`; for `/credits`, the management key named by `billing_key_env` when set, else the inference key |
| `ollama.com/api/usage` | profiles whose base URL is `https://ollama.com`, and `ollama_cloud` monitors. A profile on the local daemon (`127.0.0.1:11434`) sends nothing | that account's Ollama API key (Bearer) |
| `api.minimax.io/v1/token_plan/remains` | profiles whose base URL is MiniMax, and MiniMax `provider` monitors | that provider's API key |
| `portal.nousresearch.com/api/oauth/account` | `nous` monitors, only while Hermes' access token is unexpired; no redirects followed, 2 MiB response cap | Hermes' Nous access token (Bearer), borrowed read-only from `<hermes_home>/auth.json` |
| an Alibaba console gateway (`bailian-cs.console.aliyun.com` or its regional twin for your site) | usage poll for a Model Studio profile, whose API key cannot read its own quota | that profile's stored `[console]` session, never its API key |
| `bailian.console.aliyun.com` or `modelstudio.console.alibabacloud.com` | `tollgate login` on a Model Studio profile, opened in your browser to capture that console session | no credentials; the callback comes back to a loopback listener |
| `auth.openai.com/oauth/authorize` | `tollgate login <name> --codex --browser`, opened in your browser | no credentials; the callback comes back to a loopback listener on codex's registered ports (1455, then 1457) |
| `auth.openai.com/oauth/token` | that login's code exchange, and the refresh of a codex profile's chain when its access token nears expiry (single-use refresh tokens: each one is sent at most once, a fingerprint file remembers which) | the one-time authorization code and PKCE verifier for the exchange; the codex refresh token for a refresh; the id token for the optional API-key exchange |
| `chatgpt.com/backend-api/wham/usage` | usage poll for a codex profile on the refresh interval | the codex access token (Bearer) and the account id header |
| `chatgpt.com/backend-api/wham/rate-limit-reset-credits` and its `/consume` | only when you run `tollgate limit-reset <name>`: lists that account's banked usage-limit resets and, after you confirm, spends one, with the profile's stored access token. Nothing polls or retries it. |
| a custom base URL you set | requests against an API-endpoint profile, plus a best-effort usage probe against that same origin | whatever you configured |

Your stored Claude access tokens go to `api.anthropic.com` and nowhere else; a codex profile's tokens go to `auth.openai.com` and `chatgpt.com` and nowhere else; each provider key goes to its own provider's host. Nothing checks for or downloads updates: self-update is compiled out of this build. Your refresh token goes to `platform.claude.com`, which is the token endpoint Claude Code's own client refreshes against: every pair is minted there, whether from a refresh or from the interactive `tollgate login`, which follows Claude Code's OAuth flow by opening `claude.com` in your browser to authorize (or showing you the same link to open on any device) and posting the one-time code back to `platform.claude.com`. tollgate runs no telemetry or analytics; it talks to the hosts above and no others.

### Listening sockets

tollgate binds a socket in these places:

| Listener | When | Reachable from |
|----------|------|----------------|
| `127.0.0.1:<random port>` | for the seconds `tollgate login` waits for the browser redirect | loopback only; it checks the OAuth `state` and closes |
| `127.0.0.1:8454` (`local_api.listen`) and `~/.tollgate/api.sock` | while `tollgate daemon` runs (unless `local_api.enabled = false` or `TOLLGATE_NO_LOCAL_API=1`), or `tollgate api serve` | loopback and your own user only; a non-loopback address is refused |
| the address you pass to `tollgate daemon --listen` (bare: `0.0.0.0:8453`) | for as long as that daemon runs | wherever you bind it |

The local agent API is read-only: every route is a `GET` and reads caches, none calls a provider, and no response carries a credential. Over TCP each request needs the bearer token from `~/.tollgate/api-token`, checked before routing as a SHA-256 in constant time and re-read per request; the unix socket needs none, since only your user can open it. Limits: 8 KiB head, 64 KiB body, 10 s to arrive, 60 s or 100 requests per connection, 16 connections. No CORS headers are sent. See [docs/agent-api.md](docs/agent-api.md).

`--listen` is off unless you ask for it, and it is the only way anything outside this machine can reach tollgate. It is TLS-only (from this host's lego certificate, or the `--cert`/`--key` pair named on the command line, read at startup), and every route but the pairing redemption requires the bearer token of a device paired on this machine, checked in constant time against the SHA-256 digests in `~/.tollgate/devices.json`. A device joins through a one-time code from `tollgate devices pair` (8 characters from the OS random source, valid for 5 minutes, used once, dropped after 5 wrong tries) or a token `tollgate devices add` mints and prints once, and its tier is fixed there: `view` reads the feed, `control` may also switch accounts. `tollgate devices revoke` refuses a device's next request. It serves the health check, the status feed and its event stream, the OpenAPI document, the herdr pane list and terminal stream, the Claude Code session listing and per-session history pages (read for any paired device), the account switch and chain edits, prompts and key presses into herdr panes (control devices only), and the pairing redemption. The feed it serves carries what `status.json` carries: names, tiers, percentages, timestamps, never a token or key; the one response that carries a token is the pairing that mints it, and no log line carries one. Connections persist and may be pipelined; `Content-Length` is the only framing accepted, chunked is refused, and any framing error closes the connection rather than resynchronizing, so the ambiguity request smuggling depends on does not arise. A connection slot is claimed at `accept()`, before the handshake and before any token is seen, so a peer reaching the port occupies one while connected; the clock bounds it — a peer that connects and says nothing gets the 10s first-request timeout, not the full connection lifetime — and an unauthenticated request or a failed pairing is answered and closed at once, so no unauthenticated client can hold a slot. Limits: 8 KiB of headers, 64 KiB of body, 32 concurrent connections, 100 requests and 120 seconds per connection, a 10s deadline per read or write. `TOLLGATE_NO_API=1` disables it. See `wiki/Daemon.md`.

## What acts on your behalf

A few code paths can change account state. All are narrow and all are documented.

Background, automatic:

- **Auto-start kick.** A real, billed `/v1/messages` call (`max_tokens = 1`, a fraction of a cent) under your own OAuth token, with the Claude Code client identity, to arm the 5-hour usage window.
  - It's the same request Claude Code makes on startup. Off by default, OAuth profiles only; enable it per profile on the Setup tab or with `auto_start = true`.
- **Auto-switch.** When the fallback chain is armed, tollgate relinks the global credentials to another account on its own once the active one runs out of headroom, from the TUI or from `tollgate daemon`.
  - It sends no inference itself. The chain is empty by default, and an account outside it is never switched to.
- **Pay-as-you-go spend.** With extra usage enabled, an auto-switch can land on an account that bills real money. Three things must all be true first: the chain-wide `allow extra usage` toggle is on, that account carries a `max spend` ceiling above $0, and billing is enabled at Anthropic.
  - All three are off or zero by default. An account with subscription quota left always wins over one that costs money, and once the ceiling is spent tollgate stops using that account.
- **Token refresh.** Anthropic refresh tokens are single-use, so refreshing spends the stored token for a fresh pair. By default it fires ahead of expiry, early enough that a running `claude` never reaches its own refresh threshold.
  - Set the Config tab's `rotation` row to `lazy` to refresh only after a request is rejected. Pressing `t` forces a rotation either way.

- **Monitor polling.** Read-only `GET`s to the usage and billing endpoints above, one per monitor per interval (default 90 s), from `tollgate daemon` or `tollgate monitor refresh`. No inference, no writes to any account; a 429 holds the monitor for at least 5 minutes.

User-invoked, only when you run the command:

- **Interactive login (`tollgate login <profile>`).** Opens your browser to Claude's OAuth authorize page and binds a loopback listener on `127.0.0.1:<random port>` to catch the redirect, then exchanges the returned code for a fresh token pair written into the new profile.
  - It reproduces Claude Code's own PKCE flow, touches no other account, and never opens a usage window. On macOS this is why `tollgate login` works at all: Claude Code's own `/login` under a custom config dir writes only a per-config-dir Keychain item, never the profile's credentials file.
- **Pasting the code (`tollgate login <profile>`, or the Setup tab's login modal).** The same authorize request, with the loopback callback and a paste door through Claude Code's hosted redirect — whichever lands first wins. You open the link wherever you like, and the page shows a one-time `code#state` string that you paste back. tollgate checks the `state` half against the one it generated before exchanging the code. The pasted string is a bearer-grade secret until exchanged: read echo-off on the command line, held in memory only, never logged and never written to disk. In the TUI, <kbd>c</kbd> writes the link (which carries no credential) to your terminal's clipboard through the OSC 52 escape and nothing else.
- **`tollgate start` / `tollgate resume`.** Spawns `claude` against the profile you named, so everything that session sends bills to that account. tollgate forwards your args and sends nothing of its own.

- **Rolling session token (`tollgate rolling-token <profile>`).** Points the profile's `session-token.json` at that profile's own OAuth usage chain: the daemon re-stamps the file with the chain's current access token, minus the refresh token, so sessions still hold nothing rotatable.
  - It also **widens what that credential can reach**. A `claude setup-token` mint carries two scopes, `user:inference` and `user:sessions:claude_code`; the rolling bearer carries the chain's full granted set.
  - The browser login requests six scopes (`org:create_api_key`, `user:profile`, `user:inference`, `user:sessions:claude_code`, `user:mcp_servers`, `user:file_upload`); every real Pro/Max login observed so far grants the five without `org:create_api_key`, and the bearer carries whatever the account's grant actually was.
  - Anything that can read the sidecar, or the live `~/.claude/.credentials.json` a switch installs it into, can use every one of those scopes until the token expires — which is hours rather than the mint's year.
  - The command prints the scope list when it arms, so the widening is stated where the decision is made. `tollgate static-token <profile>` restores the narrower mint; a terminally dead chain does the same automatically.

Agent-invoked, only when the Claude Code plugin is installed:

- **`delegate` (MCP tool).** Sends a real, billed `/v1/messages` request on a target profile under its own OAuth token, opening a full 5-hour usage window on that account.
  - It fires only when an agent calls the tool, and is hard-capped at recursion depth 1 (a delegated session cannot call `delegate` again).
- **`switch_profile` (MCP tool).** Relinks the global `~/.claude` credentials to another profile, the same write the bare `tollgate <name>` performs. It changes which account the global session refreshes onto; it sends no inference itself.

Network-invoked, only while `tollgate daemon --listen` is running:

- **`POST /api/v1/switch`.** The same relink as the `switch_profile` MCP tool, performed for a device paired with control. It sends no inference itself, and it refuses the cases that need a human (a login tollgate has not saved, credentials a refresh has rejected, a disabled account) rather than resolving them unattended.
- **`POST /api/v1/pair`.** Adds the device a live pairing code names, at the tier chosen when `tollgate devices pair` minted the code, and hands it its token. The code is the only credential it takes, so while a `--control` code is live, whoever enters it first gets control.
- **`POST /api/v1/panes/<id>/prompt` and `POST /api/v1/panes/<id>/keys`.** Hand a control device's prompt text or key presses to the agent running in a herdr pane, through `herdr agent prompt` and `herdr pane send-keys`; whatever that agent then does (inference on the account the pane runs, tool calls on this host) is the agent's own, under its own permission prompts. The daemon logs the pane and the text's length or the key count, never the text or the key names, and no log line or response carries them.

Nothing else sends inference or writes to your account.

## Self-update

Compiled out. The upstream updater verified releases against upstream's pinned minisign key, which this fork cannot sign with, and a self-replace from upstream would silently turn tollgate back into clauth. So this build carries no release API URL and no pinned key, never downloads or replaces itself, and never reinstalls the herdr plugin over the network; the Config tab's `auto-update` row renders off. A fork-signed updater with its own key is planned and will be documented here when it ships.

## Install-script verification

`install.sh` (the `curl | bash` path) runs `cargo install --locked --git https://github.com/abobreshov/clauth --branch feat/tollgate tollgate` when `cargo` is available, never a crates.io install (a `tollgate` crate there is not this tool), and runs no post-install `self-heal`. Its prebuilt-binary path checks the download against `sha256sums.txt` from the same release and fails closed, but the fork has published no release yet, so that path has nothing to fetch. It writes nothing to your shell profile.

## Process execution

Every command below goes through an argument vector, never a shell, so there is no shell-injection path.

| Command | When |
|---------|------|
| `claude` (from `PATH`) | `tollgate start`, `tollgate resume`, and the MCP `delegate` tool, with `CLAUDE_CONFIG_DIR` pointed at that session's runtime and your extra args forwarded |
| `tollgate mcp`, `claude --version` | Plugin-tab checks: a JSON-RPC handshake against tollgate's own server, and Claude Code's version |
| `claude plugin …` (`marketplace add`, `install`, `list --json`) | registering the bundled plugin and repairing a broken registration: the Plugin tab's install, the `tollgate self-heal` hook, and the gated heal `tollgate start`, `tollgate mcp` and the daemon tick share, which runs only when a registry read says the registration is broken |
| `herdr` (from `HERDR_BIN_PATH`, else `PATH`) | `tollgate herdr install` / `uninstall` / `link` / `unlink`, which drive herdr's own plugin commands and let herdr validate the config before it is written; plus `pane report-metadata` during a `delegate` run, with the herdr pane knobs on |
| `git` | `tollgate herdr install` only: `ls-remote` for the newest `tollgate-v*` tag and a shallow `fetch` into a scratch dir to read the plugin manifest's id |
| `notify-send` | a monitor crossing into HIGH / CRITICAL or past its `alert_pct`, once per window; a missing `notify-send` is silent |
| `/usr/bin/security` | macOS only: reading, writing and clearing the Keychain item above |
| `xdg-open` (Linux), `open` (macOS), `rundll32` (Windows) | opening a URL: the browser login page, or a status incident from the Status tab. The URL is passed as one argument |
| `kill` / `taskkill`, plus `ps` on macOS and `tasklist` on Windows | `tollgate daemon --replace` only: the pid is checked against a running tollgate daemon before it is signalled (Linux reads `/proc/<pid>/cmdline` instead of shelling out; Windows matches the exact image name `tollgate.exe`, so upstream's `clauth.exe` is never signalled) |
| `hostname -f` (macOS, Linux), `powershell.exe` evaluating `[System.Net.Dns]::GetHostEntry($env:COMPUTERNAME).HostName` (Windows) | `tollgate daemon --listen` without `--cert`/`--key` only: once at startup, to learn which lego certificate to load. Naming the certificate outright runs neither command. No part of it comes from you — the argument vector is a compile-time constant — and the answer is rejected unless it looks like a hostname before it is used as a filename in the certificate directory named by `~/.tollgate/tls.json`, which is yours to edit and owner-only like the rest of the tree |

tollgate never executes `hermes`. It runs no other external commands.

## First-run shell completions

On the first TUI launch tollgate offers to install shell completions. For bash and zsh it asks before adding a `source` line to your rc file (`[Y/n]`, interactive sessions only); fish gets its own completions dir. The answer is saved to `~/.tollgate/.completions_installed` so the prompt doesn't come back. `TOLLGATE_NO_COMPLETIONS=1` skips it.

## Build and supply chain

- `unsafe` is denied across the crate (`unsafe_code = "deny"`, `unsafe_op_in_unsafe_fn = "deny"`).
- CI runs `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and the test suite (`--all-features`) on Linux, macOS, Windows, as defined in `.github/workflows/ci.yml` (pushes to `mommy` and every pull request that touches code). Whether GitHub Actions is enabled on the fork is not confirmed, so treat a local `cargo clippy --all-targets` and `cargo test` as the gate.
- `cargo-deny` (advisories denied by default, license allowlist, sources locked to crates.io, `openssl` banned in favor of rustls) and `cargo-audit` both run in CI.
- `Cargo.lock` is committed and every dependency-resolving CI leg passes `--locked`, so a build resolves the versions it records or fails, rather than quietly taking whatever is newest.

## Switching behaviors off

| Switch | Effect |
|--------|--------|
| `TOLLGATE_NO_COMPLETIONS=1` | skips the first-run completion-install prompt |
| `TOLLGATE_NO_API=1` | stops `tollgate daemon --listen` from opening its socket, whatever the flags say |
| `local_api.enabled = false`, or `TOLLGATE_NO_LOCAL_API=1` | the daemon hosts no local agent API (`tollgate api serve` still serves when you run it) |
| no `[[monitor]]` tables (the default) | tollgate reads no Hermes login and no monitoring key |
| an empty `fallback_chain` (the default) | tollgate never switches accounts on its own |
| `allow extra usage` off (the default) | no auto-switch can reach an account that bills money |
| `auto_start = false` (the default) | tollgate sends no inference of its own |
| self-update | compiled out of this build; nothing to switch off |
