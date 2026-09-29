# tollgate wiki

**Monitor and manage AI subscriptions and spending: Claude Code and codex accounts, API-key providers (Ollama Cloud, OpenRouter, DeepSeek, Z.ai, MiniMax, Alibaba), Nous through Hermes, from one CLI, TUI and local API.**

tollgate is a hard fork of [clauth](https://github.com/uwuclxdy/clauth) (MIT). The [README](https://github.com/abobreshov/clauth/blob/feat/tollgate/README.md) is the tour. This wiki is the reference.

## Pages

| Page | Answers |
|------|---------|
| [Install](Install) | building from source, the install script, why self-update is off, shell completions |
| [Quickstart](Quickstart) | first run, profiles and monitors, every CLI command and flag |
| [Guest mode](Guest-Mode) | running beside an installed upstream clauth: what works, what is refused, the known gaps |
| [Providers](Providers) | every provider: how to add it, what tollgate reads, what feeds the chain, the limitations; monitors in full |
| [Interface and keys](Interface-And-Keys) | the eight tabs, every keybinding, the action menus |
| [Configuration](Configuration) | `profiles.toml`, per-profile `config.toml`, `monitors.toml`, palette, local API, presets, storage layout |
| [Auto-switch](Auto-Switch) | the fallback chain: thresholds, gates, burn-aware mode, spend ceilings |
| [Codex](Codex) | OpenAI codex accounts: adopt or mint a ChatGPT login, run `codex` under a profile, the codex chain |
| [Daemon](Daemon) | `tollgate daemon`, the local agent API, the TLS REST API, and the `status.json` read contract |
| [Claude Code plugin](Claude-Code-Plugin) | the MCP server, its five tools, `delegate` in full |
| [herdr plugin](Herdr-Plugin) | the tollgate popup in herdr, the keys, the usage-aware pane tag |
| [Tokens and cost](Tokens-And-Cost) | where the token dashboard reads from, what the cost figure means |
| [Security](Security) | where credentials live, what a switch touches, secrets rules, the API token |
| [FAQ](FAQ) | common questions and what to check when something misbehaves |

## Quick answers

- Everything lives under `~/.tollgate`: `profiles.toml` (global), `codex-profiles.toml` (the codex roster), `monitors.toml` (monitoring-only accounts) and `profiles/<name>/config.toml` (per account). All are hand-editable. Nothing under upstream's `~/.clauth` is written.
- `tollgate usage` prints every account's windows and money from the caches; `--json` is the envelope agents read, `--waybar` a bar-module line.
- Agents read the same data from the local API on `127.0.0.1:8454` (bearer from `~/.tollgate/api-token`) or the unix socket `~/.tollgate/api.sock`, or through the MCP `usage` tool ([local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md)).
- Keys for monitors are stored by environment variable NAME only; the value is read at fetch time.
- A switch rewrites `~/.claude/.credentials.json`, parts of `~/.claude/settings.json`, and the stale identity block in `~/.claude.json`. In [guest mode](Guest-Mode) no switch runs at all.
- `tollgate start <profile>` runs a session in its own config dir, so it never disturbs the account your global `claude` is on, and works in guest mode.
- Trust model, network activity, and vulnerability reporting: [SECURITY.md](https://github.com/abobreshov/clauth/blob/feat/tollgate/SECURITY.md).
