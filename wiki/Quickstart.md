# Quickstart

tollgate has two kinds of account. A **profile** is one tollgate can launch and switch: a Claude Code OAuth login, an API-key endpoint, or a codex login. A **monitor** is one it only watches: a Nous Portal account read through Hermes, an OpenRouter billing key, another provider's key. Both show up in `tollgate usage`, the TUI's Usage tab, the local agent API and the MCP `usage` tool.

If upstream clauth is installed on this machine, tollgate starts in [guest mode](Guest-Mode): it shows clauth's accounts read-only and refuses anything that would change clauth's state in `~/.claude` or `~/.codex` (tollgate's own plugin and herdr entries still install). `tollgate start`, `usage`, `monitor` and the API all work as below.

## See what you have

```bash
tollgate                # the TUI
tollgate usage          # every account's windows and money, from the caches
tollgate usage --json   # the same as the stable JSON envelope agents read
tollgate usage --waybar # one {text, tooltip, class, percentage} line for a bar module
```

`tollgate usage` never fetches: it reads what the TUI, the daemon and `monitor refresh` last cached. `--watch <SECS>` repeats it, `--plain` drops the colour, `--account <id or name>` and `--provider <source or name>` filter, `--all` adds disabled profiles.

## Capture your first profile

From a shell where Claude Code is logged in:

```bash
tollgate capture work
```

or launch the TUI (`tollgate`), open the Setup tab, pick `+ new`, press ⏎ on the `+ capture current login` row, name it `work`, and ⏎ on `create account`. Either way tollgate snapshots the OAuth token and endpoint settings your running session is using. Log into a second account in Claude Code and capture that one too. In guest mode `capture` is refused; use `login` instead.

To add an account without touching the session you are in, use `tollgate login` instead: it opens a browser, runs Claude Code's own OAuth flow, and writes the minted tokens into a fresh profile.

```bash
tollgate login personal                                   # browser login
tollgate login ds --base-url https://api.deepseek.com/anthropic  # api key, prompted echo-off
```

## API-key profiles

An API-key profile runs Claude Code against another provider's Anthropic-compatible endpoint. Pass `--base-url` and leave `--api-key` off so the key is read echo-off rather than landing in shell history:

```bash
tollgate login oll-main --base-url https://ollama.com             # Ollama Cloud
tollgate login or-main  --base-url https://openrouter.ai/api      # OpenRouter
tollgate login zai      --base-url https://api.z.ai/api/anthropic # Z.ai
```

The endpoint decides which usage panel the profile gets ([Providers](Providers)). Then open the Setup tab, select the profile, press <kbd>a</kbd> and pick `apply preset`: the `Ollama-Cloud` preset adds three telemetry switches to the profile's `[env]`, the `OpenRouter` preset pins the opus / sonnet / haiku / subagent tiers to OpenRouter's `~anthropic/claude-*-latest` aliases and sets `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1`. There is no `--preset` flag on `login`; presets are applied from the Setup tab ([Configuration](Configuration#presets)).

The key is stored in the profile's `config.toml` (0600) and handed to Claude Code through `apiKeyHelper`, never through `settings.json` or `[env]`.

For a third-party endpoint tollgate recognises, `open provider console` in the TUI action menu opens the page that key is minted on ([Configuration](Configuration#where-the-keys-come-from)).

## Monitors

A monitor needs no profile and runs nothing. Keys are named by environment variable only: tollgate stores the NAME in `~/.tollgate/monitors.toml` and the daemon reads the value from its own environment at fetch time.

```bash
tollgate monitor add oll   --kind ollama_cloud --api-key-env OLLAMA_API_KEY
tollgate monitor add or    --kind openrouter   --api-key-env OPENROUTER_API_KEY --billing-key-env OPENROUTER_MGMT_KEY
tollgate monitor add nous  --kind nous                              # reads Hermes' Nous login in ~/.hermes
tollgate monitor add ds    --kind provider --provider DeepSeek --api-key-env DEEPSEEK_API_KEY
tollgate monitor                     # list them and what their caches hold
tollgate monitor refresh             # fetch now, ignoring each monitor's interval
```

Export the named variables in the environment `tollgate daemon` (or `monitor refresh`) runs in; `monitor list` marks a name that is not set as `MISSING`. Every flag and each provider's behaviour: [Providers](Providers#monitors).

## Switch

In the TUI: move to the account, <kbd>⏎</kbd>, confirm. From the shell:

```bash
tollgate work
# switched to 'work'
```

A switch repoints the credentials your global `claude` reads. A session already running adopts the new account on its next token refresh. In guest mode a Claude Code switch is refused, because upstream clauth owns those credentials; a codex switch moves only tollgate's own marker.

## Run two accounts at once

```bash
tollgate start personal                  # claude under personal's own config dir
tollgate start personal -- --model haiku # flags for claude go after --
```

`tollgate start` gives the session its own `CLAUDE_CONFIG_DIR`, so identity, settings, and billing caches never mix between accounts, and the global session is untouched.

For a session that keeps the account's auth while dropping your global `CLAUDE.md`, plugins, and hooks:

```bash
tollgate start --isolated personal -p < prompt.txt
```

Pass the prompt on stdin when you use `-p`. A variadic `claude` flag would otherwise swallow a trailing positional prompt forwarded through tollgate. Run it in an empty directory to skip project memory too.

## Check what is loaded

```bash
tollgate which          # profile that owns the current session's credentials
tollgate which --json   # plus plan tier and endpoint
tollgate list           # account table with cached usage, no network
```

## Commands

| Command | Flags | Does |
|---------|-------|------|
| `tollgate` | | open the TUI (with stdout not a terminal: command help on stderr, exit 2) |
| `tollgate <profile>` | | switch to that profile and exit — deprecated, use `tollgate switch <name>`; a codex name moves the codex active marker instead ([Codex](Codex#switch)) |
| `tollgate start <profile> [claude args…]` | `--isolated`, `--with-fallback`, `--explain` | run `claude` under that profile's own config dir; a codex profile runs `codex` under its own `CODEX_HOME` instead, and `--with-fallback` is refused there ([Codex](Codex#run)); a Hermes profile runs Hermes on its own home, with `-- <hermes args>` passed through ([Hermes](Hermes#launch)) |
| `tollgate start --auto [claude args…]` | `--isolated`, `--with-fallback`, `--explain` | start on the first fallback-chain member with headroom for the models the session will run |
| `tollgate login <profile>` | `--base-url`, `--api-key`, `--setup-token`, `--yes`, `--model` | add an account, or re-authenticate one in place |
| `tollgate login <profile> --codex` | `--browser` | adopt the `codex login` in your `~/.codex` as a codex profile; `--browser` mints a fresh ChatGPT login in the browser instead and leaves `~/.codex` alone ([Codex](Codex#add-an-account)) |
| `tollgate capture <profile>` | | save the login Claude Code is using now as a new profile; the first one becomes the active account |
| `tollgate rolling-token <profile>` | | serve the profile's sessions a rolling token re-stamped from its usage chain |
| `tollgate static-token <profile>` | `--clear`, `--yes` | bare: restore the preserved mint a rolling token superseded; `--clear` removes the long-lived token entirely |
| `tollgate delete <profile>` | `--yes`, `--force` | remove a profile and every credential it holds, a codex profile included ([Codex](Codex#remove)) |
| `tollgate disable <profile>` | `--yes` | hide it from auto-switch, polling, and the status feed; files stay |
| `tollgate enable <profile>` | | put a disabled profile back |
| `tollgate limit-reset <profile>` | `--list`, `--yes` | spend one of a codex account's banked usage-limit resets; `--list` shows them and spends nothing ([Codex](Codex#use-a-usage-limit-reset)) |
| `tollgate which` | `--json` | print the profile owning the loaded credentials; inside a `tollgate start` codex or Hermes session, that profile |
| `tollgate list` | `--all` (`--disabled`) | account table from the on-disk caches, never fetches; codex accounts follow in their own `CODEX` section and Hermes profiles in a `HERMES` one, which `--all` leaves alone |
| `tollgate jobs` | `--json` | what the delegates are doing: account, elapsed, last output, live runs first; `--json` also carries each run's `session_id`, the handle `delegate({session_id})` takes after a crash, and whether the run was isolated, which is what decides whether that id is a handle at all |
| `tollgate switch <name>` / `tollgate switch <sid> <profile>` | | one name switches the global account (the bare `tollgate <name>` form, deprecated); two names move a live session: an OAuth one at its next request, an API-key one by hot swap within its endpoint class; `--relaunch` resumes it under the profile instead ([Auto-switch](Auto-Switch#moving-a-live-session-by-hand)) |
| `tollgate sessions` | `--json`, `--tokens` | list Claude Code sessions, newest first |
| `tollgate resume <id\|latest>` | `--profile <name>` | resume a session under a chosen account |
| `tollgate info <id\|latest>` | | print a session's resume command, workspace, and storage path |
| `tollgate daemon` | `--status`, `--standby`, `--replace`, `--no-standby`, `--listen [ADDR:PORT]`, `--cert <path>`, `--key <path>`, `--dump-openapi` | run the refresh + auto-switch loop with no TUI |
| `tollgate devices` | `--json`; `pair <name> [--control] [--sessions]`, `add <name> [--control] [--sessions]`, `revoke <name>`, `allow-sessions <name>` | list, pair, add, revoke, and grant sessions to the devices that may call the REST API |
| `tollgate usage` | `--json`, `--plain`, `--waybar`, `--watch <SECS>`, `--all`, `--account <id\|name>`, `--provider <source\|name>` | every account's quota windows and money meters across providers, from the caches; `--json` is the stable envelope ([local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md)) |
| `tollgate monitor` | `--json`; `list`, `add <id> --kind <kind> …`, `remove <id>`, `refresh [id] [--json]` | list, add, remove and fetch monitoring-only accounts in `~/.tollgate/monitors.toml` ([Providers](Providers#monitors)) |
| `tollgate api serve` | `--listen <ADDR>` | serve the read-only local agent API in the foreground, for a machine with no daemon ([Daemon](Daemon#local-agent-api)) |
| `tollgate api token` | `--show` | print the API token file's path, creating the token on first use; `--show` prints the token itself |
| `tollgate api url` | | print the API's base URL and a `curl` line for each door |
| `tollgate status --json` | `--all`, `--disabled` | print the daemon's status shape once, from disk, codex accounts included |
| `tollgate mcp` | | stdio MCP server; Claude Code launches this, not you |
| `tollgate completions <bash\|zsh\|fish\|install> [shell]` | | print or install a completion script |
| `tollgate hermes new <name>` | `--provider nous\|openrouter\|ollama-cloud`, `--model <id>`, `--pool`, `--env-key`, `--stdin`, `--no-key` | create a Hermes profile, a whole Hermes home of its own ([Hermes](Hermes)) |
| `tollgate hermes key <name>` | `--stdin` | set or replace an env-mode Hermes key (prompted hidden, never argv) |
| `tollgate hermes auth <name> add\|remove\|reset …` | `--type api-key\|oauth`, `--label`, `--no-browser`, `--timeout` | hand off to Hermes' own `hermes auth` on that home |
| `tollgate hermes list` | `--json` | the Hermes profiles with this month's spend |
| `tollgate hermes show <name>` | `--json`, `--check` | home, pool, spend and latest sessions; `--check` runs every launch guard and exits 1 on a refusal |
| `tollgate hermes pool <name> strategy <s>` | | `fill_first`, `round_robin`, `random` or `least_used`, on an idle pool home |
| `tollgate hermes delete <name>` | `--yes`, `--force` | remove a Hermes profile and its home |
| `tollgate herdr install` | `--key <spec>`, `--no-config`, `--yes` | install the [herdr](https://herdr.dev) plugin and bind a key to it |
| `tollgate herdr uninstall` | `--no-config`, `--yes` | remove that plugin and the config lines it added |
| `tollgate herdr link` | `--path <dir>` | link a local checkout's `herdr-plugin/` into herdr (the dev install); writes no herdr config |
| `tollgate herdr unlink` | | `herdr plugin unlink tollgate`; the files stay |
| `tollgate herdr config get <key>` | | print one herdr knob: `popup_width`, `pane_tag`, `tag_watch_secs`, `border_label`, `delegate_dot`, `delegate_row_text` |

`--theme <full\|compatible>` is global and forces a color depth for the TUI.

### Rules worth knowing

- **`start` argument order.** tollgate's own flags go before the profile name. Anything tollgate does not recognize is forwarded to `claude` verbatim, leading hyphens included. Use `--` for a spelling both programs own, like `--help`.
- **`start --with-fallback`** hands the session its own fallback chain. Refused by name when combined with `--isolated`, on Windows without symlink privilege, or for a non-OAuth account.
  - Also refused for an account outside the chain, when the chain has no other member, or when no `tollgate daemon` is running.
- **`start --auto`** picks the account instead of you naming one: the first fallback-chain member with headroom for the models the session will run ([Auto-switch](Auto-Switch#choosing-where-a-session-starts)). It takes the profile name's place, so separate `claude`'s own args with `--` whenever the first of them starts with a hyphen: `tollgate start --auto -- -p "hi"`. With no name in that slot there is nothing to tell a passthrough `-p` from a misspelled tollgate flag. Refused when the fallback chain is empty, or when no member of it can start.
- **`start --explain`** prints the account a start would launch on and the walk behind it, then exits without launching. It runs the refusals a real launch runs and dates every usage reading it judged, so a stale cache shows as one.
- **`start --isolated` keeps the session.** Its transcripts and session state are lifted into your global store before the throwaway runtime is discarded, so the run stays resumable and its tokens are counted. A hard kill (SIGKILL) skips that teardown; the next stale-runtime sweep lifts the tree into the global store before deleting it, so a killed session is rescued too. The `--rescue`/`--no-rescue` flags and the `auto_rescue` setting that used to decide this are gone; there is nothing to opt into and no way to opt out.
- **`delete`, `disable` and `limit-reset` want a TTY.** Each prompts `[y/N]`; on a non-TTY stdin they refuse unless you pass `--yes`. `--force` is the only way past `delete`'s live-session guard, and `--yes` alone does not override it.
- **Bare names span both rosters.** `tollgate <name>`, `delete` and `start` try the Claude Code profiles first, then the codex ones; an unknown name lists both (`available: … · codex: …`). `disable`, `enable`, `rolling-token` and `static-token` refuse a codex name as one ([Codex](Codex)), and `limit-reset` refuses a Claude Code name the same way.
- **`login <existing>`** re-authenticates in place. The chain slot, env block, and model settings survive; a browser re-login replaces the subscription login after a confirm. On an account that has an endpoint and a key it can still authenticate with, whether or not tollgate recognises the provider, a browser re-login replaces the subscription login alone and leaves the endpoint and key where they are: it is the stored OAuth chain you came to renew, and the key is what that account's inference actually runs on. An endpoint with nothing left behind it is cleared as before, so a re-login never leaves a bare endpoint standing in front of a fresh subscription login. An api-key re-login replaces the endpoint set, and so does any capture that brings one of the fields; a headless one (non-interactive stdin) with no `--base-url` reuses the stored endpoint instead of prompting. The stored OAuth chain survives an api-key re-login: it is what usage polling and `rolling-token` roll from.
- **`login <alibaba account>`** opens the Alibaba Model Studio console instead, because that plan's usage figures run on a console session its api key cannot stand in for. It replaces that session and nothing else: endpoint, api key and model settings all stay put. There is no confirm either, since re-running it is the routine repair. The window it captures is measured from your aliyun console sign-in ([Configuration](Configuration#the-alibaba-console-session)). Passing `--base-url` or `--api-key` still takes the ordinary api-key path. Starting one from nothing is two steps for that reason: give the account a Model Studio endpoint first (a Qwen preset on the Setup tab, or `--base-url` here), then run a bare `tollgate login <name>`. The console a session comes from is read off the endpoint, so a name that has none yet has no console to open.
- **`login` on a box with no browser** (or over ssh): the same login prints the link under `Browser didn't open? Use the url below to sign in` and prompts `Paste code here if prompted:`; open the link on any device, sign in, and paste the code the page shows back into the prompt (read echo-off). The browser callback still wins if it lands first. It is Claude Code's own "Browser didn't open?" path, so it mints exactly what a browser login mints: usage polling, plan tier, and `rolling-token` all work. The Setup tab's login modal has the same: <kbd>c</kbd> copies the link for another device to your local clipboard through the terminal (OSC 52), <kbd>p</kbd> turns its row into a code field: type or paste the code, <kbd>⏎</kbd> submits, <kbd>esc</kbd> brings the row back.
  - A non-TTY stdin is read as one line, for a driver that takes the link off stdout and feeds the code back to the same process; EOF just leaves the browser door open. The code is bound to that process, so it cannot be piped in from an earlier run.
- **`login --setup-token`** captures a `claude setup-token` mint (echo-off, or piped on stdin) as the profile's long-lived login.
  - That token never races tollgate's refresher. It engages only for a genuinely long-lived token; a rotating pair pasted here is ignored and called out on the card.
- **`rolling-token <profile>`** points the profile's sidecar at its own tollgate-private usage chain instead of a static mint: the daemon re-stamps it with the chain's current access token — full scopes, the account's `subscriptionType` and its `rateLimitTier`, but **no refresh token** — so sessions hold nothing rotatable (the split's whole point) while running a bearer the API recognizes as the plan it is, and plan-gated models work in a tollgate-managed session. A `claude setup-token` mint carries neither `user:profile` nor a subscription stamp and gets capped. Arming widens what a session's credential can reach, and the command says so. It needs the daemon running: the bearer dies in hours, and the daemon's scan is what re-stamps it before then. The mint it supersedes is preserved at `session-token.static.json`; the bare `tollgate static-token <profile>` — or a terminally dead usage chain — restores it rather than signing sessions out. The Setup tab's `token` row switches to an hours-scale `rolling · re-stamps in ~Nh` countdown and reads `rolling token stalled` if the re-stamping ever stops.
- **`static-token --clear`** is the way back out. A stored long-lived token is what every switch installs, so a plain `tollgate login <profile>` refreshes only the OAuth pair tollgate polls usage with, and never reaches a session. The login prints a note saying so. Clearing is the FULL exit: it drops the token, the preserved mint backup, and the `rolling_token` flag together (a lingering flag would have the daemon re-stamp a fresh sidecar over the removal, and a lingering backup keeps a year-scale credential on disk under a command that just said "cleared"), then relinks the live credentials when the profile is active. It is refused when clearing would strip the profile's last credential — a stored token (or preserved mint) with no other login behind it; a profile whose only rolling piece is the flag disarms regardless, since no credential is touched. An **api key counts as that other login**, so an api-key profile clears with no OAuth pair to fall back to: the live credentials are removed rather than relinked, Claude Code is signed out (on macOS, out of the Keychain too), and the profile carries on authenticating by api key. A flag-only profile has no login at all behind it, so the sign-out leaves nothing serving and the line says to log in before switching to it. Every line tollgate prints for the clear names which of those three happened.
- **`resume latest`** refuses rather than silently picking the second-newest when a live isolated session holds a newer one. `tollgate info` names where any transcript actually lives.
- **`daemon --listen`** (bare: `0.0.0.0:8453`, upstream clauth keeps 8443 so both can listen) also serves the REST API over TLS: the status feed, the OpenAPI document, the account switch, and device pairing. It is off unless asked for, and every route but the pairing needs the token of a device paired on this machine. `tollgate devices pair <name> [--control] [--sessions]` prints a one-time code (5 minutes, one use) for the device to enter, `tollgate devices add <name> [--control] [--sessions]` mints a token here and prints it once, `--control` on either lets that device switch accounts rather than only read, and `--sessions` (requires `--control`) grants it session creation once `[serve] session_creation` is on; `tollgate devices allow-sessions <name>` grants that later. `tollgate devices revoke <name>` refuses its next request, no restart needed. The token from before pairing keeps working as the device `legacy`. TLS comes from this host's lego certificate, or from `--cert`/`--key` when the host's own name resolves to no certificate. Full detail in [Daemon](Daemon).
- **`sessions --tokens`** parses every transcript in full to total tokens and cost. On a large store that takes a while, which is why it is opt-in.
- **`herdr install`** runs herdr's own installer and passes its preview and confirm straight through, then adds the two things a herdr plugin cannot declare for itself: the key that opens the tollgate dashboard, and the sidebar row that renders which account each pane burns. Both land in your herdr `config.toml`, appended after a diff and a `[y/N]`, and herdr validates the result before anything is written. Run it a second time and it adds nothing. `--yes` skips both prompts, herdr's install preview included, and is required on a non-TTY stdin. **`herdr uninstall`** reverses both halves behind one confirm; it removes only the blocks tollgate marked as its own. For either command `--no-config` covers the plugin and leaves `config.toml` untouched. `install` fetches the plugin at the newest `tollgate-v*` release tag and checks that the manifest there has id `tollgate` before herdr runs; the fork has published no such tag and its default branch still carries upstream's `clauth` plugin, so today `install` refuses and **`herdr link`** is the way in. Guest mode refuses `install`; `link` and `unlink` write no herdr config and work there. Nothing reinstalls the plugin in the background (self-update is compiled out). The whole surface: [herdr plugin](Herdr-Plugin).

### Environment variables

| Variable | Effect |
|----------|--------|
| `TOLLGATE_NO_COMPLETIONS=1` | skips the first-run completions prompt |
| `TOLLGATE_NO_API=1` | disables the daemon's REST listener whatever `--listen` says |
| `TOLLGATE_NO_LOCAL_API=1` | stops `tollgate daemon` from hosting the local agent API, without editing `local_api` in `profiles.toml`; `tollgate api serve` ignores it |
| `NO_COLOR` | any non-empty value turns the colour off in `tollgate usage`, as does `--plain` or output that is not a terminal |
| the variables your monitors and `billing_key_env` name | read by the daemon (and `monitor refresh`) at fetch time; scrubbed from every `claude` / `codex` session tollgate spawns |
| `CLAUDE_CONFIG_DIR` | scopes `which` and `start` to that config dir's credentials |
| `CODEX_HOME` | set by `tollgate start` on a codex profile to the session's own home, which is how `which` answers inside one; read by `login --codex` as the codex home to capture from, when it is not a tollgate session home |
| `SHELL` | how `completions install` detects your shell when you do not name one |
| `COLORTERM` | what the TUI auto-detects its color depth from: `truecolor` or `24bit` picks `full`, anything else `compatible`. `--theme` and the `theme` key in `profiles.toml` both beat it |
| `HERDR_CONFIG_PATH` | which config file `herdr install` writes into, matching how herdr itself reads the override |
| `HERDR_BIN_PATH` | which `herdr` binary tollgate runs, else `herdr` on `PATH`. herdr injects it into every pane process itself |

### Exit codes

`0` success, `1` failure, `2` usage error (unknown profile, bad flags). `tollgate daemon --status` exits `0` when a daemon is running and `1` when none is.
