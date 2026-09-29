<h1 align="center">tollgate</h1>

<p align="center">
  <a href="#coming-from-clauth-guest-mode">Guest mode</a> ·
  <a href="#install">Install</a> ·
  <a href="#quickstart">Quickstart</a> ·
  <a href="#providers">Providers</a> ·
  <a href="#local-agent-api">Agent API</a> ·
  <a href="#herdr">herdr</a> ·
  <a href="#waybar-and-omarchy">Waybar</a> ·
  <a href="wiki/Home.md">Wiki</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

tollgate monitors and manages your AI subscriptions and spending from one terminal. It keeps every Claude Code OAuth account and codex (ChatGPT) login switchable in a keypress, with the fallback chain that moves you off an exhausted account, and it watches the rest of what you pay for: Ollama Cloud, OpenRouter, Nous through Hermes, DeepSeek, Z.ai, MiniMax, Alibaba Model Studio and any Anthropic-compatible endpoint. Each account becomes one observation (quota windows, money meters as exact decimals, freshness, a typed failure) that the CLI, the TUI, a Waybar module, herdr pane tags, a local HTTP API and an MCP tool all read from the same cache, without spending quota. tollgate is a hard fork of [clauth](https://github.com/uwuclxdy/clauth); Linux, macOS and Windows.

![TUI demo: switching accounts with live usage bars](media/demo.gif)

> The recording predates the fork and shows upstream clauth's TUI; tollgate keeps its layout and adds the Usage tab's monitor cards and the Omarchy palette.

## Features

- **Switch** Claude Code accounts in one keypress or `tollgate <name>`: OAuth (Pro / Max / Team / Enterprise) or a custom API endpoint, plan tier detected for you
- **Monitor** live 5h / 7d rate-limit bars, provider balances and quotas, a token dashboard with API-equivalent cost, and the Claude status-incident feed
- **Auto-switch** down a fallback chain when an account hits its limit, with weekly-window and spend-ceiling gates; opted-in accounts queue their auto-start so their 5h windows open apart
- **Run in parallel**: several accounts at once in isolated config dirs, or a clean headless session with none of your global memory, plugins or hooks
- **From inside Claude**: the MCP plugin lets a live session list accounts, switch, or delegate a prompt to another account, and tells it when the account behind it changed
- **Headless**: `tollgate daemon` runs the refresh and auto-switch loop with no TUI, publishes `status.json`, and can serve that feed, the account switch and herdr panes to another machine over HTTPS with `--listen`
- **Codex too**: adopt or mint a ChatGPT login as a codex profile, run `codex` under it in its own `CODEX_HOME`, and rotate accounts between sessions on a separate codex chain
- **Hermes profiles**: run Hermes Agent under a named account in a home of its own, with a child `HOME` that keeps it away from `~/.claude`, a launch audit that refuses every route to Anthropic, and each home's month-to-date spend from its own `state.db`
- **Quality-of-life**: browse and resume past sessions under any account, per-profile model routing, `start --auto` to pick the account by the models a session will run, shell completions, multi-instance safe

## Coming from clauth: guest mode

> [!IMPORTANT]
> tollgate installs beside an existing upstream clauth. It has its own binary, data dir (`~/.tollgate`), env prefix (`TOLLGATE_`), daemon port (8453), herdr plugin id and Claude Code plugin, so neither tool overwrites the other's files.
>
> While `~/.clauth` exists, tollgate runs in **guest mode**: it shows clauth's accounts read-only (as `upstream:<name>`, from clauth's own status feed) and **refuses every global write that would change clauth's state** (Claude Code switches, `capture`, codex adoption) with one line naming guest mode. Its own additive entries still install: the Claude Code plugin, the `mcpServers.tollgate` wiring and `herdr install` touch only tollgate's keys, under clauth's lock. `tollgate start <profile>`, `tollgate usage`, monitors and the agent API work normally.
>
> **Importing clauth's accounts is not implemented yet.** Until it is, guest mode stays on for as long as clauth is installed, and tollgate cannot take over `~/.claude`. Details and known gaps: [Guest mode](wiki/Guest-Mode.md).

## Install

From source, with a Rust toolchain:

```bash
cargo install --locked --git https://github.com/abobreshov/clauth --branch feat/tollgate tollgate
```

or from a checkout:

```bash
git clone --branch feat/tollgate https://github.com/abobreshov/clauth tollgate
cd tollgate
cargo install --locked --path .
```

The `tollgate` package lives on the `feat/tollgate` branch; the fork's default branch still carries upstream's `clauth` package. Do not `cargo install tollgate` from crates.io: a crate of that name there is not this tool. **Self-update is disabled in this build**: nothing downloads or replaces the binary, so upgrade by re-running the cargo command. No prebuilt release is published yet. More: [Install](wiki/Install.md).

## Quickstart

```bash
tollgate                   # the TUI
tollgate usage             # every account's windows and money, grouped by provider
tollgate usage --json      # the stable envelope agents read: {schema_version, generated_at, guest_mode, accounts}
tollgate usage --waybar    # one {text, tooltip, class, percentage} line for a bar module
tollgate usage --watch 30  # repeat every 30 s
```

Add a Claude Code account (browser OAuth; a link to open on any device is printed too) and run it in its own config dir:

```bash
tollgate login work
tollgate start work
```

API-key profiles take an endpoint; leave `--api-key` off and the key is read echo-off:

```bash
tollgate login oll-main --base-url https://ollama.com          # Ollama Cloud
tollgate login or-main  --base-url https://openrouter.ai/api   # OpenRouter
```

Then on the Setup tab press <kbd>a</kbd> → `apply preset` and pick `Ollama-Cloud` or `OpenRouter`: the first adds three telemetry switches to the profile's env, the second pins the Claude tiers to OpenRouter's `~anthropic/claude-*-latest` aliases. Keys reach Claude Code only through `apiKeyHelper`.

Watch accounts you never launch with a monitor. Keys are named by environment variable only; tollgate stores the NAME and the daemon reads the value at fetch time:

```bash
tollgate monitor add oll  --kind ollama_cloud --api-key-env OLLAMA_API_KEY
tollgate monitor add or   --kind openrouter   --api-key-env OPENROUTER_API_KEY --billing-key-env OPENROUTER_MGMT_KEY
tollgate monitor add nous --kind nous          # Nous Portal, through Hermes' login in ~/.hermes
tollgate monitor refresh                       # fetch now; `tollgate daemon` polls them on its own
```

| Command | Does |
|---------|------|
| `tollgate` | open the TUI |
| `tollgate usage` | every account's quota windows and money meters, from the caches (`--json`, `--waybar`, `--plain`, `--watch`, `--account`, `--provider`, `--all`) |
| `tollgate monitor` | list, `add`, `remove`, `refresh` monitoring-only accounts in `~/.tollgate/monitors.toml` |
| `tollgate login <profile>` | add or re-authenticate an account: browser OAuth, or an API key with `--base-url` |
| `tollgate start <profile>` | run `claude` (or `codex`) under that account in its own config dir |
| `tollgate switch <name>` | switch the global account (refused in guest mode) |
| `tollgate list` / `tollgate which` | account table with cached usage / who owns this session |
| `tollgate daemon` | headless refresh, monitor polling, auto-switch, the local agent API; `--listen` adds the TLS REST API on `0.0.0.0:8453` |
| `tollgate api serve` / `token` / `url` | run the local agent API without a daemon, print its token path, print its URL and `curl` lines |
| `tollgate hermes new` / `show` / `list` | Hermes Agent homes as profiles ([below](#hermes-profiles)) |
| `tollgate herdr install` / `link` | set up the herdr plugin |

Every command and flag: [Quickstart](wiki/Quickstart.md).

## Providers

| Provider | `source` | Auth | Measured | Chain-eligible windows |
|----------|----------|------|----------|------------------------|
| Claude Pro / Max / Team / Enterprise | `anthropic_oauth` | subscription OAuth | 5h, 7d, per-model 7d, extra-usage spend | 5h, 7d |
| codex (ChatGPT) | `codex` | subscription OAuth | 5h, 7d, banked usage-limit resets | 5h, 7d (codex chain) |
| Ollama Cloud | `ollama_cloud` | api key | legacy 5h / 7d, or a monthly pool; 4-week spend; per-model requests | 5h, 7d (legacy only) |
| Ollama daemon | `ollama` | the daemon's own login | nothing (no usage route) | none |
| OpenRouter | `openrouter` | api key, optional management key | wallet, daily / weekly / monthly / lifetime spend, key cap, BYOK, free-model requests | none |
| Nous Portal (monitor) | `nous` | Hermes' OAuth login (read while unexpired), or api key | monthly credits; subscription, top-up, rollover balances | none |
| DeepSeek | `deepseek` | api key | balance per currency | none |
| Z.ai | `zai` | api key | 5h / 7d / 30d limits, per-model tokens | 5h, 7d |
| MiniMax | `minimax` | api key | Token Plan 5h and 7d | 5h, 7d |
| Alibaba Model Studio | `alibaba` | api key + console session | 7d (5h when reported), tier | 5h, 7d |
| any other endpoint | `generic` | api key | best-effort scan | none |
| Hermes profile (a home tollgate launches) | `hermes` | Hermes' own (tollgate holds none) | month-to-date spend from the home's `state.db`, Nous cooldowns | none |
| upstream clauth (guest mode) | `upstream_clauth` | read only | what clauth's feed carries | none (5h / 7d are flagged, but an upstream account never joins a chain) |

Only chain-eligible windows can move the fallback chain; monitors and upstream accounts never join one, whatever their windows' `chain_eligible` flag says. Setup, what is read, and each provider's limitations: [Providers](wiki/Providers.md).

## Hermes profiles

tollgate launches [Hermes Agent](wiki/Hermes.md) under a named profile, as it does `claude` and `codex`. Each profile is a whole Hermes home, `~/.tollgate/profiles/<name>/hermes-home`, started with a child `HOME` that holds only links to `~/.gitconfig`, `~/.config/git` and `~/.ssh`, so the Hermes it launches cannot reach `~/.claude`, `~/.clauth`, `~/.codex` or `~/.hermes`. Hermes owns its credentials; tollgate writes one line of the home's `.env`.

```sh
tollgate hermes new or-main --provider openrouter   # prompts for the key, input hidden
tollgate start or-main -- chat -q "hello"
tollgate hermes show or-main --check                # every launch guard's verdict
tollgate hermes list                                # this month's spend per home
```

Every launch audits the home with Hermes' own parsers and refuses any route to Anthropic, an unpinned auxiliary provider and a bulk secrets source. Each home's spend comes from its own `state.db` (read with `sqlite3 -readonly`) as a `hermes:<name>` account. Switching is a relaunch. [Hermes](wiki/Hermes.md) has the details.

## Local agent API

A read-only JSON API for agents on this machine, hosted by `tollgate daemon` (or `tollgate api serve`). It reads the caches and never calls a provider, and no route returns a credential.

| Door | Address | Auth |
|------|---------|------|
| loopback HTTP | `http://127.0.0.1:8454` | `Authorization: Bearer` + the contents of `~/.tollgate/api-token` |
| unix socket | `~/.tollgate/api.sock` | none (0600, your user only) |
| MCP | the `usage` tool of `tollgate mcp` | none |

Routes: `GET /v1/health`, `/v1/accounts`, `/v1/accounts/{id}`, `/v1/usage`, `/v1/providers`, `/v1/status`, `/v1/openapi.json`; `/v1/accounts` and `/v1/usage` filter by `account`, `provider` and `all=1`. Non-loopback addresses are refused, since there is no TLS; `local_api = { enabled, listen }` in `profiles.toml` configures it.

```sh
curl -s --unix-socket ~/.tollgate/api.sock http://localhost/v1/usage
curl -s -H "Authorization: Bearer $(tollgate api token --show)" http://127.0.0.1:8454/v1/accounts
```

The MCP server (`tollgate mcp`, installed as the Claude Code plugin `tollgate@tollgate` from the TUI's Plugin tab) also keeps clauth's `profiles`, `switch_profile`, `delegate` and `monitor` tools. Full reference: [docs/agent-api.md](docs/agent-api.md), [Claude Code plugin](wiki/Claude-Code-Plugin.md).

## herdr

The [herdr](https://herdr.dev) plugin (id `tollgate`) opens the dashboard in a popup (`tollgate.open`) or straight on the Usage tab (`tollgate.usage`), and tags every pane with the account it burns and that account's lead figure: `work 42%`, `cx-work 23%w`, `nous-main 64% mo`, `or-main $13.67`, with `⏸` stale, `⚠` HIGH and `‼` CRITICAL marks. The severity class rides in a second token, `$tollgate_severity`, for sidebar rules.

The fork has published no `tollgate-v*` release yet, so `tollgate herdr install` refuses; link a checkout instead (this writes no herdr config, so paste the key and rows from the wiki):

```sh
tollgate herdr link          # the working directory's checkout, or --path <dir>
```

Keys, rows, knobs: [herdr plugin](wiki/Herdr-Plugin.md).

## Waybar and Omarchy

`tollgate usage --waybar` prints the lead account (the active profile, else another active account such as the active codex or upstream one, else the worst-graded) as `text`, every account in `tooltip`, the severity as `class` (`ok`, `mid`, `high`, `critical`, or `none`) and the lead window's percent as `percentage`, a key left out when the lead account has no live window. In `~/.config/waybar/config.jsonc` (Omarchy's default location), define the module and add `"custom/tollgate"` to one of `modules-left`, `modules-center` or `modules-right`:

```jsonc
"custom/tollgate": {
  "exec": "tollgate usage --waybar",
  "return-type": "json",
  "interval": 60,
  "tooltip": true
}
```

Waybar sets `class` on the module, so `~/.config/waybar/style.css` can colour it:

```css
#custom-tollgate.high     { color: #fab387; }
#custom-tollgate.critical { color: #f38ba8; }
```

Swap in your theme's colours. The figures come from the caches, so keep `tollgate daemon` running for them to stay fresh. `--account` and `--provider` narrow the module to one account or provider.

**Palette.** `palette = "auto"` (the default, in `~/.tollgate/profiles.toml`) colours the TUI and `tollgate usage` from the running Omarchy theme's `colors.toml` and reloads within about 2 s of a theme change; with no Omarchy theme it uses Catppuccin Mocha. `palette = "omarchy"` or `"catppuccin"` pins one, `theme = "full" | "compatible"` still picks the colour depth, and every severity also carries a word, so colour never carries meaning alone. [Configuration](wiki/Configuration.md#palette).

## Documentation

| Page | Covers |
|------|--------|
| [Install](wiki/Install.md) | building from source, why self-update is off, completions |
| [Quickstart](wiki/Quickstart.md) | first run, every command, flag and env var |
| [Guest mode](wiki/Guest-Mode.md) | running beside upstream clauth |
| [Providers](wiki/Providers.md) | every provider and monitor kind in full |
| [Interface and keys](wiki/Interface-And-Keys.md) | the tabs, every keybinding, the action menus |
| [Configuration](wiki/Configuration.md) | `profiles.toml`, `config.toml`, `monitors.toml`, palette, local API, presets, storage |
| [Auto-switch](wiki/Auto-Switch.md) | thresholds, exclusion rules, burn-aware mode, spend ceilings |
| [Daemon](wiki/Daemon.md) | `tollgate daemon`, the local agent API, the REST API, `status.json` |
| [Claude Code plugin](wiki/Claude-Code-Plugin.md) | the MCP server and `delegate` |
| [herdr plugin](wiki/Herdr-Plugin.md) | the popup, the keys, the usage-aware pane tag |
| [Tokens and cost](wiki/Tokens-And-Cost.md) | the token dashboard and its cost figure |
| [Codex](wiki/Codex.md) | ChatGPT logins as codex profiles |
| [Hermes](wiki/Hermes.md) | Hermes Agent homes as profiles: isolation, guards, spend, pool |
| [Security](wiki/Security.md) | where credentials live, secrets rules, the API token |
| [FAQ](wiki/FAQ.md) | common questions and troubleshooting |

## Development

```bash
cargo build
cargo clippy --all-targets
cargo test
```

`cargo test showcase -- --ignored --nocapture` drives the real interactive TUI on fake data against a throwaway home dir (no network, never compiled into the binary). Tests are hermetic: provider and monitor fetches run against canned replies, never the network. The design and its status: [docs/multi-provider-redesign-plan.md](docs/multi-provider-redesign-plan.md).

## Security

tollgate stores live OAuth tokens and API keys under `~/.tollgate` (0600 files, 0700 dirs), keeps monitoring keys out of every file by storing only environment variable names, and scrubs those variables from the sessions it spawns. [SECURITY.md](SECURITY.md) lists where credentials live, every host tollgate contacts and how to switch each behaviour off. Report a vulnerability privately through the fork's [security advisories](https://github.com/abobreshov/clauth/security/advisories/new).

## Credits and license

tollgate is a fork of [clauth](https://github.com/uwuclxdy/clauth) by uwuclxdy, used under the MIT license; clauth's copyright notice is kept in [LICENSE](LICENSE) beside the fork's. It is not endorsed by or affiliated with the clauth project. The usage cards, Waybar output and palette follow the designs of [ai-usagebar](https://github.com/akitaonrails/ai-usagebar) and [omarchy-agent-bar](https://github.com/othavi0/omarchy-agent-bar). Pricing for the Tokens tab still comes from [ai-pricelog](https://github.com/uwuclxdy/ai-pricelog).

MIT.
