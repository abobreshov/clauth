# tollgate 0.1.0 code review — 2026-09-29

Branch feat/tollgate @ d406cfd4, diff 7b231ab2..d406cfd4. Both reviewers read-only.

## Grok (grok-4.7, high, plan mode)

VERDICT: REVISE

Blocking
1. A guest tollgate start / resume still publishes the operator Claude and Codex trees into the session.
sessions_cli.rs:139-147 resumes with Isolation::Shared and no guest check. start.rs, sessions_cli.rs, and runtime.rs never call upstream_active. build_runtime_dir_with_active_env (runtime.rs:4668-4750) symlinks every top-level ~/.claude entry except settings.json and .credentials.json, including plugins/ and projects/. Real mode is symlink (runtime.rs:6299-6301), so a Claude Code write of plugins/installed_plugins.json lands in the operator registry, and the projects/ link is the path the Isolated comment (runtime.rs:4736-4739) says can delete transcripts. Fake mode then copies those bytes back: tick calls mirror_tree (runtime.rs:5658), whose skip set is only settings.json, .credentials.json, and .claude.json (runtime.rs:5883-5891).
The same start links ~/.codex/{skills,rules,agents,templates,references,AGENTS.md,plugins} into a shared Codex home (runtime.rs:6484-6497, 6665-6668). auth.json is the profile store, not the operator file, but plugins/ is the operator directory, and the comment says an install there outlives the session. The coexistence rule is that guest mode writes none of ~/.claude's plugin registry and none of ~/.codex/* (docs/multi-provider-redesign-plan.md:154; identity.rs:125-128).
Fix: in guest mode, do not link or mirror plugins/, projects/, or CODEX_OPERATOR_ENTRIES. Build the session from the per-session runtime only (the Isolated tree, including for the providers guest mode is allowed to launch). Add the coexistence assertion that installed_plugins.json and ~/.codex/plugins stay byte-identical across a guest start, not only across a runtime settings.json edit.
2. Guest mode still runs Claude and Codex OAuth legs whenever upstream's lock files are free.
FetchLease::acquire stands down only while ~/.clauth/{clauthd.lock,clauthd-standby.lock,usage-fetch.lock} is held or unreadable (daemon/probe.rs:724-736, 767-787). It does not consult upstream_active. After the lease is won, tick always calls claude_rolling_tick → restamp_rolling_token and codex_auth::standby_tick (usage/scheduler.rs:3539-3574). The only guest return in that file is scan_auto_switch (usage/scheduler.rs:4406-4408). standby_tick rotates every Codex profile store with atomic_write_600 (codex_auth.rs:1053-1058, 976). try_adopt_live_rotation has no guest check (usage/scheduler.rs:1156-1158 and 1215-1217; oauth.rs:1763-1914): for the active profile it reads ~/.claude/.credentials.json and, when the live pair is newer and the account id matches, copies that refresh token into the tollgate store. force_link_profile_credentials then refuses (oauth.rs:1956), so the copy sticks and a later refresh spends it. cmd_login is not gated (main.rs:1041-1048) and will mint a Claude or Codex chain into that store; codex_login_browser correctly avoids writing ~/.codex itself (actions.rs:1651-1688), but the standby leg above then rotates whatever it stored.
A ~/.claude/.credentials.json or ~/.codex/auth.json symlink created before ~/.clauth existed still points at that store. Guest switch refuses to create one (actions.rs:1256-1258) and does not remove one. Renaming a new inode onto the store updates the file the operator link reads, and spending the refresh token is the two-carrier death the plan forbids (docs/multi-provider-redesign-plan.md:152-154). Upstream installed but idle is the common case: the locks are not held, so the stand-down does not fire.
Fix: if upstream_active(), return before the rolling scan, the Codex standby pass, the adopt, and every refresh-token spend. Refuse tollgate login (both harnesses) the same way cmd_capture already refuses.

Important
1. Guest runtime copies are still sync members of the operator settings base. runtime_files_under always includes ~/.claude/settings.json plus every shared runtime copy (jsonsync.rs:66-73; settings_sync.rs:154-158). Guest mode only
skips the write when operator_file compares equal (jsonsync.rs:161-168). The plan says those copies are never members. A newer runtime still pushes shared keys to its siblings. The compare is lexical, and write_member renames onto
the operator path at umask mode when the strings differ (jsonsync.rs:251-256). Fix: in guest mode, do not put the operator file or the runtime copies in one member set.
2. OpenRouter and Ollama send bearer tokens through usage::http_agent (providers/openrouter.rs:112-131, providers/ollama_cloud.rs uses the same agent, usage/fetch.rs:959-970). That agent sets timeouts and http_status_as_error(false)
and does not set max_redirects. ureq 3.4.2 defaults to 10 redirects and RedirectAuthHeaders::Never, so Authorization is stripped on the redirect, but the body is still read_to_string with no cap and then parsed as billing data. The
management-key allowlist is only the original path string (openrouter.rs:138-140). The monitor agent already does this correctly: max_redirects(0) and a 2 MiB cap (usage/monitor/source.rs). Use that agent for every key-bearing
provider GET.
3. GET /v1/status returns the on-disk feed raw when the JSON does not parse, or when redact_status changes nothing (local_api/routes.rs:469-478). redact_status rewrites only profiles[].base_url (routes.rs:495-514). A credential in
any other field of a published or hand-edited status.json is served as-is. Redact before the passthrough, or rebuild from StatusBody and drop the raw branch.
Minor
• The local API never checks Host. TCP requires the 0600 bearer and a loopback peer (local_api/mod.rs:427-429), so a DNS rebind without the token only gets 401. An allowlist of localhost, 127.0.0.1, and ::1 is still the missing
check.
• bind_unix connects, unlinks, binds, then chmod 0600 (local_api/mod.rs:393-407). mkdir_700 does not tighten a pre-existing mode. The unlink is check-then-remove against another same-user binder.
• Amount::from_f64 goes through Display. Extreme magnitudes can print in scientific form and then fail parse, which drops the sample rather than storing a wrong number. derive.rs severity ladders (used ≥ 90/75/50, USD balance ≥
20/5/1, pace ≥ 10 / > 0 / ≥ −10) match the plan. The monitor cache records a non-secret fingerprint, flocks <id>.lock, holds 429s for at least five minutes, and caps reads at 2 MiB (usage/monitor/cache.rs:1-21, 36-42, 107-112).
• herdr-plugin/report-profile.sh:84 feeds session paths to xargs ls without -0. open-pane.sh:100 interpolates the pane id into a double-quoted sed expression. Herdr link/unlink itself is argv, not a shell (herdr/link.rs).

## Codex (GPT-6-Astra, read-only sandbox)

VERDICT: REVISE
Static review of 7b231ab2..d406cfd4 on feat/tollgate. No files changed; no builds, tests, binaries, credential-file reads, or provider API calls performed. Findings below are source-confirmed; runtime behavior was not exercised.
## Blocking
1. Guest-mode shared runtimes can overwrite upstream’s plugin registry.
   Evidence: Shared runtime construction excludes settings and credentials but includes plugins/ (src/runtime.rs:4719). The fake-link watchdog unconditionally invokes bidirectional mirroring (src/runtime.rs:5658); its exclusions omit plugins (src/
   runtime.rs:5883), and newer runtime files are copied back into ~/.claude (src/runtime.rs:6185). A runtime-side registry change can therefore overwrite upstream-owned registry state. Real-link mode also exposes shared plugin state through
   materialized links (src/runtime.rs:4744).
   Fix: Give guest runtimes private plugin registries and exclude upstream-owned state from reverse synchronization. Cover both real-link and fake-link launches.
2. Monitoring credentials reach notification child processes.
   Evidence: notify-send is spawned with the daemon’s inherited environment and no credential scrub (src/usage/monitor/alert.rs:61). Ordinary monitor polling uses this notifier (src/usage/monitor/poll.rs:79). Exported management keys therefore enter
   another executable, violating the explicit child-environment prohibition (design:87 (docs/multi-provider-redesign-plan.md:87)).
   Fix: Apply the existing scrub_billing_env helper before spawning notifications (src/providers/billing_key.rs:172); audit other helper launches for the same omission.
## Important
3. An unrelated management wallet can mark a funded inference account exhausted.
   Evidence: A supplied billing key replaces the inference credential for /credits without proving common ownership (src/providers/openrouter.rs:292). Although marked bound: false, that wallet remains attached to the inference observation (src/
   providers/openrouter.rs:470). Its balance unconditionally determines funded (src/providers/openrouter.rs:703) and produces unavailable stats (src/providers/openrouter.rs:771), subsequently projected as QuotaExhausted (src/usage/project.rs:498).
   Fix: Publish unbound wallets separately and exclude them from inference-account availability decisions until ownership is established.
4. One stalled HTTP body can stop all monitor refreshes indefinitely.
   Evidence: The monitor agent sets connect and response-header timeouts, but no body or overall deadline (src/usage/monitor/source.rs:389); it then reads the body synchronously (src/usage/monitor/source.rs:424). Polling is sequential (src/usage/
   monitor/poll.rs:34), and IN_FLIGHT prevents further polling until that worker exits (src/usage/monitor/poll.rs:65). A server that sends headers and stalls its body can therefore freeze the entire monitor loop. The shared provider agent has the
   same timeout configuration (src/usage/fetch.rs:959).
   Fix: Configure finite overall and body deadlines on both agents.
5. The advertised read-only API invokes credential recovery and filesystem mutations.
   Evidence: Collection calls load_config (src/usage/collect.rs:126), which creates directories, recursively adjusts permissions, and loads profiles (src/profile.rs:3817). Profile loading invokes pending-credential recovery (src/profile.rs:3404);
   recovery writes credentials and deletes the pending sidecar (src/profile.rs:3807). Thus a GET can mutate authentication state despite the API’s cache-only contract (src/local_api/mod.rs:15).
   Fix: Use a genuinely read-only roster/cache loader for API collection and status fallback. Keep recovery and permission repair in explicit initialization or maintenance paths.
6. macOS default-Keychain rotation bypasses guest protection.
   Evidence: Rotation eligibility checks Keychain enablement and the saved active profile, without checking upstream ownership (src/oauth.rs:1489). An accepted ownership check proceeds to the global mirror (src/oauth.rs:1555), whose writer
   unconditionally updates the default service (src/keychain.rs:1424). A previously active tollgate profile remains eligible after upstream appears; matching or corrupt global state can still be overwritten.
   Fix: Gate default-Keychain mutations at their common writer with upstream_active(), while preserving explicitly owned per-session writes.
7. OpenRouter wallet rate limits lose their backoff.
   Evidence: /credits failures, including 429, become notes; retry_after is discarded and the fetch returns success (src/providers/openrouter.rs:309). Successful monitor refreshes clear the hold (src/usage/monitor/cache.rs:213), allowing retries
   before the provider’s requested delay.
   Fix: Retain successful key metrics while tracking wallet-specific backoff, or propagate a typed partial failure that preserves the reading and enforces Retry-After.
## Minor
8. Host validation is absent. The request model does not retain Host (src/daemon/api/http.rs:35); local routing checks bearer authentication but no authority (src/local_api/routes.rs:65). This is a DNS-rebinding defense gap, not a demonstrated
   authentication bypass: a browser still needs the bearer. Preserve and validate a single allowed loopback Host value.
9. Redaction misses embedded credentials. The sanitizer recognizes whole whitespace-delimited words (src/usage/observation.rs:707). A compact string such as {"token":"sk-abcdefghijklmnopqrstuv0123456789"} survives because internal punctuation defeats
   the token-alphabet check. Redact embedded credential substrings and add punctuation-delimited cases; no actual provider credential echo was established.
## Good
Loopback addresses are checked at parsing and again before binding; token generation uses getrandom, and comparison uses constant-time digest comparison (src/local_api/mod.rs:100, src/local_api/mod.rs:142, src/local_api/mod.rs:220, src/local_api/
  mod.rs:329).
Borrowed Hermes tokens are size-bounded and rejected when expired or undated (src/usage/monitor/nous.rs:132, src/usage/monitor/nous.rs:163).
Self-update is structurally inert: its gate returns false and its spawn function returns None (src/update.rs:24).

## Resolution

The two reviews overlap, so their findings were merged into one numbering: **B** blocking, **I** important, **M** minor. "Source" names the finding as numbered above: G is the Grok review, C the Codex review. Each fix sits on its own branch, merged into `feat/tollgate`; the tests named are in `tests/inline/`.

| Id | Source | Finding | Verdict | Fix commit | Tests |
|----|--------|---------|---------|------------|-------|
| B1 | G Blocking 1, C Blocking 1 | Guest start and resume link or mirror the operator's `plugins/`, `projects/` and `~/.codex` entries | Confirmed; fixed, with residual gaps (see below) | `848e3530` (tg/fix-guest-runtime) | `runtime.rs`: `a_guest_session_keeps_plugins_and_projects_off_the_operator_trees_under_real_links`, `a_guest_build_drops_operator_links_a_pre_guest_build_left`, `a_guest_fake_link_mirror_never_writes_into_the_operator_tree`, `a_guest_resume_seeds_its_transcript_privately_and_leaves_the_operator_store_alone`, `the_resume_seed_is_a_no_op_outside_guest_mode`, `a_guest_codex_home_copies_the_operator_entries_instead_of_linking_them` |
| B2 | G Blocking 2 | Guest mode runs Claude and Codex OAuth legs when upstream's locks are free; `login` is not gated | Confirmed; fixed | `4c836812` (tg/fix-guest-oauth) | `scheduler.rs`: `a_401_in_guest_mode_bails_to_cache_without_spending_the_chain`, `guest_mode_polls_on_the_held_token_instead_of_rotating_ahead_of_expiry`, `claude_rolling_tick_restamps_nothing_in_guest_mode`; `oauth.rs`: `auto_start_kick_does_not_rotate_in_guest_mode`, `guest_mode_adopts_nothing_from_the_live_file`, `refresh_result_sends_nothing_in_guest_mode`; `codex_auth.rs`: `standby_rotates_nothing_in_guest_mode`; `cli.rs`: `guest_login_refuses_exactly_the_subscription_login_flows`, `guest_login_refuses_a_setup_token_capture`, `guest_login_refuses_a_codex_capture`, `guest_login_still_captures_an_api_key_profile`; `actions.rs`: `codex_browser_preflight_refuses_in_guest_mode`. The TUI login gate has no test (it would open a browser) |
| B3 | C Blocking 2 | Monitoring keys reach `notify-send` and other helper processes | Confirmed; fixed on Linux-built spawns, then on the macOS and Windows spawns in Round 2 (C2) | `91a84ac0` (tg/fix-http-secrets), `c2a68257` | `providers_billing_key.rs`: `a_helper_spawn_inherits_no_monitoring_or_billing_key`, `the_notify_send_helper_is_scrubbed`, `the_browser_opener_is_scrubbed` |
| I1 | C Important 3 | An unrelated management-key wallet can mark a funded OpenRouter account exhausted | Confirmed; fixed | `91a84ac0` | `providers_openrouter.rs`: `an_unbound_management_wallet_never_drives_the_inference_account` |
| I2 | C Important 4 | A stalled HTTP body can freeze monitor refreshes | Confirmed; fixed | `91a84ac0` | `usage_keyed_http.rs`: `a_stalled_body_ends_at_the_deadline`, `the_shared_agent_has_a_finite_deadline_and_follows_no_redirect`; `fetch.rs`: `the_shared_usage_agent_has_a_finite_end_to_end_deadline` |
| I3 | C Important 5 | Local API GETs run credential recovery and filesystem repairs | Confirmed; fixed | `c140e7b6` (tg/fix-local-api) | `local_api.rs`: `every_get_leaves_the_home_byte_identical_even_with_a_pending_sidecar` |
| I4 | C Important 6 | macOS default-Keychain rotation ignores guest mode | Confirmed; fixed, macOS build unverified | `4c836812` | `identity.rs`: `guest_mode_refuses_only_the_default_keychain_item` (predicate, on Linux) |
| I5 | C Important 7 | OpenRouter `/credits` 429s lose their backoff | Confirmed; fixed, with the hold per process until Round 2 persisted it (C3) | `91a84ac0`, `c2a68257` | `providers_openrouter.rs`: `a_credits_429_holds_the_wallet_for_its_retry_after_and_keeps_the_key`, `a_credits_429_without_retry_after_holds_for_the_floor`, `the_wallet_only_leg_honours_the_hold` |
| I6 | G Important 1 | Guest runtime settings copies stay sync members of the operator file | Confirmed; fixed. The lexical-compare rename was only reachable through a symlinked runtime dir, and is moot now that guest mode syncs nothing | `848e3530` | `settings_sync.rs`: `guest_runtime_settings_are_never_members_with_the_operator_file`; `guest_mode.rs`: `guest_sync_keeps_runtime_copies_out_of_the_operator_member_set` |
| I7 | G Important 2 | OpenRouter and Ollama bearer GETs follow redirects and read uncapped bodies | Partly confirmed: no token leak (ureq 3.4.2 drops `Authorization` on redirect), but redirects were followed and bodies capped at 10 MiB only; fixed | `91a84ac0` | `usage_keyed_http.rs`: `a_redirect_is_refused_and_the_target_never_sees_the_key`, `an_oversize_body_is_refused`, `a_body_within_the_cap_reads_with_its_status_and_retry_after` |
| I8 | G Important 3 | `GET /v1/status` serves the feed raw | Partly confirmed: an unparseable feed was already rebuilt, not sent raw; a parseable one was sent raw with only `base_url` redacted; fixed | `c140e7b6` | `local_api_routes.rs`: `the_status_route_never_passes_the_feed_through_raw`, `the_status_redactor_leaves_a_clean_body_alone`, `the_status_route_redacts_profile_endpoints` (updated) |
| M1 | G Minor 1, C Minor 8 | The local API never checks `Host` | Confirmed; fixed | `c140e7b6` | `local_api.rs`: `tcp_refuses_a_non_loopback_host_even_with_the_token`, `the_unix_socket_takes_any_host_or_none`; `local_api_routes.rs`: `only_loopback_hosts_pass`, `tcp_checks_the_host_before_the_token` |
| M2 | C Minor 9 | Redaction misses credentials embedded in punctuation | Confirmed; fixed | `91a84ac0` | `usage_observation.rs`: `redaction_finds_credentials_embedded_in_punctuation` |
| M3 | G Minor 2 | The socket directory's mode is not tightened; bind-then-chmod window | Confirmed; fixed (directory checked and set to 0700 before bind; umask left alone because it is process-wide) | `c140e7b6` | `local_api.rs`: `a_loose_data_dir_is_tightened_before_the_socket_is_bound`, `a_symlinked_data_dir_is_refused_for_the_socket`, `a_data_dir_owned_by_another_user_is_refused` |
| M4 | G Minor 3 | `Amount::from_f64` may print scientific notation | Refuted: Rust's `Display` for `f64` never uses exponent notation (checked for `1e300`, `f64::MAX`, `5e-324`, `1e-300`, `1.5e-7` on rustc 1.98.1); no change | none | none |
| M5 | G Minor 4 | herdr scripts: `xargs ls` without `-0`; pane id interpolated into `sed` | Confirmed (`sed` `e` command injection reproduced); fixed | `c140e7b6` | `herdr.rs`: `a_focused_pane_id_that_is_not_a_pane_id_never_reaches_sed`, `a_neighbor_pane_id_that_is_not_a_pane_id_is_dropped`, `a_session_row_path_with_a_blank_and_a_quote_still_resolves` |

### Adversarial recheck

A second pass over the merged fixes found two B1 defects, both fixed in one follow-up commit on `feat/tollgate`:

- **The private `plugins/` copy still pointed at the operator's tree.** Claude Code records `installLocation` (`known_marketplaces.json`) and `installPath` (`installed_plugins.json`) as absolute paths, so a byte copy still sent a guest session's plugin loads and marketplace `git pull` into `~/.claude/plugins`. The copy's top-level JSON files are now repointed at the copy. Test: `runtime.rs`: `a_guest_plugin_copy_repoints_the_registrys_absolute_paths_at_itself`.
- **A guest `delegate` resume could not find its transcript.** The fix made a shared guest runtime's `projects/` the guest store, but only `tollgate resume` seeded the transcript there, so the MCP `delegate` resume got "No conversation found". It now seeds the same way. Test: `mcp_run.rs`: `a_guest_delegate_resume_seeds_the_transcript_into_the_guest_store`.

### Left partial

These are also listed under Known gaps in `CHANGELOG.md`. B3 and I5, which this list used to carry, were closed in Round 2 (below).

- **B1:** with real links, the read-mostly `~/.claude` entries (`CLAUDE.md`, `commands/`, `agents/`, `skills/`, `hooks/`, `output-styles/`, `keybindings.json`) still link into a guest session, writable by design. A passthrough `--continue` is not seeded into the guest store.
- **B2:** a credentials link into a tollgate store made before guest mode is left in place, because removing it would write upstream's tree.
- **I4:** `src/keychain.rs` builds only for macOS and has not been compiled since the change. The same holds for Round 2's C2 edits to the macOS- and Windows-only spawn sites; their command builders compile and are tested on Linux.
- **M5:** the session-row read relies on `grep -l` printing one name per line, so it is not strictly NUL-safe.

## Round 2

A second review round over the merged resolution found five defects, in the same notation: C is the Codex review, G the Grok review. Three of them (C1, G1, G2) are B1 residuals, and C2 and C3 are what B3 and I5 had left open. Each fix sits on its own branch, merged into `feat/tollgate`. The tests named are in `tests/inline/`.

| Id | Finding | Verdict | Fix commit | Tests |
|----|---------|---------|------------|-------|
| C1 | The guest `plugins/` copy's isolation failed open. The repoint ran only when a build made the copy, so a copy an earlier build left in a reused tree kept its operator paths, and a failed rewrite was only logged | Confirmed; fixed. Every guest build now repoints and checks the copy under both transports. A failed copy, a failed rewrite, or a registry that still names or resolves into `~/.claude/plugins` replaces the copy with an empty private `plugins/` and prints a warning. A `plugins` link that cannot be removed refuses the start | `8b86bd7d` (tg/fix2-guest-isolation) | `runtime.rs`: `a_guest_build_repoints_a_plugin_copy_an_earlier_build_left_unrepointed`, `a_guest_registry_the_rewrite_cannot_repoint_starts_the_session_with_no_plugins`, `a_guest_registry_that_still_names_the_operator_tree_after_the_rewrite_is_disabled`, `a_guest_plugin_copy_that_fails_part_way_starts_the_session_with_no_plugins` |
| C2 | The macOS- and Windows-only helpers (`/usr/bin/security`, `ps`, `tasklist`, `powershell`, and `taskkill`, which the Known gaps did not name) inherited monitoring and billing keys | Confirmed; fixed. A shared `helper_command` builder applies `scrub_helper_env` before the caller's own env. The 5 Keychain sites go through `platform::security_command`, and the probe and gateway sites go through per-helper builders that compile on every platform. macOS and Windows builds are unverified | `c2a68257` (tg/fix2-secrets-backoff) | `providers_billing_key.rs`: `the_shared_helper_constructor_scrubs_before_the_callers_env`, `the_keychain_cli_is_scrubbed`, `the_gateway_start_time_probes_are_scrubbed`; `daemon_probe.rs`: `the_platform_process_helpers_are_scrubbed` |
| C3 | The OpenRouter `/credits` 429 hold was per process, so a forced `tollgate monitor refresh` (or any other process) sent `/credits` again before `Retry-After` | Confirmed; fixed. The hold is also written to `~/.tollgate/holds/openrouter-credits-<sha256>.json` (0600 file in a 0700 dir, holding only `{version, until_ms}` and never the key, with the name a domain-separated hash). The write happens under the fetch's existing single-flight lock. A reader takes the later of the in-memory and on-disk deadlines, clamped. An expired file is removed, and a torn or foreign file counts as no hold. `/key` keeps refreshing during a hold | `c2a68257` | `providers_openrouter.rs`: `a_persisted_credits_hold_binds_a_second_process_until_it_expires`, `the_wallet_only_leg_honours_a_persisted_hold`, `a_persisted_hold_is_clamped_and_a_foreign_file_is_ignored`; `usage_monitor_cache.rs`: `a_forced_refresh_in_another_process_keeps_off_a_held_wallet` |
| G1 | With real links, a guest session still wrote Claude Code state (`history.jsonl`, `todos/`, `shell-snapshots/`, …) into `~/.claude` | Confirmed; fixed. Only the read-mostly entries (`CLAUDE.md`, `commands/`, `agents/`, `skills/`, `hooks/`, `output-styles/`, `keybindings.json`) still link. Every other entry, unknown names included, links into `~/.tollgate/guest-claude/<entry>`, and a link a pre-fix build left at `~/.claude` is repointed | `8b86bd7d` | `runtime.rs`: `guest_placement_links_only_read_mostly_content_and_defaults_to_private`, `a_guest_real_link_session_keeps_claude_code_state_in_the_guest_store`, `a_guest_build_repoints_state_links_a_pre_fix_build_left_at_the_operator_tree` |
| G2 | An isolated guest teardown rescued transcripts into `~/.claude/projects`, and `sessions` / `resume` did not list the guest store | Confirmed; fixed. The rescue lands in `~/.tollgate/guest-claude`. In guest mode, the session index, the by-id and `latest` lookups, the sessions API (`store: "guest"`), and the shared-run owner stamp and prune all read the guest store. A passthrough `tollgate start <p> -- --resume <id>` is seeded into it | `8b86bd7d` | `runtime.rs`: `a_guest_isolated_rescue_lands_in_the_guest_store_not_the_operators`; `sessions.rs`: `guest_mode_lists_and_resolves_the_guest_stores_transcripts`, `a_guest_shared_run_stamps_and_keeps_the_guest_stores_owners`; `start.rs`: `resume_id_from_args_reads_every_spelling_and_skips_the_picker`, `a_guest_passthrough_resume_seeds_the_transcript_into_the_guest_store` |

After both merges, clippy (`--all-targets --all-features --release -D warnings`) and fmt are clean on Linux x86_64, and nextest passes 4884 tests with 0 failed and 1 skipped (4864 before Round 2, plus 12 guest-isolation tests and 8 secrets-backoff tests). What remains open is under "Left partial" above: B1's read-mostly links and the unseeded `--continue`, B2, I4 together with C2's macOS and Windows sites, and M5.

### Round 2 recheck

An adversarial pass over the merged Round 2 fixes ran the release binary in a bwrap sandbox: network off, the real home masked by a fake one holding `~/.clauth` (guest on), a `~/.claude` tree with an absolute-path plugin registry, `history.jsonl`, `todos/`, `projects/` and an unknown state file, and a stand-in `claude` that writes all of them, rewrites `plugins/known_marketplaces.json`, and writes into the recorded `installLocation` and `installPath`. After a shared session, an isolated session with its teardown rescue, a `tollgate resume`, a passthrough `--resume`, a pre-fix runtime tree left linked at `~/.claude`, and an unparseable registry naming the operator's tree, `~/.claude`, `~/.claude.json`, `~/.clauth`, `~/.codex` and `~/.hermes` were byte-identical each time. With guest mode off, the Round 2 binary and the `5b54f202` binary gave the same runtime link layout and the same final home tree. The only difference was the rescue log line, which now names the destination path instead of saying "the global store".

The pass found one more fail-open, next to C1. A `projects` link a pre-guest build left at `~/.claude/projects` that could not be removed was only logged. The reuse check then kept that link, so the session wrote its transcripts into the operator's store. It now refuses the start, as a surviving `plugins` link or state link already did. Test: `runtime.rs`: `a_guest_projects_link_that_cannot_be_removed_refuses_the_start`.
