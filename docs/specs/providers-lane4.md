# Spec: native CLI monitors, API-key monitors and the secret store (lane 4: P4b/P4c + key monitors)

Status: **draft for implementation**, 2026-09-29, `feat/tollgate` @ `60e4aee5` (tollgate 0.1.0). Authority:
plan v3.1 §4.2 (sources, allowlist), §4.3 (credentials by env NAME, scrub lists), §4.7, §10 "Not yet"
row P4b/P4c; `docs/tollgate-code-review-0.1.0.md` (guest rules B1/B2 stand). Evidence: the verified
research + fact-check of 2026-09-29 (UNVERIFIED items are listed as gates in §9). Paths are relative to the
repo root, and `~` means the test home. No test and no build step may touch the real `$HOME` or the network.

Machine facts (metadata only, from `ls`/`stat`/`--version`):
- **Grok 1.0.44.** `~/.grok/auth.json` is 0600 and 1764 B. `auth.json.lock` is 0644. Both have mtime 16:16, and a grok process is running.
- **agy 1.2.13** (`~/.local/bin/agy`). The login is keyring-only: gnome-keyring owns `org.freedesktop.secrets`, and `~/.gemini/antigravity-cli/antigravity-oauth-token` does not exist.
- **codex 0.157.1.** `~/.codex/auth.json` is a regular file, 0600, 4049 B, mtime Sep 26, and the app-server daemon is running.
- **Hermes.** `~/.hermes` holds **no `auth.json`**, so the existing Hermes-token Nous monitor cannot read anything until the owner logs Hermes into Nous. Hermes has no `profiles/` either.
- **Tollgate.** `~/.tollgate` exists with no `monitors.toml` and no `secrets.env`, and no tollgate daemon is running.
- **Upstream clauth.** Its herdr plugin (`watch-profile.sh`) and 4 `clauth mcp` processes are live. Guest mode is on.
- Claude Code 2.1.283.

## 1. Goal and non-goals

**Goals**
1. Three **native monitors** read another CLI's own login, **read-only and borrowed**:
   - `kind = "grok"` reads `~/.grok/auth.json`.
   - `kind = "antigravity"` reads agy's Secret Service item.
   - `kind = "codex_native"` reads `~/.codex/auth.json` when that file is not a tollgate or clauth store.

   Every native monitor follows the D10 reader rule from `nous.rs:1-21`:
   - Read only the access token and its expiry.
   - An expired or undatable token becomes `AuthRequired` with **no network call**.
   - Never refresh the token, take the CLI's lock, write its store, or spawn the CLI. The one opt-in exception is §4.4.
2. Three **API-key monitors**:
   - `kind = "openai"`: key health and rate-limit headroom from free-read headers. An optional admin key (`billing_key_env`) adds month-to-date cost.
   - `kind = "google_ai"`: key health only, labelled *spend not available for API keys*.
   - `kind = "nous"` gains an API-key mode: no call by default, or an opt-in zero-credit `:free` probe.
3. A **secret store**: `tollgate secret set|list|rm`, backed by `~/.tollgate/secrets.env`.
   - The file is 0600, written atomically and owner-checked.
   - It is visible only to tollgate's own monitor and provider fetches.
   - Every stored name joins the child scrub lists.
   - Monitors keep naming keys by env var NAME.
4. `tollgate monitor add <preset>` shortcuts, and `tollgate monitor detect` (no network, no CLI spawn).

**Non-goals**
- Refreshing any borrowed token, including agy's Google token (no client secret ships in tollgate).
- ai-usagebar's grok ACP fallback (`grok agent --no-leader stdio`).
- The xAI Management API and Grok banked resets (`GetRemainingResets`).
- OpenAI or Gemini inference micro-calls for headroom, since they cost money.
- GCP Cloud Billing and Monitoring.
- Proxying inference.
- macOS and Windows keyring reads for agy: `Unavailable` "Linux only in 0.1".
- Changing the OpenRouter source, except optionally surfacing `include_byok_in_limit`.
- A Hermes harness (H-1a…).

## 2. Surface

### 2.1 CLI (`src/cli.rs` gains `Command::Secret`, `monitor.rs` gains `Detect`)

```
tollgate secret set <NAME> [--stdin] [--force]     # value: echo-off prompt (rpassword) or one stdin line
tollgate secret list [--json]                      # names only
tollgate secret rm <NAME> [--yes]
tollgate [--prefer-store] <any command>            # root flag; also TOLLGATE_PREFER_STORE=1
tollgate monitor add <PRESET|ID> [--kind K] [--id ID] [...existing flags] [--auth-entry E]
                     [--tool-home PATH] [--probe] [--probe-model M] [--via keyring|cli]
                     [--admin-key-env VAR]         # alias of --billing-key-env for kind openai
tollgate monitor detect [--json] [--explain] [--apply [--yes]]
tollgate monitor refresh [<id>] [--json] [--capture DIR]
```

**The secret value never reaches argv.**
- `secret set` takes exactly one positional.
- A second positional is clap's usage error (exit 2).
- A NAME containing `=` is refused with exit 2: `tollgate: pass only the NAME; the value is prompted with echo off`.
- On a non-TTY stdin without `--stdin`, the command exits 2 with `tollgate: no terminal to prompt on; pipe the value with --stdin`.
- `--stdin` reads one line, at most 4096 bytes, and trims the trailing `\r?\n`.

**Monitor add forms.**
- Without `--kind`, the positional must be a preset (§4.7), and `--id` overrides that preset's id.
- With `--kind`, today's form is unchanged (`monitor/cli.rs:53-116`; `kind` becomes `Option`).
- An unknown preset without `--kind` exits 2 with `tollgate: '<x>' is not a preset (grok, antigravity, codex-native, openai, google-ai, nous, nous-key, openrouter); pass --kind to add it by id`.

**Other flags.**
- `--capture DIR` asks `refresh` to write a **shape dump** for each response (§4.8) into a 0700 directory, with each file at 0600.
- `--explain` makes `detect` print each store's parse shape (field spellings, expiry format, entry count), never a value.

### 2.2 Messages (exact; `<…>` substituted)
- **Prompt:** `Value for <NAME> (input hidden): `.
- **Stored:** `tollgate: stored <NAME> (<len> chars<, prefix>) in ~/.tollgate/secrets.env; the daemon picks it up within 10s`.
  - `<prefix>` is one of `sk-or-v1-…`, `sk-proj-…`, `sk-admin-…`, `sk-…`, `AIza…`, or it is omitted. No key-specific character is ever printed.
- **Exists:** `tollgate: <NAME> is already stored; replace it? [y/N]`. A non-TTY caller without `--force` gets exit 1 and `… pass --force to replace`.
- **Env shadow:** `note: $<NAME> is also set in this environment; tollgate processes started from it use that value (--prefer-store uses the stored one)`.
- **List line:** `<NAME>  <store|store+env (env wins)|store+env (store wins)>  used by: <ids|nothing>`.
- **Removed:** `tollgate: removed <NAME>`, plus `; monitor(s) <ids> will report it missing` when any monitor references it.
- **Unsafe store:** `tollgate: ~/.tollgate/secrets.env is not private (<mode|owner|symlink|links>); refusing to load it. Fix: chmod 600 ~/.tollgate/secrets.env`. `list` and `rm` exit 1 with this message. Loaders log it once and treat the store as empty.
- **Add with a missing key:** `note: $<VAR> is not set in the environment or the store; run 'tollgate secret set <VAR>'`.
- **Detect row:** `<found>  →  tollgate monitor add <preset> [flags]   (<why>)`, or `already monitored as '<id>'`, or `skipped: <reason>`.

### 2.3 Observation, TUI, local API, MCP, herdr
**New `SourceId`s** (`observation.rs:498-525`), serialised as `openai_api` and `google_ai`, with display names "OpenAI API" and "Google AI Studio".

**Auth kinds by source:**

| Source | Kinds |
|---|---|
| `Grok`, `Antigravity` | `Subscription`, `NativeLogin` |
| `Codex` | `Subscription`, `NativeLogin` |
| `OpenaiApi` | `ApiKey`, `ReadOnly` |
| `GoogleAi` | `ApiKey` |

Update `local_api/routes.rs:305-340` (the match is compile-forced).

**Additive fields**, all `#[serde(default, skip_serializing_if…)]`, so the existing `usage_report_golden.json` stays byte-identical:
- `QuotaWindow.attribution: Vec<Share{label, used_pct}>` (Grok per-product shares; never separate windows).
- `AccountObservation.key_health: Option<KeyHealth{state: valid|invalid|blocked|out_of_credits|spend_capped|unknown, checked_at}>`.
- `AccountObservation.note: Option<String>`: one fixed sentence per source, such as "spend and quota not available for API keys; see Google AI Studio".

**Text surfaces.** Cards render `key valid · 3m ago` and the note. Regenerate the OpenAPI dump (`tests/dump_openapi.rs`).

**herdr.** `pane_agent` (`herdr/tag.rs:73-81`) is unchanged for grok and agy: the single monitor matches. A `codex` pane with no profile falls back to `native_match` over `[Codex]`, restricted to `AuthKind::NativeLogin` (D18).

## 3. Data and files

### 3.1 `monitors.toml` additions (`config.rs:116-156`; all optional; unknown keys still refused)

| Key | Kinds | Default | Meaning |
|---|---|---|---|
| `tool_home` | grok, codex_native | `~/.grok`, `~/.codex` | where the CLI's store lives; absolute or `~/` |
| `auth_entry` | grok | none | the `issuer::client` map key; **required** when more than one official login exists |
| `via` | antigravity | `keyring` | `cli` = opt-in `agy -p /usage` (slice 2, gate AGY-CLI) |
| `probe` | nous (with `api_key_env`) | `false` | enable the `:free` 1-token probe |
| `probe_model` | nous | auto | must end `:free`; else refused at validate |
| `billing_key_env` | openai (new) | none | OpenAI **admin** key NAME, used only for `/v1/organization/costs` |

**Validation.** Each key is refused outside its kinds, with the same message shape as `hermes_home` (`config.rs:270-277`).

**TTL floors** (`ttl_secs` below the floor is refused; defaults in parentheses):

| Kind | Floor (default) |
|---|---|
| grok | 120 (300) |
| antigravity | 300 (600) |
| codex_native | 120 (300) |
| openai | 300 (900); the costs leg is 3600 |
| google_ai | 300 (900) |
| nous with `probe` | 900 (1800) |

**Fingerprint.** `fingerprint()` (`config.rs:215-224`) appends `tool_home|auth_entry|via|probe|probe_model`, so changing any of these discards the cache.

### 3.2 `~/.tollgate/secrets.env` (new `src/secrets.rs`)
```
# tollgate secrets v1 — managed by `tollgate secret`; values are never printed
OPENROUTER_API_KEY=sk-or-v1-…
```
**Format**
- One `NAME=value` per line, split on the first `=`.
- No quoting, no expansion, no `export`.
- Lines starting with `#` and blank lines are ignored.
- Any other line invalidates the whole file (`unreadable`, and nothing is loaded).

**Limits**
- At most 64 names, each value at most 4096 bytes, the file at most 64 KiB.
- A value must not be empty or contain whitespace or control characters.

**Name rule.** Both `config::validate_env_name` (`config.rs:329`) and `billing_key::valid_env_name` (`billing_key.rs:31`) must pass. So process variables (`PATH`, `TOLLGATE_*`, …) are refused, and so are `MANAGED_ENV_KEYS` (`ANTHROPIC_*`, which belong to profiles).

**Writes**
- Taken under `~/.tollgate/.secrets.lock` (flock, 5 s).
- Read, modify in memory, then `profile::atomic_write_600` (`profile.rs:2364`).
- The directory is created by `mkdir_700`.

**Loads**
- `open(O_RDONLY|O_NOFOLLOW|O_CLOEXEC)`, then `fstat`. The file must be a regular file with `st_uid == geteuid()`, `mode & 0o077 == 0` and `nlink == 1`.
- `~/.tollgate` must be owned by the uid and not writable by group or others.
- Values are held as `Secret` (`source.rs:29-51`) and zeroized on drop (a direct `zeroize` dependency; it is already in `Cargo.lock`).

### 3.3 Cache (`~/.tollgate/monitors/<id>.json`, unchanged schema, new optional fields)
- `plan_checked_at`: agy `loadCodeAssist` runs at most once a day.
- `probe_model` and `probe_model_at`: the Nous auto model, reused for 24 h.
- `costs_at`: the OpenAI costs leg runs at most once an hour.

The cache never holds a credential; the existing tests pin that.

## 4. Algorithms

### 4.1 Transport and allowlist (`keyed_http.rs`, `source.rs:337-423`)
**Transport.** `keyed_http::send(agent, &Request) -> Option<Reply>` replaces the body of `get_bearer_with` (`keyed_http.rs:78-105`); `get_bearer` stays as a wrapper.
- `Request {method: Get|Post, url, auth: None|Bearer(&Secret)|GoogApiKey(&Secret), extra: &[(&'static str, &'static str)], json_body: Option<&[u8]>}`.
- Header values in `extra` are `'static`, so no secret can ride there.
- The existing policy is kept: no redirects, a 2 MiB body cap, and the 20 s deadline.
- **Response headers.** `Reply` gains `headers: Vec<(String, String)>`, lower-cased. Only names with a prefix in `RESPONSE_HEADER_ALLOW = ["x-ratelimit-", "x-nous-credits-", "x-nous-tool-pool-", "retry-after", "content-type"]` are kept, at most 64 headers, each value at most 256 chars. `set-cookie` is never kept.

**Seam.** The `MonitorHttp` trait gains:
- `fn send(&self, kind: MonitorKind, req: &Request) -> Result<HttpReply, Failure>`;
- `fn codex_usage(&self, token: &Secret, account: Option<&str>, fedramp: bool, now: i64) -> Result<UsageInfo, FetchError>`, which wraps `codex.rs:222`.

`HttpReply` gains `headers`. `FakeHttp` (`source.rs:437-524`) records `METHOD url auth=<class>:<value> headers=[…]`.

**Allowlist.** `request_allowed(kind, &Request)` replaces `bearer_url_allowed` (`source.rs:369-381`). It is one exhaustive table; anything else is refused before sending. The Nous rows stay:

| Kind | Method | Origin | Path | Auth |
|---|---|---|---|---|
| nous (Hermes) | GET | `portal.nousresearch.com` | `/api/oauth/account`, `/api/billing/*` | Bearer |
| nous (key, probe) | POST | `inference-api.nousresearch.com` | `/v1/chat/completions` | Bearer |
| nous (key, probe) | GET | same | `/v1/models` | **None** |
| grok | GET | `cli-chat-proxy.grok.com` | `/v1/billing?format=credits`, `/v1/user?include=subscription`, `/v1/settings` | Bearer + `X-XAI-Token-Auth: xai-grok-cli` |
| antigravity | POST | `daily-cloudcode-pa.googleapis.com`, `cloudcode-pa.googleapis.com` | `/v1internal:retrieveUserQuotaSummary`, `/v1internal:loadCodeAssist` | Bearer |
| openai (plain) | GET | `api.openai.com` | `/v1/models` | Bearer (api_key only) |
| openai (admin) | GET | same | `/v1/organization/costs` | Bearer (billing key only) |
| google_ai | GET | `generativelanguage.googleapis.com` | `/v1beta/models` | GoogApiKey (never `?key=`) |

**URL checks.** The existing rules (userinfo, port, `..`, `#`, `\`) apply to every row. The query must match the table exactly: grok's query is fixed, `pageSize=1` is fixed for Google, and OpenAI costs allow `start_time`, `bucket_width=1d`, `limit` and `page` only.

### 4.2 Grok (`src/usage/monitor/grok.rs`; port of branch `grok.rs:14-230`)
1. **Narrow read.** Read `<tool_home>/auth.json` into `BTreeMap<String, GrokEntry {key: Option<Secret>, expires_at: Option<Value>}>`.
   - Serde skips the `refresh_token` and every other field, so they are never held.
   - The read is one plain `read` capped at 1 MiB. Tollgate never opens, creates or flocks `auth.json.lock`.
   - A parse failure is `Unavailable` "grok's auth.json is being rewritten; keeping the last reading".
2. **Official entries.** An entry is official when its issuer is `https://auth.x.ai` or `https://accounts.x.ai/sign-in` (branch `grok.rs:27-33`).
   - With `auth_entry` set, use that entry (it must be official).
   - With none set: 0 official entries → `AuthRequired` "no Grok login; run grok"; 2 or more → `AuthRequired` "several Grok logins; set auth_entry (see `tollgate monitor detect --explain`)".
3. **Expiry** (D7). Read `expires_at` as RFC 3339, epoch seconds or epoch milliseconds. If absent, fall back to the JWT `exp` of `key` (`codex_auth.rs:199`). If there is still no expiry, `AuthRequired` "grok's token carries no expiry; open grok". A token with `exp ≤ now + 60 s` → `AuthRequired` "Grok token expired; open grok to refresh it". None of these make a network call.
4. **Calls.** `GET billing`, then `GET user`; `GET settings` only when `user` lacks `subscriptionTier`. On 401 or 403 → `AuthRequired` "open grok to refresh its login". On 429 → `RateLimited` with `retry-after`.
5. **Map** into one `QuotaWindow` `grok.shared`, scope `Shared`, `chain_eligible = false`:
   - Label `7d` (`USAGE_PERIOD_TYPE_WEEKLY`), `30d` (`MONTHLY`), or from `end - start`.
   - `used_pct = creditUsagePercent`, unclamped. A missing value stays **`None`**, never 0 (D8).
   - `resets_at = currentPeriod.end`; `window_secs = end - start`. `exhausted` when `used_pct ≥ 100`, with verdict `QuotaExhausted`.
   - `productUsage[]` is optional and goes to `attribution`.
   - `plan` comes from `subscriptionTier` or else `subscription_tier_display`.
   - `prepaidBalance`, `onDemand*` and `monthlyLimit` are **not** mapped until gate GROK-UNITS.
   - `SourceId::Grok`, `AuthKind::NativeLogin`.

### 4.3 Antigravity via keyring (`src/usage/monitor/antigravity.rs`; port of branch `antigravity.rs`)
1. **Guard** (fact-check caveat). Through a `KeyringProbe` seam, call `org.freedesktop.DBus.NameHasOwner("org.freedesktop.secrets")` on the session bus. With no owner → `Unavailable` "no Secret Service running; tollgate will not start one". This stops D-Bus activation from bringing up a wallet prompt.
2. **Search.** Linux Secret Service `SearchItems({service: gemini, username: antigravity})` over zbus. The implementation pins the existing unique D-Bus owner, uses `NoAutoStart`, and opens a local `plain` session only after confirming one unlocked item. It exposes no `Unlock` or `Prompt` call. This lighter path was authorized on 2026-09-29: it reuses the existing Tokio runtime and avoids secret-service's DH/AES and duplicate async runtime dependencies. `/usr/bin/secret-tool lookup` is unsuitable because it can unlock items and prompt; direct zbus exposes both locked and unlocked search results.
   - More than one item in total → `AuthRequired` "several agy logins in the keyring".
   - Only a locked item → `AuthRequired` "keyring locked; unlock your session (tollgate never unlocks it)".
   - Tollgate never calls `unlock` and never creates an item.
3. **Blob.** Read `get_secret` into a `Zeroizing<Vec<u8>>`. Strip an optional `go-keyring-base64:` prefix, then base64-decode. Parse the result as either nested `{"token":{access_token, expiry}}` or flat `{access_token, expiry|expires_at|expiresAt}`. Only those fields are deserialised; the refresh token is skipped. Expiry parsing follows §4.2 step 3, including fail-closed on no expiry. Fewer than 60 s left → `AuthRequired` "agy's token expired; open agy for a moment".
4. **Quota call.** `POST …:retrieveUserQuotaSummary` with body `{}` and `User-Agent: antigravity`.
   - **Host order:** `daily-cloudcode-pa` first. On 404, a 5xx or a network error, try `cloudcode-pa`. On 401, 403 or 429, do **not** fall through.
5. **Plan.** `POST …:loadCodeAssist` with `User-Agent: agy`, at most once every 24 h (`plan_checked_at`). The tier is `paidTier.name ?? currentTier.name`, and the `response` envelope is accepted.
6. **Parse.** Accept both `response.groups` and top-level `groups` (branch bug 1). Each `groups[].buckets[]` becomes a window:
   - id `agy.<bucketId>` (for example `agy.gemini-5h`), label `5h <group>` or `7d <group>`;
   - `used_pct = (1 - remainingFraction) × 100`, `resets_at = resetTime`;
   - `window_secs` 18000 or 604800;
   - scope `Model{models:[group displayName]}`, `chain_eligible = false`.

   `fetchAvailableModels` is **not** called (fact-check 4).
7. **Errors.**
   - `403` with reason `SUBSCRIPTION_REQUIRED` → `SubscriptionInactive` "free plan: no quota summary".
   - Any other 403 or a 401 → `AuthRequired`.
   - 429 → `RateLimited`, and the cache holds for at least 15 min.
   - No groups at all → `Unavailable` "no quota summary is available for this account".

### 4.4 Antigravity via CLI (`via = "cli"`, slice 2, gate AGY-CLI)
1. **Version gate.** Run `agy --version` (60 s memo per binary `(path, mtime, len)`) and require ≥ 1.1.11. Otherwise `Unavailable` "agy <v> has no print-mode /usage".
2. **Offline probe.** `agy -p "/help" --output-format json` must list `/usage` among the print-mode commands; the result is memoised the same way.
3. **Read.** Run `agy -p "/usage" --output-format json`. The spawn goes through `billing_key::helper_command` (so it is scrubbed), with:
   - stdin set to null;
   - `DISPLAY`, `WAYLAND_DISPLAY` and `BROWSER` removed, so no browser can open (fact-check risk);
   - `DBUS_SESSION_BUS_ADDRESS` kept, since agy needs its keyring;
   - a 30 s deadline, then SIGKILL; stdout capped at 1 MiB.

   Output that names a sign-in or URL flow maps to `AuthRequired` "open agy and sign in". The schema is mapped per the owner-captured fixture. The TTL floor for this path is 900.

### 4.5 Codex native (`src/usage/monitor/codex_native.rs`)
1. **Locate the store.** `lstat(<tool_home>/auth.json)`:
   - A symlink into `~/.tollgate/profiles/<p>/` → `Unavailable` "~/.codex/auth.json is profile '<p>''s store; it is watched as codex:<p>".
   - A symlink into `~/.clauth/profiles/<p>/` → `Unavailable` "…is upstream clauth's '<p>'; see upstream:<p>".
   - Any other non-regular file → `Unavailable`.
   - Otherwise require the owner uid and a size under 1 MiB.
2. **Narrow read.** Read `{tokens: {access_token: Secret, account_id: Option<String>}, chatgpt_account_is_fedramp?}`. `refresh_token` and `id_token` are skipped (not `CodexAuth::parse`, `codex_auth.rs:88`).
3. **Expiry.** Use `jwt_exp_ms(access_token)`. None, or `exp ≤ now + 60 s` → `AuthRequired` "codex's token expired; run codex". No call is made.
4. **Fetch and map.** `http.codex_usage(...)` → `project::apply_codex_usage` (`project.rs:215`), exactly as the profile leg does. `SourceId::Codex`, `AuthKind::NativeLogin`.
5. **Errors.** A 401 or 403 is `AuthRequired` "codex's login was rejected; run codex". It **never** calls `kick_codex` (`scheduler.rs:3875`), and never touches `codex_reset`. A 429 is `RateLimited`.
6. **Slice 2** extends `map_usage` (`codex.rs:143`) with the optional fields `credits.{balance, unlimited, has_credits}`, `spend_control.reached` and `additional_rate_limits[]` (fact-check 3). These are additive fields in `UsageInfo`, and the profile leg gains them too.

### 4.6 API-key monitors (slice 2)
**OpenAI (`openai.rs`)**
1. **Plain key.** `GET /v1/models`.
   - 200 → `key_health = valid`.
   - `x-ratelimit-{limit,remaining,reset}-{requests,tokens}` headers, if present, become windows `openai.rpm` and `openai.tpm`, scope `Account`:
     - `used_pct = (limit - remaining) / limit × 100`;
     - `resets_at = now + reset`, where `reset` uses Go-style durations (`6m0s`, `1.5s`, `20ms`);
     - `chain_eligible = false`.
   - Without those headers there are no windows (gate OPENAI-HEADERS). No paid call is ever made (D10).
2. **Classification**, keyed on `error.code` (fact-check 1):
   - 401 `invalid_api_key` → `invalid`, `AuthRequired`.
   - 403 → `blocked`.
   - 429 with `credit_balance_exhausted` → `out_of_credits`, `QuotaExhausted`.
   - 429 with `organization_spend_limit_exceeded` or `project_spend_limit_exceeded` → `spend_capped`, `QuotaExhausted`.
   - Any other 429 → `RateLimited`.
3. **Admin costs** (`billing_key_env` set, at most once an hour).
   - Request: `GET /v1/organization/costs?start_time=<UTC month start>&bucket_width=1d&limit=31`, following `next_page` for at most 3 pages.
   - Sum `results[].amount.value` per currency into `MoneyMeter spend.monthly`: kind `Spend`, period monthly, label "reported spend (lags)". There is no balance meter.
   - A 403 → note "costs need an OpenAI admin key (sk-admin-…)"; the exact message is gate OPENAI-COSTS-403.
   - Auth kind is `ReadOnly` when only the admin key is set.
4. **Note:** "no balance endpoint exists for OpenAI keys".

**Google AI (`google_ai.rs`)**
- `GET /v1beta/models?pageSize=1` with `x-goog-api-key`.
- Outcomes:
  - 200 → `valid`;
  - 400 `API_KEY_INVALID` → `invalid`, `AuthRequired`;
  - 403 `PERMISSION_DENIED` → `blocked`;
  - 402 → `out_of_credits`, `QuotaExhausted` "prepaid credits depleted";
  - 429 → `RateLimited`, using `RetryInfo.retryDelay` if present (optional).
- No windows and no money. Note (always): "spend and quota not available for API keys (per project, shown only in Google AI Studio)".

**Nous key (`nous.rs:70-81` replaced)**
- **`probe = false`** (default). No call. `key_health = unknown`. Note "Nous API keys have no read endpoint; enable probe for key health (one free 1-token call)".
- **`probe = true`**
  1. **Model.** Use `probe_model`. With none set, `GET /v1/models` with **no** Authorization, pick the lexicographically first id ending in `:free`, and cache it for 24 h. A model that does not end in `:free` is refused, in config and again just before sending.
  2. **Call.** `POST /v1/chat/completions` with `{"model":M,"messages":[{"role":"user","content":"."}],"max_tokens":1,"stream":false}`.
  3. **Result.**
     - 200 → `valid`.
     - `x-nous-credits-*` headers map as `remaining-micros` → `Balance total_usable` (`Amount::from_minor(v, 6)`, USD), `subscription-*` → `subscription`, `purchased-*` → `top_up`, and `rollover-micros` → `rollover`.
     - `paid-access = false` → `QuotaExhausted` "Nous credits depleted (free models still work)".
     - `x-ratelimit-*` headers → rpm and tpm windows.
     - 404 "model not found" → drop the cached auto model and retry once.
     - 401 → `AuthRequired` "Nous says the key is invalid, blocked or out of funds" (the conflation is Nous's).
  4. **Scope.** The probe never goes to the portal (gate NOUS-SK-PORTAL).

### 4.7 Presets (`monitor add <preset>`; the shortcut for detect)

| Preset (aliases) | kind | id | label | defaults |
|---|---|---|---|---|
| `grok` | grok | `grok` | Grok (CLI login) | `tool_home ~/.grok`, ttl 300 |
| `antigravity` (`agy`) | antigravity | `agy` | Antigravity (agy login) | `via keyring`, ttl 600 |
| `codex-native` | codex_native | `codex-native` | Codex (~/.codex login) | `tool_home ~/.codex`, ttl 300 |
| `openai` | openai | `openai` | OpenAI API | `api_key_env OPENAI_API_KEY`; `--admin-key-env` sets billing |
| `google-ai` (`gemini`) | google_ai | `google-ai` | Google AI Studio key | `api_key_env GEMINI_API_KEY` |
| `nous` (`hermes`) | nous | `nous` | Nous (Hermes login) | `hermes_home ~/.hermes` |
| `nous-key` | nous | `nous-key` | Nous API key | `api_key_env NOUS_API_KEY`; `--probe` sets ttl 1800 |
| `openrouter` | openrouter | `openrouter` | OpenRouter | `api_key_env OPENROUTER_API_KEY` |

Explicit flags override the preset's defaults. An id collision gives `a monitor with id '<id>' already exists; pass --id <id>-2`.

### 4.8 `monitor detect` (no network, no CLI spawn, no keyring secret read)
The injected `FakeHttp::offline()` must never be called. The candidates are:
- **Grok:** `<home>/.grok/auth.json` present → count official entries with the §4.2 narrow parse. With several, list their `client` halves as `--auth-entry` choices.
- **agy:** `agy` on `PATH` or at `~/.local/bin/agy`, plus the §4.3 guard. `search_items` is **attributes only**, reporting the unlocked, locked or none state and never calling `get_secret`.
- **Codex:** `~/.codex/auth.json` (or `$CODEX_HOME`) as a regular file → `codex-native`. A symlink is `skipped: managed by <store>`.
- **Hermes:** `~/.hermes`, `$HERMES_HOME`, and `~/.hermes/profiles/*`, each with an `auth.json` that has `providers.nous` (D10 whitelist parse) → `nous --hermes-home …`. A home with no login shows `skipped: Hermes has no Nous login (run hermes, then /login)`.
- **Keys:** each stored secret name (§3.2) and each set env name:
  - `OPENROUTER_API_KEY` → `openrouter`;
  - `NOUS_API_KEY` → `nous-key`;
  - `OPENAI_API_KEY` → `openai`, plus `OPENAI_ADMIN_KEY` → `--admin-key-env`;
  - `GEMINI_API_KEY` or `GOOGLE_API_KEY` → `google-ai`.

**Filtering.** A candidate is dropped when an existing monitor has the same fingerprint.

**Output.** With `--json`, rows are `{found, preset, flags, reason, state}`.

**Apply.** `--apply` adds every proposed row after one `[y/N]`. `--yes` is required on a non-TTY.

**Explain.** `--explain` prints, per store, the field spellings, the expiry format (`rfc3339|epoch_s|epoch_ms|jwt|absent`), the entry count and the blob shape (`nested|flat|base64`). This settles the "expiry format" and "blob shape" unknowns without the agent ever seeing a value.

**`refresh --capture DIR`.** Writes `<id>-<n>.shape.json`: the response with every string replaced by `"<str:LEN>"`. Allowlisted enum keys keep their value: `type`, `product`, `bucketId`, `window`, `displayName`, `plan_type`, `code`, `reason`, `subscriptionTier`. Numbers are kept, since units are the question. Allowlisted header names are kept with their values, because rate-limit and credit numbers are not secret. The owner reviews each file before sharing it.

### 4.9 Store overlay (D1)
**Lookup.** `secrets::lookup(name)` is the single resolver:
- The process env (non-blank) wins.
- Otherwise the store's value applies.
- `--prefer-store` or `TOLLGATE_PREFER_STORE=1` inverts the order.

**Callers.**
- `source::process_env` (`source.rs:89`) and `billing_key::resolve` (`billing_key.rs:110`) both call it. So every monitor fetch and every provider fetch (the OpenRouter `/credits` leg) sees stored keys, and nothing else does.
- `list_rows` (`monitor/cli.rs:166`) reports `set (env)`, `set (store)` or `MISSING`.

**The OS environment is never written.** There is no `set_var`, so no child can inherit a stored value by construction.

**Scrubbing.** `referenced_env_vars` and `monitoring_only_env_vars` (`billing_key.rs:124,144`) also union the store's **names**, parsed from `NAME=` prefixes (values are dropped at parse). A same-named variable the operator exported is therefore still stripped from sessions (`harness.rs:105-113,175-187`), helpers and the gateway (D4).

**Reload.** The store loads lazily on first lookup. The daemon's monitor scan (`poll.rs`, every 10 s) reloads it when `(dev, ino, mtime_ns, len)` changes.

## 5. Failure modes and recovery

| Situation | Result | Recovery |
|---|---|---|
| Borrowed token expired, undated, or < 60 s left | `AuthRequired`, no call; last reading kept (stale) | use the CLI briefly (grok / agy / codex) |
| `auth.json` mid-rotation (grok or codex) | `Unavailable`, stale kept | next poll |
| Several grok logins or agy items | `AuthRequired` naming `auth_entry` | `detect --explain`, set `auth_entry` |
| Keyring locked, or no Secret Service owner | `AuthRequired` or `Unavailable`; never unlock, never activate | unlock the desktop session |
| agy free plan | `SubscriptionInactive` | — |
| Provider 429 | `RateLimited`, cache hold (`cache.rs` 429 hold; agy ≥ 15 min) | wait |
| 401 after the CLI rotated mid-flight | `AuthRequired`; next poll reads the new token | automatic |
| `~/.codex/auth.json` is a store symlink | `Unavailable` naming `codex:<p>` or `upstream:<p>` | use that account |
| `secrets.env` not private, foreign owner, symlink, or bad line | store empty, one logline; `secret list` exits 1 | `chmod 600`, or `secret rm` / `set` to rewrite |
| Env and store both set | env wins (list shows it); `--prefer-store` flips it | — |
| Atomic write fails (disk full) | old file intact; exit 1 | free space |
| Daemon started before `secret set` | reload within 10 s | — |
| Redirect, body > 2 MiB, header flood | refused, cut or capped (§4.1) | — |
| Grok `productUsage` or `creditUsagePercent` absent | no attribution, or `used_pct None` | — |
| Nous auto model disappears (404) | re-pick once, else `Unavailable` | set `probe_model` |
| OpenAI costs with a project key | note "admin key needed" | set `--admin-key-env` |
| `agy -p` hangs or asks to sign in (slice 2) | killed at 30 s, or `AuthRequired` | `via = keyring` |

## 6. Guest mode and other interactions
**Guest mode** (`identity.rs:135`)
- `secret *`, `monitor add|detect|refresh` and every native read are allowed in guest mode: they touch only `~/.tollgate/*` and read stores that upstream does not write (`~/.grok`, the agy keyring).
- `~/.codex/auth.json` is in upstream's set. Reading it is allowed because it is read-only and never kicks (the B2 rule is about refresh and rotation); a symlink into `~/.clauth` is refused (§4.5).
- The three Claude accounts stay on the upstream view (`upstream:<name>`). Detect prints `Claude accounts: via upstream clauth (import to manage)`.

**After import** (`docs/specs/import-clauth.md` §4.6), `~/.codex/auth.json` stays an independent login, so `codex-native` keeps working. If a later `tollgate login --codex` links it, the monitor refuses it as profile `<p>`.

**Hermes (H-1a).** The Hermes scrub list already names `NOUS_*`, `OPENROUTER_API_KEY` and `OPENAI_API_KEY`. Stored names add nothing new there, and a Hermes home reads its keys from its own `.env`.

**Hot swap (P6a).** Profiles' API keys live in the profile store, not `secrets.env`. The two never mix.

**Local API and MCP `usage`** read caches only. They never resolve a key and never load the store.

## 7. Test plan (hermetic; `HomeSandbox` per test; `FakeHttp` + `FakeKeyring`; no real network, keyring, or CLI)

**Fixtures** (`tests/fixtures/monitors/`, all synthetic; token strings are `TOKEN-CANARY-*`, refresh tokens are `REFRESH-CANARY`):
- Grok:
  - `grok_auth_one.json` and `grok_auth_two.json` (with `refresh_token`);
  - `grok_auth_epoch.json` and `grok_auth_undated.json`;
  - `grok_billing_weekly.json`, `grok_billing_no_percent.json` and `grok_billing_monthly_products.json`;
  - `grok_user.json`, `grok_settings.json` and `grok_401.json` (the observed body).
- agy: `agy_blob_nested.json`, `agy_blob_flat.b64`, `agy_quota_envelope.json` (the four bucket ids), `agy_quota_bare.json`, `agy_load_code_assist.json` and `agy_403_subscription_required.json`.
- Codex: `codex_auth_min.json` (JWT with `exp`, plus `refresh_token`), `codex_wham_full.json` (credits, spend_control and additional_rate_limits) and `codex_401.json`.
- OpenAI: `openai_models_with_headers.http`, `openai_401_invalid.json`, `openai_429_{credit_balance_exhausted,project_spend_limit_exceeded,rate_limit}.json` and `openai_costs_p{1,2}.json`.
- Gemini: `gemini_models_200.json`, `gemini_400_api_key_invalid.json`, `gemini_402.json` and `gemini_429.json`.
- Nous: `nous_models_public.json`, `nous_probe_200.http` (credit and rate-limit headers) and `nous_probe_401.json`.
- Secrets: `secrets_valid.env` and `secrets_bad_line.env`.

**Slice 1 tests** (`tests/inline/`)

`usage_keyed_http.rs`:
1. `send_keeps_only_allowlisted_response_headers`
2. `send_never_follows_a_redirect_for_post`
3. `extra_headers_are_static_so_no_secret_rides_there`

`usage_monitor_source.rs`:

4. `grok_token_reaches_only_the_three_cli_proxy_reads`
5. `agy_token_reaches_only_two_rpcs_on_two_hosts`
6. `nous_rows_are_unchanged`
7. `every_kind_refuses_userinfo_port_dotdot_and_foreign_query`

`usage_monitor_grok.rs`:

8. `grok_reads_key_and_expiry_never_the_refresh_token` (no `REFRESH-CANARY` in calls, `Debug` output or cache)
9. `grok_expired_token_is_auth_required_without_a_call`
10. `grok_undated_non_jwt_token_is_auth_required_without_a_call`
11. `grok_jwt_exp_is_used_when_expires_at_is_absent`
12. `grok_epoch_seconds_and_millis_both_parse`
13. `grok_two_official_logins_need_auth_entry`
14. `grok_garbage_auth_is_unavailable_and_keeps_stale`
15. `grok_never_creates_or_locks_auth_json_lock` (lock absent before → absent after; present → same inode, mtime and size)
16. `grok_weekly_billing_maps_one_shared_window_with_attribution`
17. `grok_missing_percent_is_unknown_not_zero`
18. `grok_prepaid_val_is_never_money`
19. `grok_401_maps_to_open_grok`
20. `grok_settings_read_only_when_user_lacks_tier`

`usage_monitor_antigravity.rs`:

21. `agy_accepts_response_envelope_and_bare_groups`
22. `agy_tries_daily_then_prod_on_404_5xx_or_network`
23. `agy_does_not_fall_through_hosts_on_401_403_429`
24. `agy_subscription_required_is_subscription_inactive`
25. `agy_under_60s_left_is_auth_required_without_a_call`
26. `agy_missing_expiry_is_auth_required_without_a_call`
27. `agy_blob_accepts_nested_flat_and_base64_prefix`
28. `agy_two_items_are_refused`
29. `agy_locked_item_is_never_unlocked` (`FakeKeyring` panics on `unlock`)
30. `agy_no_secret_service_owner_means_no_connect`
31. `agy_plan_is_read_once_per_day`
32. `agy_429_holds_at_least_15_minutes`

`usage_monitor_codex_native.rs`:

33. `codex_native_holds_only_access_token_account_and_fedramp`
34. `codex_native_expired_jwt_is_auth_required_without_a_call`
35. `codex_native_401_never_kicks_codex` (the kick queue stays empty)
36. `codex_native_refuses_a_tollgate_or_clauth_store_symlink`
37. `codex_native_maps_windows_like_the_profile_leg`

`secrets.rs` (new):

38. `secret_set_takes_no_value_positional`
39. `secret_name_with_equals_is_a_usage_error`
40. `secret_set_non_tty_needs_stdin`
41. `secret_file_is_0600_atomic_and_locked`
42. `secret_load_refuses_group_readable_foreign_owner_symlink_and_hardlink`
43. `secret_bad_line_loads_nothing`
44. `secret_list_prints_names_only` (no canary in stdout or stderr)
45. `secret_rm_names_referencing_monitors`
46. `env_wins_over_store_by_default`
47. `prefer_store_flag_and_env_let_store_win`
48. `stored_names_join_session_helper_and_gateway_scrubs`
49. `stored_values_never_enter_a_child_environment` (`helper_command("env")` output has no canary)
50. `secret_names_follow_monitor_env_name_rules`
51. `values_with_whitespace_or_control_chars_are_refused`
52. `the_poll_scan_reloads_a_changed_store`
53. `the_confirmation_prints_length_and_vendor_prefix_only`

`usage_monitor_cli.rs`:

54. `monitor_add_grok_preset_fills_defaults`
55. `monitor_add_codex_native_and_agy_aliases`
56. `preset_id_collision_suggests_id_flag`
57. `kind_form_is_unchanged`
58. `add_notes_a_key_missing_from_env_and_store`
59. `detect_makes_no_network_call_and_spawns_nothing` (PATH holds a fake `agy` that panics the test if run)
60. `detect_proposes_codex_native_only_for_a_regular_file`
61. `detect_lists_grok_auth_entries_without_values`
62. `detect_maps_stored_names_to_presets`
63. `detect_skips_configured_fingerprints`
64. `detect_apply_needs_yes_off_a_tty`
65. `detect_explain_prints_shapes_never_values`
66. `capture_writes_0600_shape_files_with_strings_masked`

`usage_monitor_config.rs`:

67. `new_keys_are_refused_outside_their_kinds`
68. `ttl_floors_per_kind`
69. `fingerprint_covers_new_keys`

`guest_mode.rs`:

70. `native_monitors_and_secrets_work_in_guest_mode_and_leave_operator_trees_byte_identical`
71. `codex_native_refuses_an_upstream_symlink_in_guest_mode`

`herdr_tag.rs`:

72. `grok_and_agy_panes_tag_their_single_monitor`
73. `a_profileless_codex_pane_falls_back_to_the_native_monitor`

`local_api_routes.rs`:

74. `providers_list_the_new_sources_and_auth_kinds`

**Slice 2 tests**

`usage_monitor_openai.rs`:
75. `openai_models_200_is_valid`
76. `ratelimit_headers_become_rpm_tpm_windows`
77. `no_headers_means_no_windows`
78. `go_style_reset_durations_parse`
79. `openai_401_is_invalid`
80. `openai_429_classifies_on_error_code`
81. `plain_key_never_reaches_organization_routes`
82. `admin_key_reaches_only_costs`
83. `costs_sum_month_to_date_per_currency`
84. `costs_follow_at_most_three_pages`
85. `costs_run_at_most_hourly`
86. `costs_403_notes_admin_key`

`usage_monitor_google_ai.rs`:

87. `gemini_200_is_valid_with_no_windows_or_money`
88. `gemini_key_is_a_header_never_a_query`
89. `gemini_400_invalid_403_blocked_402_out_of_credits_429_rate_limited`
90. `gemini_note_is_always_present`

`usage_monitor_nous.rs`:

91. `nous_key_without_probe_makes_no_call`
92. `probe_sends_one_token_to_a_free_model`
93. `probe_refuses_a_non_free_model_at_send`
94. `auto_model_comes_from_the_public_list_without_credentials`
95. `probe_headers_map_credits_paid_access_and_rate_windows`
96. `probe_401_reports_the_conflated_verdict`
97. `probe_404_repicks_once`
98. `probe_ttl_floor_is_900s`

`codex_usage.rs`:

99. `map_usage_parses_credits_spend_control_and_additional_limits`
100. `the_profile_leg_output_is_unchanged_without_the_new_fields`

`usage_monitor_antigravity.rs` (CLI path):

101. `cli_path_needs_1_1_11`
102. `cli_path_needs_usage_in_the_help_listing`
103. `cli_spawn_has_null_stdin_no_display_no_browser`
104. `cli_timeout_kills_the_child`
105. `cli_sign_in_output_is_auth_required`

`usage_report.rs`:

106. `golden_is_byte_identical_without_new_fields`
107. `key_health_and_note_render_on_cards`

Finally, update the `tests/dump_openapi.rs` golden.

## 8. Implementation slices (each green on nextest + clippy `-D warnings` + fmt)

**Slice 1: foundation and native monitors** (tests 1–74)
- **Code:** `keyed_http::send` plus the header allowlist; the `MonitorHttp::send` and `codex_usage` seams; the `request_allowed` table; `SourceId::{OpenaiApi, GoogleAi}` (stubs, so routes compile); `attribution`; `MonitorKind::{Grok, Antigravity, CodexNative}`, the §3.1 keys and floors; `grok.rs`, `antigravity.rs` (keyring path, `KeyringProbe`), `codex_native.rs`; `src/secrets.rs` and `Command::Secret`, the overlay and scrub unions, `--prefer-store`; presets; `detect`, `--explain`, `--capture`; the herdr codex fallback.
- **Deps:** `zbus` (Linux; existing Tokio runtime, blocking API), `zeroize`.
- **Docs:** `wiki/Monitors.md`, `CHANGELOG.md`, and plan §10 rows P4b/P4c.

**Slice 2: key monitors and opt-ins** (tests 75–107)
- **Code:** `openai.rs`, `google_ai.rs`, Nous key mode and probe; `key_health` and `note`, with card rendering and the OpenAPI golden; the `map_usage` extension; agy `via = "cli"` (enabled only after gate AGY-CLI is recorded); the Grok money mapping (enabled only after GROK-UNITS); optionally `include_byok_in_limit`.
- **Docs:** `docs/agent-api.md` (new fields).

## 9. Decisions (defaults) and open questions

**Decided now**
- **D1.** Overlay, not `std::env::set_var`. Under edition 2024 `set_var` is `unsafe` and races the daemon's threads, and an overlay cannot leak into a child. This deviates from the brief's wording ("loaded into the process env"), but the semantics are the same for every fetch.
- **D2.** Env wins; `--prefer-store` or `TOLLGATE_PREFER_STORE=1` inverts. A blank env var counts as unset.
- **D3.** A dotenv subset with no quoting.
- **D4.** Stored names join all three scrub lists.
- **D5.** agy uses the keyring Cloud Code read; the CLI path is opt-in and slice 2.
- **D6.** Tollgate never refreshes a borrowed token.
- **D7.** A missing expiry fails closed; the JWT `exp` is the only fallback.
- **D8.** Grok's missing percent is `None`.
- **D9.** Grok money is hidden until GROK-UNITS.
- **D10.** No paid micro-call for OpenAI or Gemini headroom.
- **D11.** The Nous probe is opt-in and `:free`-only, with a 900 s floor and an auto model from the public list.
- **D12.** The OpenAI admin key uses `billing_key_env`, with the costs leg hourly.
- **D13.** Gemini is key-health only, and its note is always shown.
- **D14.** The agy read is Linux-only and requires a `NameHasOwner` guard.
- **D15.** `codex_native` refuses store symlinks.
- **D16.** The TTL floors in §3.1.
- **D17.** Capture files mask strings and keep numbers.
- **D18.** A herdr codex pane falls back to the native monitor.
- **D19.** The set confirmation shows only the length and the vendor prefix.
- **D20.** Detect never spawns any CLI.

**Gates** (each is settled by one owner-run check in `docs/testing/real-machine-campaign.md`, and recorded here before its code path is enabled):
- **GROK-UNITS:** one `/v1/billing` capture, compared with the grok.com UI.
- **AGY-CLI:** `agy -p "/help"` and `-p "/usage"` JSON captures, run while the owner watches for a browser.
- **OPENAI-HEADERS:** whether `/v1/models` carries `x-ratelimit-*`.
- **NOUS-SK-PORTAL:** whether the Nous portal accepts an `sk-` key, and whether `:free` probe headers carry `x-nous-credits-*`.
- **OPENAI-COSTS-403:** the status a project key gets from `/v1/organization/costs`.

**Also open**
- Grok's `expires_at` format and token lifetime (`detect --explain`).
- The agy blob shape (`detect --explain`).
- Whether codex's multi-day token lifetime makes `AuthRequired` rare.

## 10. Code anchors (to change or call)
- **Monitor config** (`src/usage/monitor/config.rs`):
  - `:85-113` `MonitorKind` (+3 kinds), `:116-156` `MonitorConfig` (+keys);
  - `:215-224` `fingerprint`, `:227-305` `validate` (kind matrix, floors), `:329` `validate_env_name` (reused by the secret store);
  - `:412` `SECRET_KEYS`, `:622` `to_table`.
- **Monitor source** (`src/usage/monitor/source.rs`):
  - `:29-51` `Secret`, `:89` `process_env` (→ overlay), `:94-131` `resolve_target`;
  - `:168-186` `UsageSource` / `source_for`, `:337-359` `HttpReply` / `MonitorHttp`;
  - `:369-381` `bearer_url_allowed` (→ `request_allowed`), `:386-423` `LiveHttp`, `:437-524` `FakeHttp`.
- **Nous monitor** (`src/usage/monitor/nous.rs`): `:1-21` (the reader rule to copy), `:62-86` (key mode), `:113-175` `read_hermes_token` (pattern).
- **Monitor CLI** (`src/usage/monitor/cli.rs`): `:21-51` `MonitorCommand` (+`Detect`, `--capture`), `:53-116` `MonitorAddArgs` (`kind` becomes `Option`, presets), `:166-188` `list_rows`, `:216` hint text.
- **Monitor cache and poll:** `src/usage/monitor/cache.rs:169-181` `RefreshDeps` / `refresh_one`; `src/usage/monitor/poll.rs` (the scan triggers the store reload).
- **Transport:** `src/usage/keyed_http.rs:59-105` (`Reply` and `get_bearer_with` → `send`).
- **Observation model** (`src/usage/observation.rs`): `:387` `AccountObservation` (+`key_health`, `note`), `:498-585` `SourceId` (+2), `:590-601` `AuthKind`, `:818` `QuotaWindow` (+`attribution`).
- **Codex:**
  - `src/usage/codex.rs:20,143,197-228` (fetch and `map_usage`);
  - `src/codex_auth.rs:88` (not to be used), `:199` `jwt_exp_ms`, `:654` `read_store_auth` (not to be used);
  - `src/usage/scheduler.rs:3875` (the `kick_codex` arm must stay unreachable from monitors);
  - `src/usage/project.rs:215` `apply_codex_usage`.
- **Keys and scrubbing:**
  - `src/providers/billing_key.rs:31` `valid_env_name`, `:50` `is_process_env_name`, `:110` `resolve` (→ overlay), `:124,144` the scrub name sets, `:205` `helper_command` (agy CLI);
  - `src/harness.rs:105-113,175-187` `scrub_env`.
- **Other surfaces:**
  - `src/herdr/tag.rs:73-81,140-151` (codex fallback);
  - `src/local_api/routes.rs:305-340` (provider list, `auth_kinds`);
  - `src/identity.rs:135` `upstream_active`;
  - `src/main.rs:132-138,236` (root `--prefer-store`, `Command::Secret` dispatch), `:700` (the rpassword prompt pattern);
  - `src/cli.rs:456-468` (`Monitor`);
  - `Cargo.toml:41` (`rpassword`, already present).
- **Ports** (from `feat/provider-monitoring:src/provider_monitor/`):
  - `grok.rs:10-11,27-33,88-133,142-212`;
  - `antigravity.rs:11,24-45,47-70` (fails open: fix it), `:72-100` (keyring), `:132-160` (parse: add the envelope);
  - `codex.rs:24-102` (credits parser, for slice 2).
- **ai-usagebar references:** `supergrok/{direct.rs,types.rs}`, `antigravity/{credential.rs:1-55,cloud.rs:25-66,141-185,fetch.rs:668-802}`, `docs/vendor-endpoints.md:90-91`.
- **Hermes:** `agent/credits_tracker.py:1-30,205-221,410-560` and `agent/rate_limit_tracker.py:1-20` (Hermes 0.19.0).
