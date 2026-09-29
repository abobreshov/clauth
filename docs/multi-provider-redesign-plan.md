# Multi-provider + look-and-feel redesign — plan

Status: **v2.3, approved.** Round 5 (2026-09-29): Grok (grok-4.7, high) `APPROVE`, Codex
(GPT-6-Astra) `APPROVE`, no blockers left. It was revised over four earlier rounds. The raw round-1 reviews are in
`docs/multi-provider-redesign-reviews.md`, and §9 records every change.
2026-09-29 · base `mommy` @ b7d7cb02 (= upstream uwuclxdy/clauth) · fork github.com/abobreshov/clauth

## 1. Goal

1. **Look and feel.** Bring clauth's CLI and TUI closer to two reference tools:
   [ai-usagebar](https://github.com/akitaonrails/ai-usagebar) (Rust, Waybar + TUI) and
   [omarchy-agent-bar](https://github.com/othavi0/omarchy-agent-bar) (Quickshell widget).
   Calm metric cards, one lead metric per account, pace and countdown lines, severity shown with
   words as well as colour, and an Omarchy palette that follows the desktop theme live.
2. **Multi-provider, including API-key accounts.** Every account clauth knows about shows usage in
   one lossless, provider-agnostic observation. That covers subscription quota windows, money meters
   for API-key and prepaid accounts, and a local estimate only where attribution is exact.
3. **Hot configuration swap inside one provider.** Keep today's live OAuth swap exactly as it is.
   Add a *separate* live swap between API-key profiles that share an effective transport (same
   endpoint, models and env). A swap across providers stays a restart, offered as an explicit
   relaunch.
4. **Herdr.** Everything works in the herdr popup, pane tags and daemon bridge on herdr ≥ 0.9.1, and
   the fork can ship its own herdr plugin.

### Non-goals (v1)

- Hot swap across providers mid-process. Claude Code reads the endpoint and model env at spawn; see
  §4.4.
- Codex hot swap. A Codex session binds `auth.json` at start (`harness.rs:154-157`), and
  `run_switch` already refuses codex (`sessions_cli.rs:212`).
- Rotating the fallback chain on wallet or balance exhaustion. There is no balance-exhaustion
  predicate yet (see §4.4).
- **Merging tabs.** The 8 tabs stay; they are restyled, not consolidated (see §4.5).
- The `src/tasks/*` handoff subsystem on `feat/provider-monitoring`, and IDE scrapers such as
  Cursor, Copilot and Kiro.

## 2. What exists today (verified by both reviewers)

| Area | State on `mommy` | Where |
|---|---|---|
| Harnesses | `Harness{Claude,Codex}`, decided by which state file holds the profile | `harness.rs:21`, `docs/codex-plan.md` |
| API providers | Closed `enum Provider{DeepSeek,Zai,Alibaba,OpenRouter,MiniMax}` derived from `base_url`, plus a Generic scanner | `providers/mod.rs:128` |
| Third-party → chain | `to_usage_info` keeps only bars labelled 5h / 7d. Z.ai's 30d and credit rows are deliberately excluded from fallback | `providers/mod.rs:434-469` |
| API key into CC | `apiKeyHelper = <exe> __api-key <profile>`. The printer calls `load_profile`, which takes the state flock and may rewrite `credentials.json`. `profile_name_from_helper` rejects extra tokens | `claude.rs:2115,2232,2481`, `main.rs:2170-2180`, `profile.rs:3130` |
| Env precedence | `profile.env` is applied last, so `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` there override the helper; `routing_endpoint()` prefers env `ANTHROPIC_BASE_URL` | `claude.rs:2423-2451`, `profile.rs:496` |
| Live swap | Daemon writes `intended_member` → watchdog `SessionSwap::poll` → `swap_to` (drain, mtime bump, repoint under `RotationGuard` + state flock) → session publishes `current_member`. `swap_eligible` refuses non-OAuth, and differences in env or models; the api-key check compares **presence** only. `LaunchTransport` is frozen from `process.env` | `live_sessions.rs:60-75`, `runtime.rs:2677-2731,3079-3293` |
| IDs | A runtime session id is `<pid>-<seq>`; a resumable conversation id is a transcript stem. `resume` never passes `--with-fallback` | `runtime.rs:226`, `sessions.rs:515`, `sessions_cli.rs:93-147` |
| Money today | Wallet amounts are `f64`; OpenRouter flags `remaining < 0.005`, including negatives; DeepSeek formats amounts to strings early; MCP `funded_wallets` | `providers/mod.rs:52-118`, `openrouter.rs:58-79`, `deepseek.rs:43`, `usage/burn.rs:196` |
| Token ledger | Aggregates by date → model with no profile. Shared isolation symlinks `projects/` | `token_ledger.rs:117-125`, `runtime.rs:4736` |
| Scheduler | One `spawn_refresher` does credential fingerprints, host pacing, `Retry-After`, cache retention and the third-party leg | `usage/scheduler.rs:325,2736,3093,3143,4064` |
| TUI | ratatui 0.30, 8 tabs, `app.rs` 11,434 lines. `HomeTab` is persisted; `tab_activity` is sized by `Tab::ALL` | `profile.rs:662-707`, `app.rs:2086` |
| Theme | Catppuccin Mocha, with **hand-picked** RGB and xterm-256 pairs behind accessor fns. `theme` / `--theme` already means **colour depth** (`full` / `compatible`) | `tui/theme.rs:112-167`, `profile.rs:616`, `cli.rs:31` |
| CLI output | `clauth list` renders `ProfileEntry` as plain text with no colour; `status.json` is schema 2, additive-only | `list.rs:1-39,240`, `daemon/status_json.rs:35`, `codex-plan.md:22` |
| Herdr | Plugin install is **hard-coded to upstream** `uwuclxdy/clauth/herdr-plugin`. The reporter and watcher exclude grok and agy. The REST bridge joins sessions by PID. 0.9.1 rect / agent-name fixes are in | `herdr.rs:32-36,519`, `herdr-plugin/report-profile.sh:119,172`, `watch-profile.sh:67`, `daemon/api/panes.rs:4` |
| Shunt gateway | The daemon-supervised proxy's `base_url` is **not** an upstream provider | `gateway.rs:1-11` |
| Omarchy theme | `~/.local/state/omarchy/current/theme/colors.toml` (verified on this machine) | — |

## 3. Design principles

1. **Real data or a typed unavailable state.** Never a fake 0. Freshness is kept separate from fetch
   failure, so stale data can coexist with `auth_required`.
2. **Last good data stays visible while it is honestly stale**, up to 7 days.
3. **Colour never carries meaning alone.** Every severity rung has a word; pace uses `↑ → ↓`.
4. **Domain first, presentation second.** Domain observations serialise on their own. `Sections` is
   a *presentation* projection built from them, and nothing parses display rows back into data.
5. **One scheduler, one owner per target.** No second polling pipeline.
6. **Credential boundaries.**
   - Inference keys (served to Claude Code) and billing / admin keys (monitoring only) live in
     separate slots.
   - `__api-key` can never print a billing key.
   - Billing keys never enter child env, the generic scanner, or inference helpers.
7. **Hermetic tests.** Fixtures only; any network call fails the test.
8. **Upstream-shaped.** Additive modules; today's look stays the default in upstream-bound PRs.

## 4. Architecture

### 4.1 Observation model (new `src/usage/observation.rs`)

The domain types serialise independently of presentation. The shapes are lossless supersets of
`UsageInfo`, `ThirdPartyStats`, the codex row and the branch's `ProviderReport`:

```rust
pub(crate) struct AccountObservation {
    pub account: AccountRef,              // stable id: "claude:<profile>" | "codex:<profile>" | "native:<target>"
    pub source: SourceId,                 // anthropic_oauth, deepseek, zai, openrouter, minimax, alibaba, codex, grok, antigravity, generic …
    pub auth: AuthKind,                   // Subscription | ApiKey | Hybrid | NativeLogin   (billing keys are not an AuthKind; see MoneyScope)
    pub plan: Option<String>,
    pub freshness: Freshness,             // Fresh | Stale{since} | NotFetched
    pub failure: Option<Failure>,         // AuthRequired | RateLimited{retry_after} | Unavailable | InvalidResponse | ConsoleExpired  (sanitised message)
    pub windows: Vec<QuotaWindow>,
    pub money: Vec<MoneyMeter>,
    pub estimate: Option<LocalEstimate>,  // only when attribution is exact (S3), otherwise None, never guessed
    pub resets: Vec<BankedReset>,
    pub best_effort: bool,                // generic scanner
    pub observed_at: Option<Timestamp>,   // when the provider produced the data
    pub checked_at: Option<Timestamp>,    // when clauth last asked
}
pub(crate) struct QuotaWindow {
    pub id: String, pub label: String,
    pub used_pct: Option<f64>,            // unknown % is representable
    pub exhausted: bool,
    pub resets_at: Option<Timestamp>, pub window_secs: Option<u64>,
    pub scope: WindowScope,               // Shared | Model{models: Vec<String>} | Product
    pub chain_eligible: bool,             // true only for today's 5h/7d fold; keeps the to_usage_info exclusions
}
pub(crate) struct MoneyMeter {
    pub label: String, pub kind: MoneyKind,         // Balance | Spend | Limit | Budget
    pub amount: Decimal, pub currency: Currency,    // decimal, not cents: sub-cent balances and negatives must survive
    pub limit: Option<Decimal>, pub budget_source: Option<BudgetSource>, // Provider | UserConfig
    pub scope: MoneyScope,                          // Key | Profile | Workspace | Organization
    pub period: Option<Period>,                     // explicit [start, end) boundaries, not just "month"
}
```

- **Projection at read time.** `UsageInfo`, `ThirdPartyStats`, the codex row and the native reports
  each get `fn observe(&self) -> AccountObservation`. The fallback chain keeps reading `UsageInfo`.
  `profiles[].windows`, the chain and MCP `funded_wallets` are unchanged.
- **`Sections`** (`src/usage/sections.rs`) is built from observations for the TUI, the CLI text and
  herdr tags. It is presentation only.
- **Derived at render time:**
  - severity: the **worst** of used%, balance and pace, with a word at every rung;
  - pace: `delta = used − elapsed`, with a ±5 pt band;
  - the lead window: session first; else the worst critical window; else the soonest reset.
  - A switch threshold below 50 is clamped, so `critical` never fires before `high`.
- **Contracts.**
  - `status.json` stays at schema 2 and gains an additive `accounts[]` of `AccountObservation`.
    Existing fields and both existing readers are preserved.
  - `clauth usage --json` emits `{schema_version: 1, generated_at, accounts: [...]}` with RFC 3339
    timestamps and the stable ids above.
  - The UI never implies a *switch* happens on a displayed balance severity.

### 4.2 Sources on the one scheduler

```rust
pub(crate) trait UsageSource: Send + Sync {
    fn meta(&self) -> &'static SourceMeta;   // display/short name, glyph, AuthKind, login cmd, sign-in hint, billing origin(s)
    fn fetch(&self, target: &Target, cred: &Credential, http: &ProviderHttp) -> Result<Observation, Failure>;
}
```

- **One scheduler.** The native and billing-only targets become **another leg of `spawn_refresher`**,
  per target. That leg reuses the existing fingerprints, host pacing, `Retry-After`,
  429 / AuthExpired holds and cache retention.
  - Each target has exactly one polling owner. There is no second poller and no second codex path:
    the codex target uses upstream's `usage/codex_headers.rs` and the roster poll.
  - Cache identity = (source, endpoint, account / scope, credential fingerprint).
- **`ProviderHttp`** is a *separate* agent for provider fetches, with a 2 MiB body cap and refusal
  of any redirect that leaves the origin. The shared OAuth agent (`oauth.rs:332`) is not changed.
  It has **two origin policies**:

  | Policy | Allowed origins | Credentials it may carry |
  |---|---|---|
  | **Billing** | only the fixed origin(s) declared in `SourceMeta` (e.g. `api.anthropic.com` for `cost_report`) | billing keys; nothing else |
  | **Configured endpoint** | the profile's own `routing_endpoint()` origin (existing DeepSeek / Z.ai / OpenRouter / MiniMax / generic fetchers, unchanged behaviour) | that profile's inference key only; **billing keys are categorically excluded** |
- **Migration order.** In P4a, the existing third-party fetchers are wrapped behind the trait
  *in place*; the refresher legs call through the trait with no behaviour change. New sources come
  after that.

### 4.3 Credentials (new `src/usage/credentials.rs`)

| Slot | Holds | Stored as | Who can read it | Enters child env? |
|---|---|---|---|---|
| Inference key | the key Claude Code uses (`Profile::api_key`) | `config.toml` 0600 (today) | `__api-key`, the minimal resolver | no (served via the helper) |
| Billing key | Anthropic Admin, xAI management, OpenAI admin, OpenRouter provisioning | **env reference only** (`billing_key_env = "…"`); inline storage refused | the scheduler's billing leg only | **never**. Every referenced var is added to `MANAGED_ENV_KEYS` / `CODEX_MANAGED_ENV_KEYS` scrubbing |
| Native login | `~/.codex`, `~/.grok`, agy keyring | read in place; never refreshed, never unlocked | the native leg | no |

- The resolver is defined for the CLI, the daemon and the helper process separately.
- A missing env var in the daemon yields a typed `Failure::AuthRequired{hint: "set X in the daemon's
  environment"}`.
- Keys are only ever displayed masked (`sk-or-v1-f33…2cd`).
- Rotating a key means rewriting the file or env; the fingerprint change invalidates its cache.

### 4.4 Hot swap: two executors

**Executor A — subscription (OAuth).** Unchanged: today's `swap_eligible` checks verbatim, plus
`swap_to` with the drain, mtime bump, repoint, `RotationGuard` and state flock. Nothing in this plan
touches it.

**Executor B — API-key, same effective transport.**
- **Class.** Same harness; same `routing_endpoint()` origin (conservative normalisation: scheme,
  host, port, path; query preserved); same `ModelSettings`; same env with the endpoint key removed;
  no `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` in env on either side; **no OAuth store on either
  side**. Hybrids (OAuth pair + key) are excluded, and so is the shunt gateway endpoint.
- **Launch snapshot.** At spawn, the session persists its effective transport (resolved endpoint,
  models, env hash). Eligibility compares against that snapshot, never against a re-read of mutable
  profile config (today the daemon reconstructs it: `scheduler.rs:4635`).
- **Dispatch.**
  - `SessionSwap` records its executor (A or B) at spawn, from the launch snapshot.
  - `poll` routes by it **before** anything else. A B-session never reaches `swap_to`, and an
    A-session never reaches B.
  - Today `poll` calls `swap_to` whenever `intended_member ≠ SwapCell.member` (`runtime.rs:3079-3094`),
    and only `swap_to` moves the cell. B keeps its own cell, so its intent converges and is not
    refused on every tick.
- **Initial state.**
  - At registration a B-session publishes `current_member = start_profile` and `key_generation = 0`
    through `SessionFields`. The helper therefore never sees `None`.
  - As a defensive fallback, the helper also falls back to `start_profile` if the row predates the
    field.
- **Protocol** (requested → committed → served):
  1. The daemon or `clauth switch <sid> <p>` writes `intended_member` via `DaemonFields` (as today).
  2. Executor B takes the **state flock first** (rank 500). It holds `SwapCell` (rank 550) only
     around the `SessionFields` publish in step 3, the same order as A and the watchdog reader
     (`lockorder.rs:190-204`). Inside the state hold it:
     - **claims the target member's liveness marker**, keeping every marker the session has held
       for its whole life (the same invariant as A's `held` vec, `runtime.rs:3203`);
     - **re-validates inside the hold**: the profile still exists and is not disabled, the class
       still matches the launch snapshot, the key resolves, and the session is not shutting down
       and not isolated (`runtime.rs:3163`).

     A `Foreign` marker, or any failed re-check, is refused with a typed `SwapRefused`, and nothing
     is published.
  3. **Commit** through `SessionFields` only (`live_sessions.rs:148-160`; `DaemonFields` cannot set
     it): `current_member = p`, `key_generation += 1`, `committed_at`.
     - No `RotationGuard`: there is no credential store to rotate.
     - If publishing fails after the marker claim, the claim is released (rollback).
     - At teardown, `release_swapped_markers` releases every held marker, as it does for A.
  4. **Served (acknowledged).** The helper records which generation it served in the sidecar
     `live_sessions/<sid>.helper`. It never writes the live-session row, so the row keeps exactly
     two writers. The sidecar has its own rules:
     - **Ack only after success.** The helper acknowledges only after it has written the key to
       stdout and that write has flushed with exit status 0. Any output failure leaves the
       generation unacknowledged.
     - **Monotonic, atomic, concurrent-safe.** Several helpers can run at once (the session itself
       plus child Claude processes). Each one takes a short flock on a **separate, stable**
       `live_sessions/<sid>.helper.lock` file. That file is never renamed, because a lock on an
       inode that a rename replaces does not exclude later writers. Holding that lock, the helper
       reads the current ack, compares generations, and only if its own is **newer** writes a
       temporary file and renames it over `<sid>.helper`. An invocation that read generation N−1
       can never overwrite an N ack.
     - A swap counts as **served** when the sidecar's generation ≥ the committed generation.
     - Until then the TUI, CLI and herdr all show `swapping…`. There is **no guaranteed deadline**:
       the expected upper bound is one helper TTL while the helper is healthy.
     - Requests already in flight may complete on the previous key. That is documented and accepted.
     - A helper failure (exit ≠ 0, or an empty key) leaves the state at committed-not-served and
       raises a typed warning after 2 × TTL.
- **Helper.**
  - Only `write_merged_settings` (the runtime settings, `runtime.rs:4869`) emits
    `<exe> __api-key --session <sid>`. The global `settings.json` keeps `<exe> __api-key <profile>`.
  - `profile_name_from_helper` learns the new form.
  - `apiKeyHelper` stays in `PER_PROFILE_TOP_FIELDS`, so sync never distributes it.
  - The session form works as follows:
    - it reads `current_member` + `key_generation` from a lock-free live-session read;
    - it reads the key from a lock-free `config.toml` parse;
    - it prints and flushes the key, and only then writes its monotonic sidecar ack;
    - it does no `load_profile`, takes no state flock and makes no network call, with a target
      under 50 ms. The only lock it takes is the per-session `<sid>.helper.lock`, around the ack.
  - Child Claude processes inherit the runtime `settings.json`, so they share the session's key
    (intended).
- **TTL.** `CLAUDE_CODE_API_KEY_HELPER_TTL_MS=30000` is set on the **child process env, after**
  `scrub_profile_env` (`runtime.rs:4442`), and never in settings `env`. Otherwise `sync_once` would
  publish it into `~/.claude/settings.json`.
- **Chain.** `--with-fallback` currently refuses `!is_oauth()` (`fallback.rs:1452`), and the
  `StartBlock::NotOauth` and daemon decision leg would need class awareness. v1 supports **manual**
  API-key swaps only (`clauth switch <sid> <p>`, TUI, herdr). Chain rotation across keys needs a
  balance-exhaustion predicate and is deferred to a later phase (P6c).
- **What Claude Code documents** (2026-09-29): the helper output is cached for 5 min by default and
  `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` overrides it; the value is sent as both `Authorization` and
  `x-api-key`; `apiKeyHelper` is hot-reloaded from settings files; auth env beats the helper; `model`
  is read at startup only.

  Undocumented: whether `env` hot-reloads, whether a 401 forces a re-run, and whether the cache is
  keyed by the command string.

**Relaunch in place (cross-provider, separate phase P6b).**
1. Resolve the runtime id `<pid>-<seq>` to a conversation id (the transcript stem). If more than
   one matches, refuse.
2. Resolve every precondition first: the target profile, the cwd, the original args, and
   `follows_chain`.
3. Then stop gracefully, wait for the transcript flush, and start
   `clauth start [--with-fallback] <p> -- --resume <conv>`. Note that `resume` alone drops
   `--with-fallback`.
4. If the relaunch fails, restart under the original profile.

In herdr this uses the existing `pane run` path (`daemon/api/create.rs:100`). It always asks for
confirmation.

### 4.5 Look and feel

**Palette engine** (`tui/theme.rs`)
- There is a **new** key, `palette = "catppuccin" | "omarchy" | "auto"`. The existing `theme`
  (colour depth) is untouched.
- The Catppuccin table is kept **verbatim**, including the hand-picked 256 indices, so it renders
  pixel-identical. For Omarchy, the RGB comes from `colors.toml` and the 256 index is computed as
  the nearest match.
- Sources are read in order: `~/.local/state/omarchy/current/theme/colors.toml`, then
  `~/.config/omarchy/current/theme/colors.toml`.
- Mapping:

  | Omarchy | clauth role |
  |---|---|
  | `accent` | accent |
  | `orange` | accent_2 |
  | `foreground` / `dark_foreground` | text / text_faint |
  | blend(fg, bg, .72) | text_dim |
  | `background` / `darker_background` / `lighter_background` | bg / bg_sunken / bg_hover |
  | `muted` / `selection` | line / line_strong |
  | `red` / `yellow` / `green` / `cyan` | danger / warning / success / info |
  | blends | `bg_danger` / `bg_warning` banner tints |

- **Reload.** The file is checked every 2 s by mtime and by symlink-target change. A malformed file
  keeps the last good palette and shows one toast.
- The CLI uses the same palette. It turns off under `NO_COLOR`, when stdout is not a TTY, or with
  `--plain`.
- **Default** — see D6.

**TUI: restyle the 8 tabs; no navigation change in v1**
- **Metric card** (Usage tab and the account detail): three rows — a bold label with the countdown
  and local time right-aligned; a full-width bar with the value, a pace glyph and an elapsed marker
  `│`; a dim footnote (`40% elapsed · 12 pts under`).
- **Accounts pane** (Overview): grouped by provider. Each row shows the active mark `●`, the lead
  metric (% or $), a 12-cell mini bar, the countdown, flags (`⏸` stale, `⚠` error, `↻` refreshing)
  and the route.
- **API-key card:**
  ```
  or-main · OpenRouter · key sk-or-v1-f33…2cd
  Credit balance                                        $13.67 left
  ███████████████████████████████████████████████████░   98% used   CRITICAL
    $886.33 of $900.00 (provider limit)
  Spend          today $0.00 · week $4.08 · month $4.46
  ```
- **Narrow mode** (< 100 cols, or the herdr popup): the account list collapses into a top strip.
- The existing header, footer and keys stay. Tab merging (6 tabs) is a separate, later project and
  needs a `HomeTab` alias and migration map.
- Every render is covered by `buffer_rows` goldens at 80, 120 and popup widths, at both tiers, with
  both palettes.

**Severity (one table, shared by all sinks)**

| Measure | ok | mid | high (`LOW`) | critical (`CRITICAL`) |
|---|---|---|---|---|
| used % | < 50 | ≥ 50 | ≥ 75 | ≥ 90, or ≥ switch threshold (clamped ≥ 75) |
| balance (USD equivalent) | ≥ 20 | < 20 | < 5 | < 1 or negative |
| pace delta | < −10 | ≥ −10 | > 0 | ≥ +10 |

The overall severity is the worst of the three. Colour: accent / warning / danger.

**Countdown.** `4d 1h`, `3h 05m`, `5m`, `now`, `—`, followed by the local time in parentheses.
Values truncate and never round up. One fixture table is shared by the CLI, TUI, JSON and herdr.

**CLI**

| Command | Change |
|---|---|
| `clauth usage [--json] [--account A] [--provider P] [--watch N] [--plain]` | **new.** Grouped report built from observations. |
| `clauth list` | Colour and mini bars on a TTY only. The plain path stays on today's formatter (`list.rs:240`), which is the byte-identical compatibility oracle. |
| `clauth providers list \| detect \| status` | **new.** `detect` never touches the network; `add` comes later. |
| `clauth switch <sid> <p> [--relaunch]` | Executor B within a class; `--relaunch` across classes (P6b). |
| `clauth bar` | optional, P8. |

### 4.6 Herdr

| # | Change |
|---|---|
| H0 | **Fork distribution.** `GITHUB_SOURCE` / `GITHUB_REMOTE` (`herdr.rs:32-36`) become configurable, defaulting to the build's repository, so the fork installs its own plugin. |
| H1 | The `$clauth` tag is built from observations, e.g. `leadtone 2%`, `or-main $13.67`, with a severity class passed through `pane report-metadata`. It reads `current_member` (committed), never `intended_member`. For a B-session it shows `old → new swapping…` until the helper sidecar acknowledges the generation. |
| H2 | Native panes (codex, grok, agy): define account-to-pane matching; an ambiguous or unmanaged pane gets no tag; tags are cleaned up on exit. `report-profile.sh` / `watch-profile.sh` get grok / agy legs. Port `tests/herdr_provider_labels.rs`. |
| H3 | The popup uses the narrow layout. |
| H4 | `clauth.swap` action: executor B for the focused pane's session. It joins by **PID** as the REST bridge does (never by a display tag), refuses delegate panes, and offers relaunch across classes. |
| H5 | Compat suite: recorded herdr 0.8.x / 0.9.x JSON (`api snapshot`, `pane list`, `plugin list --json`) replayed through the parsers. |

## 5. Delivery plan

Each row is one PR, green on `cargo nextest` + clippy, **with its docs and wiki change in the same PR**.

| # | Content | Depends on |
|---|---|---|
| P0 | Branch `feat/redesign`. Cherry-pick `bddf0535` (RotationGuard unlock-on-drop), also PR'd upstream. Countdown, severity and pace fixture tables. | — |
| S1 | Spike, version-pinned against the installed `claude`, driving a **local stub endpoint** (a fake claude cannot prove CC's cache). Prove: (a) an **unchanged** helper command is re-executed after a 30 s TTL and its new stdout is sent; (b) behaviour on 401; (c) whether `env` hot-reloads; (d) a helper failure or empty output mid-session, and what CC does next; (e) a helper invocation spanning a commit, i.e. which generation is served; (f) whether requests in flight at the commit keep the old key. **Release gate for executor B:** (a) passes; (d) leaves the swap committed-not-served with a warning, and never reports it as served; (e) and (f) never regress an acknowledgement and never mark a stale generation served. Otherwise API-key swaps fall back to relaunch. | P0 |
| S2 | Spike: Antigravity — the branch's Cloud Code read (never unlocks the keyring, Linux-only) vs `agy --print /usage` (offline probe, version gate ≥ 1.1.11). Does either spend quota or die? | P0 |
| S3 | Spike: exact attribution. Can a transcript *record* be tied to the profile that served it? Shared runtimes symlink `projects/`, conversations span swaps, and the owner stamp overwrites. If record-level attribution isn't possible, `estimate = None`. | P0 |
| P1a | Observation types + projections + `status.json` `accounts[]` + `clauth usage --json`. No visual change. | P0 |
| P1b | Credential slots + `ProviderHttp` policy + env scrubbing for billing refs. | P0 |
| P2 | Palette engine (`palette` key, Omarchy source, reload). Catppuccin pixel-identical. | P0 |
| P3 | CLI: the `clauth usage` text report, colour `list` on a TTY, a shared countdown / pace / severity module. | P1a, P2 |
| P4a | `UsageSource` trait; existing providers wrapped in place; money meters for DeepSeek and OpenRouter from raw amounts. | P1a, P1b |
| P4b | Native Grok + Antigravity legs (port `provider_monitor`); `clauth providers`. | P4a, S2 |
| P4c-1…n | **One PR per new source**: Anthropic Admin cost_report · Moonshot · xAI management · OpenAI admin costs. | P4a, P1b |
| P4d | Local estimate. | S3 (only if it passes) |
| P5 | TUI restyle of the 8 tabs: metric cards, accounts pane, API-key card, narrow mode. | P1a, P2, P4a |
| P6a | Executor B: launch snapshot, class, commit protocol, session helper form + parser, TTL on the child env, manual switch in CLI / TUI. | S1, P1a |
| P6b | Relaunch in place. | P6a |
| P6c | *(later)* Class-aware chain rotation with a balance-exhaustion predicate. | P6a, P4a |
| P7 | Herdr H0–H5. | P5, P6a |
| P8 | README / wiki screenshots via `showcase.rs`; optional `clauth bar`. | all |

Parallel lanes after P0: {S1 → P6a → P6b}, {P1a + P1b → P4a → P4b / P4c}, {P2 → P3 → P5}.
**Offered upstream:** P0, P1a, P1b, P2 (default Catppuccin), P4a. **Fork-first:** P5, P6, P7-H0.

## 6. Test strategy

- **Goldens.** `buffer_rows` goldens at each width, tier and palette. There is no new snapshot
  dependency unless the owner wants `insta`.
- **Parity fixtures.** The countdown, severity and pace tables are shared by the CLI, TUI, JSON and
  herdr.
- **Providers.**
  - Recorded fixtures for happy, empty, 401, 429 + `Retry-After`, 5xx, malformed, negative and
    sub-cent balances, CNY, paginated or partial billing results, and a cross-origin redirect
    (which must be refused).
  - Any real network call fails the test.
- **Credentials.**
  - A missing env reference; the billing key never printed by `__api-key`; the billing ref scrubbed
    from child env.
  - Fingerprint change invalidates the cache.
- **Swap.**
  - The class matrix as property tests.
  - Rejected intent: disabled, isolated, class mismatch, key missing.
  - Races: concurrent swap / delete / shutdown.
  - The helper form survives `write_merged_settings` rebuilds and `sync_once`; the TTL var never
    reaches `~/.claude/settings.json`.
  - The OAuth mtime receipts and macOS Keychain convergence are unchanged.
  - Daemonless and older-daemon behaviour.
- **Palette.** Reload on mtime and on symlink change; a malformed file keeps the last good palette;
  tests are isolated from `TierTest`.
- **Compat.**
  - `profiles.toml` round-trips unchanged.
  - `status.json` schema 2 is a superset (a golden diff).
  - Plain `clauth list` is byte-identical to today's formatter.
  - The `HomeTab` values are unchanged.

## 7. Risks

| Risk | Mitigation |
|---|---|
| Upstream velocity: 74 commits in 8 days, and `app.rs` is 11.4k lines | Additive modules; small PRs; a weekly rebase; the restyle stays inside `render/`. |
| CC's helper caching differs from the docs, or changes between versions | S1 is version-pinned; executor B is gated on it; relaunch is the fallback; the CC version is recorded in the spike result. |
| Private endpoints (wham, the grok proxy, Cloud Code) drift | A typed `InvalidResponse`, stale retention, and fixtures refreshed with each fix. |
| Billing keys are org-wide | Env reference only, fixed origins, never in child env, and masked everywhere. |
| Money correctness | `Decimal` with an ISO currency; a sign rule (`-$5.71`); no parsing of display strings. |
| Wrong per-profile totals | No estimate unless S3 proves record-level attribution. |
| Omarchy palette surprises existing users | Covered by D6. The `catppuccin` table is verbatim. |

## 8. Open decisions (for the owner)

| # | Decision | Grok | Codex | Recommendation |
|---|---|---|---|---|
| D1 | Hot-swap scope | API-key live swap only with no OAuth store and no auth env; OAuth unchanged; hybrids → relaunch | Same-effective-transport swaps gated by S1; relaunch separately | **Executor B as in §4.4, gated by S1; relaunch as P6b** (both agree) |
| D2 | Upstream strategy | P0–P2 upstream; P5 / P6 fork | Additive upstream PRs; isolate fork navigation | **As in §5** (both agree) |
| D3 | Provider priority | Anthropic API (Admin + estimate) first | Existing OpenRouter / DeepSeek / Z.ai first; Anthropic billing after the security gates | **Reviewers split — owner's call.** Suggest the existing meters first (cheap, low risk), then Anthropic Admin. |
| D4 | TUI navigation | Keep 8 tabs, restyle | Restyle 8 first; merge later | **Keep 8** (both agree) |
| D5 | `clauth bar` | Optional P8 | Optional follow-up | **Optional P8** |
| D6 | Default palette | `catppuccin`; `auto` would change every Omarchy user's screen on upgrade | `auto` for this fork, with a Catppuccin fallback | **Reviewers split — owner's call.** Suggest `auto` in the fork's build and `catppuccin` in upstream PRs. |
| D7 | Antigravity source | Branch's Cloud Code read; `agy --print` only if S2 fails | Provisionally the direct read; needs S2 and a platform fallback | **Branch read, confirmed by S2** |

## 9. Review log (v1 → v2)

| Finding (raised by) | Change |
|---|---|
| A helper reading `intended_member` bypasses validation and drives the OAuth executor (both) | Two executors; B commits `current_member`; the helper reads committed state lock-free (§4.4). |
| `--relaunch` confuses a runtime id with a conversation id, and `resume` drops `--with-fallback` (both) | An explicit resolution protocol; its own phase P6b. |
| Env-referenced secrets have no ownership model; an admin key could leak into inference (both) | Credential slots (§4.3), `ProviderHttp`, scrub lists. |
| A whole transcript can't be attributed to its latest profile (both) | S3 requires record-level attribution, or no estimate. |
| Swap class must be the effective transport; the launch snapshot is re-read from mutable config (both) | A class definition plus a persisted launch snapshot. |
| A second poller double-fetches (both) | Another leg of the one scheduler; single owner; cache identity. |
| The report shape loses unknown %, exhaustion, models, timestamps, best_effort; `Cents` loses sub-cent and negatives (both) | `AccountObservation` and `Decimal` (§4.1). |
| The TTL var would sync into global settings (Grok) | Set on the child env after the scrub. |
| S1 tested the wrong mechanism, and a fake claude can't prove caching (both) | S1 retargeted at an unchanged command against a stub endpoint, version-pinned. |
| `theme` already means colour depth; the 256 pairs are hand-picked (both) | A new `palette` key; the Catppuccin table is verbatim. |
| Merging tabs breaks the persisted `HomeTab` and per-tab keys (both) | Keep 8 tabs in v1. |
| A class filter doesn't make wallets rotate; `--with-fallback` refuses non-OAuth (Grok) | Manual API-key swaps in v1; chain rotation deferred to P6c. |
| The herdr install is hard-coded to upstream; grok / agy are excluded from the reporters; the bridge joins by PID (Codex) | H0, H2, H4. |
| The mockup's 28% used / 40% elapsed is not "on pace" (Codex) | Fixed: "12 pts under". |
| Phases were too large (both) | P1a / b, P4c per source, P6a / b / c, docs with each PR. |

**Round 2 (v2 → v2.1):** Grok gave `APPROVE WITH CHANGES`, Codex gave `REVISE`.

| Finding (raised by) | Change |
|---|---|
| `poll` still calls `swap_to` on any intent difference, so executor B is never reached (Grok) | Executor chosen at spawn; `poll` routes before `swap_to`; B has its own cell. |
| `current_member` is `None` at spawn, so the helper fails closed (Grok) | Publish `start_profile` + generation 0 at registration; `start_profile` fallback. |
| `DaemonFields` cannot set `current_member` (Grok) | Commit through `SessionFields`. |
| An elapsed TTL is not evidence the swap took effect (Codex) | Generation counter + helper sidecar ack; `swapping…` until served; no deadline claim; failure warning; S1 (d)–(f). |
| Executor B lacked marker ownership and in-hold re-validation (Codex) | Claim and hold markers for life; re-validate under the state flock; rollback; teardown release. |
| "Fixed billing origins only" contradicted unchanged generic fetchers (Codex) | Two origin policies; billing keys excluded from the configured-endpoint policy. |
| The herdr tag flips at commit while CC still sends the old key (Grok) | H1 shows `swapping…` until acknowledged. |

**Round 3 (v2.1 → v2.2):** Grok gave `APPROVE WITH CHANGES`, Codex gave `REVISE`.

| Finding (raised by) | Change |
|---|---|
| An ack could be written without successful key output (Codex) | Ack only after the key is written and flushed with exit status 0. |
| Concurrent helpers (the session plus children) overwrite the sidecar and can regress it (both) | Temp file + atomic rename; write only if the generation is newer, under a sidecar flock. |
| Executor B took ranked locks before the state flock, inverting `State` 500 < `SwapCell` 550 (Grok) | State first; `SwapCell` only around the publish. |
| S1's release gate ignored the safety cases (Codex) | The gate now requires the correct outcomes for (d)–(f). |

**Round 4 (v2.2 → v2.3):** Grok gave `APPROVE WITH CHANGES`, Codex gave `REVISE`, and both raised the
same issue.

| Finding (raised by) | Change |
|---|---|
| A flock on a file that a rename replaces does not exclude later writers (Codex) | A separate, stable `<sid>.helper.lock` is held across read, compare and rename. |
| "The helper takes no flock" contradicted the ack lock (both) | Reworded: no *state* flock; the per-session ack lock is the only lock. |
