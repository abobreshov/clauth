# Plan review — Grok (grok-4.7, high, plan mode) — 2026-09-29

VERDICT: REVISE
Blocking issues
1. A session-scoped helper that reads intended_member drives the OAuth executor, and the other readers never see it. intended_member is daemon-owned (src/live_sessions.rs:68-74). SessionSwap::poll calls swap_to whenever it differs from the cell (src/runtime.rs:3079-3094). swap_to drains, bumps mtime, and repoints .credentials.json under
RotationGuard and the state flock (src/runtime.rs:3244-3287); precondition requires install_source_path to exist (src/runtime.rs:3180-3184, src/claude.rs:890-898). Today swap_eligible returns NotOauth when base_url is set (src/runtime.rs:2715-2716, src/profile.rs:469-470), so the write is refused, current_member stays empty, and clauth
switch <sid> still reports that the switch lands (src/sessions_cli.rs:238-252). Herdr reads current_member, then start_profile (herdr-plugin/report-profile.sh:119-125). live_sessions::get is lock-free (src/live_sessions.rs:348-355), but the only key printer calls load_profile (src/main.rs:2170-2180), which takes the state flock and can
rewrite credentials.json (src/profile.rs:3130-3134). profile_name_from_helper rejects any token after the profile name (src/claude.rs:2115-2120). Fix: two executors. OAuth keeps today's swap_to. An API-key swap is allowed only with no install source and no auth env; it publishes current_member and does not take RotationGuard. The helper
reads that field from a lock-free config.toml parse. Only write_merged_settings (src/runtime.rs:4869) may emit --session; the global caller of build_claude_settings_json keeps <exe> __api-key <profile> (src/claude.rs:2481-2486). apiKeyHelper stays in PER_PROFILE_TOP_FIELDS (src/settings_sync.rs:70-80).
2. --relaunch uses the wrong id and the wrong command. switch <sid> is a runtime id <pid>-<seq> (src/runtime.rs:226-263, src/sessions_cli.rs:183-191). resume resolves a Claude conversation, spawns claude --resume, and never passes --with-fallback (src/sessions_cli.rs:93-147). Fix: stop the runtime child, start from the conversation id,
keep follows_chain, and in herdr use the existing pane run path (src/daemon/api/create.rs:100).
3. A class filter does not make wallets rotate, and it changes who may OAuth-swap. to_usage_info keeps only bars labeled 5h/7d (src/providers/mod.rs:438-469). OpenRouter publishes credit rows (src/providers/openrouter.rs:58-79). --with-fallback refuses !is_oauth() (src/fallback.rs:1452-1453); the session walk only skips candidates
swap_eligible refuses (src/fallback.rs:1132-1158). LaunchTransport is frozen in process.env (src/runtime.rs:2677-2685). profile.env is applied last, so ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY override the helper (src/claude.rs:2423-2451, src/claude.rs:2348-2353). routing_endpoint prefers env ANTHROPIC_BASE_URL (src/profile.rs:496-501).
Load drops base_url only when a pair exists and there is no key (src/profile.rs:3059-3069); a pair plus a key is a hybrid one AuthKind cannot name. ApiKeyDiffers is presence (src/runtime.rs:2727-2728). poll_converge still moves a rolling sidecar through the credential executor (src/runtime.rs:3111-3125). Models-in-class and "two Z.ai
plans" disagree unless ModelSettings match. Fix: keep the five current checks as the subscription class. API-key class = same harness, same routing_endpoint() origin, same models, same env with the endpoint key removed, no auth-env on either side, no OAuth store. Add a balance exhaustion predicate before promising wallet rotation. Leave
Z.ai 30d out of UsageInfo.
4. A second poller and one Credential double-fetch and can send an admin key as the inference key. spawn_refresher already polls third-party and folds to_usage_info (src/usage/scheduler.rs:2736, src/usage/scheduler.rs:4064). get_json sends Authorization: Bearer and reads the whole body (src/providers/mod.rs:540-569) on the agent that
also refreshes OAuth (src/oauth.rs:332-349). Alibaba quota is a console session (src/providers/mod.rs:216-228). Fix: project AccountReport at read time. Class-M targets are another leg of this scheduler, per profile, on third_party_cache and the existing 429/AuthExpired hold. Billing keys are a separate env-only slot that __api-key never
prints, scrubbed via MANAGED_ENV_KEYS and CODEX_MANAGED_ENV_KEYS (src/runtime.rs:4424-4434, src/harness.rs:142-150). Cap bodies and refuse cross-origin redirects on provider fetches only.

Important issues
• TTL. CLAUDE_CODE_API_KEY_HELPER_TTL_MS is not in MANAGED_ENV_KEYS, so a settings env entry is Shared and the watchdog's sync_once (src/runtime.rs:4183) will publish it into ~/.claude/settings.json (src/settings_sync.rs:127-137). Set it on the child after scrub_profile_env (src/runtime.rs:4442-4448).
• S1 tests a mechanism P6 does not use. The design keeps the helper string stable. Gate P6 on TTL re-exec of that same command and on the new stdout being sent. If the command is sticky, relaunch.
• settings_sync will not clobber a per-runtime helper (src/settings_sync.rs:70-80). The next write_merged_settings will, unless the session form is rebuilt there.
• theme is already color depth. profiles.toml and --theme are full|compatible (src/profile.rs:616-619, src/cli.rs:31-40). Accessors are functions with hand-picked 256 indices (src/tui/theme.rs:112-167); tests pin them under TierTest (src/lockorder.rs:95-98). New key: palette. Keep the Catppuccin table verbatim.
• 8→6 tabs contradict "unchanged App state". HomeTab is persisted (src/profile.rs:662-707); tab_activity is [_; Tab::ALL.len()] (src/tui/app.rs:2086); keys differ per tab (wiki/Interface-And-Keys.md:44-54); tests/inline/tui_app.rs walks every tab. app.rs is 11434 lines.
• accounts[] is a second projection. Schema 2 stays additive (src/daemon/status_json.rs:35-38, docs/codex-plan.md:22). Plain clauth list renders ProfileEntry with no color (src/list.rs:1-39), so byte-identical output is real. profiles[].windows, the chain, and MCP funded_wallets (src/usage/burn.rs:196-200) will not match a duration-
normalised report. The UI must not imply a switch at a displayed balance CRITICAL.
• Cents(i64) loses the existing wallet. Amounts are f64 (src/providers/mod.rs:56-65); OpenRouter flags remaining < 0.005, including negatives (src/providers/openrouter.rs:59-75). Severity needs one rule: the worst of used%, balance, and pace, with a word at every rung. A switch threshold under 50 makes "critical" fire before "high".
• P6 and P4a are each two designs in one row. P6 is the executor, fallback.rs, the helper parser, settings build, and relaunch. P4a both "moves providers behind the trait" and "leaves the refresher legs as they are".
• Shared transcripts have no profile. Shared isolation symlinks projects/ (src/runtime.rs:4736-4743). The ledger is date → model (src/token_ledger.rs:117-125). If S3 fails, omit the estimate.
Minor / nits
• §2 matches the tree: swap_eligible at src/runtime.rs:2711, helper at src/claude.rs:2232 and :2468, Provider at src/providers/mod.rs:128-134, herdr 0.9.1 rects at src/herdr.rs:519, codex install bail at src/harness.rs:154-157, and run_switch already refuses codex (src/sessions_cli.rs:212-213).
• feat/provider-monitoring tip is bddf0535 (not on mommy); bbff3341 is an ancestor. The cherry-pick target is right.
• Isolated sessions are already refused (src/runtime.rs:3163-3165). Do not wire the session helper there.
• fallbackModel is read (src/claude.rs:2185) and syncs as a shared field. Leave it out of the class.
• Keep buffer_rows goldens. app.rs is 11434 lines, not 11.3k.
Missing from the plan
• StartBlock::NotOauth and the daemon decision leg.
• Publishing current_member, herdr's parser, and the rule that the helper is not a third writer.
• Shunt base_url (src/gateway.rs:1-11) is not the upstream provider.
• Alibaba ConsoleExpired is usage-only (src/providers/mod.rs:530-535).
• Scrub entries for every new secret env var.
• HomeTab aliases if a tab is renamed.
• Child Claude processes inherit the runtime settings.json and therefore the parent sid.
• Body cap and redirect policy stay off the shared OAuth agent.
Decisions D1–D7
• D1: API-key live swap only with no OAuth store and no auth env. OAuth executor unchanged. Hybrids and cross-provider relaunch. One intended_member cannot mean both a relink and a new key.
• D2: Offer P0, P1, and P2 upstream. P4a only as a read-time projection. P5 and P6 stay on the fork until S1 passes on the mechanism P6 uses.
• D3: Anthropic API (Admin + estimate), then OpenRouter and DeepSeek as meters, Z.ai only where model maps match, Grok and Antigravity monitor-only after S2. No wallet chain-rotation in v1.
• D4: Keep 8 tabs and restyle. Persisted HomeTab and per-tab keys make the merge its own project.
• D5: clauth bar stays optional P8, after AccountReport exists.
• D6: Default catppuccin. auto changes every Omarchy user's screen on upgrade; ship it as an explicit palette value.
• D7: Port the branch's Cloud Code read that never unlocks the keyring. Use agy --print only if S2 shows that read spends quota or dies, with the offline probe and version gate.
Top 3 changes
1. Rewrite §4.4 as two executors, relaunch from the conversation id, and today's swap_eligible checks kept verbatim for subscription profiles.
2. Make §4.2 a projection over UsageInfo, ThirdPartyStats, and codex, on the one scheduler, with a billing credential __api-key cannot print.
3. Drop tab consolidation and the theme key overload from v1, and retarget S1 at an unchanged helper command.


# Plan review — Codex (GPT-6-Astra, read-only sandbox) — 2026-09-29

VERDICT: REVISE

## Blocking issues

1. The helper must not authenticate from uncommitted intended_member. That field is a daemon request; current_member is session-owned (src/live_sessions.rs:68). The watchdog validates intent, checks shutdown/isolation/credential availability, and publishes only after executing the swap (src/runtime.rs:3085, src/runtime.rs:3153, src/runtime.rs:3293).
   Reading intent directly would bypass refusals and disagree with reporting. Fix: define a distinct API-key executor that validates and atomically commits a credential selection, preserves marker ownership, and distinguishes requested, committed, and client-effective state. Retain the existing rotation-before-state lock order for OAuth (src/
   runtime.rs:3233).

2. --relaunch uses the wrong session identifier. Runtime IDs are <pid>-<seq> (src/runtime.rs:226); resumable conversation IDs are transcript filename stems (src/sessions.rs:515). resume <sid> cannot simply consume the switch target. Fix: specify runtime-to-conversation resolution, ambiguity refusal, graceful shutdown, transcript flush, preserved working
   directory/arguments, and recovery if replacement launch fails. Resolve everything before stopping the original process.

3. Environment-backed secrets lack a workable ownership model. The plan strips secret variables from children but expects a helper executed by that child to resolve api_key_env. Existing scrubbing covers managed names and outgoing profile variables, not arbitrary new billing-secret references (src/runtime.rs:4424). Fix: separate inference credentials
   from monitoring/admin credentials, define resolution for CLI, daemon, and helper processes, and forbid admin credentials from entering inference helpers, generic scanners, or child environments. Specify fixed billing origins, redaction, permissions, and credential-rotation behavior.

4. Transcript ownership cannot support the proposed historical per-profile estimates. The ledger aggregates by date/model (src/token_ledger.rs:119); exact-owner stamping overwrites the conversation’s previous owner (src/sessions.rs:1026). A conversation can span swaps and resumes. Fix: make S3 establish request/record-level attribution or explicitly
   unattributed usage, including the helper-cache transition interval. Never assign an entire transcript to its latest profile. Add migration and deduplication rules before promising profile totals.

## Important issues

1. Swap classes must describe effective launch transport, not raw profile fields. is_oauth() merely checks absence of base_url; hybrid credentials and environment endpoint overrides are explicitly supported (src/profile.rs:469). Profile environment entries override generated endpoint/model settings (src/claude.rs:2449). Define conservative URL
   normalization preserving meaningful paths/query parameters, effective model/header settings, credential precedence, and supported OAuth/session-token/rolling-token transitions. Persist the launch snapshot: the daemon currently reconstructs it from mutable profile configuration (src/usage/scheduler.rs:4635).

2. The helper design is feasible, but its implementation contract is incomplete. Runtime settings currently generate a profile-scoped helper (src/runtime.rs:4869). Settings synchronization correctly excludes apiKeyHelper, so it should preserve a session-specific override rather than distribute it (src/settings_sync.rs:70). Explicitly wire the session ID
   into construction and test rebuild/sync persistence. The existing helper calls load_profile, which reads credentials and invokes pending-credential recovery; it is not a single-file pure reader (src/main.rs:2180, src/profile.rs:3307). Introduce a minimal read-only credential resolver.

   Claude documents five-minute caching and the TTL override, but that does not establish completed authentication within 30 seconds. Keep S1 version-pinned and gate success reporting on demonstrated behavior. A fake Claude cannot verify Claude’s caching. Official helper documentation.

3. Keep the projection; avoid creating a competing fetch/cache pipeline. Existing scheduling already handles credential fingerprints, host pacing, cache retention, and server-directed retry delays (src/usage/scheduler.rs:325, src/usage/scheduler.rs:3093, src/usage/scheduler.rs:3143). Assign exactly one polling owner per target and retain Retry-After.
   Cache identity needs source, endpoint, account/scope, and credential identity—not only credential bytes. Prefer fetch(target, credential, http) -> Observation; assemble account identity and scheduler state separately.

4. The report shapes lose essential information. The branch supports unknown percentages, explicit exhaustion, model membership, and separate observation/check timestamps (feat/provider-monitoring:src/provider_monitor/types.rs:37,79). Preserve these, plus best_effort and provider availability (src/providers/mod.rs:350). Separate freshness from fetch
   failure so stale data can coexist with auth_required. Add money scope—organization/workspace/key/profile—period boundaries, budget provenance, and sub-cent precision. Do not parse formatted display rows back into financial data; DeepSeek already converts raw amounts into strings (src/providers/deepseek.rs:43). Preserve the deliberate exclusion of
   generic and non-5h/7d windows from fallback decisions (src/providers/mod.rs:438).

5. Herdr needs lifecycle and identity work beyond labels. Both reporter and watcher explicitly exclude Grok/Antigravity (herdr-plugin/report-profile.sh:172, herdr-plugin/watch-profile.sh:67). Define native account-to-pane matching, ambiguous/unmanaged panes, cleanup, and severity metadata. The REST bridge joins sessions by PID, never display tags (src/
   daemon/api/panes.rs:4); the swap action must preserve that distinction and avoid delegates. Also address fork distribution: installation currently names the upstream plugin (src/herdr.rs:32).

6. Tab consolidation is not merely a rendering change. Existing keys have tab-specific semantics, and persisted home-tab values name all eight tabs (wiki/Interface-And-Keys.md:44, wiki/Interface-And-Keys.md:123). Tests exercise that dispatch (tests/inline/tui_app.rs:13357). Require an explicit navigation/config migration map, or retain eight tabs
   initially.

## Minor / nits

Runtime palette replacement fits the accessors, but the existing palette uses hand-selected RGB/indexed pairs, not runtime quantization (src/tui/theme.rs:112). Preserve those pairs for pixel parity; include banner tints, malformed-file retention, symlink changes, and isolated palette tests.
Correct §2: API-key eligibility compares presence, not key values (src/runtime.rs:2727).
Resolve D2’s “defaults to today’s look” versus D6’s auto. The mockup’s 28% used/40% elapsed is not “on pace” under the proposed ±5-point rule.

## Missing from the plan

Smaller delivery units: separate P4c by provider; separate attribution, API-key swapping, relaunch, and tab migration. Move credential security and report contracts before fetchers. Ship documentation with each behavior change.
Tests: concurrent swap/delete/shutdown, rejected intent, missing environment secrets, cache identity changes, helper settings synchronization, OAuth mtime receipts and macOS convergence, redirect leakage, pagination/partial billing results, palette reload failure, and daemonless/older-daemon behavior.
Output contracts: serialize domain reports independently of presentation Sections; define timestamps and IDs. Preserve schema-2 fields and both existing readers (docs/codex-plan.md:22). Keep the current plain-list formatter as the compatibility oracle (src/list.rs:240).


## Decisions D1–D7

D1: Same-effective-transport swaps, gated by S1; relaunch separately.
D2: Additive upstream PRs; isolate fork-specific navigation.
D3: Existing OpenRouter/DeepSeek/Z.ai first; Anthropic billing after attribution/security gates.
D4: Restyle eight tabs first; consolidate later.
D5: Optional follow-up, outside core acceptance.
D6: auto for this fork, with deterministic Catppuccin fallback.
D7: Provisionally direct read-only API; require S2 and explicit platform fallback—the branch’s native keyring reader is Linux-only (feat/provider-monitoring:src/provider_monitor/antigravity.rs:71,101).

## Top 3 changes you would make to the plan

1. Replace intent-driven authentication with a validated, committed swap protocol.
2. Establish lossless observations, credential boundaries, and attribution before new providers.
3. Split relaunch and navigation migration from the initial reporting/theme release.

Worked for 3m 5s · 9:39 AM


