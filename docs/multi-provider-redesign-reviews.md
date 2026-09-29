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




# Plan v3 review — Grok (grok-4.7, high, plan mode) — 2026-09-29

VERDICT: APPROVE WITH CHANGES

Blocking issues
1. Import treats the live Claude login as a symlink, and upstream can mint a second carrier after the store is moved. The inventory says ~/.claude/.credentials.json → ~/.clauth/profiles/scifoo/credentials.json (plan line 69) and step 4 says "Repoint" it (lines 180–181). The codex sentence on that same line handles a regular file; the Claude sentence does not. Metadata: ~/.claude/.credentials.json is a regular file, mode 0600, 6498 bytes, not a symlink. ~/.codex/auth.json is also a regular file (0600, 4049 bytes), which the plan states. No inode or byte comparison was done. classify_link_at treats a non-symlink as content equality of the access token (src/claude.rs:949-968). After credentials.json is renamed away, is_first_login_at is true when the expected store is gone and the live path is a regular file with a login (src/claude.rs:1003-1016), and snapshot_active_credentials then adopts: it copies the live file back into ~/.clauth/profiles/<active>/ and symlinks (src/claude.rs:2494-2530). Upstream 0.16.0 does not read MIGRATED and is still installed until a later, owner-confirmed cargo uninstall (plan lines 187–196). The tool only stands down when it sees clauthd.lock or usage-fetch.lock held (plan lines 144–145); it does not hold those leases, and upstream never checks the fork. Rollback makes this worse: it cargo installs 0.16.0 before the moved stores are renamed back (plan lines 198–203). Fix: under one critical section, symlink_metadata plus nlink. Symlink: retarget it in the same step as the rename. Regular file: abort on Diverged; if it is the same carrier, the live file is the survivor and the profile copy is not refreshed again. Hold ~/.clauth/clauthd.lock and usage-fetch.lock from the first rename until retire. Put the upstream binary back only after the stores are back. Journal move and copy separately; wipe only copy-action paths. "Originals were never touched" (plan line 204) is false for a rename(2).
2. The move set is not the credential set, so a sidecar refresh token stays behind. credential_fingerprint is exactly credentials.json, session-token.json, and session-token.static.json (src/claude.rs:152-170). The install source is the sidecar when it is long-lived (src/claude.rs:886-898). A sidecar that still contains a refresh token is a mis-fill and "by construction a copy of credentials.json" (src/claude.rs:48-52, 200-202). The import table moves only credentials.json and codex auth.json (plan lines 165–166). Today's three profile dirs (leadtone, personal, scifoo) list no sidecar, so this is not armed on disk now. The rolling-token writer creates that file (src/claude.rs:315-355). Leaving a mis-fill in ~/.clauth while the tool refreshes the moved copy is the permanent-death case in docs/codex-plan.md:17-20 (decisions 7 and 8: one physical file, copy means a second carrier). Same rule for codex: there is no codex-profiles.toml and no profiles/*/auth.json today, and the regular ~/.codex/auth.json must stay untouched. If a profile auth.json ever exists beside that regular file, do not refresh the imported copy. Fix: rename(2) all three Claude files, never copy them. Refuse a codex profile whose auth.json is not the single linked inode of ~/.codex/auth.json.

Important issues
1. profiles.toml does not drop unknown keys. save_app_state reattaches them (src/profile.rs:2477-2500). The comment at line 2492 describes the
bug that function closes. codex-profiles.toml does drop them: plain toml::to_string_pretty (src/codex_profiles.rs:205-208; the comment at lines
10–11 is only half right). The rollback warning (plan lines 148, 206) is wrong for the Claude roster.
2. The Hermes home rule is inverted relative to main.py. HERMES_HOME is trusted and left alone only when its parent directory is named profiles
(hermes_cli/main.py:580-592). Otherwise startup reads <root>/active_profile and can replace HERMES_HOME (main.py:606-640).
get_default_hermes_root returns the grandparent when the parent is profiles, and returns the home itself otherwise (hermes_constants.py:184-
191). A layout whose parent is not profiles avoids sharing ~/.hermes/shared/nous_auth.json, and that shared store copies tokens with no account
check (hermes_cli/auth.py:4808-4840). It also skips the trust return, so "never create active_profile" is load-bearing and the plan's causal
sentence (line 554) is not. HERMES_SHARED_AUTH_DIR is honored first (auth.py:4748-4750) and is the real private-store guard. Pin that, and make
a present active_profile abort launch.
3. A binary built before the updater is compiled out still self-replaces from upstream. API_URL is uwuclxdy/clauth (src/update.rs:12), the
minisign key is pinned (line 31), an empty key skips verification (update.rs:175-176, 195-197), and auto_update defaults on (update.rs:57-63).
A non-cargo binary downloads and self_replaces (update.rs:84-115, 282); ~/.cargo/bin only notifies (update.rs:288-298). R0 says "Updates
disabled" and R2 adds the config key (plan lines 575, 577). Fix: R0 must refuse spawn() at compile time, before any fork binary is executed.
The plan's R1 key, fork URL, and default off are the right end state.
4. Guest mode forbids writing ~/.claude/settings.json (plan line 146) while the helper exe check rewrites that file on mismatch (line 132).
Gate the rewrite on a completed import.
5. Executor A/B is not weakened. The restated protocol matches the approved one: state flock rank 500 before SwapCell 550 (src/
lockorder.rs:191-205), commit through SessionFields, ack only after a flushed exit-0 write under <sid>.helper.lock, helper takes no state
flock. HarnessEngine (src/harness.rs:65-82) and the Codex install bail (harness.rs:153-161) match a third harness that installs nothing.
os.replace of Path.home()/.claude/.credentials.json is real and ignores CLAUDE_CONFIG_DIR (agent/anthropic_adapter.py:958, 1167, 1210);
refusing provider anthropic in tool-managed homes is the right v1 cut. Pool source claude_code refreshes that same file (agent/
credential_pool.py:681-688, 1218-1246).
Top 3 changes
1. Rewrite §4.0 step 4 for a regular-file live slot, hold the upstream leases across import, and install 0.16.0 on rollback only after the
stores are restored.
2. Move session-token.json and session-token.static.json with credentials.json. Never copy a file that can hold a refresh token.
3. Correct the Hermes parent-name rule to main.py:580-592, and compile out self-update in R0 before any fork binary runs.



# Plan v3 review — Codex (GPT-6-Astra, read-only sandbox) — 2026-09-29

VERDICT: REVISE
## Blocking issues
1. Import omits credential files that determine Claude’s login behavior. The migration inventory moves only credentials.json and Codex auth.json (plan:166
   (docs/multi-provider-redesign-plan.md:166)). However, session-token.json can be the installed credential, while session-token.static.json preserves its
   recovery token. Without the sidecar, installation falls back to the rotating OAuth store (src/claude.rs:23, src/claude.rs:118, src/claude.rs:890). Fix:
   migrate the complete credential set, preserve its installation semantics, and test static, rolling and ordinary OAuth profiles. Also inventory durable
   profiles/<p>/codex-home data; it contains session history and databases omitted from the table (src/runtime.rs:6338).
2. Migration does not specify a transaction that excludes credential writers. Import checks processes before moving files, but does not require holding both
   tools’ state locks and upstream leases throughout the transaction. Rollback excludes tool processes and helper-based CC sessions, but misses bare OAuth CC
   sessions and native Codex sessions using adopted links (plan:145 (docs/multi-provider-redesign-plan.md:145), plan:154 (docs/multi-provider-redesign-
   plan.md:154), plan:198 (docs/multi-provider-redesign-plan.md:198)). Upstream writes synchronize on its own .lock; fork locks cannot exclude them (src/
   lock.rs:347). Fix: define lock acquisition, retained leases, process revalidation, and exclusion of every session sharing a moved store. Specify durable
   journal ordering, destination-conflict handling and refusal of cross-filesystem moves. A MIGRATED file alone does not disable the unchanged upstream
   dispatcher (src/main.rs:215).
3. Hermes isolation remains porous, including a route to the owner’s Claude credentials. Creating a home .env does not prevent installation .env loading: it
   still fills missing variables; managed .env subsequently overrides them. This contradicts the mitigation at plan:553 (docs/multi-provider-redesign-
   plan.md:553). Evidence: $HSP/hermes_cli/env_loader.py:331–336,366–370, where $HSP is defined at plan:13–15. Moreover, blocking the primary provider anthropic
   is insufficient: auxiliary Anthropic requests call the same credential resolver ($HSP/agent/auxiliary_client.py:2797–2818), which reads and refreshes the
   real Claude credential file ($HSP/agent/anthropic_adapter.py:1311–1332). Fix: enforce account binding against all effective environment/configuration layers
   and reject Anthropic auxiliary/fallback/pool routes as well as the primary provider. A warning cannot establish isolation.
## Important issues
4. The identity inventory misses executable behavior. These are not cosmetic literals eligible for lazy renaming:
    • Session creation invokes upstream clauth start (src/daemon/api/create.rs:375).
    • Pane matching recognizes only a process named clauth (src/daemon/api/panes.rs:324).
    • Daemon replacement recognizes only clauth daemon (src/daemon/probe.rs:588).
    • Pane metadata reads tokens.clauth (src/daemon/api/panes.rs:347).
   Fix: add these to R0/R2 and test using the renamed binary while upstream remains installed. Otherwise fork actions can launch upstream or fail to recognize
   their own processes.
5. OpenRouter wallet attribution is unproven. The plan accepts a separate management key, then labels its /credits result using the inference key’s identity
   (plan:529–532 (docs/multi-provider-redesign-plan.md:529)). But /credits reports the authenticated credential’s wallet and returns no account identifier. A
   management key belonging to account B can therefore display B’s balance against inference account A. Fix: verify the binding or represent the monitoring
   account separately; keep organization scope unresolved until S6(c). Add a mismatched-key fixture. OpenRouter credits contract.
6. Two observation rules produce incorrect output. Deduplicating solely by (source, scope_id) can collapse subscription/top-up balances, different periods, or
   unrelated accounts with missing IDs (plan:281 (docs/multi-provider-redesign-plan.md:281)). Require known scope identity plus meter identity, currency and
   period. Separately, Nous used_pct omits ×100 and a positive-denominator guard (plan:518 (docs/multi-provider-redesign-plan.md:518)); Hermes explicitly
   implements both ($HSP/agent/account_usage.py:174–181). Test $22/$7.90 → 64.09%, zero cap, unknown identities and multiple balance categories.
7. Ollama exhaustion does not guarantee continued billing. The plan promises continuing overage at plan:400 (docs/multi-provider-redesign-plan.md:400) and
   plan:451 (docs/multi-provider-redesign-plan.md:451). Published pricing instead describes consuming purchased credits after included credits; Team
   additionally supports automatic usage billing. Fix: display “included credits consumed” until purchased balance/automatic billing is known, and gate this
   mapping on S4(d), not merely S4(a). Ollama pricing.
8. Hermes automatic rotation is not a specified manual hot swap. The proposed strategies select credentials automatically, while H4 relaunches and account homes
   cannot resume across homes (plan:388 (docs/multi-provider-redesign-plan.md:388), plan:559 (docs/multi-provider-redesign-plan.md:559), plan:598 (docs/multi-
   provider-redesign-plan.md:598)). The cited strategy descriptions confirm automatic selection ($HSP/hermes_cli/auth_commands.py:736–747). Fix: explicitly
   distinguish automatic failover from user-selected credential switching; specify a proven live-selection mechanism and acknowledgement, or disclose relaunch-
   only manual switching.
## Minor / nits
9. Hermes credential storage contradicts itself. “Only <home>/.env” (plan:317 (docs/multi-provider-redesign-plan.md:317)) conflicts with prescribed auth add
   --type api-key, which stores the key as a pooled credential ($HSP/hermes_cli/auth_commands.py:210–220). Document both storage modes and their fingerprint/
   attribution rules.
10. Signing custody needs an executable CI design. The plan requires a passphrase-protected key, but the cited workflow provides only MINISIGN_SECRET_KEY and
   invokes signing without supplying a passphrase (plan:214 (docs/multi-provider-redesign-plan.md:214), .github/workflows/release.yml:103). Specify
   noninteractive signing and make installer verification mandatory, resolving “or document the gap” at plan:219.
## Missing from the plan
Acceptance fixtures for the findings above: concurrent import/rollback writers, session-token migration, foreign environment layers, auxiliary Anthropic
  resolution, and mismatched monitoring identities. Existing additions do not cover these cases (plan:638–655 (docs/multi-provider-redesign-plan.md:638)).
Bounded delivery slices: H-1 combines engine, roster, environment handling, launcher, UI, status and herdr integration; R3 combines migration, rollback and
  retirement (plan:578 (docs/multi-provider-redesign-plan.md:578), plan:595 (docs/multi-provider-redesign-plan.md:595)). Separate the isolation and transaction
  foundations from their UI/integration consumers.
Self-contained preservation of approved invariants: v3 retains A, dispatch, State→SwapCell ordering and stable acknowledgement locking, but delegates details
  to v2.3. Restate the two-writer row/sidecar rule and exact in-lock revalidation conditions rather than relying on the inaccurate historical line range
  (plan:336–357 (docs/multi-provider-redesign-plan.md:336); 87ceb3da:docs/multi-provider-redesign-plan.md:199–246).
## Decisions
D1: Describe Hermes manual switching as relaunch-only until live selection is proven; automatic pool rotation does not establish the requested interaction.
D12(b): Refuse Anthropic across primary, auxiliary, fallback and pool routes; a primary-provider check leaves the credential hazard reachable.
## Top 3 changes
1. Make import/rollback a complete, writer-exclusive credential migration.
2. Enforce Hermes isolation across every environment and inference route.
3. Complete operational renaming and correct wallet attribution, deduplication and exhaustion semantics.
