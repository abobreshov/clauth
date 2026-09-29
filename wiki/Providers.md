# Providers

Every account tollgate knows becomes one observation: its quota windows (percent used, reset time), its money meters (balances, spend, caps, budgets, as exact decimals), how fresh the reading is, and a typed failure when there is one. `tollgate usage`, the TUI's Usage tab, the local agent API and the MCP `usage` tool all read the same observations from disk; none of them fetches.

An account reaches tollgate one of three ways:

| Origin | Id | Where it comes from | Can tollgate launch it? |
|--------|----|---------------------|-------------------------|
| profile | `claude:<name>` | `~/.tollgate/profiles.toml`: a Claude Code OAuth login or an API-key endpoint | yes, `tollgate start` |
| codex profile | `codex:<name>` | `~/.tollgate/codex-profiles.toml` ([Codex](Codex)) | yes, `tollgate start` |
| monitor | `monitor:<id>` | `~/.tollgate/monitors.toml` ([below](Providers#monitors)) | no, read only |
| upstream | `upstream:<name>` | upstream clauth's own status feed, in [guest mode](Guest-Mode) only | no, read only |

## At a glance

| Provider | `source` | Auth kind | As a profile | As a monitor | What is measured | Chain-eligible windows |
|----------|----------|-----------|--------------|--------------|------------------|------------------------|
| Anthropic (Claude Pro / Max / Team / Enterprise) | `anthropic_oauth` | subscription | yes | no | 5h, 7d, per-model 7d windows; extra-usage spend and cap | 5h, 7d |
| OpenAI codex (ChatGPT login) | `codex` | subscription | codex profile | no | 5h, 7d; banked usage-limit resets | 5h, 7d, on the codex chain |
| Ollama Cloud | `ollama_cloud` | api key | yes | `ollama_cloud`, or `provider` with `OllamaCloud` | legacy plans: 5h and 7d; new pricing: one `month` window; spend over the last 4 weeks; per-model request counts | 5h, 7d (legacy only); `month` never |
| Ollama daemon (`127.0.0.1:11434`) | `ollama` | native login | yes | no | nothing: the daemon has no usage route | none |
| OpenRouter | `openrouter` | api key, optional management key | yes | `openrouter` | wallet balance; key spend today / week / month / lifetime; key cap; BYOK spend; free-model daily requests | none (`free_daily` is display only) |
| Nous Portal, through Hermes | `nous` | Hermes' login, or api key | no | `nous` | monthly credits window; subscription, top-up, rollover and total-usable balances | none |
| DeepSeek | `deepseek` | api key | yes | `provider` | balance per currency | none |
| Z.ai | `zai` | api key | yes | `provider` | limit windows (5h / 7d / 30d), per-tool rows, plan level, 7-day per-model tokens | 5h, 7d |
| MiniMax | `minimax` | api key | yes | `provider` | Token Plan 5h interval and 7d window, per-bucket rows | 5h, 7d |
| Alibaba Model Studio | `alibaba` | api key + console session | yes | no | 7d (and 5h when reported) with absolute allowances, tier, subscription status | 5h, 7d |
| Any other endpoint | `generic` | api key | yes | no | whatever the best-effort scan finds | none (a best-effort window never feeds the chain) |
| Upstream clauth | `upstream_clauth` | read only | no | no | the 5h / 7d figures upstream's feed carries | none (never a chain member) |

"Chain-eligible" is the `chain_eligible` flag on each window in the JSON: only those windows can make a fallback chain move ([Auto-switch](Auto-Switch)). A monitor is never a chain member, whatever its windows say.

`GET /v1/providers` on the [local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md) also lists `hermes`, `grok` and `antigravity`. Those sources are named in the catalog so agents can plan for them; no reader for them ships in 0.1.0.

## Anthropic

Claude Pro, Max, Team and Enterprise logins. Leave `base_url` blank and add the account with `tollgate capture <name>` or `tollgate login <name>` ([Quickstart](Quickstart)). tollgate polls `api.anthropic.com/api/oauth/usage` with the profile's access token on the refresh interval and reads the plan tier from `/api/oauth/profile`. Behaviour is unchanged from clauth: see [Configuration](Configuration#account-types) and [Auto-switch](Auto-Switch).

In guest mode a profile is still created by `tollgate login`, but it is never linked into `~/.claude`; run it with `tollgate start <name>`.

## OpenAI codex

ChatGPT logins as codex profiles, polled at `chatgpt.com/backend-api/wham/usage`. Everything is on [Codex](Codex). In guest mode `login --codex` without `--browser` is refused, since it would replace `~/.codex/auth.json`.

## Ollama Cloud

As a profile, Claude Code talks to `https://ollama.com` directly with an inference key; nothing runs locally.

```bash
tollgate login oll-main --base-url https://ollama.com   # key prompted echo-off
```

Then apply the `Ollama-Cloud` preset and pick models ([Configuration](Configuration#setting-up-an-ollama-cloud-account)). Keys are minted at <https://ollama.com/settings/keys>.

As a monitor, with the key in an environment variable:

```bash
tollgate monitor add oll --kind ollama_cloud --api-key-env OLLAMA_API_KEY
```

**What it reads.** `GET https://ollama.com/api/usage` with the key as Bearer. The route is undocumented; it is what the ollama.com settings page calls.

- A legacy plan reports `limits.session` and `limits.weekly`, which become the `5h` and `7d` windows and feed the chain.
- A new-pricing plan reports `limits.monthly` alone, a `month` window for the dollar pool. It is never chain-eligible.
- `usage` is a fraction; it is shown unclamped (a window can read above 100 %). A window with no `usage` reads `usage not reported`, never 0 %.
- Per-model `request_count`s become a breakdown per window. `activity.cost` is kept as an exact USD spend meter for its period (the last 4 weeks).
- 401 is `auth_required`; a 429 naming a usage limit is `quota_exhausted`; any other 429 is `rate_limited`; a body in neither shape is `invalid_response`.

**Limitations.** The API publishes no reset time for any window, so there is no countdown and no elapsed marker. A month at or past 100 % reads `included credits used up` and grades at most HIGH, because extra use draws on purchased credits or team billing, which the API does not show. The plan label (`pro-legacy`, `pro`, `max`, `team`) is not in the response. Personal and team keys are separate accounts; give each its own profile or monitor.

**The local daemon.** A profile pointed at `http://127.0.0.1:11434` or `http://localhost:11434` is typed as the Ollama daemon: it signs requests with its own machine-global `ollama signin` key, which tollgate never reads, and has no usage route. Its panel reads `usage needs an ollama.com API key` and nothing is requested. To watch that account, add an `ollama_cloud` monitor with an ollama.com key.

## OpenRouter

As a profile:

```bash
tollgate login or-main --base-url https://openrouter.ai/api
```

Then apply the `OpenRouter` preset, which pins the Claude tiers to `~anthropic/claude-*-latest` and sets `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` ([Configuration](Configuration#presets)). The key reaches Claude Code only through `apiKeyHelper`. Keys: <https://openrouter.ai/settings/keys>.

As a monitor, with an inference key, a management key, or both:

```bash
tollgate monitor add or --kind openrouter --api-key-env OPENROUTER_API_KEY --billing-key-env OPENROUTER_MGMT_KEY
```

A profile can use a management key too: `billing_key_env = "OPENROUTER_MGMT_KEY"` in its `config.toml` ([Configuration](Configuration#configtoml)).

**What it reads.**

1. `GET /api/v1/key` first. It is the auth probe (a 401 here is the only thing that marks the inference key dead) and carries the key's own figures: `usage_daily` / `usage_weekly` / `usage_monthly` / lifetime `usage`, the key cap (`limit`, `limit_remaining`, `limit_reset`), BYOK spend and `free_model_daily_requests`.
2. `GET /api/v1/credits` second, for the wallet: `total_credits − total_usage`, negative once the account is overdrawn. It is read with the management key when one is named, else with the inference key. OpenRouter documents it as management-key only, so any failure there (403, 404, 401, 5xx, network) drops the wallet meter alone, with a note, and never fails the reading.

Money is parsed from the raw JSON numbers into exact decimals, so sub-cent spend and a negative wallet survive. With only a management key, a monitor reads `/api/v1/credits` alone.

**Limitations.** The wallet belongs to whoever owns the key that read it; tollgate does not yet check that a management key and an inference key belong to the same account. A key's guardrails are invisible to an inference key. `free_daily` is shown but never feeds the chain.

## Nous Portal, through Hermes

Nous inference is OpenAI-wire only, so it cannot run Claude Code; tollgate watches it as a monitor.

```bash
tollgate monitor add nous --kind nous                               # Hermes home ~/.hermes
tollgate monitor add nous-work --kind nous --hermes-home ~/work/.hermes
```

**What it reads.** Hermes owns the Nous OAuth chain and its refresh tokens are single-use, so tollgate borrows only the access token:

- One plain read of `<hermes_home>/auth.json`, taking `providers.nous.access_token` and `expires_at` and nothing else. The refresh token and the credential pool are never deserialized. No lock, no write, and Hermes' own resolver (which may refresh) is never called.
- The token is used only while it is more than 30 s from expiry. An expired or undated token is `auth_required` ("run hermes to refresh") and no request is sent.
- `GET https://portal.nousresearch.com/api/oauth/account` with that token, on a fixed allowlist, with no redirects followed and a 2 MiB response cap.

**Mapping.** A `subscription` window labelled `Monthly credits`, `used_pct = (monthly_credits − credits_remaining) / monthly_credits × 100`, unclamped, resetting at the period end, never chain-eligible. Money meters: `subscription` (limit = monthly credits), `top_up` (purchased credits), `rollover` and `total_usable`; the last two are not additive and are never summed. `paid_access = false` reads as `quota_exhausted` ("credits depleted"). 401 / 403 are `auth_required`, 429 is `rate_limited` with its retry time.

**With a Nous API key** (`--api-key-env NOUS_API_KEY`) there is no balance endpoint to read, so the monitor reports money unavailable without fetching.

**Limitations.** Hermes must have run recently enough to hold an unexpired token; the reading goes stale in between. An unreadable `auth.json` (mid-write) keeps the last reading, marked stale. There is no Hermes harness yet: tollgate does not launch Hermes, and a herdr `hermes` pane is tagged from the Nous monitor that reads Hermes' own login.

## DeepSeek, Z.ai, MiniMax

API-key profiles, with a built-in preset each. The same readers serve a `provider` monitor:

```bash
tollgate login ds --base-url https://api.deepseek.com/anthropic
tollgate monitor add zai --kind provider --provider Zai --api-key-env ZAI_API_KEY
```

| Provider | Reads | Shows |
|----------|-------|-------|
| DeepSeek | `GET https://api.deepseek.com/user/balance` | balance rows per currency: api balance, granted, topped up |
| Z.ai | `https://api.z.ai/api/monitor/usage/quota/limit` and, best effort, `…/model-usage` | percentage windows (5h / 7d / 30d), per-tool rows, plan level, 7-day per-model token totals |
| MiniMax | `GET https://api.minimax.io/v1/token_plan/remains` | the `general` bucket's 5h interval and 7d window as bars, every bucket as a row |

MiniMax's mainland-China endpoint is a separate account on another host and falls to the best-effort scan. Details and key pages: [Configuration](Configuration#third-party-usage-data).

## Alibaba Model Studio

Profiles only. Its api key cannot read its own quota, so usage runs on a separate console session that `tollgate login <name>` captures ([Configuration](Configuration#the-alibaba-console-session)). A `provider` monitor naming Alibaba is refused for that reason.

## Any other endpoint

A profile on an endpoint tollgate does not recognise is scanned best-effort: a short list of usage paths on the same origin its key already authorises, whatever percentage, fraction-left or balance shapes come back. The panel invites a report, since the shape is guessed. Its windows never feed the chain.

## Upstream clauth

In [guest mode](Guest-Mode) tollgate reads upstream clauth's non-secret status feed, `~/.clauth/status.json` (schema 1 or 2), and shows each of its profiles as `upstream:<name>`, labelled `<name> (clauth)`. Nothing else under `~/.clauth` is read and nothing there is written. The feed is only as fresh as upstream's daemon keeps it; with no upstream daemon running there may be no feed, and so no upstream accounts.

## Monitors

`tollgate monitor add <id> --kind <kind> [flags]` writes one `[[monitor]]` table to `~/.tollgate/monitors.toml`; the file's keys are on [Configuration](Configuration#monitorstoml).

| Flag | Meaning |
|------|---------|
| `--kind <kind>` | `nous`, `ollama_cloud`, `openrouter`, or `provider` (required) |
| `--provider <name>` | with `--kind provider`: `DeepSeek`, `Zai`, `MiniMax`, `OpenRouter` or `OllamaCloud` |
| `--label <label>` | the name shown; the id when omitted |
| `--api-key-env <VAR>` | NAME of the env var holding the api key |
| `--billing-key-env <VAR>` | NAME of the env var holding a monitoring-only key, preferred for reads |
| `--hermes-home <path>` | with `--kind nous`: the Hermes home (default `~/.hermes`) |
| `--budget-usd-month <USD>` | a monthly budget, graded like a cap |
| `--alert-pct <PCT>` | one desktop notification past this share of the budget, or of the worst window |
| `--ttl-secs <SECS>` | refresh interval, minimum 30, default 90 |
| `--disabled` | add it disabled |

| Command | Does |
|---------|------|
| `tollgate monitor` / `tollgate monitor list` | each monitor, its cache state, and whether its named variables are set (`MISSING` when not); `--json` on the bare form |
| `tollgate monitor refresh [id]` | fetch now, ignoring the interval: every enabled monitor, or that one even when disabled; `--json` prints the `usage --json` envelope of what was refreshed |
| `tollgate monitor remove <id>` | drop the table and its cache |

**How polling works.** The daemon polls due monitors on its tick, on a detached thread, each on its own `ttl_secs`. Each monitor caches its last reading in `~/.tollgate/monitors/<id>.json` (0600, never a credential). A 429 holds the monitor for at least 5 minutes, or longer when the server says so, and `monitor refresh` respects the hold. A failed fetch keeps the last reading, marked stale, for up to 7 days. Changing a monitor's kind, variable names or Hermes home discards its old cache. Two refreshes of one monitor never run at once.

**Where keys come from.** Only in the environment of the process that fetches: the daemon, or the shell running `monitor refresh`. Start the daemon from an environment that exports the variables (a systemd unit's `EnvironmentFile=`, a launchd `EnvironmentVariables` block). Every variable a monitor or a profile's `billing_key_env` names is scrubbed from the `claude` and `codex` sessions tollgate spawns and from the shunt gateway, except a monitor's `api_key_env`, which is an inference key the gateway may need.
