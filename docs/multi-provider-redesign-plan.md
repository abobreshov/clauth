# `<tool>` — separate multi-provider tool forked from clauth — plan

Status: **v3.1 — review round v3-1 applied (Grok APPROVE WITH CHANGES, Codex REVISE).** Supersedes
v2.3 (Grok + Codex approved, round 5, 2026-09-29; `87ceb3da:docs/multi-provider-redesign-plan.md`).
The executor A/B protocol, credential slots, `ProviderHttp`, observation model and tests are restated
here in full and are normative; v2.3 is history only (§9). Reviews: `docs/multi-provider-redesign-reviews.md`.
2026-09-29 · base `mommy` @ b7d7cb02 (= upstream uwuclxdy/clauth) · fork github.com/abobreshov/clauth

**Owner's direction (2026-09-29):** a *separate tool*, never pulled into the original; first
providers **Ollama Cloud, Nous (Portal + Hermes Agent), OpenRouter**. Earlier asks stand: the look of
ai-usagebar / omarchy-agent-bar, API-key usage, hot swap inside one provider, herdr.

`<tool>` is a placeholder name used throughout; see D8. Hermes source citations (`hermes_cli/…`,
`agent/…`, `hermes_*.py`) are relative to `$HSP` =
`~/.local/share/mise/installs/pipx-hermes-agent/0.19.0/hermes-agent/lib/python3.13/site-packages/`.
Line numbers of the form `87ceb3da:…:N` point into the superseded v2.3 file. Bare `src/` paths are
upstream clauth at b7d7cb02.

## 1. Goal

1. **Separate tool.** A hard fork with its own binary, data dir, plugin ids, release keys and update
   channel. It can be installed next to upstream clauth 0.16.0 during migration, in **guest mode**
   until import (§4.0), and it replaces upstream once the owner's accounts are imported.
2. **Look and feel.** Bring the CLI and TUI close to
   [ai-usagebar](https://github.com/akitaonrails/ai-usagebar) and
   [omarchy-agent-bar](https://github.com/othavi0/omarchy-agent-bar): calm metric cards, one lead
   metric per account, pace and countdown lines, severity in words as well as colour, and the Omarchy
   palette by default. Both references are primarily **desktop bar** frontends (an Omarchy Quattro
   panel + Waybar widget, `refs/ai-usagebar/README.md:3,17`; a Quickshell chip + popup,
   `refs/omarchy-agent-bar/README.md:3-13`), so a Waybar JSON output ships in P3 (§4.5, D5).
3. **Multi-provider, including API-key accounts.** One lossless observation per account, covering
   quota windows, money meters and a local estimate only where attribution is exact. The priority
   order is **Ollama Cloud → Nous → OpenRouter**, then the rest of the v2.3 matrix (§4.7).
4. **Hot configuration swap inside one provider** = **credential / account swap only**.
   - Claude Code: executor A (OAuth, unchanged) and executor B (API key, same effective transport).
   - Hermes: **automatic pool failover only**; a manual account switch is a **relaunch** (H-4) unless
     S7(e) proves a live-selection mechanism with an acknowledgement (§4.8, D1).
   - A model or preset change within a provider is a relaunch (P6b) or CC's own in-session
     `/model`; CC reads `model` at startup only. Owner to confirm (D1).
   - Anything across providers stays an explicit relaunch.
5. **Herdr.** The popup, pane tags and daemon bridge work on herdr ≥ 0.9.1, for the `claude`,
   `codex` and `hermes` agent kinds, and ship as the fork's own herdr plugin.

### Non-goals (v1)

- Hot swap across providers mid-process. CC reads the endpoint and model env at spawn (§4.4).
- Codex hot swap. A session binds `auth.json` at start (`harness.rs:153-157`, `sessions_cli.rs:212`).
- **Claude Code on Nous.** Nous inference is OpenAI-wire only, no `/v1/messages`
  (`hermes_cli/proxy/adapters/nous_portal.py:33-41`, https://portal.nousresearch.com/api/openapi).
- **Owning Hermes' Nous OAuth refresh.** Hermes is the only writer of its own refresh chain
  (`hermes_cli/auth.py:5224-5246`).
- Chain rotation on wallet exhaustion before P6c; `src/tasks/*` of `feat/provider-monitoring`; IDE
  scrapers; upstreaming anything (D2).

## 2. What exists today

| Area | State on `mommy` | Where |
|---|---|---|
| Harnesses | `Harness{Claude,Codex}`. The `HarnessEngine` trait has two seams: credential install, and runtime spawn (`command`, `home_env_key`, `scrub_env`) | `harness.rs:19-83`, `docs/codex-plan.md:11-16` |
| API providers | Closed `enum Provider{DeepSeek,Zai,Alibaba,OpenRouter,MiniMax}` from the **managed** `base_url` only, plus a Generic scanner. No Ollama, Nous or Hermes | `providers/mod.rs:128-152`, `profile.rs:3190-3207` |
| Third-party → chain | `to_usage_info` keeps only 5h / 7d bars | `providers/mod.rs:434-469` |
| API key into CC | `apiKeyHelper = <exe> __api-key <profile>`. The writer clears `ANTHROPIC_AUTH_TOKEN` and never sets `ANTHROPIC_API_KEY`; `profile.env` is applied last. `profile_name_from_helper` accepts **any** exe before `__api-key`. The printer calls `load_profile`, which takes the state flock and may rewrite `credentials.json`. The helper string is rebuilt from `current_exe()` with a `(deleted)` strip after an in-place update | `claude.rs:2096-2130,2225-2244,2415-2489`, `main.rs:2156-2195`, `profile.rs:3130` |
| Claude credential set | `credentials.json` + optional `session-token.json` (install source when long-lived) + `session-token.static.json`; `quarantine/` copies; a crashed rotation leaves `credentials.json.pending`, adopted by the next `load_profile` | `claude.rs:39-53,152-175,315-355,385-473,585-598,890-899`, `profile.rs:2227-2228`, `runtime.rs:3963-3966` |
| Codex credential set | `profiles/<p>/auth.json` + `auth.lkg.json` + `auth.quarantine.json`; durable `codex-home/`; adoption makes `~/.codex/auth.json` an absolute **symlink**; `LinkMode::Fake` homes hold a copy | `codex_auth.rs:474-476,520-528,555-571`, `runtime.rs:6262-6265,6323-6343,6412-6440`, `actions.rs:1331-1343` |
| TUI exit | `shutdown` snapshots, then `detach_credentials_link` turns the live symlink into a regular same-bytes file ("external writes land in the standalone file"); the next start re-adopts | `tui/app.rs:11403-11429`, `claude.rs:1003-1017,2498-2531,2634-2655` |
| MCP server | tools `profiles`, `switch_profile`, `delegate`, `monitor`; Claude Code accounts only; exposed as `mcp__plugin_clauth_clauth__*` | `mcp/mod.rs`, `plugins/hooks/hooks.json` |
| Env scrub | `MANAGED_ENV_KEYS` (incl. `ANTHROPIC_API_KEY`) removed from the child's inherited env; a blank value doesn't count as auth | `runtime.rs:4424-4448`, `profile.rs:3080-3084` |
| Live swap | `intended_member` → `SessionSwap::poll` → `swap_to` under `RotationGuard` + state flock; with no standing intent `poll` runs `poll_converge`; `swap_eligible` refuses non-OAuth | `live_sessions.rs:60-75`, `runtime.rs:2677-2731,3079-3293` |
| Money | `f64` wallets; OpenRouter `fetch` reads `/credits` fatally (`?`) then `/key` best-effort (`.ok()`); `get_json` maps 401→AuthExpired and other ≥ 400 →Status | `src/providers/openrouter.rs:43-53` (fetch: `/credits` 44-46, `/key` 47-51), `58-136` (stats); `src/providers/mod.rs:540-571` |
| Usage leg | Schedules a third-party profile only when `Profile::api_key` is non-empty | `usage/scheduler.rs:2812-2830` |
| Presets | `base_url` + `ModelSettings` only, no env. OpenRouter = `openrouter/auto` default, no tier pins | `presets.rs:3-26,74-82,120-141` |
| TUI | ratatui 0.30, 8 persisted `HomeTab`s (closed serde enum, no `#[serde(other)]`), `app.rs` 11,434 lines. Catppuccin with hand-picked 256 pairs; `theme` means colour depth | `profile.rs:667-691`, `tui/theme.rs:112-167` |
| Herdr | Plugin id `clauth`, token `$clauth`, source hard-coded upstream; auto-heal only for owner `uwuclxdy`; reporters accept `claude\|codex` only | `herdr.rs:28-44,856-864`, `report-profile.sh:172-173,281-282`, `watch-profile.sh:67-68` |
| Live upstream on this machine | clauth 0.16.0 in `~/.cargo/bin`; `~/.claude/.credentials.json` is a **regular file** (0600, inode 5528140, 10:46), newer than and separate from the active `scifoo` store (inode 5480662, 09:47); `~/.codex/auth.json` regular (nlink 1), no `codex-profiles.toml`, no sidecars in the three profiles; `clauth@clauth` plugin at `~/.local/share/clauth/current@claude`; herdr block + `$clauth` sidebar; `.bashrc` completion; **10** `clauth mcp` processes; no `--standby` unit; `~`, `~/.clauth`, `~/.claude`, `~/.codex`, `~/.local/share` on one btrfs (dev 57) | `stat`/`ls`/`pgrep` 2026-09-29; `~/.claude/settings.json:40,71-74`, `~/.config/herdr/config.toml:105-114`, `~/.bashrc:25-26` |
| Hermes | Agent 0.19.0 behind the Omarchy shim `~/.local/bin/hermes`, which installs via mise when its interpreter dir is missing. `~/.hermes` has config.yaml (herdr-agent-state plugin), no auth.json / .env / provider. `$HSP` is user-owned (755), `$HSP/.env` absent, `/etc/hermes` absent | `~/.local/bin/hermes:14-31`, `~/.hermes/config.yaml:1-18`, `stat` |
| Ollama | 0.33.3 system service, `User=ollama`, `HOME=/var/lib/ollama`; the signed-in key belongs to the daemon (never read by the tool) | `/usr/lib/systemd/system/ollama.service` |

## 3. Design principles

1. **Real data or a typed unavailable state.** Never a fake 0. Freshness is separate from failure.
2. **Last good data stays visible while it is honestly stale**, up to 7 days.
3. **Colour never carries meaning alone.** Every severity rung has a word; pace uses `↑ → ↓`.
4. **Domain first, presentation second.** Observations serialise on their own; `Sections` is a
   projection.
5. **One scheduler, one owner per target.**
6. **Credential boundaries.** Inference keys and monitoring credentials are in separate slots.
   `__api-key` never prints a monitoring credential, and monitoring credentials never enter child env.
7. **Hermetic tests.** Fixtures only; any network call fails the test.
8. **One writer per refresh chain.** Every file that can carry a chain is *moved*, never copied
   (`docs/codex-plan.md:17-20`). The tool never refreshes a token that another program (upstream
   clauth, Hermes, the Ollama daemon) owns. A same-bytes copy that nobody refreshes (the detached live
   slot, D18) is tolerated only under the reconcile rule of §4.0.
9. **Coexist, then replace.** Every global resource is either renamed or owned by exactly one tool at
   a time (§4.0).

**Relaxed now that the tool is not upstream-bound:**

| v2.3 constraint | Why it existed | v3 |
|---|---|---|
| Principle 8 "upstream-shaped"; "offered upstream"; P0 PR'd upstream (`87ceb3da:docs/multi-provider-redesign-plan.md:73,367,387`) | upstream PRs | **Dropped** |
| D6 Catppuccin default | upstream users' screens | **Dropped**: default `auto` (D6). The Catppuccin table stays verbatim as the fallback |
| "Orange once per screen" (`tui/theme.rs:164`) | upstream theme rule | **Dropped**: Omarchy `orange` → `accent_2` freely |
| Plain `list` byte-identical to upstream (v2.3 §4.5) | upstream scripts | **Relaxed**: a documented, golden-tested plain format |
| `status.json` schema 2 additive-only (`codex-plan.md:22`) | upstream readers of `~/.clauth/status.json` | **Relaxed**: the feed moves to `~/.<tool>/status.json`. It starts as schema 2 + `accounts[]`, and later bumps are allowed on breaking change. Readers ignore unknown fields |
| Keep 8 tabs | persisted `HomeTab` (a user-data reason, `profile.rs:667-691`) | **Relaxed**: consolidation allowed via an alias map applied at import and load (D4) |
| Presets must match upstream's builtin table | upstream parity | **Dropped**: presets may carry a non-secret env allowlist (§4.7) |

**Kept, because they protect real users and data:** credential slots and `ProviderHttp`, one
scheduler, the lock order (`lockorder.rs:190-205`), the S1 gate for executor B, hermetic tests,
executor A untouched, `profiles.toml` round-trip, and the Decimal money rules.

## 4. Architecture

### 4.0 Separate tool: identity, coexistence and migration

**Identity module (R0).** Add `src/identity.rs` with `APP`, `DATA_DIR` (`.<tool>`), `ENV_PREFIX`,
`REPO_OWNER` / `REPO_NAME`, `RELEASE_API_URL`, `MINISIGN_PUBLIC_KEY`, `ASSET_PREFIX`,
`HERDR_PLUGIN_ID`, `HERDR_TOKEN`, `HERDR_DELEGATE_TOKEN`, `CC_PLUGIN_NAME`, `API_KEY_HELPER_SUBCMD`
and `DEFAULT_LISTEN`. Also add a `<TOOL>_HOME` override. Every chokepoint points at it. **Function,
module and file names stay** (`clauth_dir`, `claude.rs`, …), so an upstream change to those bodies
conflicts in one line at most. Cosmetic `"clauth"` literals (~580–591) are renamed lazily, only in
files the fork rewrites anyway; **executable-name matches are never lazy** (row below). Spawns of
the tool itself already use `current_exe()` (`daemon/mod.rs:281`, `claude.rs:2482`).

**Must-change inventory**

| Site | What | Where |
|---|---|---|
| Package | name, repository, homepage, version (reset); feeds `CARGO_PKG_NAME` to the MCP server info and the OpenAPI title | `Cargo.toml:2-10`, `mcp/mod.rs:5424-5428`, `daemon/api/routes.rs:975-980` |
| Binary in tests | `CARGO_BIN_EXE_clauth` in **7** files | `tests/{closed_reader,bare_non_tty,dump_openapi,devices_cli,mcp_await_job_hook,mcp_handshake,gateway_daemon}.rs` |
| CLI | clap `name`, `after_help` path; default listen `0.0.0.0:8443` → a new port | `cli.rs:22,25-31` |
| Data dir | `clauth_dir()` + two hard-coded bypasses | `profile.rs:1769-1771`, `completions.rs:479,555` |
| Env prefix | every `CLAUTH_*` goes through `identity::ENV_PREFIX`: `NO_UPDATE`, `NO_API`, `NO_COMPLETIONS`, `MCP_PROBE`, `MCP_DEPTH`, `DELEGATE_SESSION_ID` (the last two are inherited, so both tools would read each other's markers) + 3 in scripts. Tests follow the prefix. Renaming moves the only pre-R1 update kill switch (see R0) | `update.rs:14,63`, `daemon/mod.rs:238,241,248`, `completions.rs:506`, `mcp/mod.rs:60,1846,1852`, `report-profile.sh:30`, `watch-profile.sh:20,79`, `tests/gateway_daemon.rs:97,125`, `tests/inline/update.rs:127-151` |
| **Executable name matches** (runtime, not cosmetic) | every spawn of `clauth` by PATH name and every match of a process or herdr token named `clauth` goes through `identity::APP` / `identity::HERDR_TOKEN`: `pane run … clauth start` → `identity::APP`; `is_clauth_session` stem; `cmdline_is_daemon` argv0; the Windows `tasklist` match becomes an **exact** image name `identity::APP`.exe (a substring would let `daemon --replace` taskkill upstream's `clauth.exe` on a recycled pid, `probe.rs:515-521,572-582`); the `HerdrTokens` field via `#[serde(rename = …)]` or a map lookup; the plugin probe's `Command::new("clauth")` → `current_exe()` (never PATH: upstream's `clauth mcp` would run `gc_stale_runtimes` on `~/.clauth`, `plugin_probe.rs:158-162`); the `mcpServers` / `plugin.json` `command` value; `on_path("clauth")` → `on_path(identity::APP)`; herdr_report `TOKEN_KEY` and `--source`; `source_repo` → `identity::REPO_NAME` | `daemon/api/create.rs:375`, `daemon/api/panes.rs:324-335,347`, `herdr.rs:593-595,861`, `daemon/probe.rs:572-598`, `plugin_probe.rs:165,320`, `plugins/.claude-plugin/plugin.json:15`, `tui/app.rs:4357,4410`, `mcp/herdr_report.rs:117,293` |
| Self-update | `API_URL`, `MINISIGN_PUBLIC_KEY`, asset names, UA; compiled out in R0 (§5) | `update.rs:12,16-31,201-254,301-313` |
| Release / install | matrix `asset_name`; the `clauth-*` globs for sums and upload come from one `ASSET_PREFIX`, and CI checks that sums lines == uploaded binaries; trusted comment; `REPO`; the default `cargo install clauth` path (upstream's crate) | `.github/workflows/release.yml:51-136` (globs 113,136), `install.sh:4-110` (cargo 16-18) |
| Herdr | `PLUGIN_ID`, `OPEN_ACTION`, `GITHUB_SOURCE/REMOTE`, `TOKEN`, `DELEGATE_TOKEN`, `MARKER`; the heal owner check. Scripts call `$APP` (substituted at install or read from `HERDR_PLUGIN_ID`) instead of `clauth`: `herdr-plugin.toml:15` `command`, `report-profile.sh:68,207` (argv match `'clauth mcp'*`), `:232` (`clauth which`), `:244,251-265` (`clauth herdr config get`, token `clauth=`, `--clear-token clauth`), `watch-profile.sh:20,70`, `open-pane.sh:29` | `herdr.rs:28-44,856-864`, `herdr-plugin/herdr-plugin.toml:1-28`, `herdr-plugin/*.sh` |
| CC plugin | marketplace + plugin name, author, `mcpServers` key **and `command`**; hooks commands and matcher `mcp__plugin_clauth_clauth__delegate$`; agentgear `#[plugin(name)]` → `~/.local/share/<name>`; registry keys `clauth` / `clauth@clauth` / `data_dir/clauth/current@claude` | `.claude-plugin/marketplace.json:3-13`, `plugins/.claude-plugin/plugin.json`, `plugins/hooks/hooks.json:1-58`, `plugin_host.rs:36-37,423,447,504` |
| Plugin probe | `clauth@clauth`, `MARKETPLACE_KEY`, manual `mcpServers.clauth` | `plugin_probe.rs:23-25,226,290-320` |
| Helper token | `claude.rs`'s private `API_KEY_HELPER_SUBCMD = "__api-key"` becomes a re-export of `identity::API_KEY_HELPER_SUBCMD` (one constant, no collision). **Exe check** in the parser: accept when the canonicalised path equals `current_exe()` after the `(deleted)` strip, or when the file name equals `identity::APP`; on mismatch, **rewrite** (self-heal). The rewrite targets per-session runtime settings always, and global `~/.claude/settings.json` only when `import-journal.json` shows a completed, not rolled-back import. In guest mode a mismatch is reported, never rewritten. (The upstream helper carries `__api-key`, which the fork's parser never matches, `claude.rs:2109-2130,2230,2244-2250`) | `claude.rs:2096-2130,2225-2250` |
| Completions | bash `_clauth` / `clauth __complete` / `complete -F _clauth clauth` (10-83), zsh `#compdef clauth` (85-204), fish `__clauth_*` / `complete -c clauth` (206-294); the two `~/.clauth` bypasses | `completions.rs:10-294,479,555-578` |
| Links | TUI "report at uwuclxdy/clauth/issues"; SECURITY.md + issue-template security link | `tui/render/usage.rs:1596`, `SECURITY.md:62`, `.github/ISSUE_TEMPLATE/config.yml` |
| CI | octocov repo + datastore (delete or repoint); artifact names | `.octocov.yml:1,31,35`, `.github/workflows/ci.yml:176-191` |
| Keep | data-dir file names, lego TLS paths, parity UAs (CC / codex imitation), `deny.toml` | `daemon/mod.rs:77-94`, `daemon/api/tls.rs:54-58`, `oauth.rs:85` |
| Keep, risk noted | price feed `raw.githubusercontent.com/uwuclxdy/ai-pricelog` (upstream-owned data; mirror in P8, D15) | `pricing.rs:109-135` |

**Coexistence with the live upstream 0.16.0**

| Global resource | Collision | Rule |
|---|---|---|
| `~/.claude/.credentials.json` slot | only one owner; the other sees `LinkState::Diverged` (`claude.rs:917-976`). Upstream TUI exit detaches it into a regular file (`app.rs:11428`), and upstream re-adopts a regular file whose store is absent (`claude.rs:1003-1017,2498-2531`) | Upstream owns it until `import`; after that, `<tool>` owns it and upstream's binary is retired in the same transaction. The fork keeps the detach (D18) and reconciles by access token |
| OAuth refresh chains | two stores holding one chain is the permanent-death case (`codex-plan.md:17-20`) | `import` **moves** every chain carrier (table below); nothing that can hold a chain is copied |
| Daemons **and TUIs** | separate singletons would both auto-switch the slot (`daemon/mod.rs:78-91`); every upstream TUI also runs the refresher, and whichever process holds `~/.clauth/usage-fetch.lock` "fetches usage, rotates tokens, and decides switches" (`wiki/Daemon.md:37`, `daemon/mod.rs:88-91`) | `<tool>` (daemon **and** TUI refresher) try-locks `~/.clauth/clauthd.lock` and `~/.clauth/usage-fetch.lock` at hard-coded upstream paths outside `identity::DATA_DIR`, and probes the standby slot; if any is held, it refuses to rotate or switch. `clauthd-standby.lock` holders are parked and never refresh (`daemon/mod.rs:491-495`), but promote to a full daemon the instant `clauthd.lock` frees (`mod.rs:402-470`); import refuses while `probe::standby_waiting()` is true (`probe.rs:675-688`) and rechecks it before releasing its own `clauthd.lock` hold |
| **Guest mode (pre-import)** | a pre-import `<tool>` could otherwise write upstream-owned files | Until `~/.<tool>/import-journal.json` shows a completed import, `<tool>` writes none of `~/.claude/settings.json`, `~/.claude/.credentials.json`, `~/.claude.json`, `~/.codex/*`, the herdr config or the CC plugin registry; runs no Claude / Codex OAuth legs; allows only Ollama, OpenRouter, Nous and Hermes profiles, launched through per-session runtime settings; disables the `settings_sync` write-back to the base (`settings_sync.rs:10-23,30-34`) and every `~/.claude.json` write (`claude_json::strip_home_oauth_account`, the jsonsync reconciler); guest runtime copies are never sync members of the operator base; the plugin-host heal is a no-op. The import transaction (M0, M6–M8) is the only exception |
| `settings.json` `apiKeyHelper` / env / model / `permissions.allow` | both writers target it (`claude.rs` apply, `settings_sync.rs:19-29`) | new subcommand token + exe check; one owner at a time (guest mode writes none) |
| `~/.clauth/profiles.toml` | upstream re-attaches unmodelled keys on rewrite (`profile.rs:2477-2512`); `codex-profiles.toml` drops them (`codex_profiles.rs:201-209`; its header doc at 10-11 is stale). A fork-only `home_tab` value is a hard parse error for upstream (`profile.rs:667-679`) | never shared; `<tool>` uses `~/.<tool>`; rollback never copies the fork's rosters back |
| CC plugin, marketplace, agentgear root, `mcpServers.clauth` | same name overwrites the pointer (`agentgear host.rs:470-490`); `plugin_host::heal_detached` repairs `clauth@clauth` from any pre-rename fork `mcp` / daemon, ungated by updates (`plugin_host.rs:329-340`, `mcp/mod.rs:5547`) | new names; the heal is a no-op from R0 until R2's rename |
| herdr plugin id / action / token | two plugins sharing an id are undocumented (https://herdr.dev/docs/plugins/) | new id; spike S8(c) gates R2 |
| Port 8443, completion fns, env prefix | bind failure / clobbering | new defaults |

**Migration: `<tool> import clauth [--dry-run]` — one writer-exclusive transaction**

`--dry-run` runs the M1 / M2 checks and the M3 try-locks (released at once), prints the M0 actions
and the journal it would write, and changes nothing. The real run:

| # | Step | Rule |
|---|---|---|
| M-1 | Precheck (no locks, no change) | Run M1 + M2 without locks; refuse before any change |
| M0 | Pre-phase (reversible, journaled, before any lock) | Disable `clauth@clauth` in `enabledPlugins` and run `clauth herdr uninstall` (`herdr.rs:1557-1659`). Otherwise upstream's SessionStart hooks keep running `clauth self-heal` / `clauth hook-profile-changed-note` against the emptied `~/.clauth` (`plugins/hooks/hooks.json:14-28`). It runs before the locks because the upstream command takes the state lock itself. Journaled in a `pre` section of `import-journal.json` created here (M4 appends the main section). Any refusal before M5 undoes M0. `pre` entries never count as a completed import |
| M1 | Stop-the-world check | Refuse while any `clauth`, `claude` or `codex` process is alive (close every CC session first: the 10 `clauth mcp` processes), a `live_sessions` row exists, a sessions marker is flock-held, or `probe::standby_waiting()` |
| M2 | Static refusals | Refuse on: any `profiles/<p>/*.pending` (a crashed rotation's staged, possibly newest chain, adopted by the next `load_profile`, `profile.rs:2227`, `runtime.rs:3963-3966`: run upstream `clauth list` once, then retry); any entry the M5 table does not list; any destination that exists (guest-mode profiles may share names; roster names stay unique across the claude / codex / hermes rosters, `actions::validate_profile_name`, `codex_profiles.rs:13-19`); `src` and `dst` on different `st_dev` (a rename would give `EXDEV`; never fall back to copying a carrier; moot on this machine, needed for `<TOOL>_HOME`) |
| M3 | Take and **hold** until M8 commits | Upstream `~/.clauth/clauthd.lock` (try; refuse if held), `usage-fetch.lock` (try; refuse if held), `rotation-locks/<p>.lock` for every moved profile in name order (= upstream's `RotationGuard`) via the non-blocking form (`RotationGuard::try_acquire`, `runtime.rs:2482-2497`; `Ok(None)` → refuse; the blocking `acquire` has no deadline, `runtime.rs:2188-2189`), then state `~/.clauth/.lock`, then the fork's state lock. Rank order as upstream: rotation outermost, then state (`actions.rs:1443-1447`; ranks `lockorder.rs:117` Rotation = 100, `:191` State = 500; `runtime.rs:2132-2197`). Upstream state-lock waiters time out after 25 s (`lock.rs:47`), so M3–M8 is non-interactive and short; nothing waits on the owner inside the hold |
| M4 | Revalidate under the locks + journal | Repeat M1 and M2. Then write-ahead journal (the main section of `~/.<tool>/import-journal.json`, after M0's `pre` section): per entry `{op: move\|copy\|link\|rewrite, src, dst, prior_state (inode, type, link target)}`, fsync the file and its directory **before** the operation, mark done (fsync) after. Crash replay resumes or reverses from the last done entry |
| M5 | Move / copy per path | Table below |
| M6 | Live slots | Claude and Codex rules below |
| M7 | Rewrites | Remap `~/.claude/plugins/installed_plugins.json` install paths under `~/.clauth/profiles/` with the same `agentgear::repoint_install_paths` remap upstream uses (`plugin_host.rs:86-110`). Rewrite `settings.json` `apiKeyHelper` to the new exe + subcommand, and any `permissions.allow` entry naming `mcp__plugin_clauth_clauth__*` to `mcp__plugin_<tool>_<tool>__*` (none on this machine) |
| M8 | Commit | Tombstone **plus binary retire** in the same transaction: write `~/.clauth/MIGRATED` naming the target; rename `~/.cargo/bin/clauth` to `clauth-0.16.0.retired` (journaled); install a shim `clauth` that prints the MIGRATED target and exits 1. Upstream never reads the tombstone (`main.rs:215-240`), and one upstream TUI run re-creates a carrier (detach `app.rs:11428`, adopt `claude.rs:1003-1017,2498-2531`). Recheck `standby_waiting()`, mark the journal committed, release the locks in reverse order |

M5 per-path table (inventory of `~/.clauth` on this machine, 2026-09-29):

| Path | Action |
|---|---|
| `profiles/<p>/credentials.json`, `session-token.json`, `session-token.static.json`, `quarantine/` (claude); `profiles/<p>/auth.json`, `auth.lkg.json`, `auth.quarantine.json` (codex) | **move** (`rename(2)`), never copy; every one can carry a refresh chain (`claude.rs:152-175,585-598,890-899`, `codex_auth.rs:474-476,555-571`). `install_source_path` must select the same file after the move |
| `profiles/<p>/credentials.json.pending` | **refuse** (M2) |
| `profiles/<p>/codex-home/` | copy (durable sqlite / history / rollouts, `runtime.rs:6338-6343,6412-6440`); **refuse** if it contains `auth.json` (a fake-transport carrier, `runtime.rs:6323-6336,6550-6557`) |
| `profiles/<p>/config.toml` | **copy 0600, marked secret** (holds the API key of key profiles, `profile.rs:343`) |
| `profiles/<p>/{account_id,profile_fetched,usage_cache}.json`, `usage_history.jsonl`, `wallet_history.jsonl` | copy |
| `profiles.toml` (+ `HomeTab` alias map, D4), `codex-profiles.toml` | copy |
| `conversations/`, `session_profiles.json` | copy; **needed for P6b resume** (runtime id → conversation, start profile) |
| `profiles/<p>/runtime*`, `sessions*`, `codex-home-*` | skip (per-session; their absolute symlinks into `profiles/<p>/` would dangle, `runtime.rs:6588-6600`); M1 refuses if any sessions marker is held |
| `status.json`, `status_cache.json`, `ai_pricelog_v4_price_cache.json` | skip (regenerated) |
| `completions/`, `.completions_installed` | skip (the tool writes its own) |
| `live_bare/`, `mcp_live/`, `live_sessions/` | skip (per-process state; M1 guarantees no live process) |
| `rotation-locks/`, `.lock`, `clauthd.*`, `usage-fetch.lock` | **never** (held by the transaction) |
| any entry not listed | **refuse** in both dry-run and the real run; an unknown file may be a carrier |

M6 live slots:

| Slot | State → action |
|---|---|
| `~/.claude/.credentials.json` | **Regular file** (the normal state after any upstream TUI exit, `claude.rs:2634-2655`; true on this machine): classify against the active profile's install source by access token (`claude.rs:943-968`). Same → replace with a symlink to the moved store. Diverged → capture it into the store first with upstream's snapshot semantics (`claudeAiOauth` replaced, `mcpOAuth` preserved; the superseded store bytes are not kept), then symlink; refuse if it cannot be classified. No active profile → untouched. **Symlink** into `~/.clauth/profiles/<p>/` → repoint in the same journaled step as the store's rename |
| `~/.codex/auth.json` | (i) **Symlink** into `~/.clauth/profiles/<p>/auth.json`: move the store and repoint the link in one journaled step, under that profile's rotation lock (held from M3), with no `codex` process alive (a refresh through a dangling absolute link would re-create a carrier at the old path, `runtime.rs:6262-6265`). (ii) **Regular file** whose refresh token equals a codex profile store's: **refuse**, a second carrier from a fake / no-symlink capture (`actions.rs:1340-1343`). (iii) Otherwise an independent operator login (`actions.rs:1383-1399`), untouched. Today: (iii), no codex profiles |

**Retire upstream, after a committed import** (interactive, outside the lock hold):
1. `claude plugin uninstall clauth@clauth`; remove the `clauth` marketplace and `mcpServers.clauth`.
2. `<tool> plugin install` (the embedded-source path, `plugin_host.rs:44-46`) registers marketplace
   `<tool>` at `~/.local/share/<tool>/current@claude` and flips `enabledPlugins` `clauth@clauth` →
   `<tool>@<tool>`.
3. `<tool> herdr install` installs the fork's herdr plugin, rewrites the sidebar layout token
   `["agent", "$clauth"]` to `$<tool>` (`~/.config/herdr/config.toml:112-114`), and adds `hermes` /
   `codex` `rows_by_agent` lines.
4. Replace the `.bashrc` completion line.
5. Remove the shim (`cargo uninstall clauth` removes it and cargo's record) and delete
   `clauth-0.16.0.retired`, last, and only after the owner confirms.

**Rollback** replays the journal in reverse. It is refused while any `<tool>`, `claude` (bare or
helper) or `codex` process is alive, and it holds the fork's state lock and the same `~/.clauth`
locks while it runs.
- **Order:** move the stores back (renamed back to their original paths and inodes, never copied);
  restore the live links and the helper, `permissions.allow` and the registry remap; wipe only the
  destinations journaled as `copy` (copied rows: upstream's originals were untouched); restore
  `enabledPlugins` and the herdr plugin; **last**, remove the shim and restore
  `clauth-0.16.0.retired` (or, after retire, `cargo install clauth --version 0.16.0 --locked`); then
  `clauth self-heal` / `clauth herdr install`, and remove the tombstone.
- A live slot captured in M6 is restored as a symlink to the restored store (`LinkState::Linked`,
  which upstream accepts), never re-created as a regular-file copy.
- Profiles created after import stay in `~/.<tool>` and are listed. Rollback keeps upstream's
  untouched original rosters and never copies the fork's `profiles.toml` / `codex-profiles.toml`
  back, since D4 alias values would fail upstream's `HomeTab` parse (warned).
- `~/.config/herdr/config.toml.bak-pre-clauth` is a reference copy.

**Self-update and distribution**

| Item | Rule |
|---|---|
| Self-revert hazard | A build that keeps upstream `API_URL` + key replaces itself with upstream's **signed** release (`update.rs:84-115,282`; v0.16.0 ships `sha256sums.txt.minisig`). The trigger is `is_newer(upstream_tag, CARGO_PKG_VERSION)` (`update.rs:87`); R0's version reset makes 0.16.0 newer, so any fork binary outside `~/.cargo/bin` (target/debug, target/release, the installer dir, `update.rs:288-299`) with the updater compiled in self-replaces on first TUI launch (`tui/app.rs:2321`). The same gate (`update.rs:58-64`; default on at `profile.rs:815-819`) drives the herdr reinstall of `uwuclxdy/clauth/herdr-plugin` (`herdr.rs:31,774-786,884-885`). Hence R0 compiles the updater out (§5) |
| Keys | New minisign keypair, generated offline with a passphrase (`minisign -G`). The encrypted secret-key file goes in the Actions secret `MINISIGN_SECRET_KEY`, its passphrase in a separate secret `MINISIGN_PASSWORD`. Signing is noninteractive: `printf '%s\n' "$MINISIGN_PASSWORD" \| minisign -S -s minisign.key -m sha256sums.txt -t …` (minisign reads the password from stdin, https://github.com/jedisct1/minisign/blob/master/src/get_line.c). Key file and passphrase are backed up off GitHub, separately. The public key is published in the README and `install.sh`. CI checks that the key file's key id matches the embedded `MINISIGN_PUBLIC_KEY` (`minisign -V` of a test signature). The upstream comment `update.rs:29` (`-G -W` passwordless) is rewritten |
| Unsigned release | `release.yml:114-117`'s skip-when-no-secret branch is removed; a missing `MINISIGN_SECRET_KEY` or `MINISIGN_PASSWORD` fails the publish job |
| Fail closed | A blank key skips verification (`update.rs:174-177,195-197`). The fork adds a compile-time assert that `MINISIGN_PUBLIC_KEY` is non-empty whenever the `self-update` feature is on |
| Rotation | The last release signed by the old key ships the new public key |
| Update mode | Config key `update = "off" \| "notify" \| "auto"`, default **`off`** until R1 ships a signed fork release. `off` covers the notify path too: cargo installs still call `fetch_latest()` and show a version (`update.rs:84-101`). abobreshov/clauth is `fork:true` with 0 releases |
| Actions on a GitHub fork | "Workflows don't run in forked repositories by default. You must enable GitHub Actions in the Actions tab of the forked repository" (https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows). Enabled today: UNCONFIRMED, S8(b). Detaching the fork is an option (D13) |
| `install.sh` | **Signature verification is mandatory.** Resolve the release tag once (API `releases/latest` → `tag_name`; today it resolves `latest` twice, `install.sh:57,74`) and download the asset, `sha256sums.txt` and `sha256sums.txt.minisig` from `releases/download/<tag>/`. Run `minisign -Vm sha256sums.txt -P <embedded pubkey>` before the SHA-256 check; abort if minisign is missing (print how to install it) or verification fails. No SHA-256-only fallback. The cargo branch never runs `cargo install clauth` (today's default when cargo exists, `install.sh:16-18`); it is removed or becomes `cargo install --git https://github.com/abobreshov/<repo> --tag <tag> --locked` (D13) |
| Release notes | `release.yml:18-24` expects an off-repo script; the fork needs its own or accepts bare notes |
| crates.io | `clauth` is owned by uwuclxdy (https://crates.io/api/v1/crates/clauth). `<tool>` publishes under its own name or not at all (D13) |
| herdr plugin | New id, source `abobreshov/<tool>/herdr-plugin` (repo renamed, D8); the heal check compares against `identity::REPO_OWNER` |
| CC plugin | New marketplace + plugin name; hooks matcher `mcp__plugin_<tool>_<tool>__delegate$` |

**Upstream sync.** Hard fork; nothing flows back. `git fetch --unshallow` once. Until P5:
`git merge upstream/mommy` weekly or per upstream release, with `rerere`; after P5, cherry-pick fixes
in non-render modules (oauth, runtime, settings_sync, codex, herdr bridge, security). Record the last
reviewed upstream commit in `docs/UPSTREAM.md`; never rebase the published branch; `merge=ours` in
`.gitattributes` for README, `wiki/**`, `.github/**`, `install.sh` and the manifests. Upstream moved
75+ commits in 9 days (a lower bound, shallow clone).

**MIT obligations.** Keep `LICENSE` ("Copyright (c) 2026 cloudy", `LICENSE:3`) and the permission
notice in all copies or substantial portions (https://opensource.org/license/mit). The fork may add
its own copyright line. README states "forked from uwuclxdy/clauth" with no endorsement claim.

### 4.1 Observation model (new `src/usage/observation.rs`) — unchanged except as noted

```rust
pub(crate) struct AccountObservation {
    pub account: AccountRef,              // "claude:<p>" | "codex:<p>" | "hermes:<p>" | "native:<target>"
    pub source: SourceId,                 // anthropic_oauth, ollama_cloud, nous, openrouter, deepseek, zai, minimax, alibaba, codex, grok, antigravity, hermes_local, generic …
    pub auth: AuthKind,                   // Subscription | ApiKey | Hybrid | NativeLogin
    pub plan: Option<String>,             // provider value, else UserConfig label
    pub freshness: Freshness,             // Fresh | Stale{since} | NotFetched
    pub failure: Option<Failure>,         // AuthRequired{hint} | RateLimited{retry_after} | QuotaExhausted{window} | Unavailable | InvalidResponse | ConsoleExpired
    pub windows: Vec<QuotaWindow>,
    pub money: Vec<MoneyMeter>,
    pub estimate: Option<LocalEstimate>,  // only when attribution is exact
    pub resets: Vec<BankedReset>,
    pub best_effort: bool,
    pub observed_at: Option<Timestamp>, pub checked_at: Option<Timestamp>,
}
pub(crate) struct QuotaWindow {
    pub id: String, pub label: String,
    pub used_pct: Option<f64>,            // unclamped; unknown is None
    pub exhausted: bool,
    pub resets_at: Option<Timestamp>, pub window_secs: Option<u64>,
    pub scope: WindowScope,               // Shared | Account | Model{models} | Product
    pub chain_eligible: bool,             // true only for today's 5h/7d fold
    pub breakdown: Vec<ModelCount>,       // NEW: per-model request counts (Ollama); not a scope
}
pub(crate) struct MoneyMeter {
    pub meter_id: String,                            // NEW: stable per source: wallet, subscription, top_up, rollover, total_usable, spend.daily|weekly|monthly|lifetime, key_limit, byok.*
    pub label: String, pub kind: MoneyKind,          // Balance | Spend | Limit | Budget
    pub amount: Decimal, pub currency: Currency,     // signed; debt survives
    pub limit: Option<Decimal>, pub budget_source: Option<BudgetSource>,
    pub scope: MoneyScope,                           // Key | Profile | Workspace | Organization
    pub scope_id: Option<String>,                    // NEW: org / account id for wallet de-dup
    pub scope_origin: ScopeOrigin,                   // NEW: Provider | MonitoringCredential{bound: bool} | UserLabel
    pub additive: bool,                              // NEW: false for derived totals (Nous total_usable)
    pub period: Option<Period>,                      // explicit [start, end); `derived: bool` flag
}
```

- **Projection at read time**; `Sections` is presentation only. Derived at render time: severity =
  worst of used %, balance and pace; pace `delta = used − elapsed` (±5 pt band); lead window =
  session, else the worst critical window, else the soonest reset; a switch threshold < 50 is clamped.
- **New.** `Failure::QuotaExhausted` is kept separate from `RateLimited`. An Ollama "session usage
  limit" 429 is quota, not a transient (https://github.com/NousResearch/hermes-agent/issues/65563).
- **New.** `WindowScope::Account` covers account-wide counters, e.g. OpenRouter
  `free_model_daily_requests {used, limit, remaining}`
  (https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key).
- **New. Money de-dup.** Two meters render once only when `source`, `meter_id`, `kind`, `currency`,
  `period` (equal bounds, or both None) and a **known** `scope_id` all match, with `scope_origin =
  Provider` or a bound monitoring credential. A `None` scope_id never de-dups. A `UserLabel` scope
  (Ollama `account = …`) groups rows visually as "grouped by label" but never merges amounts. A meter
  with `additive = false` is never summed with others. Profiles show only their per-key meters.
- **Contracts.** `~/.<tool>/status.json` is schema 2 plus `accounts[]` (relaxed rule, §3).
  `<tool> usage --json` emits `{schema_version: 1, generated_at, accounts}` with RFC 3339 times. The UI
  never implies a switch on a displayed balance severity.

### 4.2 Sources on the one scheduler — unchanged except the rows marked new

`UsageSource { meta(), fetch(target, cred, http) }` is another leg of `spawn_refresher`, with a single
owner per target. Cache identity is (source, endpoint, account / scope, credential fingerprint).
Existing fetchers are wrapped in place in P4a. `ProviderHttp` has a 2 MiB cap and refuses redirects
that leave the origin.

| Policy | Allowed origins | Credentials |
|---|---|---|
| **Monitoring** (was "Billing") | fixed origin(s) in `SourceMeta`: `api.anthropic.com`, `portal.nousresearch.com`, `openrouter.ai` (management), `ollama.com` (monitor-only key) | monitoring slot only, plus the borrowed Hermes Nous access token (D10), under the same `portal.nousresearch.com` allowlist (`/api/oauth/account`, `/api/billing/*`); it is never sent to `inference-api.nousresearch.com` or any other origin |
| **Configured endpoint** | the profile's `routing_endpoint()` origin | that profile's inference key only; monitoring credentials excluded |

- **New: per-credential path allowlist in `SourceMeta`.** OpenRouter (`openrouter.ai`: management
  vs inference key) and Ollama (`ollama.com`: monitor-only vs inference key) share one host between
  the two policies, so the origin alone cannot keep a monitoring credential off inference paths. The
  management key may reach only `GET /api/v1/credits`, `GET /api/v1/key` (its own identity, for
  binding, §4.7) and read-only `GET /api/v1/keys*`; the Ollama monitor key only `GET /api/usage`; the
  Nous monitoring login, plus the borrowed Hermes Nous access token (D10), only the
  `/api/oauth/account` and `/api/billing/*` reads on `portal.nousresearch.com`; the borrowed token is
  never sent to `inference-api.nousresearch.com` or any other origin. Any other method or
  path on the same origin is refused before sending.
- **New.** A per-meter failure degrades that meter only. OpenRouter `/credits` returning 401 / 403
  means "wallet unavailable" and never `AuthExpired` for the inference key; `/key` is the auth probe
  (the fix for `src/providers/openrouter.rs:44-46`, where `/credits` is fatal and `/key` best-effort
  at 47-51, and `src/providers/mod.rs:555-565`, where 401 → `AuthExpired` and 403 → `Status` fail the
  whole fetch).
- **New.** The `hermes_local` source reads `<home>/state.db` read-only (`?mode=ro`, busy_timeout,
  WAL) and `<home>/rate_limits/nous.json` (`hermes_state.py:153,762-856`,
  `agent/nous_rate_guard.py:25-36`). It never executes `hermes`.

### 4.3 Credentials (new `src/usage/credentials.rs`)

| Slot | Holds | Stored as | Reader | Child env? |
|---|---|---|---|---|
| Inference key | the key CC or Hermes uses | CC profiles: `config.toml` 0600. **Hermes profiles, two modes.** (a) *env mode*, the account-home default: the key lives only in `<home>/.env`, created 0600 via `O_CREAT`, then atomically replaced, written only while the home is idle; Hermes mirrors it into its pool as source `env:<VAR>` and persists only metadata + `secret_fingerprint` (unsalted `sha256:` + hex[:16]), never the key (`credential_pool.py:2300-2372`, `credential_persistence.py:123-174`). (b) *pool mode*, pool homes only: `<tool> hermes auth <p> add <prov> --type api-key --label <account>` prompts masked; Hermes stores the **raw** key as a `manual` entry in `<home>/auth.json` `credential_pool.<prov>` (`auth_commands.py:196-221`, `credential_persistence.py:103-111`). Nous OAuth is a `device_code` entry (raw tokens) + `providers.nous` in `auth.json`, and `<home>/shared/nous_auth.json` (`credential_persistence.py:20-26`). **Roster attribution:** env mode stores the fingerprint in Hermes' own format and matches the `env:<VAR>` entry's `secret_fingerprint`; pool mode matches by the entry `label` the tool passed plus the entry `id`; the tool never reads `access_token` | `__api-key` / the Hermes home | CC: never, served via the helper. Hermes: via its home `.env` or pool |
| Monitoring credential (was "Billing key") | Anthropic Admin, xAI / OpenAI admin, **OpenRouter management key** (renamed from "provisioning", which is deprecated), an **Ollama monitor-only key** for daemon-transport accounts | env reference only | scheduler monitoring leg | **never**; referenced vars are added to every scrub list |
| **Monitoring login** (new) | the tool's **own** Nous device-code OAuth pair (opt-in until S5(a), D10) | `~/.<tool>/credentials/nous-<p>.json` 0600, locked, atomic replace; the tool is its single writer | scheduler | never |
| Native login | `~/.codex`, `~/.grok`, agy keyring, **Hermes `auth.json` / `.env`**, **Ollama daemon signin key** | read in place, or not at all (the Ollama key is never read). **Exception:** the §4.8 pool view and the Nous unexpired-token read parse `auth.json` metadata-only: only whitelisted keys (`id`, `label`, `source`, `auth_type`, `priority`, `last_status*`, `secret_fingerprint`, `request_count`, `expires_at*`, `active_provider`, the provider keys of `credential_pool` / `providers` (for the §4.8 anthropic check), plus the Nous access token for the D10 read) are deserialized; other secret keys are never deserialized, logged or held. The pool view and the D10 Nous reader are separate structs: the pool-view struct holds no `access_token` / `refresh_token`; the D10 reader holds only `providers.nous` access token + `expires_at`, never `refresh_token` | native leg | no |

Rules carried over from v2.3: a typed `AuthRequired{hint}` for a missing daemon env var; masked
display; a fingerprint change invalidates the cache.
- An OpenRouter management key can create and delete keys, and it can expire with `401 API key
  expired` (https://openrouter.ai/docs/guides/overview/auth/management-api-keys). The UI warns on
  expiry, and a 401 on it never marks the inference key dead.
- The Nous monitoring login uses `client_id hermes-cli` as ai-usagebar does (`ai-usagebar
  src/nous/oauth.rs:14-18`). Whether a second session is independent, and whether reusing that
  client id is acceptable, is D10 / S5.

### 4.4 Hot swap: two executors

**Executor A — OAuth.** Unchanged: today's `swap_eligible` checks verbatim, plus `swap_to` with the
drain, mtime bump, repoint, `RotationGuard` and state flock. Nothing in this plan touches it. The R2
upstream-lock / standby refusal is a gate at the daemon decision and CLI / MCP entry points, outside
`swap_eligible` / `swap_to`, whose code stays verbatim.

**Executor B — API key, same effective transport.** Restated in full from v2.3 §4.4
(`87ceb3da:docs/multi-provider-redesign-plan.md:179-263`); this text is normative and v2.3 is only
history:
- **Class.** Same harness; same `routing_endpoint()` origin (conservative normalisation: scheme,
  host, port, path; query preserved); same `ModelSettings`; same env with the endpoint key removed;
  no non-blank `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` on either side; **no OAuth store on either
  side**; **`LinkMode::Real`** (under `Fake` a shared session uses one bare-stem tree per profile,
  `runtime.rs:6-11`, so a per-session `--session <sid>` helper would serve another session's sid; `swap_support` already refuses `Fake` for A,
  `runtime.rs:2534-2539`). Hybrids (OAuth pair + key) and the shunt gateway endpoint are excluded.
- **Launch snapshot.** At spawn the session persists its effective transport (resolved endpoint,
  models, env hash, `LinkMode`, provider identity below). Eligibility compares against it, never
  against a re-read of mutable profile config (today the daemon reconstructs it: `scheduler.rs:4635`).
- **Dispatch.** `SessionSwap` records its executor (A or B) at spawn from the launch snapshot. `poll`
  routes by executor before anything else, **including the `poll_converge` leg**
  (`runtime.rs:3085-3091`, which re-selects the store via `precondition` / `install_source_path` /
  `converge_in_place`, `runtime.rs:3111,3166-3175,3250-3251`): a B-session never reaches `swap_to`,
  `precondition` or `converge_in_place`, and an A-session never reaches B. B keeps its own cell, so
  its intent converges instead of being refused on every tick.
- **Initial state.** At registration a B-session publishes `current_member = start_profile` and
  `key_generation = 0` through `SessionFields` (new setters `set_key_generation`, `set_committed_at`;
  today it has only `set_current_member` / `set_last_swap_at`, `live_sessions.rs:136-160`; new row
  fields are `#[serde(default)]`). The helper falls back to `start_profile` if a row predates them.
- **Protocol (requested → committed → served).**
  1. The daemon or `<tool> switch <sid> <p>` writes `intended_member` via `DaemonFields` only (it
     exposes only `set_intended_member` / `set_chain_cursor`).
  2. B takes the state flock first (rank 500) and holds `SwapCell` (550) only around the publish in
     step 3 (`lockorder.rs:191,205`; the same order as A and the watchdog reader). Inside the state
     hold it (i) claims the target's liveness marker and keeps every marker it has held for its whole
     life (A's `held` vec, `runtime.rs:3203,3259-3290`); (ii) re-validates: the profile exists and is
     not disabled; the class still equals the launch snapshot; the key resolves; the session is not
     shutting down; it is not isolated; `swap_support(mode)` is Ok (`runtime.rs:3153-3165`). A
     `Foreign` marker or any failed check returns a typed `SwapRefused` and publishes nothing.
  3. Commit through `SessionFields` only (`update_as_session` takes the re-entrant state lock itself,
     `live_sessions.rs:361-388`, `lock.rs:437-443`): `current_member = p`, `key_generation += 1`,
     `committed_at`, on a fresh row loaded inside the hold. There is no `RotationGuard` (no store to
     rotate). If the publish fails after the claim, the claim is released. At teardown
     `release_swapped_markers` releases every held marker.
  4. **Served.** The helper records the generation it served in the sidecar
     `live_sessions/<sid>.helper` and never writes the row, so the row keeps exactly two writers
     (daemon: `DaemonFields`; session: `SessionFields`). It acknowledges only after the key is on
     stdout, flushed, with exit 0. Under a short flock on the stable, never-renamed
     `live_sessions/<sid>.helper.lock` (a lock on an inode a rename replaces would not exclude later
     writers) it reads the current ack and, only if its own generation is newer, writes a temp file
     and renames it over `<sid>.helper`; an N−1 reader can never overwrite an N ack. The swap is
     served when sidecar gen ≥ committed gen; until then TUI, CLI and herdr show `swapping…`. There
     is no guaranteed deadline (the expected bound is one TTL while the helper is healthy); in-flight
     requests may finish on the old key; a helper failure (exit ≠ 0 or an empty key) leaves it
     committed-not-served, with a typed warning after 2 × TTL.
- **Helper.** Only `write_merged_settings` (the runtime settings, `runtime.rs:4857-4869`) emits
  `<exe> <API_KEY_HELPER_SUBCMD> --session <sid>`; the global `settings.json` keeps
  `<exe> <API_KEY_HELPER_SUBCMD> <profile>`. `profile_name_from_helper` learns the `--session` form.
  `apiKeyHelper` stays in `PER_PROFILE_TOP_FIELDS`, so sync never distributes it. The session form
  reads `current_member` + `key_generation` from a lock-free row read (`live_sessions.rs:354-356`)
  and the key from a lock-free `config.toml` parse, prints and flushes, then acks. It does no
  `load_profile`, takes no state flock and makes no network call, with a target under 50 ms; its only
  lock is `<sid>.helper.lock`, around the ack. Child Claude processes inherit the runtime settings and
  share the key (intended).
- **TTL.** `CLAUDE_CODE_API_KEY_HELPER_TTL_MS=30000` on the child env after `scrub_profile_env`
  (`runtime.rs:4442`), never in settings `env` (else `sync_once` would publish it globally).
- **Chain.** Manual swaps only until P6c: `--with-fallback` still refuses `!is_oauth()`
  (`fallback.rs:1452`) and `StartBlock::NotOauth` stands. Manual = `<tool> switch <sid> <p>`, TUI,
  herdr, MCP `switch_profile`. Before P6c the daemon's decision leg never writes `intended_member`
  for a B session; only the manual paths do (the class awareness v2.3 required,
  `87ceb3da:docs/multi-provider-redesign-plan.md:253-254`).
- **Precondition (new).** The launch snapshot records that there is no Claude apps gateway session
  and no `forceLogin*` managed setting; otherwise B is refused and the session is relaunch-only.
- **What CC documents** (2026-09-29): the helper output is cached 5 min by default and
  `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` overrides it; the value is sent as both `Authorization` and
  `x-api-key`; `apiKeyHelper` hot-reloads from settings files; auth env beats the helper; `model` is
  read at startup only.

v3 changes (see §9):

| Change | Why |
|---|---|
| "No auth env" means no **non-blank** `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`. The settings writer strips a blank `ANTHROPIC_API_KEY` from `profile.env`, so the two sides can't differ by a blank entry | Matches `env_has_inference_token` (`profile.rs:3080-3084`); OpenRouter and Ollama docs set it blank |
| The launch snapshot also records provider identity where the API exposes it: OpenRouter `workspace_id`, `organization_id` / `creator_user_id`, `allowed_data_regions`. A different `workspace_id` **refuses** B (D11) | Workspace guardrails change the effective models or providers behind identical env (https://openrouter.ai/docs/guides/features/guardrails) |
| The swap picker labels **same account** vs **different account**. Account id = `organization_id` if non-null, else `creator_user_id`, from each inference key's **own** `/key`, never from a wallet read by a monitoring credential. An org key and the same person's personal key are different accounts; a `workspace_id` difference within one org is still "same account" but refuses B (row above) | A same-account OpenRouter swap changes only the per-key cap; the wallet and rate limits are shared (https://openrouter.ai/docs/api/reference/limits) |
| Helper contract pinned: **bare key on stdout**, not JSON | CC docs (settings-reference, `apiKeyHelper`) and the 2.1.283 binary string "The script must print only the key to stdout" |
| CC re-runs the helper on 401 / 403 and when a cached JWT expires, only while `ANTHROPIC_AUTH_TOKEN` is unset; a helper slower than 10 s warns | https://code.claude.com/docs/en/settings-reference , https://code.claude.com/docs/en/authentication#credential-management . S1(b) confirms rather than discovers it |
| Precedence reminder: cloud env > `AUTH_TOKEN` > `API_KEY` > helper > `CLAUDE_CODE_OAUTH_TOKEN` > Anthropic profiles and federation (`ANTHROPIC_PROFILE`; `ANTHROPIC_FEDERATION_RULE_ID` + `ANTHROPIC_ORGANIZATION_ID`, with `ANTHROPIC_IDENTITY_TOKEN_FILE`; an `oidc_federation` active profile in `~/.config/anthropic`) > `/login`. A signed-in Claude apps gateway session outranks everything and the helper is then not used; `forceLoginOrgUUID` / `forceLoginMethod` in managed settings block `apiKeyHelper` at startup. `ANTHROPIC_PROFILE` and the three federation vars join the scrub list; gateway / `forceLogin*` make B relaunch-only | https://code.claude.com/docs/en/authentication#authentication-precedence |

**Per-transport eligibility**

| Transport | Executor | Note |
|---|---|---|
| Anthropic OAuth | A | unchanged |
| OpenRouter `https://openrouter.ai/api`, helper-only | **B** | stock profiles already qualify (`claude.rs:2427,2479-2489`) |
| Ollama Cloud direct `https://ollama.com`, helper-only | **B** | Bearer is required, and the helper is sent as Bearer + x-api-key |
| Ollama via local daemon `:11434` | **none** | the daemon overwrites `Authorization` with its own signature (`ollama server/cloud_proxy.go:372-386`); the account is machine-global |
| Nous for CC | n/a | no `/v1/messages` |
| Hermes, one provider, pool home | **automatic failover only** (Hermes rotates on 429 / 402 / 401 by strategy; no user-selected live switch) | the tool never rewrites `auth.json` or `config.yaml` under a running process |
| Hermes, manual account switch | **relaunch** (H-4): pool home = same home after a strategy edit made while the home is idle, with `--resume` (picking a specific entry is not supported in v1: use account homes); account home = new session in the other home, no resume | always asks for confirmation |
| Across providers or homes | relaunch (P6b / H-4) | always asks for confirmation |

**Relaunch in place (P6b).** (1) Resolve the runtime id `<pid>-<seq>` to a conversation id (the
transcript stem); if more than one matches, refuse. (2) Resolve every precondition first: target
profile, cwd, original args, `follows_chain`. (3) Stop gracefully, wait for the transcript flush,
start `<tool> start [--with-fallback] <p> -- --resume <conv>` (`resume` alone drops
`--with-fallback`). (4) On failure, restart under the original profile. In herdr this uses the
`pane run` path (`daemon/api/create.rs:100`). Always asks for confirmation. For Hermes it is
`hermes --resume <id> --provider <p> -m <model>` in the **same** home; the CLI `--provider` flag beats
`config.yaml` (`hermes_cli/runtime_provider.py:541-556`). A cross-home switch can't resume, because
sessions live in that home's `state.db` (`hermes_state.py:153`).

**P6c exhaustion predicates** (new facts):
- OpenRouter 402 `limit_source`:
  - `openrouter_key_limit` → a sibling key in the same org;
  - `openrouter_credits` → a different org, unless the reason is `weight_exceeds_budget` (lower
    `max_tokens` instead);
  - `openrouter_in_flight_budget` → honour `Retry-After`, never rotate.
- OpenRouter guardrail budget / allowlist blocks are **403**
  (https://openrouter.ai/docs/api/reference/limits.md, …/guardrails.md).
- Ollama quota 429 → `QuotaExhausted`. When the monthly pool runs out, use draws on purchased usage
  credits (every plan) or Team's automatic usage billing, which a team can turn off
  (https://ollama.com/pricing). Neither is visible in `/api/usage` today, so a monthly `usage ≥ 1.0`
  is not a rotation trigger, and a later 429 / 402 is handled as the response says. Gated on S4(d).

### 4.5 Look and feel

- **Palette engine.** A new `palette = "catppuccin" | "omarchy" | "auto"` key; `theme` stays colour
  depth; the Catppuccin table is verbatim (hand-picked 256 indices kept); Omarchy RGB comes from
  `colors.toml` with the nearest 256 index computed; 2 s mtime / symlink reload; last good palette
  and one toast on a malformed file. Mapping (`orange` → `accent_2` has no once-per-screen limit):

  | Omarchy | role |
  |---|---|
  | `accent` / `orange` | accent / accent_2 |
  | `foreground` / `dark_foreground` / blend(fg, bg, .72) | text / text_faint / text_dim |
  | `background` / `darker_background` / `lighter_background` | bg / bg_sunken / bg_hover |
  | `muted` / `selection` | line / line_strong |
  | `red` / `yellow` / `green` / `cyan` | danger / warning / success / info |
  | blends | `bg_danger` / `bg_warning` banner tints |
- **Default `auto`** (D6): Omarchy when `~/.local/state/omarchy/current/theme/colors.toml` (or
  `~/.config/omarchy/current/theme/colors.toml`) exists, otherwise Catppuccin. The CLI follows it and
  turns off under `NO_COLOR`, on a non-TTY, or with `--plain`.
- **Metric card** (Accounts tab and the account detail): three rows — a bold label with the
  countdown and local time right-aligned; a full-width bar with the value, a pace glyph and an
  elapsed marker `│`; a dim footnote (`40% elapsed · 12 pts under`).
- **Accounts pane**, grouped by provider (now incl. Ollama, Nous and Hermes, with a harness badge
  `CC` / `CX` / `HM`). Each row shows the active mark `●`, the lead metric (% or $), a 12-cell mini
  bar, the countdown, flags (`⏸` stale, `⚠` error, `↻` refreshing) and the route. An unbound
  monitoring wallet renders as its own `monitoring account <id>` row (§4.7).
- **Narrow mode** (< 100 cols, or the herdr popup): the account list collapses into a top strip.
- **Waybar output (P3).** `<tool> usage --waybar` prints `{text, tooltip, class, percentage}` from
  observations: ai-usagebar's `{text, tooltip, class}` shape (`refs/ai-usagebar/src/waybar.rs:1`) plus
  Waybar's `percentage` key (https://man.archlinux.org/man/extra/waybar/waybar-custom.5.en); `class`
  reuses the severity words. A Quickshell / Quattro widget stays optional P8 (D5).
- **API-key card (fixed mockup).** The wallet uses the balance ladder, and the per-key cap uses
  used %. Never grade "used % of lifetime purchases". "same account" follows the §4.4 definition.
  ```
  or-main · OpenRouter · key sk-or-v1-f33…2cd · org acme (same account as or-alt)
  Credit balance                                  $13.67 left      mid
  Key cap        ████████████████░░░░   $41.00 of $50.00 monthly   82% used   HIGH
  Spend          today $0.00 · week $4.08 · month $4.46
  oll-main · Ollama Cloud · pro-legacy        5h 82% · 7d 23% (no reset time from API)
  nous-main · Nous Portal · Plus    Monthly credits 64% used · $7.90 left · renews 12 Oct · top-up $10.00
  ```
- **Tabs (D4).** P5 restyles the 8 tabs. P5b consolidates them to 6: `Accounts` (overview + usage),
  `Tokens`, `Chain` (fallback), `Setup`, `Config` (config + plugin), `Status`. Persisted values map via
  aliases `overview|usage → accounts`, `fallback → chain`, `plugin → config`, applied at import and on
  load.
- Goldens: `buffer_rows` at 80, 120 and popup widths, both tiers, both palettes.

**Severity table.** Unchanged (used % < 50 / ≥ 50 / ≥ 75 / ≥ 90; balance ≥ 20 / < 20 / < 5 / < 1 or
negative; pace < −10 / ≥ −10 / > 0 / ≥ +10). Additions:
- A per-key cap with `limit_remaining ≤ 0` is CRITICAL.
- Ollama monthly `usage ≥ 1.0` is shown as `included credits used up` (HIGH, not CRITICAL). The row
  reads `extra use draws on purchased credits / team billing (balance unknown)` until S4(d) finds a
  purchased-balance or auto-billing field; only then may it say `billing continues` or `exhausted`.
- Nous `paid_access = false` is `depleted` (CRITICAL).

**Countdown.** `4d 1h`, `3h 05m`, `5m`, `now`, `—`, then the local time in parentheses; values truncate,
never round up; one fixture table for CLI, TUI, JSON and herdr. Unknown `resets_at` → `—`, no elapsed marker.

**CLI**

| Command | Change |
|---|---|
| `<tool> usage [--json] [--account A] [--provider P] [--watch N] [--plain]` | new |
| `<tool> list` | colour on a TTY; plain format documented + golden-tested |
| `<tool> providers list \| detect \| status` | new; `detect` never touches the network and never executes `hermes` |
| `<tool> switch <sid> <p> [--relaunch]` | executor B within a class; relaunch across |
| `<tool> import clauth [--dry-run]` / `<tool> import rollback` | new (§4.0) |
| `<tool> usage --waybar` | new, P3 |
| `<tool> hermes new <p> --provider nous\|openrouter\|ollama-cloud` | new (§4.8) |
| `<tool> hermes auth <p> [args]` | new: runs the §4.8 guards and the anthropic check, sets `HERMES_HOME` + `HERMES_SHARED_AUTH_DIR` for `<p>`'s home and execs the resolved entrypoint's `auth`, so pool edits (incl. `hermes auth add nous --type oauth`) never land in `~/.hermes` |
| `<tool> login nous <p>` | new, opt-in until S5(a) (D10): the monitoring device login |
| `<tool> bar` | optional, P8 |

### 4.6 Herdr

| # | Change |
|---|---|
| H0 | **Fork distribution.** A new plugin id, action and token from `identity`; source `abobreshov/<tool>/herdr-plugin`; heal owner check against `identity::REPO_OWNER` (`herdr.rs:856-864`); scripts ask the binary for its data dir instead of hard-coding `~/.clauth` (`report-profile.sh:17-21,108-109`) and invoke `$APP`, never `clauth` (the §4.0 Herdr row lists the script sites) |
| H1 | The `$<tool>` tag is built from observations: `or-main $13.67`, `oll-main 82%·23%w` (legacy) or `oll-main 12% mo · $4.08/4w` (monthly), `nous-main 64% mo`. It reads committed `current_member`, and shows `swapping…` until acknowledged. Tags carry **only the profile name and numbers** — never ids such as `creator_user_id`, emails or key fragments, since other herdr clients can read pane metadata |
| H2 | Native panes: account-to-pane matching; no tag when ambiguous; cleanup on exit; **`hermes` legs** in the reporter and watcher, joined via `HERMES_HOME`; codex / grok / agy legs. herdr documents the agent only as `hermes` (https://herdr.dev/docs/integrations/); an alias `hermes-agent` is UNCONFIRMED |
| H2h | At home creation, before the first launch, run `HERMES_HOME=<home> herdr integration install hermes`: it respects `HERMES_HOME`, writes `plugins/herdr-agent-state/` and enables it in `config.yaml`; Hermes must be restarted afterwards (https://herdr.dev/docs/integrations/). Needed because plugins load per home (`hermes_cli/plugins.py:1349-1350`) |
| H3 | The popup uses the narrow layout |
| H4 | `<tool>.swap`: joins by PID and refuses delegate panes. CC → executor B; Hermes → relaunch (§4.4) |
| H5 | Compat suite: recorded herdr 0.8.x / 0.9.x JSON |

### 4.6b MCP surface (D17)

The fork's MCP server keeps `profiles`, `switch_profile`, `delegate` and `monitor`, renamed with the
plugin to `mcp__plugin_<tool>_<tool>__*` (import M7 rewrites `permissions.allow`). `profiles`
returns the `accounts[]` observations (all providers, same redaction as H1); `switch_profile` also
drives executor B for B-eligible sessions; `delegate` stays CC-only in v1.

### 4.7 Priority providers

**Ollama Cloud** (`source = ollama_cloud`)

| Aspect | Design |
|---|---|
| Transports | **(1) `OllamaCloudDirect`**: `base_url https://ollama.com`, CC's `/v1/messages` at the root, no local install needed (https://docs.ollama.com/integrations/claude-code). **(2) `OllamaDaemon`**: `http://127.0.0.1:11434` with `:cloud` names; `AuthKind::NativeLogin`. Hermes: provider `ollama-cloud`, `https://ollama.com/v1`, `OLLAMA_API_KEY` (`hermes_cli/auth.py:421-428`). `Provider::OllamaCloud` matches host `ollama.com`; localhost:11434 is detected as the daemon, not Generic |
| Auth + slot | Direct: inference key from https://ollama.com/settings/keys (keys don't expire and are revocable) via the helper only. Bearer is required and x-api-key alone is rejected (https://docs.ollama.com/api/authentication). Scrub inherited `AUTH_TOKEN` / `API_KEY`; set nothing blank. Daemon: no key; the daemon signs with its own key; the tool never reads it |
| Preset (env allowlist) | `CLAUDE_CODE_ATTRIBUTION_HEADER=0`, `DISABLE_ERROR_REPORTING=1`, `CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY=1`, `CLAUDE_CODE_AUTO_COMPACT_WINDOW=<sonnet-slot ctx, 100000–1000000>` (`ollama cmd/launch/claude.go:67-87`; CC env-vars doc). `CLAUDE_CODE_AUTO_MODE_SERVER=0` is HEAD-only and absent from 0.33.3 — optional. Context lengths come from the server, since `ollama launch`'s table lacks today's models |
| Models | Hosted ids for direct, `:cloud` for the daemon (https://docs.ollama.com/api/anthropic-compatibility). The preset maps opus / sonnet / haiku / subagent (= sonnet) and **fable too** to ids picked from the live list at preset time (no ids pinned in this plan) (`ollama launch` doesn't set it; `claude.rs:2429-2436`). The list comes from unauthenticated `GET /api/tags` or `/v1/models`, never hard-coded |
| Usage | Undocumented `GET https://ollama.com/api/usage`, Bearer inference key, same origin → **Configured endpoint** policy (`ai-usagebar src/ollama/fetch.rs:13-14`, `docs/vendor-endpoints.md:31,69`). The daemon has no `/api/usage` (404). Daemon accounts need an optional monitor-only key, else `NotFetched` + `AuthRequired{"mint a key at ollama.com/settings/keys"}`. Monitor-key meters carry `scope_origin = MonitoringCredential{bound: false}` and render as `monitor key (account unverified)`; they are never de-duped or merged until S4(b) gives an account id |
| Mapping | `limits.session` → `{id: session, label: 5h, window_secs: 18000, resets_at: None, chain_eligible: true}`; `limits.weekly` → `{7d, 604800, chain_eligible: true}`; `limits.monthly` → `{month, window_secs: None, chain_eligible: false}`. `used_pct = usage × 100` unclamped; missing → `None` (not ai-usagebar's 0). `exhausted = usage ≥ 1.0` for session / weekly only. `models[].request_count` → `breakdown`. `activity.cost` (a string) → `MoneyMeter{meter_id: spend.period, Spend, Decimal, USD, period [starting_at, ending_at)}`, never on the balance rung. Known bodies expose no purchased balance and no auto-billing flag (`ai-usagebar src/ollama/types.rs:10-60,199-218`). The plan label comes from config (`pro-legacy`, `pro`, `max`, `team`, `free`); shape hint: session + weekly = legacy, monthly = new pricing |
| Parser | Tolerant, but `models[].name` and `period.type` are required in ai-usagebar (`types.rs:40-65`), so one bad row must not fail the whole parse; `activity.models` is ignored |
| Pricing shift | New plans are monthly dollar pools (Pro $20 / $60, Max $100 / $300, Team $500 / $1,000) with "no 5-hour or weekly limits"; auto-renewing subscribers "may remain on the legacy pricing model" (https://ollama.com/blog/transparent-pricing; the 2026-08-31 start date is UNCONFIRMED there). Legacy accounts still reporting 5h / weekly windows rests on ai-usagebar, which live-verified both shapes under the same `pro` label (`refs/ai-usagebar/docs/vendor-endpoints.md:69`). The 5h / 7d chain mapping is legacy-only and will fade. Past the pool, use continues only from purchased credits (every plan) or Team auto-billing, which a team can turn off (https://ollama.com/pricing FAQ); the blog's "keep going at the same per-token rate" assumes one of these is funded. A shown "included $X" is a `UserConfig` budget |
| Multi-account | "One account per person"; joining a team creates a **separate team account** (https://ollama.com/pricing FAQ). So personal and team keys are distinct accounts by default. Profiles may carry `account = "…"` (`scope_origin = UserLabel`): grouped visually, never merged (§4.1), until an account id is available (S4) |
| Hot swap | Direct: **B**. Daemon: none (`ollama signin` is machine-global) |
| Failures | 401 `{"error":"invalid credentials"}` → `AuthRequired`; 429 with "session usage limit" → `QuotaExhausted`; other 429 → `RateLimited`; an unknown shape → `InvalidResponse` + 7-day stale |
| Risks | Undocumented endpoint; the compat page contradicts itself on `tool_choice` / `metadata` / streaming errors; no `count_tokens` or prompt caching (yet a cached-input price exists); a non-first-party base URL disables MCP tool search and Remote Control (CC env-vars doc); off-peak prices are published for the two DeepSeek models only |
| Spikes | S4 (§5) |

**Nous Research — Portal + Hermes** (`source = nous`)

| Aspect | Design |
|---|---|
| Transports | **Hermes only** (provider `nous`, `https://inference-api.nousresearch.com/v1`, OpenAI-wire). Not CC (non-goal) |
| Auth + slot | Hermes owns its inference auth: OAuth device flow (`client_id hermes-cli`, scope `inference:invoke`; single-use rotating refresh tokens, where reuse of Hermes' refresh token revokes the chain, `hermes_cli/auth.py:75-84,5224-5246`), or an `sk-nous-…` key as `NOUS_API_KEY` in the home `.env`. **Monitoring (v1 default):** read Hermes' pool access token **only while unexpired** (Hermes refreshes it 120 s before expiry, `auth.py:84`; it is the token `/api/oauth/account` takes, `nous_account.py:563-576`); expiry → Stale + `AuthRequired`. Hermes' keepalive rewrites the pool entry (`hermes_cli/nous_auth_keepalive.py:91-145`), so the read is a single atomic read with the §4.3 whitelist parse; a parse failure is `Stale`, never an error; the file is never locked. **Own device login:** opt-in until S5(a) (D10); it never refreshes, copies or reuses Hermes' refresh token and never calls Hermes' resolver (`nous_account.py:373-380`, which may refresh). The unexpired-token read needs a tool-managed home (H-1b) |
| Usage | `GET https://portal.nousresearch.com/api/oauth/account`, OAuth Bearer, **Monitoring** policy on that fixed origin (`nous_account.py:36-66,563-576`). Optional: `/api/billing/state`, `/api/billing/subscription` (OAuth, no scope, decimal strings; `nous_billing.py:1-60,479-559`). API-key accounts: no balance endpoint (`nous_account.py:431-461`) → money `Unavailable` + `hermes_local` estimate |
| Mapping | `QuotaWindow{id: subscription, label: Monthly credits, used_pct = (monthly_credits − credits_remaining) / monthly_credits × 100, unclamped (debt reads > 100); None unless monthly_credits is finite and > 0 and credits_remaining is finite and ≤ monthly_credits, resets_at: current_period_end, chain_eligible: false}` (`agent/account_usage.py:160-189`, guard 174-178, ×100 at 181; Hermes also clamps to [0,100], which the tool deliberately does not). Money (`nous_account.py:691-694,713-715`): Balance `meter_id: subscription` (limit = monthly_credits, period start derived and flagged), `top_up` (`purchased_credits_remaining`), `rollover`, and `total_usable` (a derived sum of subscription + top-up: `additive = false`, never summed). Plan = `subscription.plan/tier`. `paid_access=false` → depleted. `rate_limits/nous.json` → `RateLimited{retry_after}`. Parse strings to `Decimal`; header micros ÷ 1e6; negatives survive |
| Plans | Free $0, Plus $20 / $22 credits ($10 rollover cap), Super $100 / $110 ($50), Ultra $200 / $220 ($100) (https://portal.nousresearch.com/). RPM / TPM per tier (https://portal.nousresearch.com/api/openapi). Top-ups never expiring is **likely, not confirmed** |
| Failover (automatic) | Hermes' credential pool (`<tool> hermes auth <p> add nous --type oauth`), strategy via `credential_pool_strategies.nous`. Two Nous OAuth accounts in one pool is UNCONFIRMED (S7(d)). A manual switch is a relaunch (§4.4) |
| Risks | Reusing the `hermes-cli` client id (ToS, D10); whether a second grant collides with Hermes' (S5(a)); the shared Nous store `<root>/shared/nous_auth.json` copies differing tokens with no identity check (`auth.py:4712-4841`); `auth.json` schema churn (Hermes ships several times a week) |
| Spikes | S5 |

**OpenRouter** (`source = openrouter`)

| Aspect | Design |
|---|---|
| Transports | CC: `https://openrouter.ai/api` (Anthropic skin). Hermes: `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY` (`hermes_constants.py:1259`). OpenRouter is **not** in Hermes' `PROVIDER_REGISTRY` (`auth.py:459-462`) |
| Auth + slot | CC: inference key via the helper. OpenRouter documents `AUTH_TOKEN` + blank `API_KEY` (https://openrouter.ai/docs/cookbook/coding-agents/claude-code-integration.md). Its GitHub-Action example sends the key in both headers and calls that "fine", which matches the helper shape. **Do not** adopt the `AUTH_TOKEN` env form, since it excludes B, unless S1(g) fails (Risks); the usage leg then still reads the key from `config.toml`. Optional **management key** in the Monitoring slot (it can't do inference) |
| Preset v2 | `ANTHROPIC_DEFAULT_FABLE_MODEL=~anthropic/claude-fable-latest[1m]`, `OPUS=~anthropic/claude-opus-latest[1m]`, `SONNET=~anthropic/claude-sonnet-latest[1m]`, `HAIKU=~anthropic/claude-haiku-latest`, `CLAUDE_CODE_SUBAGENT_MODEL=~anthropic/claude-opus-latest[1m]` (the current guide). Env allowlist: `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1` (this also stops the helper key being sent to api.anthropic.com for the fast-mode check, https://code.claude.com/docs/en/llm-gateway-connect) and optional `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`. Drop the `openrouter/auto` default, which can route to non-Anthropic models, while "Claude Code with OpenRouter is only guaranteed to work with the Anthropic first-party provider" (same guide). `/fast` needs a concrete opus id (D12) |
| Usage | `/api/v1/key` first (the auth probe; per-key `usage*`, `limit`, `limit_remaining`, `limit_reset`, `byok_*`, `workspace_id`, `organization_id`, `creator_user_id`, `is_management_key`, `expires_at`, `free_model_daily_requests`; no `hash`). Then `/api/v1/credits`: "Management key required", 200 = only `data.total_credits` + `data.total_usage`, **no account id** (https://openrouter.ai/docs/api/api-reference/credits/get-remaining-credits.md), although clauth observed it working with regular keys on 2026-08-17 (`src/providers/openrouter.rs:3-8`), so it degrades per meter (§4.2). List-keys returns `hash` / `creator_user_id` / `workspace_id` but no `organization_id`, default workspace only unless `?workspace_id=` (…/api-keys/list-api-keys.md) |
| Mapping | (a) Balance `meter_id: wallet` = `total_credits − total_usage` (may be negative), attributed to the credential that read `/credits`. Inference key: `scope_id = organization_id ?? creator_user_id` from the same key's `/key`, `scope_origin = Provider`; the scope stays `Key-owner (unresolved)` until S6(c) settles whether an org-owned key sees the org wallet or the member's. Management key: before showing its wallet, call `GET /api/v1/key` with it and bind only if its `organization_id ?? creator_user_id` equals the inference key's (`MonitoringCredential{bound: true}`). On a mismatch, or while S6(d) is open, show it as a separate `monitoring account <id>` row, never attached to the profile, never de-duplicated with it, never used for the same / different-account label (§4.4), with the warning `management key belongs to a different account`; (b) Spend `spend.daily` / `spend.weekly` (Mon–Sun UTC) / `spend.monthly`, scope Key (three rows, never merged); (c) `spend.lifetime`, Key; (d) Limit `key_limit` = `limit` / `limit_remaining`, period from `limit_reset`; (e) optional `byok.*` spend; (f) `free_model_daily_requests` → `QuotaWindow{scope: Account, chain_eligible: false}`, resets at UTC midnight. Everything is parsed from raw JSON into `Decimal`, never from `"%.2f USD"` |
| Hot swap | **B** within one class, labelled same / different account (§4.4 definition). Rate limits are global per account (https://openrouter.ai/docs/api/reference/limits) |
| Risks | **Helper as `x-api-key`**: the guide says `ANTHROPIC_API_KEY`, "sent as `x-api-key`, is treated as a direct-Anthropic credential" and must be blank (claude-code-integration.md). The helper sends its value as both Bearer and `x-api-key`, so **S1(g) is a hard gate for P4-OR's helper preset**; if it fails, OpenRouter profiles are relaunch-only (the `AUTH_TOKEN` env form). `/credits` enforcement; guardrails invisible to an inference key (listing them needs a management key); a deleted workspace yields `workspace_id = null`; Remote Control and voice are off with a helper or a non-Anthropic base URL (llm-gateway-connect). Hermes' own OpenRouter reader clamps the wallet with `max(0.0, …)` and treats `/credits` as fatal (`agent/account_usage.py:816-828`), so Hermes' snapshot is **not a parity oracle** for the signed Decimal balance |
| Spikes | S6 |

**The rest of the v2.3 matrix follows, unchanged:** DeepSeek / Z.ai / MiniMax / Alibaba meters (P4a),
Grok + Antigravity native legs (P4b; port only the fetch/parse halves of `feat/provider-monitoring`,
whose two commits are bbff3341 and bddf0535), then Anthropic Admin · Moonshot · xAI management · OpenAI
admin (P4c-x).

### 4.8 Hermes: role and harness

**Decision (D9): both.** Hermes is a **third harness**, which the tool launches and to which it binds
a profile. It is **also monitored**: the tool reads its `state.db`, rate-limit files and (read-only,
whitelist-parsed) pool status. Monitor-only would lose the herdr and account binding, which is the
owner's ask. Owning Hermes' credentials (the codex-sized path, ~2.5k LOC) is out of v1.

**Launch guards** run before every launch and every `<tool> hermes auth` call, not only at home
creation; any failure refuses (a warning alone never establishes isolation).

| Touch point | Change | Where |
|---|---|---|
| `Harness::Hermes` + `HermesEngine` | `home_env_key = HERMES_HOME`; `install_credentials` bails like Codex | `harness.rs:21-27,113-184` |
| Scrub list | `HERMES_HOME`, `HERMES_SHARED_AUTH_DIR` (both then set by the tool), `HERMES_INFERENCE_PROVIDER`, `HERMES_MODEL`, `HERMES_S6_SUPERVISED_CHILD`, `NOUS_*`, `HERMES_PORTAL_BASE_URL`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY` / `OPENAI_BASE_URL`, `OLLAMA_API_KEY` / `OLLAMA_BASE_URL`, `ANTHROPIC_API_KEY` / `ANTHROPIC_TOKEN` / `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`, every static `api_key_env_vars`, plus a runtime check for plugin-provider vars (`auth.py:471`). **`HERMES_MANAGED_DIR` is passed through**, never scrubbed or set: scrubbing silently moves an IT-set managed dir back to `/etc/hermes`, and a non-dir value disables managed scope entirely (`managed_scope.py:65-71`) | `hermes_cli/auth.py:177-445` |
| Env layering | Hermes loads, in order: (1) `<home>/.env`, override; (2) `<home>/.op.env`, fill-only; (3) the install `.env` at `$HSP/.env`, loaded **always**: fill-only when (1) exists, override when it doesn't (every entrypoint: `main.py:654`, `cli.py:229-230`, `run_agent.py:126-127`, `gateway/run.py:1425`); (4) external secret sources named in the home config; (5) the managed `.env` (`$HERMES_MANAGED_DIR` if it is an existing dir, else `/etc/hermes`), override, last. The managed `config.yaml` also overlays `config.yaml` leaf keys. Scrubbing the process env does not stop (3)–(5) refilling a scrubbed var. So at every launch the tool writes the home `.env`; **refuses** when `$HSP/.env` exists (`$HSP` is user-writable); **refuses** when `<home>/.op.env` exists and sets any key other than `OP_SERVICE_ACCOUNT_TOKEN` (it is fill-only, so it can refill a scrubbed var, `env_loader.py:326-327`); **refuses** when the managed `.env` / `config.yaml` sets any scrubbed var, any `model` / `provider` / `auxiliary` / `delegation` / `fallback` key, or any `ANTHROPIC_*` / `CLAUDE_CODE_OAUTH_TOKEN`; otherwise only warns that a managed dir exists. Layer (4) cannot be checked by value without fetching the secrets, so the tool owns the home `config.yaml`: it **refuses** any enabled `secrets:` source of shape `bulk` (Bitwarden, whose var names are unknown until `bws secret list` runs, `agent/secret_sources/bitwarden.py:600`, `registry.py:305-308`), and for `mapped` sources (1Password, `onepassword.py:502`) **refuses** when any mapped target name is scrubbed, `ANTHROPIC_*` / `CLAUDE_CODE_OAUTH_TOKEN`, or not named by the binding (`env_loader.py:402,446-464`) | `hermes_cli/env_loader.py:313-336,341-385`; `managed_scope.py:4-5,65-71,115-137`; `main.py:461,654` |
| Home layout | `~/.local/share/<tool>/hermes/<name>/`: outside `~/.hermes`, parent **not** named `profiles`. Hermes therefore does **not** trust `HERMES_HOME` as-is: at startup it reads `<home>/active_profile`, and for a non-default name switches `HERMES_HOME` to `<home>/profiles/<name>` (only a `profiles` parent returns early, `main.py:589-592`). The layout is still required: root == home, so there is no read fallback to any root `auth.json` (`_global_auth_file_path()` is None), and the default shared Nous store is `<home>/shared`. A `profiles` parent would make the parent the root, shared by every tool home. **Guards:** (1) `HERMES_SHARED_AUTH_DIR=<home>/shared`, honoured before the root; (2) abort if `<home>/active_profile` exists with any value other than empty or `default` (`hermes profile use` would write it), or if `<home>/profiles/` exists; (3) reject `-p` / `--profile` anywhere in pass-through argv (scanned anywhere, `main.py:526-560`; `-p default` would stay in the home); (4) the home must not match `runtime*` / `sessions*` | `hermes_cli/main.py:526-560,589-640`; `hermes_constants.py:153-191`; `hermes_cli/auth.py:916-940,4748-4750`; `hermes_cli/profiles.py:264-291,367-372`; `codex-plan.md:86` |
| Profile modes | (1) **account home**: one home per account, fully isolated, env-mode key. (2) **pool home**: one home with several credentials of one provider in Hermes' pool (pool-mode keys, §4.3). Hermes forbids two live processes on one home (https://hermes-agent.nousresearch.com/docs/user-guide/profiles), so parallel sessions need mode 1 (D11) | |
| Roster | `hermes-profiles.toml` (`hermes_profiles.rs`, copying `codex_profiles.rs`); `validate_profile_name` over three rosters | `actions.rs:80-95` |
| Launch | Resolve the real entrypoint once, at a user-initiated launch (`mise which hermes` or the pipx venv bin), and cache it. The daemon, `detect` and health checks never execute `hermes` or the shim | `~/.local/bin/hermes:14-31` |
| `which` / TUI / status | `which.rs` gets a `HERMES_HOME` arm; `HarnessFilter` All → Claude → Codex → Hermes; `active_hermes_profile` in status / list / completions | `which.rs:59-126`, `tui/app.rs:1635-1664` |
| Automatic failover | Hermes' pool: strategies `fill_first` / `round_robin` / `random` / `least_used`, selected automatically by priority (`_select_unlocked`); 401 → 5 min, 429 / 402 → 1 h cooldown, unless `reset_at`; mid-session rotation via `mark_exhausted_and_rotate`. No slash command selects an entry (the only knob is the interactive strategy menu), and a running agent holds the pool it loaded. **This is not a hot swap:** a manual switch is relaunch-only unless S7(e) proves a live-selection mechanism plus an acknowledgement. The tool writes only `credential_pool_strategies.<prov>` in `config.yaml` for an idle home (`credential_pool.py:476`, `config.py:1002`). It never writes `auth.json`: priority is a per-entry field there (`credential_pool.py:170,2598`), Hermes has no reorder command (only add / list / remove / reset, `auth_commands.py:164,437,464,502`), writing it would round-trip raw secrets (breaking the §4.3 whitelist rule), and Hermes rewrites priorities itself during rotation (`credential_pool.py:1677-1678`). It adds pool entries through `<tool> hermes auth <p> add <provider> --type api-key --label <account>` run by the user **without `--api-key`**, so Hermes prompts masked and the key never reaches argv (`ps`) or shell history | `agent/credential_pool.py:106-123,470-483,1649-1694`; `agent_runtime_helpers.py:853-933,1403,2071`; `hermes_cli/auth_commands.py:196-221,733-760`; `hermes_cli/commands.py` |
| Live reload | `/reload` exists and re-reads `.env` into a running session (`commands.py:213`, `cli.py:9002-9003`), so "write `.env` only when idle" is a floor, not a guarantee; S7(b) asks whether it rebuilds the live client / pool entry (`credential_pool.py:2308-2323`) | |
| Version | Read without executing `hermes`: `Version:` in `hermes_agent-*.dist-info/METADATA` under the resolved `mise where 'pipx:hermes-agent[extras=all]'` dir, re-read at each launch because Omarchy's `mise up` reinstalls underneath (`~/.local/bin/hermes:12-24`). The same resolved `$HSP` is checked for `$HSP/.env` (env layer 3). The gate is tested against 0.19.0 | `$HSP/hermes_agent-0.19.0.dist-info/METADATA:3` |
| Pool view | Read-only, tolerant, **whitelist** parser for `auth.json` `credential_pool` (labels, `last_status` incl. `dead`, exhausted-until, masked; §4.3 key list); secret keys are never deserialized, logged or held; gated on that version. It never writes | `agent/credential_pool.py:93-98`, `credential_persistence.py:20-26,103-111` |
| Estimate | `hermes_local` from `session_model_usage` (tokens, estimated / actual cost, `billing_provider`, `billing_base_url`). Exact per home but **not per pool entry**, so pool homes show it per provider | `hermes_state.py:762-856` |
| **Anthropic hazard** | Hermes' anthropic path reads and **refreshes** `Path.home()/.claude/.credentials.json`, ignoring `CLAUDE_CONFIG_DIR`, writing back with `os.replace` (`resolve_anthropic_token`). That detaches the tool's symlink and rotates the chain outside `RotationGuard`. `claude_code` pool entries are *borrowed* (secrets stripped before `auth.json` is written), so the harm is the direct read + refresh, not a copied token. **v1 refuses anthropic on EVERY route**, with `claude` / `claude-code` normalized to `anthropic`: `model.provider`; `--provider` / `-m` in pass-through argv; `fallback_providers` / `fallback_model`; each `auxiliary.<task>.provider`; `delegation.provider`; a `providers:` / `custom_providers` entry whose `base_url` host is `api.anthropic.com`; `auth.json` `active_provider`; any `credential_pool.anthropic` entry; a non-blank `ANTHROPIC_API_KEY` / `ANTHROPIC_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` in any env layer (1)–(5). Why every route: the auto auxiliary chain and pool seeding from `~/.claude` skip anthropic only until one of these makes it "explicitly configured"; an explicit auxiliary / vision route skips even that gate. Residual: the in-session `/model` picker (§7, S7(f)) | `agent/anthropic_adapter.py:953-958,1153-1210,1221-1231,1298-1332`; `agent/auxiliary_client.py:290-291,1941-1951,2797-2818,5155-5157,5512-5513`; `credential_pool.py:682-693,1218-1244,1987-1997`; `auth.py:1567-1620`; `config.py:1001,1621,2332-2334,5643-5690`; `providers.py:291-292`; `runtime_provider.py:1380-1392`; `credential_persistence.py:20-26,151-174` |

**Estimated size** (UNCONFIRMED): 1.2–1.8k LOC plus tests without refresh ownership.

## 5. Delivery plan

Each row is one PR, green on `cargo nextest` + clippy, with its docs and wiki change in the same PR.
Nothing is PR'd upstream.

| # | Content | Depends on |
|---|---|---|
| P0 | Branch `feat/redesign`; `git fetch --unshallow`; cherry-pick `bddf0535`; countdown, severity and pace fixture tables | — |
| **R0** | **First commit, before the version reset and before any fork build runs outside `cargo nextest`:** self-update compiled out behind a cargo feature `self-update` (off by default; R1 turns it on). With it off, `update::spawn` returns `None`; `try_update` / `download_and_replace` / `self_replace` are not compiled; `updates_enabled` returns false (which also disables `herdr::heal_detached`'s reinstall); `plugin_host::heal_detached` / `self_heal` are no-ops until R2's CC plugin rename. `API_URL` and `MINISIGN_PUBLIC_KEY` move into `identity`, pointing at the fork URL and an empty key that fails the build with the feature on. Until that commit lands, every run of a fork build exports `CLAUTH_NO_UPDATE=1`. Then: `identity.rs` + chokepoints (§4.0 inventory, **incl. the executable-name row**) + manifests, tests and the 7 `CARGO_BIN_EXE` sites. Name from D8 | P0, D8, S8(a) |
| **R1** | Self-update + release: new minisign key, fork `API_URL`, `self-update` feature on, assets from `ASSET_PREFIX` (globs + sums-count check), `release.yml` noninteractive passphrase signing that fails without the secrets, `install.sh` (mandatory minisign verification, single resolved tag, no upstream cargo path), octocov removed; test asserting no `uwuclxdy` in the update / herdr / plugin constants | R0, S8(b) |
| **R2** | Coexistence: helper subcommand token (`claude.rs` re-exports `identity::API_KEY_HELPER_SUBCMD`) + exe check with the gated self-heal rewrite; `<TOOL>_HOME`; the new port; guest mode (incl. `settings_sync` / `~/.claude.json` off); herdr H0 incl. the script invocations; CC plugin rename; daemon **and** TUI refresher refuse rotation / switching while upstream's `clauthd.lock` or `usage-fetch.lock` is held or the standby-slot probe is true; the D18 reconcile rule; `update` config key | R0, S8(c) |
| **R3a** | Import transaction foundation: writer exclusion (M3 locks held from revalidation to commit), process revalidation after the locks, write-ahead journal (fsync before each step), a per-path move / copy engine that refuses EXDEV, existing destinations, unknown entries and `*.pending`, the lock-free M-1 precheck, the journal `pre` section, `--dry-run`, crash-replay tests. Writes no global config | R2, S8(d) |
| **R3b** | Import consumers: M0 plugin disable + herdr uninstall, the full credential-set moves, the M6 live-slot rules (claude classify / capture; codex (i)–(iii)), registry remap, `apiKeyHelper` + `permissions.allow` rewrite, M8 tombstone + binary retire shim, `HomeTab` alias hook | R3a |
| **R3c** | `import rollback` (reverse replay; stores restored before the upstream binary is restored or reinstalled) | R3a, R3b |
| **R3d** | Retire checklist (`<tool> plugin install`, `<tool> herdr install`, `.bashrc`, `cargo uninstall`) | R3b |
| S1 | Spike, version-pinned against the installed `claude`, driving a **local stub endpoint** (a fake claude cannot prove CC's cache). Prove: (a) an **unchanged** helper command is re-executed after a 30 s TTL and its new stdout is sent; (b) behaviour on 401; (c) whether `env` hot-reloads; (d) a helper failure or empty output mid-session, and what CC does next; (e) a helper invocation spanning a commit, i.e. which generation is served; (f) whether requests in flight at the commit keep the old key; (g) Ollama direct and OpenRouter each accept the helper value in both headers, against a stub first and the real endpoint only under the spike protocol; (h) CC with `ANTHROPIC_API_KEY=""` + helper; (i) the 401 / 403 re-run with `AUTH_TOKEN` unset. **Release gate for executor B:** (a) passes; (d) leaves the swap committed-not-served with a warning, never reported as served; (e) and (f) never regress an acknowledgement and never mark a stale generation served. Otherwise API-key swaps fall back to relaunch. **(g) additionally gates P4-OR's helper preset** | P0 |
| Spike protocol (S1(g), S4–S7) | Owner-run only, with throwaway or low-limit keys (an OpenRouter key with a `limit`, a separate Ollama key); the management key and the Nous login are used only for read endpoints; responses are redacted and committed as fixtures; results go in `docs/spikes/S<n>.md` with the date and CLI version | — |
| S2 / S3 | S2: Antigravity — the branch's Cloud Code read (never unlocks the keyring) vs `agy --print /usage` (≥ 1.1.11); does either spend quota. S3: can a transcript *record* be tied to the profile that served it; if not, `estimate = None` | P0 |
| **S4** | Ollama: (a) `/api/usage` per key or per account; (b) hypothesis, no source found: an `ollama.com/api/me`-style route accepting Bearer (plan, account id); (c) 429 headers, and legacy vs monthly exhaustion bodies; (d) meaning and ceiling of `monthly.usage`, any purchased-credit balance or auto-billing field, and whether a Pro / Max account with zero purchased credits gets a 429 / 402 at `usage = 1.0`; (e) CC behaviour on the unsupported features (`count_tokens`, caching, `tool_choice`) and whether extra env is needed; (f) a `fable` request without a mapping; (g) are keys scoped to personal vs team, and can one key be switched between them; (h) session window rolling vs fixed | P0 |
| **S5** | Nous: (a) a second `hermes-cli` grant is independent of Hermes' session (no invalidation of Hermes' chain, no single-session-per-client limit); (b) an `inference:invoke` token reads `/api/oauth/account`; (c) access-token TTL; (d) any API-key balance endpoint; (e) `x-nous-credits-*` on API-key requests; (f) the subscription period start; (g) ask Nous about a client id for the tool | P0 |
| **S6** | OpenRouter: (a) `/credits` with a regular key today; (b) `limit_reset` weekly / monthly boundaries and a negative `limit_remaining`; (c) the org-owned key `/credits` scope (org or member); (d) does `GET /api/v1/key` accept a management key and return its `organization_id` / `creator_user_id`; (e) the `/credits` wallet for an org-member management key vs an org-owned inference key | P0 |
| **S7** | Hermes (file reads + a throwaway home, never the owner's `~/.hermes`): (a) does herdr resume keep `HERMES_HOME`; (b) does `/reload` (or the pool's `_get_env_prefer_dotenv`) rebuild the live client / pool entry after a changed `OPENROUTER_API_KEY`; (c) `credential_pool` schema across releases; (d) two Nous OAuth entries in one pool; (e) any live-selection mechanism for a pool entry, with an acknowledgement; (f) does a `HOME` redirect for the Hermes child close the `/model` anthropic route without breaking herdr / git | P0 |
| **S8** | Identity: (a) the chosen name free on crates.io and GitHub; (b) Actions enabled and `MINISIGN_SECRET_KEY` + `MINISIGN_PASSWORD` present on the fork; (c) herdr with two plugins sharing an id (throwaway id); (d) Anthropic replay tolerance (one `invalid_grant` loser) still holds — bounds the carrier race if an import step is interrupted | P0 |
| P1a | Observation types (+ `breakdown`, `scope_id`, `meter_id`, `scope_origin`, `additive`, `QuotaExhausted`, `WindowScope::Account`) + projections + `status.json` `accounts[]` + `usage --json` + MCP `profiles` `accounts[]` | R0 |
| P1b | Credential slots (Monitoring credential, Monitoring login) + borrowed native-token policy (D10 Nous token) + `ProviderHttp` policies with per-credential path allowlists + scrub lists (incl. `ANTHROPIC_PROFILE`, federation vars) | R0 |
| P2 | Palette engine; default `auto` | P0 |
| P3 | CLI `usage` report, `usage --waybar` JSON, colour `list`, the shared countdown / pace / severity module; plain `list` golden | P1a, P2 |
| P4a | `UsageSource` trait; existing providers wrapped in place; DeepSeek money meters; per-meter failure | P1a, P1b |
| **P4-OR** | OpenRouter v2: the (a)–(f) meters from raw JSON, `/key` first, 403 degrade, management-key binding, org de-dup, preset v2 with the env allowlist, blank-key strip | P4a, S6, S1(g) (helper preset only) |
| **P4-OLL** | Ollama Cloud: `Provider::OllamaCloud` + daemon detection, direct and daemon presets, `/api/usage` source + fixtures, catalog refresh, `account` label | P4a, S4(a); monthly-exhaustion wording S4(d) |
| **P4-NOUS** | Nous: the `/api/oauth/account` source + fixtures, the unexpired-token read (v1 default); `<tool> login nous` + Monitoring login store behind a flag, **gated by S5(a)** | P4a, P1b, H-1b, S5(b), D10; own-login leg S5(a) |
| **H-1a** | Hermes isolation foundation: `Harness::Hermes` + `HermesEngine`, home layout + guards (a present `active_profile` / `profiles/` / `-p` aborts), env-layer audit (home `.env` writer, `$HSP/.env`, `.op.env`, secret sources, managed `.env` + `config.yaml`), scrub list, whitelist `auth.json` parser (§4.3; used by the anthropic check, P4-NOUS and H-3), anthropic refusal across every route, launch resolver + version read | R2, P1b |
| **H-1b** | `hermes-profiles.toml` roster (Hermes-format fingerprints), `<tool> hermes new`, `<tool> hermes auth` | H-1a |
| **H-1c** | `which` arm, TUI `HarnessFilter`, status / list / completions fields | H-1b, P1a |
| **H-1d** | herdr H2 `hermes` leg + H2h; daemon live-slot inode / link watcher on `~/.claude/.credentials.json` while a Hermes session is live, alerting on detachment (§7) | H-1b, S7(a); S7(f) for the optional HOME redirect |
| **H-2** | `hermes_local` source: `state.db` estimate + `rate_limits/nous.json` | H-1b, P4a |
| **H-3** | Read-only pool-view UI (on H-1a's whitelist parser) + pool-strategy writer (idle homes only) | H-1b, S7(c) |
| **H-4** | Relaunch-to-switch for Hermes (pool home: same home + `--resume`; account home: new session), herdr H4 leg | H-1d, P6b |
| P4b | Grok + Antigravity native legs; `<tool> providers` | P4a, S2 |
| P4c-1…n | Anthropic Admin · Moonshot · xAI management · OpenAI admin, one PR each | P4a, P1b |
| P4d | Local estimate for CC | S3, P1a, P4a |
| P5 | TUI restyle of the 8 tabs: metric cards, accounts pane (with harness badge), API-key card, narrow mode | P1a, P2, P4a |
| **P5b** | Tab consolidation 8 → 6 + `HomeTab` alias map (D4) | P5, R3b |
| P6a | Executor B (class incl. `LinkMode::Real`, dispatch before `poll_converge`, gateway / `forceLogin*` precondition), manual switch, MCP `switch_profile` for B | S1, P1a, P1b, R2; the `workspace_id` check lands with P4-OR, or is `None`-tolerant before it |
| P6b | Relaunch in place | P6a, R3b (imported `conversations/`, `session_profiles.json`) |
| P6c | *(later)* Class-aware chain rotation with the §4.4 predicates | P6a, P4-OR, P4-OLL |
| P7 | Herdr H1, H2 (codex / grok / agy legs, cleanup on exit), H3–H5 | P5, P6a |
| P8 | README / wiki screenshots; ai-pricelog mirror (D15); optional Quickshell / Quattro widget or `<tool> bar` | all |

Lanes after P0: {R0 → R1 / R2 → R3a → R3b → R3c / R3d}, {S1 → P6a → P6b}, {P1a + P1b → P4a →
P4-OR / P4-OLL / P4-NOUS}, {H-1a → H-1b → H-1c / H-1d → H-2 / H-3 → H-4}, {P2 → P3 → P5 → P5b}.

**R0's first commit comes before the version reset and before any fork build runs outside
`cargo nextest` (P0 is green on `cargo nextest`, whose `CARGO_BIN_EXE_clauth` tests execute the
build). Until then every run, `cargo nextest` included, exports `CLAUTH_NO_UPDATE=1`. R0–R3b come
before the fork's first execution outside `cargo nextest` on this machine.**

## 6. Test strategy (v2.3 kept; additions marked +)

| Area | Fixtures and assertions |
|---|---|
| Goldens / parity | `buffer_rows` at each width, tier and palette; + the plain `list` golden replaces the byte-identity oracle. Countdown, severity and pace tables shared by all sinks |
| Providers (all) | Recorded fixtures for happy / empty / 401 / 429 + `Retry-After` / 5xx / malformed / negative and sub-cent / CNY / paginated / cross-origin redirect (refused); no real network |
| + Ollama | session + weekly, monthly-only, `limits:{}`, missing activity, `usage > 1`, a 5-decimal cost, 401 `invalid credentials`, the session-limit 429 body, a model row missing `name`; monthly `usage = 1.0` and `1.2` render `included credits used up`, not `exhausted` or `billing continues`; an Ollama monitor-key meter renders as `monitor key (account unverified)` (`MonitoringCredential{bound: false}`), is never de-duped or merged with an inference profile's meters, and is never attributed to the inference profile |
| + Nous | account payload, remaining > cap, negative subscription micros, `paid_access=false`, refresh 400 → re-login; $22 cap / $7.90 remaining → 64.09 %; `monthly_credits = 0` and missing → `None`; NaN / Infinity → `None`; remaining < 0 → > 100 %; all four balance categories render four rows and `total_usable` is never summed; the Hermes store is untouched (asserted) |
| + OpenRouter | `/credits` 403 with `/key` 200, negative balance, `organization_id` null, 402 per `limit_source`, guardrail 403; a management key whose `/key` owner differs from the inference key's (wallet unbound with a warning, no de-dup, no same-account label); an org-owned key with `organization_id` set and `creator_user_id` a member; `/key` with a management key → 403; the same / different-account label for two individual accounts, an org key vs a personal key, and two workspaces in one org |
| + Cross-provider | two accounts with `scope_id = None` never merge; OpenRouter daily / weekly / monthly Spend stay three rows; two profiles on one OpenRouter org render the wallet once; two `UserLabel` Ollama profiles group but never merge amounts |
| Credentials | a missing env ref; `__api-key` never prints a monitoring credential; monitoring refs scrubbed; + the Monitoring login file 0600 with a single writer under the lock; + a monitoring credential sent to a non-allowlisted path of the same origin (OpenRouter, Ollama) is refused; + **leak test**: seeded key, token and email strings appear in none of `status.json`, `usage --json`, `usage --waybar`, MCP output, herdr `report-metadata`; + the Hermes pool view never deserializes a non-whitelisted key (the pool-view struct holds no `access_token` / `refresh_token`, and fixture values are absent from it and from logs); the D10 Nous reader is a separate struct holding only the `providers.nous` access token + `expires_at`, never `refresh_token`; + the borrowed Nous token sent to `inference-api.nousresearch.com` or a non-allowlisted portal path is refused |
| Swap | the class matrix as property tests (+ blank-key strip, + `workspace_id` difference, + `LinkMode::Fake` refused); the rejected intents; the races; helper form survival; TTL never in `settings.json`; OAuth receipts unchanged; daemonless mode; + a B-session never enters `poll_converge`, `swap_to` or `precondition` |
| + Update / identity | no `uwuclxdy` in the update, herdr or plugin constants; non-empty `MINISIGN_PUBLIC_KEY` with the feature on; without `self-update`, `update::spawn` returns `None` and `herdr::heal_detached` spawns nothing even with `auto_update = true` and no kill-switch env; a symbol test that `self_replace` is not linked; `plugin_host::heal_detached` is a no-op pre-R2. With the renamed binary built and a recording `clauth` shim first on PATH (standing in for live 0.16.0), the create-agent API, the Plugin-tab MCP probe, the plugin `mcpServers` command, `daemon --replace`, pane session joining and herdr reporting run or match only `identity::APP`; the shim's record stays empty. A grep test fails on a `"clauth"` literal in `Command::new`, `args([...])`, `== "clauth"`, `contains("clauth")` or serde field position outside an allowlist |
| + Distribution | `install.sh` against a fixture release: unsigned / bad-sig / wrong-key / minisign-missing all abort; a `release.yml` lint asserts no `exit 0` skip on a missing secret and one `ASSET_PREFIX` glob |
| + Coexistence | daemon and TUI refresher refuse rotation / switching while a fake upstream `clauthd.lock`, a fake `usage-fetch.lock`, or a waiting standby slot is present. Guest mode over a temp HOME: `~/.claude/settings.json`, `~/.claude.json`, `.credentials.json`, `~/.codex/*`, the herdr config and `installed_plugins.json` stay byte-identical, also after a guest session edits its runtime `settings.json`; a fork-token helper in global settings with no completed import is reported, not rewritten. The exe check accepts a `(deleted)` path and a moved binary named `identity::APP`, and rewrites a foreign one (runtime settings only before import). D18: a detached Diverged live slot is captured, not overwritten, at the next start |
| + Import: transaction | dry-run / move / rollback over a temp HOME, with journal replay after a crash at each step; a fixture `installed_plugins.json` with `~/.clauth/profiles/` paths is remapped and restored; `config.toml` copies are 0600; `rotation-locks/` never carried over; rollback after retire, rollback with a live `<tool>`-helper CC session (refused), rollback restores stores before the upstream binary. An M-1 refusal leaves `enabledPlugins` and the herdr plugin untouched; a refusal at M3 or M4 undoes M0's `pre` entries and never counts as a completed import; dry-run changes no file (M0 actions printed only). **Concurrent writers:** a fake upstream process holds a `rotation-locks/<p>.lock` at M3 (refused without blocking), or takes `~/.clauth/.lock`, `clauthd.lock` or `usage-fetch.lock` mid-transaction, and a CC or Codex session sharing a moved store starts between the precheck and the first rename; each aborts before the next journaled step, or rolls back, and leaves no second carrier; an existing destination, an EXDEV move, an unknown entry and a `credentials.json.pending` are refused |
| + Import: credential sets | ordinary OAuth (`credentials.json` only), rolling (`credentials.json` + `session-token.json`) and static (`session-token.json` + `session-token.static.json`) profiles, plus `quarantine/` and codex `auth.lkg.json` / `auth.quarantine.json`: after import `install_source_path` selects the same file as before, all were renamed and none copied, and rollback restores the same inodes |
| + Import: live slots | a regular-file claude slot Same → symlinked; Diverged → captured (`mcpOAuth` preserved) then symlinked; a codex regular file matching a profile store's refresh token → refused; an adopted codex symlink → moved and repointed in one step; after M8 the upstream binary is the shim and an upstream TUI run cannot re-adopt |
| + Hermes: layout / env | parent ≠ `profiles`, `HERMES_SHARED_AUTH_DIR` set; a present `active_profile`, a present `profiles/` dir and `-p` in argv each abort; `.env` created 0600 and never written under a live process; the scrub list covers every var the registry declares (a registry fixture); `HERMES_MANAGED_DIR` passes through unchanged. **Foreign env layers:** a fixture install with `$HSP/.env`, `<home>/.op.env`, an external secret source and a managed `.env` / `config.yaml` (`$HERMES_MANAGED_DIR`), each setting a provider key or `HERMES_INFERENCE_PROVIDER` the binding does not name: launch refuses; a warning alone fails the test |
| + Hermes: anthropic routes | a fixture per route (auxiliary task, vision, delegation, fallback chain, alias `claude-code`, a `providers:` entry on `api.anthropic.com`, `active_provider`, a `credential_pool.anthropic` entry, `ANTHROPIC_API_KEY` in the managed `.env`, `$HSP/.env` present) each refuses launch and `<tool> hermes auth`; `~/.claude/.credentials.json` keeps its inode and mtime |
| + Hermes: other | no code path executes `hermes` in the daemon or `detect`; the version gate against a 0.19.0 `METADATA` fixture; a torn Nous pool file reads as `Stale`; `state.db` read with `mode=ro` against a WAL fixture; env-mode roster attribution matches Hermes' `secret_fingerprint` format; a detached `~/.claude/.credentials.json` slot during a live Hermes fixture session raises the alert; an enabled `bulk` secret source, and a `mapped` one targeting an unbound or scrubbed name, each refuse launch; the pool-strategy writer never touches `auth.json` |
| Palette | reload on mtime and symlink change; + the `auto` resolution with and without `colors.toml` |
| Compat | `profiles.toml` round-trip; `status.json` schema-2 superset at the new path; + the `HomeTab` alias map (every old value loads) |

## 7. Risks

| Risk | Mitigation |
|---|---|
| Silent self-revert to upstream via self-update | R0 first commit compiles the updater out before the version reset and before any fork run outside `cargo nextest` (`CLAUTH_NO_UPDATE=1` until then); test on the constants; updates off until a signed fork release |
| Two carriers of one refresh chain (upstream + `<tool>`, Hermes + `<tool>`) | Full credential-set move under held upstream locks; `*.pending` / unknown entries refused; upstream binary retired in the commit; the tool never refreshes a foreign chain; Nous own login opt-in |
| Upstream TUI detach + re-adopt re-creates a carrier after import | Binary retire inside M8 (shim); D18 reconcile rule for the fork's own detach |
| Upstream standby promotes when `clauthd.lock` frees | Import and the refresher refuse while `standby_waiting()`; rechecked before release |
| Hermes rewrites `~/.claude/.credentials.json` | Refuse anthropic on every route and env layer at every launch |
| Hermes `/model` switch to anthropic mid-session (the picker lists it whenever the file holds an access token, without the explicit-config gate, `hermes_cli/model_switch.py:2057-2072`; selecting it goes to `resolve_anthropic_token`, `runtime_provider.py:1380-1392`) | Residual; a launch-time refusal can't block it. The daemon (H-1d) watches the `~/.claude/.credentials.json` symlink target / inode while a Hermes session is live and alerts on detachment; S7(f) HOME redirect |
| Foreign Hermes env layers (`$HSP/.env`, managed scope) re-inject scrubbed vars | Launch guards refuse; `HERMES_MANAGED_DIR` never altered |
| Upstream velocity (75+ commits in 9 days; `app.rs` 11.4k) | Edge-only rename; merge cadence with rerere until P5, then cherry-picks; `UPSTREAM.md` |
| Ollama plan shift removes 5h / 7d; `/api/usage` undocumented; overage funding invisible | Monthly mapped as money, not chain; "included credits used up" wording until S4(d); typed `InvalidResponse`; stale retention |
| OpenRouter enforces `/credits` management-only; management key of another account | Per-meter degradation; binding via `/key`; unbound wallet shown separately |
| Nous `hermes-cli` client id reuse is refused, or collides | D10: unexpired-token read by default; own login opt-in behind S5(a) |
| Hermes ships several times a week; the schemas drift | Tolerant whitelist parsers, version gate, read-only access |
| CC helper semantics change | S1 version-pinned; B gated; relaunch fallback |
| Money correctness | `Decimal` + ISO currency; signed; no parsing of display strings; `meter_id`-aware de-dup |

## 8. Decisions (for the owner)

| # | Decision | Recommendation |
|---|---|---|
| D1 | Hot-swap scope | Executor B gated by S1; relaunch P6b; **Hermes: automatic pool failover (not a hot swap); a manual switch = relaunch (H-4)**. **Confirm:** in-provider hot swap = credential / account only; a model or preset change is a relaunch or CC's `/model` |
| D2 | Upstream strategy | **Hard fork, never upstreamed.** Merge upstream on a cadence until P5, then cherry-pick fixes (§4.0) |
| D3 | Provider priority | **Ollama Cloud → Nous → OpenRouter** (P4-OLL, P4-NOUS, P4-OR), then DeepSeek / Z.ai / MiniMax / Alibaba, Grok / agy, Anthropic Admin etc. P4-OR is cheapest and can land first in calendar time |
| D4 | TUI navigation | **Restyle 8 in P5, consolidate to 6 in P5b** (Accounts, Tokens, Chain, Setup, Config, Status) with the alias map at import and load |
| D5 | Bar frontends | `usage --waybar` in P3, because both references are primarily bar frontends (§1.2); a Quickshell / Quattro widget stays optional P8 |
| D6 | Default palette | **`auto`** (Omarchy when present, else Catppuccin verbatim) |
| D7 | Antigravity source | Branch read, confirmed by S2 |
| D8 | Name | Proposals: **`tollgate`**, **`paceline`**, **`quotabar`**. All three returned 404 from the crates.io API on 2026-09-29 and are not on this machine's PATH; GitHub and trademark not checked (S8(a)). Recommendation: `tollgate` (data dir `~/.tollgate`, env `TOLLGATE_*`). The repo is renamed to `<tool>`, which sets the herdr source `abobreshov/<tool>/herdr-plugin` and the marketplace URL |
| D9 | Hermes role | **Third harness + monitored** (§4.8); no credential ownership in v1 |
| D10 | Nous monitoring login | **v1 default: the unexpired-token read only** (Hermes' pool access token, a single atomic read, expiry → Stale + `AuthRequired`). The tool's own `hermes-cli` device login is opt-in behind a flag; it becomes the default only after S5(a) shows a second grant is independent of Hermes' session and, ideally, after Nous agrees or issues a client id (S5(g)). The own login refreshes only its own chain and never touches Hermes' refresh token, `auth.json` or `HERMES_SHARED_AUTH_DIR` (Hermes documents chain revocation only for reuse of *its* refresh token, `auth.py:5224-5246`) |
| D11 | Hermes profile mode default; OpenRouter `workspace_id` mismatch | Account-home default (parallel-safe); pool home is opt-in. A workspace mismatch **refuses** B (§4.4) |
| D12 | OpenRouter opus pin / Hermes + anthropic | a) `~anthropic/claude-opus-latest[1m]` default; a concrete id only if the owner uses `/fast`. b) Refuse anthropic on **every** route (primary, auxiliary, vision, delegation, fallback, pool, custom providers, env layers) in managed Hermes homes in v1 |
| D13 | Publishing | GitHub releases only; crates.io optional under the new name. Consider detaching the GitHub fork |
| D14 | `CLAUTH_*` fallback env | No fallback; clean cut (avoids cross-reading markers) |
| D15 | ai-pricelog feed | Keep upstream's for now; mirror in P8 |
| D16 | Default branch | Keep `mommy` until P5 (cheap merges), then rename |
| D17 | MCP surface | `profiles` returns `accounts[]` observations; `switch_profile` supports B-eligible sessions; `delegate` stays CC-only in v1 (§4.6b) |
| D18 | Live slot after the fork's TUI exits | **Keep upstream's detach** (TUI exit disowns: CC keeps refreshing in the standalone file, `claude.rs:2634-2655`) and state the invariant as one *writer* at a time: while the live slot is a detached regular file, the fork's refresher never rotates the store that is its install source; the next TUI / daemon start reconciles by access token (Same → relink; Diverged → capture live into the store, `mcpOAuth` preserved, then relink). Alternative: drop the detach (always linked). Owner to confirm |

## 9. Review log

**v1 → v2.3** (4 rounds, approved in round 5; full tables at `87ceb3da`): the two executors, the
commit / sidecar-ack protocol, credential slots, `ProviderHttp`, scrub lists, S3 attribution, the
launch snapshot, `AccountObservation` + `Decimal`, the S1 gate and the palette key.

**v2.3 → v3** (research topics: `separate-tool`, `ollama-cloud`, `nous-hermes`, `openrouter`, each
fact-checked)

| Change | Why (topic) |
|---|---|
| Reframed as a separate tool `<tool>`; new §4.0 (identity module, inventory, coexistence, import / retire / rollback, self-update, sync, MIT) | Owner's direction; separate-tool |
| Principle 8 replaced by "one writer per refresh chain" and "coexist, then replace"; the relaxed-constraints table | separate-tool (`87ceb3da:docs/multi-provider-redesign-plan.md:73,367,387,424,437,441`; `codex-plan.md:17-20`) |
| D2 → hard fork; D3 → Ollama / Nous / OpenRouter; D4 → consolidate in P5b; D6 → `auto`; D8–D17 added; delivery R0–R3, P4-OR / OLL / NOUS, H-1…H-4, P5b, S4–S8 | Owner's direction; all topics |
| New §4.7 with per-provider transport / auth / usage / mapping / swap / risks / spikes | ollama-cloud, nous-hermes, openrouter |
| New §4.8 Hermes as a third harness + monitored; home layout outside `~/.hermes`; `.env` override; anthropic hazard | nous-hermes, separate-tool |
| Observation: + `breakdown`, `scope_id`, `QuotaExhausted`, `WindowScope::Account`; `used_pct` unclamped | ollama-cloud, openrouter |
| `ProviderHttp`: "Billing" policy renamed "Monitoring", with fixed Nous / OpenRouter / Ollama origins; per-meter failure; `/key` as the auth probe | openrouter, nous-hermes |
| Credentials: + Monitoring login slot (own Nous OAuth); OpenRouter "provisioning" → "management key"; Hermes and the Ollama daemon as native logins | nous-hermes, openrouter, ollama-cloud |
| Executor B: helper contract = bare key on stdout + the 401 / 403 re-run; blank `ANTHROPIC_API_KEY` stripped; provider identity (`workspace_id`) in the snapshot; same / different account label; `ANTHROPIC_PROFILE` scrubbed. Core protocol unchanged | openrouter, ollama-cloud |
| Per-transport eligibility table; P6c predicates; OpenRouter preset v2; API-key card mockup (`87ceb3da:…:313-335`) | openrouter, ollama-cloud, nous-hermes |
| Tests: identity / coexistence / import, Hermes layout and scrub, new provider fixtures; the plain `list` golden | separate-tool, all providers |
| Critic round 1 on v3 (31 fixes) and fact-check corrections (7 `CARGO_BIN_EXE` files, 2 branch commits, codex-plan lines 17 / 20, OpenRouter not in Hermes' registry, off-peak DeepSeek only, top-up non-expiry "likely") | critic, fact-checks |

**v3 → v3.1** (review round v3-1: Grok APPROVE WITH CHANGES, Codex REVISE; each finding verified
against source before applying)

| Id | Reviewer | Verdict | Change |
|---|---|---|---|
| F1 | Grok B2, Codex B1 | partly | M5 moves the full carrier set (claude sidecars + `quarantine/`, codex `auth.lkg.json` / `auth.quarantine.json`); `*.pending` refused; `codex-home/` copied (refused if it holds `auth.json`); `wallet_history.jsonl`; unknown entries refused; codex live-slot rules (i)–(iii) replace the "single inode" rule |
| F2 | Grok B1, Codex B2 | partly | §4.0 rewritten as the M0–M8 transaction: upstream locks held M3–M8, revalidation, write-ahead journal, EXDEV / existing-destination refusal, regular-file live-slot classify / capture, binary retire (shim) inside the commit, rollback restores stores before the upstream binary; line-69 facts fixed |
| F3 | Codex B3 | partly | §4.8 env-layering row (5 layers + managed `config.yaml` overlay; refuse on `$HSP/.env` and on conflicting managed / secret-source values); anthropic refused on every route and layer; per-route fixtures |
| F4 | Grok I2 | partly | Home-layout row restated with the correct `main.py:589-640` behaviour; guards at every launch (`active_profile`, `profiles/`, `-p`); layout kept |
| F5 | Codex I4 | confirmed | New "Executable name matches" inventory row (+ plugin probe, `mcpServers` command, `on_path`, herdr_report, `source_repo`, scripts); exact Windows image match; shim-on-PATH test + grep test |
| F6 | Codex I5 | confirmed | OpenRouter wallet attributed to the reading credential; management-key binding via `GET /api/v1/key` (allowlisted); unbound wallet rendered separately; S6(d)(e); fixtures |
| F7 | Codex I6 | confirmed | `MoneyMeter.meter_id` / `scope_origin` / `additive`; de-dup needs known scope + meter identity + currency + period; Nous formula ×100 with guard, unclamped; fixtures |
| F8 | Codex I7 | partly | Ollama exhaustion = "included credits used up" (HIGH) until S4(d); P6c bullet, pricing row, S4(d) extended; fixtures |
| F9 | Codex I8 | confirmed | Hermes pool = automatic failover, not hot swap; manual switch = relaunch (H-4); §4.4 rows split; §4.7 / §4.8 / D1 renamed; S7(e) |
| F10 | Codex N9 | confirmed | Hermes env mode vs pool mode storage and attribution (§4.3); whitelist parse for the pool view |
| F11 | Codex N10 | confirmed | `MINISIGN_PASSWORD` + noninteractive signing; unsigned-release branch removed; mandatory `install.sh` verification with one resolved tag; upstream cargo path removed; R1 / S8(b) / §6 updated |
| F12 | Grok I1 | confirmed | `profiles.toml` row corrected (`profile.rs:2477-2512` re-attaches; `codex_profiles.rs:201-209` drops); `HomeTab` parse hazard; rollback never copies fork rosters back |
| F13 | Grok I3 | partly | R0 first commit compiles out self-update and the two heals; `CLAUTH_NO_UPDATE=1` until then; self-revert row gains the `is_newer` trigger; "first execution", not "first install" |
| F14 | Grok I4 | partly | Helper rewrite gated on a completed import (runtime settings always); guest mode also disables `settings_sync` write-back and `~/.claude.json` writes; test |
| F15 | verifier (Codex B2) | confirmed | Standby-slot rule in the Daemons row; R2 probe; import refusal + recheck |
| F16a | Codex (missing) | confirmed | §6 acceptance fixtures: concurrent writers, credential sets, foreign env layers, auxiliary anthropic, mismatched monitoring identity |
| F16b | Codex (missing) | confirmed | R3 → R3a–R3d; H-1 → H-1a–H-1d; dependents and lanes retargeted |
| F16c | Codex (missing) | confirmed | Executor B restated self-contained in §4.4 (dispatch, initial state, 4-step protocol, helper, TTL, chain); correct v2.3 range 179-263 |
| F17 | Grok | partly | D10: unexpired-token read is the v1 default; own login opt-in behind S5(a); P4-NOUS own-login leg gated |
| CITES | both | partly | `src/providers/openrouter.rs:43-53,58-136`; `src/providers/mod.rs:555-565`; `openrouter.rs:3-8` prefixed |
| NI-M1 | verifier | new | D18 (keep detach + reconcile rule); principle 8 wording; §7 row |
| NI-M2 | verifier | new | §2 live-machine row: regular-file live slot, 10 `clauth mcp` processes |
| NI-M4/M5/M6 | verifier | new | destination-conflict refusal + cross-roster name uniqueness; move+repoint in one step under the rotation lock; short non-interactive hold (25 s waiter timeout) |
| NI-H1..H6 | verifier | new | `/model` residual risk (§7) + S7(f); `HERMES_MANAGED_DIR` passed through, managed `config.yaml` checked; `$HSP/.env` checked in the version gate; `/reload` row + S7(b) narrowed; Hermes-format fingerprint; §4.3 native-login exception |
| NI-I1..I5 | verifier | new | `plugin_host::heal_detached` no-op in R0; exact Windows image name; env-prefix row lists all vars + tests; `install.sh` cargo default removed; `ASSET_PREFIX` globs + sums-count check |
| NI-P1..P5 | verifier | new | `/key` in the management allowlist; same-account definition + fixtures; Ollama personal / team distinct; Hermes OpenRouter snapshot not a parity oracle; Nous `total_usable` non-additive |
| NI-S1..S5 | verifier | new | B routed before `poll_converge` (+ test); `LinkMode::Real` in the class and recheck; deps (P4-NOUS → H-1b, H-1a → P1b, H-1c → P1a, P4d → P1a / P4a, P6b → R3b); R2 → S8(c), R1 → S8(b), S8(d) → R3a; D11 "refuses" only, D15 in P8, H0 source `abobreshov/<tool>/herdr-plugin` |
| NI-S6 | verifier | new, no change | No leftover upstream-PR language; the relaxed-constraints citations were verified |
| L1–L16 | landing check | confirmed | `--dry-run` scope; M-1 lock-free precheck, M0 `pre` journal section + undo, guest-mode import exception; M3 cites (`actions.rs:1443-1447`, `lockorder.rs:117,191`) and non-blocking rotation locks (`runtime.rs:2482-2497`); secret sources: `bulk` refused, `mapped` checked by target name; `.op.env` rule; Hermes pool: strategy-only writer, never `auth.json`; borrowed Nous token in the Monitoring policy + P1b; whitelist gains `active_provider` / provider keys and moves to H-1a, split pool-view / D10 structs; Ollama monitor key `bound: false` label; `/model` watcher in H-1d + test; R0 ordering vs `cargo nextest`; R2 gate outside executor A; daemon decision leg never targets B before P6c; §9 step name; Nous auth-row sentence |

**Reviewed, refuted** (not applied):

| Claim | Reviewer | Why refuted |
|---|---|---|
| The Hermes home rule is "inverted" relative to `main.py` and the plan's causal sentence is not load-bearing | Grok I2 | The facts are right but the layout is the correct choice: with root == home, `_global_auth_file_path()` is None (`auth.py:916-940`), so there is no root `auth.json` fallback, and a `profiles` parent would make one shared root for every tool home. Only the guards were missing (F4) |
| Hold `clauthd.lock` / `usage-fetch.lock` / state lock "from the first rename until retire" | Grok B1 | Retire is interactive and upstream state-lock waiters time out after 25 s (`lock.rs:47`); the hold spans M3–M8 only and the post-import window is closed by retiring the binary inside the commit |
| Refuse a codex profile whose `auth.json` is not "the single linked inode" of `~/.codex/auth.json` | Grok B2 | Adoption makes `~/.codex/auth.json` a symlink, not a shared inode (`actions.rs:1331-1343`), and a profile legitimately coexists with an independent regular login (`actions.rs:1383-1399`); only a regular file carrying the same chain is refused |
| Rollback misses bare OAuth CC sessions at import | Codex B2 (part) | Import M1 (and its M4 recheck) already refused open CC sessions; rollback now names bare / helper `claude` and `codex` explicitly |
| An upstream helper could trigger the fork's self-heal rewrite in guest mode | Grok I4 (part) | The upstream helper carries `__api-key`, which the fork's parser never matches (`claude.rs:2109-2130,2230,2244-2250`); the gate is still added for a partial / crashed rollback |
| Refreshing the tool's own `hermes-cli` chain revokes Hermes' chain | Grok (F17) | `auth.py:5224-5246` documents revocation only for reuse of *Hermes'* refresh token; a separate grant has its own chain. The open question is per-client session binding, which is S5(a) |
| `/credits` 403 would only lose the wallet today | CITES (part) | Code shows `/credits` is fatal (`?`) and fails the whole fetch, `/key` rows included; the plan's degrade rule stands with corrected cites |
| Hermes clamps `used_pct` to [0,100], so the tool should too | Codex I6 (part) | The tool's `used_pct` is unclamped by design (§4.1) so debt stays visible; only the ×100 and the guard are copied |
| Ollama overage "continues at per-token rates" | plan v3 (Codex I7) | Continuation depends on purchased credits or Team auto-billing (https://ollama.com/pricing FAQ); neither is visible in `/api/usage` |

## 10. Implementation status (0.1.0, 2026-09-29)

The tool shipped as **tollgate** 0.1.0 on `feat/tollgate` (D8). The user-facing summary is `CHANGELOG.md`; this maps the phases of §5 onto what landed.

**Shipped**

| Phase | What landed | Deviations from this plan |
|---|---|---|
| R0 identity | `src/identity.rs` (name, data dir `~/.tollgate`, `TOLLGATE_` prefix, repo slug, herdr ids and tokens, CC plugin `tollgate@tollgate`, helper subcommand `__tollgate-api-key`, default listen `0.0.0.0:8453`); mechanical rename; executable-name matches through `identity`; Windows exact image-name match | The updater is removed outright (every path a no-op, no URL, no key) rather than put behind a `self-update` cargo feature. No `TOLLGATE_HOME` override |
| R2 coexistence (part) | Guest mode (§4.0 row) with `GUEST_REFUSAL`; helper token + exe check; herdr H0 (fork-owned `tollgate-v*` tags only, manifest id check, scripts call the binary); CC plugin rename; fetch lease stands down while upstream's `clauthd.lock` / `clauthd-standby.lock` / `usage-fetch.lock` is held; ownership by location for credential links | No `update = off\|notify\|auto` key (the old `[update] auto_update` is inert). `import-journal.json` is written by the R3 import |
| P1a observation | `usage/observation.rs`, `derive.rs`, `project.rs`, `collect.rs`; `tollgate usage --json` envelope, byte-for-byte golden; countdown / severity / pace fixtures | `status.json` gained no `accounts[]`; agents read observations through the local API and the MCP `usage` tool instead (D17 realised as a new tool, `profiles` unchanged) |
| P1b credential slots (part) | Monitoring keys by env var name only (`monitors.toml`, `billing_key_env`), refused secret-shaped keys and process variables, scrub from every child and from the gateway; monitor HTTP on an allowlist with no redirects | No general `ProviderHttp` policy layer; no Monitoring-login store |
| P2 palette | `palette = auto\|omarchy\|catppuccin`, live Omarchy reload, Catppuccin verbatim | — |
| P3 CLI | `usage` text cards, `--plain`, `--watch`, `--waybar` | `list` is not coloured and has no plain-format golden |
| P4-OLL | `Provider::OllamaCloud` + `OllamaDaemon`, `/api/usage` source for both body shapes, `Ollama-Cloud` preset with the env allowlist | No catalog refresh, no plan / `account` label from config |
| P4-OR | `/key` first, per-meter `/credits`, raw-number exact meters (a)–(f), preset v2, `billing_key_env` | No management-key binding by `organization_id ?? creator_user_id`, no org de-dup; S1(g) and S6 not run |
| Monitors / P4-NOUS (part) | `monitors.toml`, `tollgate monitor`, per-monitor cache (TTL, 429 hold, 7-day stale, single flight, fingerprint), budgets and `notify-send` alerts, daemon polling; Nous via Hermes' unexpired access token (D10 reader); `provider` monitors; upstream read-only view from `~/.clauth/status.json` | No own Nous device login (S5(a)); the monitor layer is new relative to §5's rows |
| Lane 4 / P4b (native legs) | Read-only Grok, Linux Antigravity keyring, native Codex; private secret store; monitor presets, offline detect/explain, shape capture | Socket-dependent tests require an owner run outside the sandbox; Grok monetary units and agy CLI print path require owner-run gates; `tollgate providers` remains separate |
| Lane 4 / P4c (key monitors) | OpenAI key health and separate hourly admin costs; Google AI key health; opt-in free Nous probe; Codex additive credits and limits; health cards and OpenAPI fields | Owner checks for response headers and status semantics remain; agy CLI and Grok money stay gated |
| Agent API | Read-only loopback HTTP (`127.0.0.1:8454`, bearer `~/.tollgate/api-token`) + unix socket, seven routes, OpenAPI, redaction, `tollgate api serve\|token\|url`, MCP `usage` | New relative to §5; documented in `docs/agent-api.md` |
| R3a–R3d import (after 0.1.0, Unreleased) | `tollgate import clauth [--dry-run] [--resume]`, `import rollback`, `import status`, `import retire` (`src/import/`, spec `docs/specs/import-clauth.md`): M-1 survey and report, post-confirmation recheck, M0 `pre` (G2), the one-rank fence (M3), M4 revalidation, M5–M8 with F1–F3 and G1/G3/G4, write-ahead journal with crash replay, automatic reversal, rollback with the split G2 undo, retire R1–R4; `tollgate plugin install\|uninstall`; the local API `import` block | The engine landed in `src/import/txn.rs` (not in the spec's file list). G1 runs in M7 inside the fence (spec I20), not M0. The retire section is undone before the rollback's fence (its undo spawns `claude` and herdr). Diverged slots need `--adopt-live` (I3), not an `expiresAt` rule. The `interrupted` warning skips `__complete` |
| herdr H1 / H2 / H4-lite | Usage-aware `$tollgate` tag + `$tollgate_severity`; native `hermes` / `grok` / `agy` panes matched to a single owning account; `tollgate.usage` action (`--tab usage`); `tollgate herdr link` / `unlink` | H4-lite is the usage action only: no `tollgate.swap`. No H2h, H3 narrow popup layout or H5 compat suite |
| S1 (a)–(f), (h), (i) | Spike run against Claude Code 2.1.283 with a local stub (`docs/spikes/s1-apikeyhelper.md`, `S1 RESULT: PASS`); machine gate block appended, compiled in by `src/hot_swap.rs` | S1(g) against real endpoints and the gateway precondition stay owner-run (`not_run` in the block); they gate the P4-OR helper preset, not B |
| P6a executor B | `src/hot_swap.rs` (transport key, `LaunchClass`, gate + `cc-version.json` cache, executor choice at spawn, `SwapView`); per-session helper `__tollgate-api-key --session <sid>` with its ack sidecar and row-missing fallback; `SessionSwap::poll_api_key` dispatched before `poll_converge` (commit in one State hold: revalidate, runtime-settings drift check, marker claim, row write, settings touch, publish); request core shared by `tollgate switch <sid> <p> [--wait]`, the TUI `m` modal and MCP `switch_profile({session})`; served-member attribution (panes + `state`, `LiveTally` + `…`, herdr tag `--session`, `which`); local API `live_sessions` | Manual switches only (P6c is not built, and the decision leg skips B rows). Settings touch at commit, with the 30 s TTL as backstop, per S1(c). Stall is reported on a recorded helper failure, not after 2 × TTL (S1(a)). |
| P6b relaunch in place | `src/relaunch.rs`: `tollgate switch <sid> <p> --relaunch [--yes] [--conversation <id>]`, claim by rename, nonce-verified hand-off env scrubbed from every child, SIGTERM then SIGKILL at 20 s, transcript flush wait, termios restore, exec of the resume form with a one-shot fallback to the original profile; conversation from `runtime_sid` hook records, then the transcript window | Ships before R3b: imported `conversations/` records would only add a faster lookup. Unix only (other platforms refuse with the manual resume line). No TUI / MCP relaunch |
| Shipped: H-1a/H-1b (Hermes part 1, `docs/specs/hermes-harness.md` §8) | `Harness::Hermes` + `HermesEngine`; `hermes-profiles.toml`; the home layout with a sibling child `HOME` (`profiles/<n>/{hermes-home,child-home}`); guards G1–G15 and the `ProjectionV1` projector (Hermes' own interpreter and parsers); the `.env` one-line writer; the env scrub; `tollgate hermes new\|key\|auth\|list\|delete`; `tollgate start <hermes>` with the marker + live row, PDEATHSIG and the teardown evidence checks; the three-roster name validate; the S7(f) spike (`docs/spikes/s7f-hermes-home.md`), compiled in as the start gate | Home under `~/.tollgate/profiles/<n>/hermes-home`, not `~/.local/share/<tool>/hermes/<n>` (D-H1). The entrypoint is re-resolved at every launch, never cached (D-H4). The H-1d daemon live-slot watcher is dropped: the child `HOME` removes the route it guarded (D-H17). |
| Shipped: H-1c/H-1d-lite/H-2/H-3/H-4 (Hermes part 2, spec §8) | `usage/hermes_local.rs` (`sqlite3 -readonly -json` over `state.db`, the usage cache, `Origin::HermesProfile`, the collect hook, the daemon tick, the start-teardown refresh); the pool view and `hermes show [--json] [--check]`; `hermes pool <n> strategy <s>`; `hermes list` with the estimate; `which` + the scrub; the TUI's fourth filter and read-only rows; `status.json` `hermes_profiles[]`; the list section; completions; herdr: the Hermes origin, the `--hermes-home` join, the `native_match` exclusion, the `/proc` read of `HERMES_HOME` alone, H2h; the MCP `switch_profile` refusal; the `hermes_projector_real` tier and the sqlite CI job | The strategy writer refuses an account home (it has one credential). H2h runs outside guest mode only (D-H11); in a sandboxed run herdr 0.9.1 wrote only `plugins/herdr-agent-state/` inside the Hermes home. The OpenAPI document has no `Origin` enum (observations are schema `Object`), so the only schema change is `StatusBody.hermes_profiles`. Automated stop-and-resume (H-4) waits for the §4.9 harness command builder (P6b's relaunch has shipped, claude only) |

**Not yet**

| Item | Phase |
|---|---|
| Class-aware chain rotation of API-key sessions (the daemon moving a B session on its own) | P6c |
| Hermes: automated stop, strategy and `--resume` (H-4 automation), the herdr `tollgate.swap` leg | the §4.9 harness command builder (P6b shipped for claude) |
| TUI restyle of all tabs and consolidation 8 → 6 with the `HomeTab` alias map | P5 (only the Usage tab's monitor cards landed), P5b |
| Fork-signed self-update: new minisign key, release workflow, mandatory verification in `install.sh` | R1 |
| `tollgate providers`, Anthropic Admin and other management sources, a local estimate for Claude Code | Remaining P4b, P4c, P4d |
| The spikes S2–S8, and S1(g) against real endpoints | — |

**Known gaps found in review** (also in `CHANGELOG.md`): `tollgate login` still runs the Claude OAuth flow in guest mode; `list` and `status --json` print `base_url` unredacted; `tollgate api serve` leaves `api.sock` on SIGTERM; the herdr `--display-agent` scope is unverified.
