# `tollgate import clauth` — implementation spec (lane R3a–R3d)

Status: **draft for implementation**, 2026-09-29, `feat/tollgate` @ `c53123a0` (tollgate 0.1.0). Design
authority: `docs/multi-provider-redesign-plan.md` v3.1 §4.0 (migration table, M-1…M8, retire, rollback),
§5 rows R3a–R3d, D18, §10; `docs/tollgate-code-review-0.1.0.md` (guest-mode gates this lane must open).
Where this spec deviates from the plan it says so in §9. Running the import on the owner's machine is the
owner's decision and is not part of the build. No test and no build step may touch the real `$HOME`.

Machine facts re-read for this spec (metadata only, `ls`/`stat`): `~/.clauth` holds `profiles.toml`,
`profiles/{leadtone,personal,scifoo}/{account_id,profile_fetched,usage_cache}.json + config.toml +
credentials.json + usage_history.jsonl` (all nlink 1), `conversations/` (386 files), `session_profiles.json`,
`live_bare/`, `mcp_live/`, `completions/`, `.completions_installed`, `rotation-locks/`, `.lock`,
`usage-fetch.lock`, `status.json`, `status_cache.json`, `ai_pricelog_v4_price_cache.json`, **`clauth.log`**
(missing from the plan's M5 table; added in §3.4). No `clauthd.lock`, no `clauthd-standby.lock`, no
`codex-profiles.toml`. `~/.claude/.credentials.json` is **currently a symlink** to
`~/.clauth/profiles/personal/credentials.json` (plan §2 recorded a regular file on the same day: the slot
flips with every upstream TUI exit, so both cases are first-class). `~/.codex/auth.json` is a regular
file (nlink 1), an independent login. Upstream binary: `~/.cargo/bin/clauth` (cargo record
`clauth 0.16.0 (path+file:///home/abobreshov/Work/clauth)`). `settings.json` has no `apiKeyHelper`; the
registry has no `~/.clauth/profiles` path. Everything on one btrfs device (st_dev 57). Off-`PATH` upstream
builds also exist: `~/Work/clauth/target/{debug,release}/clauth` (regular, nlink 2), and a `cargo run` on the
`mommy` branch of the same repo runs 0.16.0 (§4.9 F1). herdr state beyond `config.toml`:
`~/.config/herdr/plugins.json`, `.plugins.lock` and `plugins/{config,github}/` (§4.8 G2).

## 1. Goal and non-goals

**Goal.** One command moves the owner's upstream clauth 0.16.0 accounts into `~/.tollgate` so that
tollgate becomes the only writer of every refresh chain, the live slots and the shared Claude Code
config, and guest mode ends (`identity::upstream_active()` false via `"state":"complete"`).
1. `tollgate import clauth --dry-run`: inventory of `~/.clauth` plus the live slots, each entry classified
   `move` / `copy` / `copy-0600` / `merge` / `skip` / `never` / `refuse`, conflicts, blockers, the global edits
   and the journal it would write; text or JSON; changes nothing, creates no file (not even a lock file).
2. `tollgate import clauth`: the writer-exclusive transaction M-1…M8 (§4), journaled write-ahead in
   `~/.tollgate/import-journal.json`, crash-resumable, automatically reversed on any refusal before commit.
3. `tollgate import rollback`: reverse replay with the reinstall-order rule (stores before the binary).
4. The global edits G1–G4 (upstream plugin off, upstream herdr uninstall, helper rewrite, registry remap),
   each journaled and reversible, all behind one confirmation (`--yes` non-interactive); and the post-commit
   retire checklist `tollgate import retire` (R1–R4).

**Non-goals.** macOS and Windows (refused, §9 I1); importing into a non-empty same-name profile (refused or
`--rename`); any network call (no identity probe, no plugin fetch inside the hold); running `cargo install`
or `cargo uninstall`; deleting `~/.clauth` or the retired binary; the D18 start-time reconcile rule (R2, not
this lane); executor B (lane 2) and `Harness::Hermes` (lane 3) — only their hooks are named (§6).

## 2. Surface

### 2.1 Commands (new `Command::Import { cmd: ImportCommand }` in `src/cli.rs`)
| Command | Flags | Effect |
|---|---|---|
| `tollgate import clauth --dry-run` | `--json`, `--rename OLD=NEW` (repeatable), `--adopt-live` | M-1 checks + read-only lock probes; prints the report; exit 0 or 3 |
| `tollgate import clauth` | `--yes`/`-y`, `--json`, `--rename OLD=NEW`, `--adopt-live`, `--resume` | the transaction; `--resume` continues an interrupted journal forward |
| `tollgate import rollback` | `--yes`/`-y`, `--json`, `--adopt-live` | reverse replay of `retire`, `main`, then `pre` sections |
| `tollgate import status` | `--json` | journal state, step counts, next action; never takes a lock |
| `tollgate import retire` | `--yes`/`-y`, `--step r1\|r2\|r3\|r4` (repeatable; default all pending) | post-commit checklist (§4.11) |

`--adopt-live` authorises capturing a **diverged** regular-file live slot (§4.6). `--rename` maps an upstream
profile name to a free tollgate name (§4.7). `--json` prints one JSON document on stdout (schema §2.4) and
nothing else there; progress lines go to stderr. No `import` path exists in the TUI, MCP or local API (§2.5).

### 2.2 Exit codes (downcasts in `main::exit_code`, through the named `ExitCode` table)
The numbers are per command. The shared prep PR (hot-swap spec §8) replaces the literals in
`main::exit_code` with a named table, so `ImportBlocked = 3` here and `SwitchNotServed = 3` for
`tollgate switch --wait` coexist, each documented on its command's wiki page.
| Code | Meaning | Error type |
|---|---|---|
| 0 | dry-run clean; import committed; rollback finished; status printed | — |
| 1 | runtime failure **after automatic reversal completed** (journal `aborted` or `rolled_back`); this includes every reversal after M5 started (a process appearing mid-M5, `EXDEV`) | `anyhow` |
| 2 | usage: clap error; non-TTY stdin without `--yes`; `--resume` with no interrupted journal | `UsageError` |
| 3 | blocked: a refusal at M-1, the post-confirmation recheck, M3 or M4; nothing changed (or only `pre` was undone) | `ImportBlocked{blockers}` |
| 4 | attention: an interrupted journal exists, the journal disagrees with disk, or automatic reversal stopped part-way | `ImportNeedsAttention{state, step}` |

### 2.3 Messages (exact; `<…>` substituted, `~` = the home as `~`)
- Dry-run header: `tollgate import clauth --dry-run: nothing was changed`
- Blocker line: `  blocked  <code>: <sentence>`; warning line: `  warning  <code>: <sentence>`.
- `process_alive`: `<name> (pid <pid>) is running; close every Claude Code session and every clauth process first`
- `tollgate_process_alive`: `another tollgate process (pid <pid>, <role>) is running; stop it first`
- `pending_rotation`: `~/.clauth/profiles/<p>/<file> is a crashed rotation's staged chain; run 'clauth list' once so upstream adopts it, then retry`
- `unknown_entry`: `~/.clauth/<path> is not in the import inventory; it may carry a credential, so nothing is imported`
- `destination_exists` / `name_collision`: `a tollgate <harness> profile named '<p>' already exists; pass --rename <p>=<new>` (`<harness>` = `claude`, `codex` or `hermes`: whichever roster holds it, or `claude` for a bare `profiles/<p>` dir)
- `tollgate_live_session`: `a tollgate session (<sid>, profile '<p>') is live; exit it first (Hermes sessions included)`
- `lock_held`: `<path> is held by another process; stop it first`
- `dev_build_exe`: `this tollgate is <exe>, not the installed binary on PATH; run the installed tollgate so the rewritten helper points at it`
- `upstream_offpath_build` (warning): `upstream clauth build <path> is not on PATH and is not retired; do not run it (or 'cargo run' on the mommy branch) until R2's start-time reconcile ships`
- `cross_device`: `<src> and <dst> are on different filesystems; a credential is never copied`
- `claude_live_diverged`: `~/.claude/.credentials.json holds a login that differs from profile '<p>'; pass --adopt-live to import it as '<p>' (the stored chain is kept in its quarantine/)`
- `live_is_other_profile`: the diverged slot holds another stored profile's login (refresh or access token); refused even under `--adopt-live` (review lens credentials #1)
- `live_older_than_store`: under `--adopt-live`, the slot's `expiresAt` is older than the store's it would supersede
- `upstream_binary_is_tollgate`: a `clauth` on `PATH` resolves to this tollgate (by path or inode) or to a file not named `clauth`; F1 never retires it (review lens credentials #2)
- `codex_second_carrier`: `~/.codex/auth.json is a copy of profile '<p>''s chain; relink it with 'clauth' first`
- Confirmation: `import <n> claude and <m> codex profiles and make the <k> global edits above? [y/N]`
- Non-TTY: `tollgate import clauth: refusing to change files without --yes on a non-interactive stdin` (exit 2)
- Commit: `tollgate: imported <n> claude and <m> codex profiles from ~/.clauth; guest mode is off. Next: tollgate import retire`
- Interrupted (stderr, every command while journal state is `pre`/`in_progress`/`rolling_back`):
  `tollgate: an import of clauth was interrupted at step <seq>; run 'tollgate import clauth --resume' or 'tollgate import rollback'`
- `identity::GUEST_REFUSAL` tail becomes `…, or run 'tollgate import clauth --dry-run' to import clauth.`

### 2.4 JSON report (`--json`, schema_version 1; same shape for dry-run, run and rollback)
```json
{"schema_version":1,"command":"import clauth","mode":"dry_run|run|rollback","generated_at":"<RFC3339>",
 "source":"/h/.clauth","target":"/h/.tollgate","ok":false,
 "blockers":[{"code":"process_alive","pid":4242,"path":null,"message":"…"}],"warnings":[…],
 "entries":[{"src":"profiles/personal/credentials.json","dst":"profiles/personal/credentials.json",
             "action":"move","kind":"file","secret":true,"carrier":true,"reason":"claude store"}],
 "live_slots":{"claude":{"state":"symlink|regular|missing","profile":"personal","verdict":"relink|capture|untouched|refuse"},
               "codex":{"state":"regular","profile":null,"verdict":"untouched"}},
 "roster":{"claude":["leadtone","personal","scifoo"],"codex":[],"active":"personal","renames":{}},
 "global_edits":[{"id":"G1","file":"~/.claude/settings.json","change":"enabledPlugins.clauth@clauth true -> false"}],
 "locks":[{"path":"~/.clauth/usage-fetch.lock","state":"free|held|absent"}],
 "processes":[{"pid":4242,"name":"claude","role":"claude"}],
 "journal":{"state":"none","steps_planned":37}}
```
Paths are relative to `source`/`target` unless absolute outside them. No value from a credential file,
`config.toml`, `settings.json` `env`, or process environment ever appears (test `import_report_never_echoes_a_fixture_secret`).

### 2.5 TUI, MCP, local API, herdr
- **TUI**: no import action (the TUI holds `~/.tollgate/usage-fetch.lock`, which the fence refuses). The
  `[ guest ]` pill tooltip/footer line adds `import: tollgate import clauth --dry-run` (part 2).
- **MCP**: no tool. `tollgate mcp` processes are children of Claude Code sessions, which already block.
- **Local API** (read-only, additive): `GET /v1/health` and `GET /v1/status` gain
  `"import":{"state":"none|pre|in_progress|complete|rolling_back|rolled_back|aborted","completed_at":<RFC3339|null>}`;
  OpenAPI updated; no write route. **herdr**: nothing new; G2 and R3 reuse existing commands.

## 3. Data and files

### 3.1 Paths
| Path | Mode | Writer | Content |
|---|---|---|---|
| `~/.tollgate/import-journal.json` | 0600 | import, rollback, retire | journal (§3.2); top-level `state` is the guest-mode contract |
| `~/.tollgate/import-journal.<unix_ms>.json` | 0600 | a new import after `aborted`/`rolled_back` | the archived previous journal |
| `~/.tollgate/import-backup/` | 0700 | import | non-secret byte backups only: `<seq>-profiles.toml`, `<seq>-codex-profiles.toml`, `<seq>-herdr-config.toml` |
| `~/.clauth/MIGRATED` | 0600 | M8 | tombstone text: `migrated to tollgate <version> at <RFC3339>; data in ~/.tollgate; undo: tollgate import rollback` |
| `<bindir>/clauth-0.16.0.retired` | original | M5.0 | the renamed upstream binary (same inode) |
| `<bindir>/clauth` | 0755 | M5.0 | shim: `#!/bin/sh` + `echo "clauth: migrated to tollgate (~/.tollgate); run tollgate, or 'tollgate import rollback'" >&2; exit 1` |
| `~/.clauth/{clauthd,clauthd-standby,usage-fetch}.lock`, `~/.clauth/rotation-locks/<p>.lock` | 0600 | fence (real run only) | created empty when absent so they can be held; left behind (inert) |

### 3.2 Journal schema (version 1)
```json
{"schema_version":1,"state":"pre|in_progress|complete|rolling_back|rolled_back|aborted",
 "tool_version":"0.2.0","started_at":"…","updated_at":"…","completed_at":null,"uid":1000,
 "source":"/h/.clauth","target":"/h/.tollgate","source_dev":57,
 "options":{"renames":{"old":"new"},"adopt_live":false},
 "upstream_bins":[{"path":"/h/.cargo/bin/clauth","ino":1,"size":1,"sha256":"…"}],
 "pre":[entry…],"main":[entry…],"retire":[entry…],"rollback_from":null}
```
Entry: `{"seq":N,"op":<op>,"src":…,"dst":…,"secret":bool,"prior":{…},"after":{…},"status":"planned|done|undone|skipped"}`.
Ops: `mkdir`, `move` (prior `{ino,dev,mode,nlink:1}`), `move_relink` (store move + live symlink repoint, prior
adds `{link,link_target}` or `{live_regular:{ino,sha256}}`; `after` holds `{temp}`, the exact
`~/.claude/.credentials.json.tollgate-import.<pid>` path, so replay removes a leftover temp after a crash), `capture` (live regular file renamed onto a store; prior `{live_ino,store_ino}`),
`move_relink` and `capture` also record `after.quarantine_dir_created` when they park a login, so undo removes only a directory created by that op;
`copy` / `copy_secret` (after `{size}`; `copy` also `sha256`), `copy_tree` (after `{created:[names]}`),
`merge_roster`, `merge_json` (after `{added_keys}`), `rewrite_json` (`{file, pointer, prior_value, new_value}`;
never for secret-bearing keys), `rewrite_toml` (`{file, key, prior_value, backup}`), `retire_bin`
(`{bin, retired, bin_sha256, shim_sha256}`), `write` (tombstone), `exec` (`{argv, env_keys, config_sha_before,
config_sha_after}`). **No entry, backup or report ever holds a credential byte**; secret copies record size only.

Durability: every journal write is `write_durable_600` = create `.<name>.tmp.<pid>.<seq>` 0600, write, `fsync`,
`rename`, `fsync(dir)`. Step protocol: append entry `planned` → durable write → perform op → `fsync` of every
directory the op touched (src parent, dst parent, live-slot dir) → set `done` → durable write. `state` flips:
`pre` at M0, `in_progress` at M4, `complete` as the very last durable write of M8, `rolling_back` at rollback
start, `rolled_back`/`aborted` at its end. `identity::import_completed` (`identity.rs:153-167`) stays the only
reader that decides guest mode; a new `identity::import_state()` returns the enum for status/API/warnings.

### 3.3 Name mapping
`dst_name(p) = options.renames.get(p).unwrap_or(p)`. It applies to `~/.tollgate/profiles/<dst_name>`, both
rosters, `active_profile`, `fallback_chain`, `auth_broken`, `session_profiles.json` `known` values, and the G3
helper token. Rollback applies the inverse map.

### 3.4 Inventory classification (`import::inventory`, exhaustive; anything unlisted is `refuse unknown_entry`)
| Upstream path (`~/.clauth/…`) | Action | Notes |
|---|---|---|
| `profiles/<p>/credentials.json`, `session-token.json`, `session-token.static.json`, `mcp-logins.json`, `quarantine/` | **move** | chain carriers (`mcpOAuth` too); regular file/dir, owner = uid, nlink 1, not a symlink |
| `profiles/<p>/auth.json`, `auth.lkg.json`, `auth.quarantine.json` | **move** | codex carriers; single-inode rule §4.5 |
| `profiles/<p>/*.pending`, `profiles/<p>/.*.tmp.*`, `~/.clauth/.*.tmp.*` | refuse | `pending_rotation` / `stray_temp` (either may hold the newest chain) |
| `profiles/<p>/config.toml` | copy-0600 | secret (api key); journaled `copy_secret` |
| `profiles/<p>/{account_id,profile_fetched,usage_cache,third_party_cache,third_party_auth,throughput_cache,touch-receipt}.json`, `usage_history.jsonl`, `wallet_history.jsonl` | copy | 0600 at dst |
| `profiles/<p>/{adopt_refusal,kick_block}.json`, `auth.attempt` | skip | standing state re-derived by the scheduler |
| `profiles/<p>/codex-home/` | copy_tree | refuse `codex_home_carrier` if an `auth.json` exists at any depth |
| `profiles/<p>/runtime*`, `sessions*`, `codex-home-*` | skip | held marker → `session_marker_held`; a regular-file `.credentials.json`/`auth.json` inside → `stale_runtime_carrier` |
| `profiles.toml`, `codex-profiles.toml` | merge_roster | §4.7; then fence F2 edits the upstream file |
| `conversations/` | copy_tree | dst wins on an existing name |
| `session_profiles.json` | merge_json | union; same id, different owner → `contested` |
| `token_ledger.json`, `presets/` | copy / copy_tree | skip + warning if dst exists |
| `gateway.toml`, `shunt.toml`, `shunt.yaml`, `shunt.yml`, `shunt/` | copy-0600 | `destination_exists` if dst present |
| `status.json`, `status_cache.json`, `*price_cache*.json`, `throughput_cache.json`, `clauth.log`, `daemon.log`, `clauthd.pid`, `gateway-child.json` | skip | regenerated / logs |
| `completions/`, `.completions_installed` | skip | tollgate writes its own |
| `live_bare/`, `mcp_live/`, `live_sessions/`, `jobs/` | skip | held marker or live-pid row → blocker (§4.3) |
| `devices.json`, `pairing.json`, `auth_token.json`, `tls.json`, `gateway-admin-token` | skip | daemon-API secrets; tollgate pairs its own devices |
| `keychain-item-owners.json`, `keychain-deletes-in-flight.json` | skip | macOS-only state (import is Linux-only) |
| `rotation-locks/`, `.lock`, `clauthd.lock`, `clauthd-standby.lock`, `usage-fetch.lock` | never | held by the fence |
| `MIGRATED` | refuse `already_migrated` | unless the journal is `in_progress` (resume) |

Live slots are classified separately (§4.6). The inventory also refuses: an entry not owned by the uid
(`foreign_owner`), a symlink where a file/dir is expected (`symlinked_source`), a carrier with nlink > 1
(`hardlinked_carrier`), and a `src` whose parent `st_dev` differs from the dst parent's (`cross_device`).

## 4. Algorithms

### 4.1 Phases
Phase names follow the plan (§4.0): **M1** = the stop-the-world scan (processes and markers, §4.3);
**M2** = the static refusals (inventory §3.4, rosters and collisions §4.7, live slots §4.6, `dev_build_exe`).
**M-1 precheck** (no lock, no write): platform is Linux else `unsupported_platform`; `~/.clauth` exists else
`upstream_absent`; journal state `complete` → `already_imported`, `pre|in_progress|rolling_back` →
`journal_pending` (exit 4 unless `--resume`); M2; M1; the off-`PATH` build scan (§4.9, warnings); lock probes
(every fence item 1–8 file that exists: open read-only, `try_lock_shared`, release; absent = free). **A held
fence file is a blocker** (`lock_held`) here, so M3 finding one busy is exceptional.
Dry-run stops here and prints. Real run: print the same report, then confirm (§2.3) unless `--yes`.
**Post-confirmation recheck** (still no lock, no write): the prompt is unbounded, so M1 and the lock probes run
again immediately after the answer. A new blocker → exit 3, nothing changed.
**M0 pre-phase** (before any lock, part 2): archive a terminal journal, write journal `state:"pre"`; G2 (the only
`pre` edit). Any later refusal before M5 reverses `pre` and writes `aborted`.
**M3 fence**: acquire §4.2 items 1–9; a busy item → release all → blocker `lock_held` (exit 3 after `pre` undo).
**M4 revalidate + journal**: repeat M-1 checks under the fence (inventory hash must equal M-1's, else
`inventory_changed`); plan every `main` entry; `state:"in_progress"`; durable write.
**M5 moves** in this order: M5.0 `retire_bin` for every upstream binary (F1, §4.9); M5.1 per profile in name
order: `mkdir ~/.tollgate/profiles/<dst>`, carriers (`move`/`move_relink`), then `copy`/`copy_secret`/`copy_tree`;
M5.2 top-level copies and merges, then `copy_tree ~/.tollgate/guest-claude/projects → ~/.claude/projects`
(destination wins on an existing name; `after.created` journaled; rollback deletes only `created`; §6);
M5.3 `merge_roster` (tollgate's roster files written with raw `toml_edit` + `write_durable_600`, never through
`Config`/`ProfileTtl`-ranked helpers, §4.2). Before **each** carrier step: process rescan (§4.3); a new
blocking match → abort and reverse (exit 1, `aborted`); an exempt read-only tollgate subcommand is a warning.
**M6 live slots** (§4.6; codex (i) is folded into M5.1's `move_relink`). **M7 rewrites** (part 2): G1, G3, G4,
all pure JSON edits inside the fence (upstream takes `~/.clauth/.lock`, fence item 8, before it writes
`settings.json`, so no upstream writer can lose-update them).
**M8 commit**: F2 upstream roster edit; tombstone `write`; final process rescan; assert
`install_source_path(dst)` names the same file as upstream's did for every claude profile; journal
`state:"complete"`, `completed_at`; release the fence in reverse (9 → 1). Nothing inside M3–M8 prompts, spawns a
process or touches the network; target hold < 5 s plus the non-carrier tree copies (`conversations/`, the guest
store), whose file and byte counts the dry-run reports so the owner sees the hold cost up front.

### 4.2 Lock order (fence; `src/import/fence.rs`)
New rank in `src/lockorder.rs` `ranks!` (between `ApiSwitch = 60` at `:113` and `Rotation = 100` at `:117`):
`ImportFence = 80`. `Fence::acquire` enters it once (one `RankGuard`), then takes raw `File` flocks, each
**non-blocking** (`try_lock`, exclusive), in exactly this order:
1. `~/.clauth/clauthd.lock` · 2. `~/.clauth/clauthd-standby.lock` (a parked upstream standby holds it → refuse;
this subsumes an upstream `standby_waiting`) · 3. `~/.clauth/usage-fetch.lock` · 4. `~/.tollgate/tollgated.lock`
· 5. `~/.tollgate/tollgated-standby.lock` · 6. `~/.tollgate/usage-fetch.lock` · 7. `~/.clauth/rotation-locks/<p>.lock`
for every upstream claude and codex profile, sorted by name bytes (upstream `RotationGuard` files; never
tollgate's `RotationGuard`, which would re-enter rank 100 once per profile and violate strict ordering) ·
8. `~/.clauth/.lock` (upstream state; `lock::lock_file_with_timeout(file, 2 s)`) · 9. tollgate state via
`lock::StateLock::acquire_with_timeout(2 s)` → rank `State = 500` (`lockorder.rs:191`).
This matches upstream's own nesting (daemon/fetch leases held for life, `RotationGuard` outermost and
held across HTTP, rotation outer to state: `mommy:src/lockorder.rs:8-23,117`; tollgate's copy of the rule is
the `RotationGuard` doc, `runtime.rs:2205-2220`). Upstream state-lock waiters time out after 25 s
(`lock.rs:47`); none may exist because M4 refused every upstream process. After fence item 9 the code
enters **no rank below `State` (500)**: roster writes use raw `toml_edit` + `write_durable_600`, never a
`Config` (400) or `ProfileTtl` (450) helper, and the only lock re-entered is the re-entrant
`with_state_lock`; a debug build asserts this, and that no `Rotation` rank is entered (test 21). Rollback
and resume take the same fence.
While the fence holds item 1 or 3, `probe::upstream_refresher_active()` is true, so any tollgate refresher that
slips in stands down (`probe.rs:797-812`).

### 4.3 Process revalidation (`src/import/procs.rs`)
Linux `/proc/<pid>` scan of the uid's processes, excluding self: read `cmdline` and `exe` link only (never
`environ`). Role by argv[0] basename, or argv[1] basename/path when argv[0] is `node|bun|deno`:
`claude` or a path containing `/claude-code/` → `claude`; `codex` or `/@openai/codex/` → `codex`; `clauth` or
`clauth-*.retired`, or `exe` = an upstream binary → `clauth` (covers `clauth mcp`, TUI, daemon); `exe` =
`current_exe()` or basename `tollgate` → `tollgate`. Blocks: every `claude` and `clauth`; `codex` only when a
codex carrier or codex slot case (i)/(ii) is in scope (else warning); every other `tollgate`, **including a
`tollgate start <hermes-profile>` supervisor** (Hermes sessions block the import; the Hermes spec §6.2 says
so), the daemon, the TUI, `api serve` and `mcp`.
**Exempt short-lived tollgate runs.** A `tollgate` process whose argv[1..] is one of the read-only subcommands
`herdr tag` (run by `report-profile.sh:294` on every pane refresh), `usage` (incl. `--waybar`), `__complete`,
`which`, `status`, `list` or `import status` is a **warning**, never a blocker. It takes no tollgate
state or rotation lock the fence does not already hold, so it can only block on the fence and exit. At M1 and
in each per-carrier rescan such a match is logged and the scan repeats (up to 3 × 200 ms), and it never aborts.
Marker checks, upstream: each file in `~/.clauth/{live_bare,mcp_live}/`, `~/.clauth/profiles/<p>/sessions*/`
probed with `try_lock_shared` (held → `session_marker_held`); `~/.clauth/live_sessions/` row with a live pid →
`live_session_row`, stale → warning. Marker checks, tollgate: a `~/.tollgate/live_sessions/<sid>.json` row whose
pid is live, or a held `~/.tollgate/profiles/*/sessions-*/<sid>` marker (claude, codex and hermes alike) →
`tollgate_live_session`; a stale row → warning. The scan reads `cmdline` and `exe` only, never `environ`.
Test seam: `procs::testing::with_table(Vec<FakeProc>)` replaces the scan.

### 4.4 Move engine (`src/import/fsops.rs`)
`move(src,dst)`: verify `symlink_metadata(src)` = journaled `{ino,dev}`, `nlink == 1`, dst absent, `st_dev(src
parent) == st_dev(dst parent)`; `rename(2)`; on `EXDEV` (or any error) abort the step as not done — **never** a
copy fallback. `chmod 0600` (dirs 0700) after the move; prior mode journaled. `copy`: read src, write dst via
`write_durable_600`; dirs recurse without following symlinks (a symlink inside a copied tree is skipped + warned).
Test seam `fsops::testing::force_exdev(path)`.

### 4.5 Credential sets and the single-inode rule
Per claude profile: move `credentials.json`, `session-token.json`, `session-token.static.json`, `mcp-logins.json`,
`quarantine/` (whole dir, one rename) as present; before M5 record upstream's install source (long-lived
`session-token.json` else `credentials.json`, the `claude.rs:890-898` rule evaluated on the upstream dir via a new
path-parameterised `claude::install_source_in(dir)`), and at M8 assert tollgate's `install_source_path(dst)` has
the same file name. Per codex profile: move `auth.json`, `auth.lkg.json`, `auth.quarantine.json`. Invariant
after each step, asserted by tests: every chain has exactly one inode in the whole home (nlink 1, no copy in
`codex-home/`, none in a skipped runtime tree, live slots are symlinks to it or independent logins).

### 4.6 Live slots (`src/import/slots.rs`)
Claude `~/.claude/.credentials.json`, classified at M-1 and again at M4:
- **Symlink** into `~/.clauth/profiles/<p>/<f>` where `<f>` is a carrier the table moves → `move_relink`: rename the
  store, then create `~/.claude/.credentials.json.tollgate-import.<pid>` → `~/.tollgate/profiles/<dst>/<f>` and
  rename it over the slot, fsync `~/.claude`; one journal entry, whose `after.temp` names the temp path (written
  `planned` before the temp is created, so crash replay in either direction removes a leftover temp). Target profile = the link's profile (warning when
  it differs from upstream `active_profile`). A link to anything else → `live_link_foreign`.
- **Regular file**, upstream active `<a>`: compare the access token with `<a>`'s install source (the
  `classify_link_at` rule, `claude.rs:943-976`, called on upstream paths). **Same**, and
  `install_source_in(<a>'s dir)` is `credentials.json` → `capture`: after `<a>`'s store moved, rename the live
  file onto `~/.tollgate/profiles/<dst>/credentials.json` (the live inode becomes the store; its newer
  `mcpOAuth` survives; the superseded store inode is unlinked by the rename), then symlink the slot as above.
  **Same**, and the install source is `session-token.json` → **no capture**: CC's live file is
  `.credentials.json`-shaped (it carries a refresh token), so renaming it onto the sidecar would change
  `sidecar_kind_of` (`claude.rs:226-240`: a refresh token classifies as `Misfilled`) and with it the install
  source, and M8's install-source assert would fail. Instead verify that the live access token equals the
  sidecar's static token, then repoint the slot at the moved store and keep the live copy in the profile's
  `quarantine/credentials.json.live` (its `mcpOAuth` and any refresh token exist nowhere else). This is journaled
  as `move_relink` with `prior.live_regular = {ino, sha256}` and `after.quarantine`; the live file is renamed to
  the entry's temp path first and into the quarantine last, and a revert renames it back to the slot. A revert
  that finds the slot still the live inode (the store moved, the slot not yet) leaves the slot alone. **Diverged** → blocker `claude_live_diverged` unless `--adopt-live`, which runs
  `capture` on a `credentials.json` source, parking the superseded store in
  `quarantine/credentials.json.superseded` (`after.quarantine`) instead of unlinking it. A revert swaps it back
  only while the captured inode is still at the destination; after a refresh, the current inode returns to the
  upstream store and the superseded inode stays quarantined. The journal records whether the capture created
  `quarantine/`, so undo leaves an upstream carrier directory in place. A slot holding another stored profile's
  chain refuses
  `live_is_other_profile`, and one older than the store refuses `live_older_than_store`, `--adopt-live` or not; on a `session-token.json` source it refuses
  `live_diverged_on_static_token` (tollgate cannot tell which one the owner wants).
  Unparseable → `live_unclassifiable`. Needs `st_dev(~/.claude) == st_dev(~/.tollgate)` else `cross_device`.
- **Regular file, no upstream active**: its refresh token equals some upstream store's → treat as Same for that
  profile (a detached duplicate carrier); otherwise untouched (independent login). **Missing** → untouched.
Codex `~/.codex/auth.json`: (i) symlink into `~/.clauth/profiles/<p>/auth.json` → `move_relink` under `<p>`'s
upstream rotation lock (fence item 7); (ii) regular file whose refresh token equals a codex store's →
`codex_second_carrier`; (iii) otherwise untouched (this machine). The slot's `auth.json` is never copied.

### 4.7 Rosters, names, merges (`src/import/roster.rs`)
Collision: for every `dst_name(p)`, call `actions::validate_profile_name(dst, harness, None)` (`actions.rs:87-112`;
`harness` = Claude for a claude profile, Codex for a codex one) **and** check `profile_dir(dst).exists()`. That
one function owns uniqueness across every roster, case-insensitively: when the Hermes lane generalises
`validate_foreign_harness_free` over `Harness::ALL`, the import sees the Hermes roster with no code of its own.
There is no import-private roster reader. A refusal becomes `name_collision`, naming the harness whose roster holds
the name. `--rename OLD=NEW`: OLD must be an upstream name, NEW passes the same two checks. Neither check takes a
lock (`claude_roster_names` and `CodexState::load` are plain file reads), so they run at M-1 and again at M4
under the fence. Merge (toml_edit on tollgate's
file, under the fork state lock): `profiles` = tollgate's then upstream's (mapped) in upstream order;
`fallback_chain` = tollgate's then upstream's missing members; `auth_broken` ∪ mapped upstream entries;
`active_profile` = the claude slot's profile from §4.6 if any, else upstream's (tollgate has none in guest mode;
if it has one, it is kept and a warning printed); every other top-level key and table: tollgate's value when its
file sets it, else upstream's, except `[serve]`, `[update]` and `[local_api]`, which are never imported; `home_tab`
passes the D4 alias hook `profile::home_tab_alias` (identity today). `codex-profiles.toml` the same way.

### 4.8 Global edits (part 2; all `rewrite_json` with prior values; confirmation covers all)
- **G1** (M7, inside the fence) `~/.claude/settings.json` `enabledPlugins["clauth@clauth"]` → `false` (only if
  present and true). It is a pure JSON read-modify-write. Upstream writes `settings.json` only under
  `~/.clauth/.lock` (`mommy:src/claude.rs:2196-2201`), which the fence holds (item 8), and M1 has refused
  every CC session. So neither a lost update nor a hot-reload mid-edit can happen.
- **G2** (M0) back up `~/.config/herdr/config.toml` (herdr's resolved path, `herdr::config_path`). Journal the
  upstream plugin record from `herdr plugin list --json` (the parser at `herdr.rs:673-760`): `source.kind`,
  `owner`, `repo`, `resolved_commit`, `managed_path` and `enabled` only. Then run the upstream binary
  `<bin> herdr uninstall --yes` with `helper_command` env scrub plus `CLAUTH_NO_UPDATE=1`,
  `CLAUTH_NO_COMPLETIONS=1`, `CLAUTH_NO_API=1`, stdin null, 20 s deadline; record config sha before/after. No
  herdr or no binary → entry `skipped` + warning. Non-zero exit → refusal (pre undone). Uninstall needs no
  network; reinstall does, so undo is split (§4.10 step 4).
- **G3** (M7) `settings.json` `apiKeyHelper` of shape `<exe> __api-key <p>` with exe basename `clauth` → rebuilt by
  `claude::build_api_key_helper_command(current_exe, dst_name(p))`. M2 refuses `dev_build_exe` unless
  `platform::installed_exe_path(current_exe())` canonicalises to the same file as the first `tollgate` on `PATH`.
  Otherwise the global helper would point at a `target/debug` build that `cargo run` produced. (The integration
  tests put the built binary's dir first on their stub `PATH`, so they pass the same rule.); each `permissions.allow` entry starting
  `mcp__plugin_clauth_clauth__` → `mcp__plugin_tollgate_tollgate__` + suffix.
- **G4** (M7) `~/.claude/plugins/installed_plugins.json` via `agentgear::repoint_install_paths`: a value under
  `~/.clauth/profiles/` → (a) the same suffix under `~/.tollgate/profiles/` when that path exists (copied content),
  else (b) its `~/.claude/plugins/<suffix>` twin when it exists (the `plugin_host::registry_remap` rule,
  `plugin_host.rs:146-190`, generalised over the root and without its "still resolves" keep), else (c) kept +
  warning. Rollback applies the recorded `{from,to}` pairs backwards.

### 4.9 Fence against re-adoption
Upstream 0.16.0's TUI exit detaches any symlinked slot into a regular file (`mommy:src/claude.rs:2637-2655`, no
foreign-link guard) and its `snapshot_active_credentials` adopts a regular file into an absent store
(`is_first_login`). Three guards: **F1** (M5.0) each user-owned upstream binary on `PATH` (resolved with `which`,
canonicalised, regular, in a uid-owned dir) is renamed to `clauth-0.16.0.retired` in its own dir and replaced by
the shim; a root-owned or unwritable one → blocker `upstream_binary_unretirable`; none found → warning. **F2**
(M8) `active_profile` removed from `~/.clauth/profiles.toml` and `codex-profiles.toml` (toml_edit, byte backup), so
any other upstream build finds nothing to snapshot or adopt into. **F3** tombstone. The binary is never
executed after F1 by the import; G2 runs before F1.
**Off-`PATH` builds** (M-1, warning `upstream_offpath_build`, metadata only: no exec, no read of contents). List
every uid-owned regular executable named `clauth` under `<src>/target/*/`, where `<src>` is the path source of
the cargo install record (`~/.cargo/.crates2.json`, `path+file://…`). On this machine that is
`target/{debug,release}/clauth`. Also print that a `cargo run` on the `mommy` branch of `<src>` runs 0.16.0. F1
does not rename these, because they are build artefacts of a repo that now builds tollgate. F3 does not stop
them either: 0.16.0 never reads `MIGRATED`. F2 removes their adopt target. The residual is §6.

### 4.10 Rollback (`src/import/rollback.rs`)
Preconditions: journal `complete`, `in_progress` or `rolling_back` (resume), or `pre` (undo pre only); M-1 process
scan with the same roles (a `tollgate` other than self, any `claude`, any `codex` when codex was imported, any
`clauth` shim run); `*.pending`/chain staging temps (`.credentials.json.tmp.*`, `.session-token*.tmp.*`, `.auth*.json.tmp.*`) in
imported tollgate profiles → `pending_rotation` (a copy's own `.<name>.tmp.*` is swept by its revert instead); the claude slot
linked to a profile created after import → `live_slot_on_new_profile` (remedy `tollgate switch <imported>`);
upstream src paths must be absent (else `destination_exists`). Take the fence, `state:"rolling_back"`, then:
1. `retire` entries in reverse (§4.11 undo). 2. `main` entries in reverse **except** `retire_bin`: live slots first
   become symlinks to the restored upstream stores (a tollgate-detached regular slot: Same → symlink, its bytes kept in
   `quarantine/credentials.json.rollback-live` when they differ from the store's; Diverged → `--adopt-live`
   capture onto the store, the superseded store kept in `quarantine/credentials.json.rollback-superseded`,
   refused when the slot holds another imported profile's chain or is older than the store; else refuse); carriers renamed back — the **current** dst file, whatever
   its inode after tollgate rotations (journaled ino is audit only); copies deleted only when journaled `copy*`
   (`copy_tree` deletes only `created` names); roster entries for imported names removed (post-import profiles
   stay); `~/.tollgate/profiles/<dst>` removed only when no carrier-shaped file remains; F2 restored from
   backup when the file's sha equals the post-edit sha, else `active_profile` re-set to the prior value; tombstone
   removed; G3/G4 reversed. 3. **Then** `retire_bin`: verify `retired` sha, remove shim, rename back (reinstall-
   order rule: stores before the binary). If the retired file is gone, print
   `git -C <src> worktree add <tmp> v0.16.0 && cargo install --path <tmp> --locked` as a manual step (never
   `cargo install --path <src>`: `<src>` now builds tollgate) and leave the shim (warning). G1 (a `main` M7
   entry) → prior value, with G3/G4. 4. `pre` in reverse, inside the fence: G2 → restore the config bytes if
   its current sha equals `config_sha_after`; nothing is spawned. 5. `state:"rolled_back"` (or `aborted` for an
   automatic reversal); release the fence. 6. **After the fence**, best-effort and outside the journal's
   byte-identity promise: reinstall the journaled herdr plugin at its recorded commit,
   `herdr plugin install <owner>/<repo>/herdr-plugin --ref <resolved_commit> --yes` (the argv shape of
   `herdr.rs:913`; the subdir is upstream's `GITHUB_SOURCE`, `mommy:src/herdr.rs:32`), stdin null. It needs the
   network, so it never runs inside the hold. On any failure (offline, herdr refuses a commit ref) print that
   command, plus `clauth herdr install --yes --no-config` as the fallback, and exit with the rollback's own
   code. The fork's `profiles.toml`/`codex-profiles.toml` are never copied back into `~/.clauth`.

### 4.11 Retire checklist (`tollgate import retire`, part 2; requires `complete`; each step journaled in `retire`)
**R1** remove `enabledPlugins["clauth@clauth"]`, the `clauth` marketplace from `settings.json`
(`extraKnownMarketplaces`), `plugins/known_marketplaces.json` and `installed_plugins.json` (`clauth@clauth`), and
`mcpServers.clauth` from `~/.claude.json`; prior values recorded (JSON, no secrets). **R2**
`plugin_host::install()` (`tollgate@tollgate`; undo: agentgear uninstall). **R3** `herdr::install(key, false, yes,
_)` (undo `herdr::uninstall(false, true)`); `--yes` without a key uses `--no-config`. **R4** `.bashrc` completion
line: the `clauth` line replaced by `tollgate completions install bash` output (backup of the one line). Never:
delete `clauth-0.16.0.retired`, run `cargo uninstall`, or remove `~/.clauth`; printed as the owner's last steps.

## 5. Failure modes and recovery

| Crash / event at | On-disk state | Recovery |
|---|---|---|
| M-1 / dry-run | nothing written | rerun |
| confirmation prompt open, a CC session or lock holder starts | nothing written | the post-confirmation recheck refuses, exit 3 |
| during G2 subprocess | entry `planned` | `exec` is idempotent: resume re-runs it; rollback restores bytes by sha rule, then reinstalls the journaled plugin commit after the fence (best effort) |
| M3 lock busy (rare: M-1 and the recheck gate on the same files) | `pre` | automatic: restore herdr config bytes, `aborted`, exit 3; then the best-effort plugin reinstall at the recorded commit, printing the command on failure |
| M4 revalidation fails | `pre` | same as M3 |
| M5 step N `planned`, op not done | src intact | replay: disk = prior → redo (resume) or skip (rollback) |
| M5 step N op done, not marked | src absent, dst ino = prior | replay marks `done`, continues or reverses |
| `move_relink` between rename and relink | slot dangles | replay finishes the relink (resume) or renames back and restores the link target (rollback) |
| disk matches neither prior nor after | — | exit 4 `journal_disagrees`, no further change; manual |
| blocking process appears mid-M5 | partial | automatic reverse of `main` then `pre`, `aborted`, exit 1 |
| exempt read-only tollgate run (`herdr tag`, `usage --waybar`, `__complete`, …) appears mid-M5 | none | warning; it blocks on the fence and exits |
| EXDEV at rename | step not done | automatic reverse, `aborted`, exit 1 (`cross_device` named) |
| crash inside `move_relink` after the temp link was created | temp present | replay removes `after.temp` in both directions |
| power loss after `complete` | committed | nothing; `retire` next |
| upstream command started during the hold | blocks on our flock or hits the shim | exits (shim) or times out (25 s) with nothing changed |
| tollgate daemon started during the hold | `tollgated.lock` busy (fence item 4) → `Claim::Redundant`, or its refresher stands down (`upstream_refresher_active`) | none needed |
| reversal itself fails | state `rolling_back`/`in_progress` | exit 4; `import rollback` resumes it |

Partial state is always named by the journal; guest mode stays on until `complete`, so a half-done import never
enables tollgate's Claude/Codex OAuth legs over half-moved stores.

## 6. Interactions

- **Guest mode.** The import is the only code allowed to write the guest-protected files, so its writers call
  raw fs helpers, never the guest-gated wrappers (`force_link_profile_credentials` refuses, `claude.rs:2651`).
  `complete` opens every gate at once (scheduler OAuth legs, settings sync write-back, plugin heal, helper
  self-heal on global settings). Guest-mode profiles already in `~/.tollgate` are untouched. The guest transcript
  store `~/.tollgate/guest-claude/projects` is kept (I19), but it is listed and resumable **only while guest mode
  is on**: `sessions::shared_stores()` adds it only under `upstream_active()` (`sessions.rs:804-807`), and
  `start::seed_guest_passthrough_resume` is a no-op outside guest mode (`start.rs:258-261`). So M5.2 copies it
  into `~/.claude/projects` (destination wins, `created` journaled, rollback deletes only `created`), and every
  guest conversation stays listed in `tollgate sessions` and resumable by `--resume` and by hot-swap relaunch
  after `complete`. No code change is needed at either site, and nothing is listed twice. The upstream
  read-only view (`usage/upstream.rs:37-41`) turns off by itself.
- **Residual: off-`PATH` upstream builds.** A `target/*/clauth` 0.16.0 build, or a `cargo run` on `mommy`, is not
  retired by F1. Its TUI exit would turn the slot link into a regular file (`mommy:src/claude.rs:2637-2655`), a
  second carrier of tollgate's chain, and F3 is never read by 0.16.0. F2 leaves it nothing to adopt into, and
  M-1 warns (`upstream_offpath_build`). The fix is R2's D18 start-time reconcile: relink a regular slot whose
  content equals a tollgate chain. Until R2 ships, the owner must not run those builds.
- **Lane 2 (executor B, same-provider API-key hot swap).** Imported api-key profiles arrive with their 0600
  `config.toml`; G3 makes the global helper tollgate's (profile form). `conversations/` and
  `session_profiles.json` are imported for P6b relaunch. Every `tollgate start` supervisor, and any live
  `~/.tollgate/live_sessions` row, blocks M1 (`tollgate_live_session`), so no B session spans an import.
- **Lane 3 (Hermes harness).** Import never reads or writes `~/.hermes` or any Hermes home. Uniqueness goes through
  `actions::validate_profile_name` (§4.7), so the Hermes roster is covered once Hermes generalises it over
  `Harness::ALL`. A live Hermes session is a tollgate supervisor and blocks M1; stop Hermes sessions before
  importing. Whichever lane lands second adds `import_refuses_a_name_held_by_a_hermes_profile`.
- **herdr.** G2 removes upstream's plugin and `$clauth` rows via upstream's own command; R3 installs tollgate's.
- **Local API / MCP.** Read-only `import` block (§2.5). A running `tollgate api serve` is a tollgate process and
  blocks the import (it would publish half-moved state).

## 7. Test plan (hermetic; `testutil::HomeSandbox` per test; `tests/inline/import_*.rs`)

Fixtures (new, `src/testutil.rs`): `UpstreamTree` builder over the sandbox home — `.reference()` (this
machine's shape, see the header), `.oauth(p)`, `.rolling(p)`, `.static_token(p)`, `.api_key(p)`, `.codex(p)`, `.quarantine(p)`,
`.mcp_logins(p)`, `.pending(p)`, `.stray_temp(p)`, `.hardlink(p)`, `.runtime_tree(p, fake_copy: bool)`,
`.live_symlink(p, file)`, `.live_regular_same(p)`, `.live_regular_diverged(p)`, `.codex_symlink(p)`,
`.codex_regular_copy(p)`, `.helper_in_settings(p)`, `.registry_path(path)`, `.herdr_config()`, `.upstream_bin()`,
`.unknown_entry(path)`; synthetic tokens `sk-ant-oat01-FIXTURE-<p>-<n>` / `FIXTURE-RT-<p>-<n>` so leaks are greppable.
`FakeProcs` (§4.3 seam), `LockHolder::hold(path)` (second open + `try_lock`), `TreeSnapshot::of(home)` (type, ino,
mode, nlink, link target, sha256 per path), seams `journal::testing::crash_at(seq, BeforeOp|AfterOp)`,
`fsops::testing::force_exdev(path)`, `import::testing::pause_at(seq)`; fake `clauth`/`herdr` shell scripts on a
pinned `PATH` (`EnvPin`) that log argv and env keys. Integration: `tests/import_cli.rs` runs the binary with
`HOME`=tempdir, a stub `PATH`, and `TOLLGATE_NO_API=1`.

Part 1 (`import_inventory.rs`, `import_fence.rs`, `import_txn.rs`, `import_slots.rs`, `import_rollback.rs`):
1. `dry_run_changes_no_byte_and_creates_no_lock_file` — `TreeSnapshot` of home identical before/after.
2. `dry_run_classifies_the_reference_inventory` — golden JSON of `.reference()` (every action, `clauth.log` skip).
3. `an_unknown_top_level_or_profile_entry_is_refused` · 4. `a_pending_rotation_or_stray_temp_is_refused_with_its_remedy`
5. `a_hardlinked_or_symlinked_carrier_is_refused` · 6. `codex_home_holding_auth_json_is_refused`
7. `a_carrier_copy_in_a_stale_runtime_tree_is_refused_and_a_symlink_there_is_not` · 8. `a_name_colliding_with_either_roster_or_a_profile_dir_is_refused_case_insensitively`
9. `rename_maps_a_name_in_dirs_rosters_chain_session_owners_and_rollback_maps_it_back` · 10. `cross_device_source_and_destination_are_refused_without_a_copy` (seam)
11. `import_report_never_echoes_a_fixture_secret` — text and JSON, dry-run, run, rollback.
12. `dry_run_reports_held_free_and_absent_locks` · 13. `already_complete_no_upstream_and_non_linux_are_refused`
14. `each_fence_lock_held_elsewhere_refuses_within_three_seconds_and_changes_nothing` (items 1–9, parametrised)
15. `a_parked_upstream_standby_refuses` · 16. `claude_node_claude_and_clauth_mcp_processes_refuse_and_are_named`
17. `a_codex_process_blocks_only_when_codex_carriers_are_in_scope` · 18. `another_tollgate_process_refuses`
19. `a_live_session_row_with_a_live_pid_refuses_and_a_stale_one_warns` (upstream and `~/.tollgate/live_sessions`) · 20. `a_held_session_marker_refuses` (upstream and `~/.tollgate/profiles/*/sessions-*`, a Hermes marker included)
20a. `a_tollgate_hermes_supervisor_blocks` · 20b. `exempt_read_only_tollgate_runs_warn_and_never_abort` (`herdr tag`, `usage --waybar`, `__complete`, `which`, `status`, `list`, `import status`, at M1 and in a per-carrier rescan)
20c. `a_held_fence_file_at_m_minus_1_is_a_blocker` · 20d. `the_post_confirmation_recheck_refuses_a_session_started_during_the_prompt` (prompt seam)
21. `the_fence_holds_rank_import_fence_and_enters_no_rank_below_state_after_item_9` (debug rank stack: no `Rotation`, `Config` or `ProfileTtl`) · 22. `a_process_appearing_before_a_carrier_move_reverses_the_transaction` (FakeProcs flips at seq N; exit 1, `aborted`)
23. `a_paused_transaction_blocks_upstream_state_and_daemon_lock_takers` · 24. `oauth_rolling_and_static_sets_move_by_rename_and_keep_their_install_source` (same inodes, nlink 1)
25. `quarantine_and_mcp_logins_move_and_nothing_carrier_shaped_is_copied` · 26. `a_codex_store_lkg_and_quarantine_move_and_each_chain_has_one_inode`
27. `config_toml_copies_are_0600_and_the_journal_records_only_their_size` · 28. `upstream_originals_of_copied_entries_stay_byte_identical`
29. `a_symlinked_live_slot_is_repointed_in_the_move_step` · 30. `a_same_regular_live_slot_becomes_the_store_then_a_link`
30a. `a_same_regular_slot_on_a_session_token_profile_is_relinked_not_captured` (install source unchanged; the live copy is quarantined; M8's assert passes)
31. `a_diverged_regular_live_slot_refuses_without_adopt_live_and_is_captured_with_it` · 32. `a_detached_duplicate_with_no_active_profile_is_relinked_to_its_store`
33. `an_independent_or_missing_live_slot_is_untouched` · 34. `a_live_link_to_a_non_carrier_is_refused`
35. `a_codex_symlink_slot_moves_and_repoints_under_its_rotation_lock`
36. `a_codex_regular_copy_of_a_store_chain_is_refused` · 37. `an_independent_codex_login_is_untouched`
38. `roster_merge_appends_names_keeps_set_tollgate_keys_and_takes_unset_upstream_keys` · 39. `session_owners_merge_and_conflicts_become_contested`
40. `commit_writes_complete_last_and_upstream_active_turns_false` · 41. `after_commit_upstream_has_no_active_profile_its_binary_is_the_shim_and_the_slot_links_into_tollgate`
42. `a_crash_at_every_step_resumes_to_the_uninterrupted_tree_or_rolls_back_to_the_original` (loop over seq ×
    BeforeOp/AfterOp; compares `TreeSnapshot`s; carriers compared by inode; no `.credentials.json.tollgate-import.*`
    temp survives either direction)
43. `a_journal_disagreeing_with_disk_stops_with_exit_4` · 44. `every_journal_write_is_fsynced_before_its_op` (op log)
45. `rollback_restores_carriers_to_their_paths_and_inodes` · 46. `rollback_after_a_tollgate_rotation_moves_the_current_store_back`
47. `rollback_relinks_a_detached_live_slot_as_a_link_never_a_copy`
48. `rollback_refuses_while_tollgate_claude_or_codex_lives` · 49. `rollback_restores_stores_before_the_upstream_binary`
50. `rollback_keeps_post_import_profiles_and_never_writes_the_fork_roster_into_clauth` · 51. `rollback_refuses_a_live_slot_on_a_post_import_profile`
51a. `a_dev_build_exe_is_refused_and_the_installed_path_passes` · 51b. `offpath_upstream_builds_are_listed_as_warnings_without_exec` (a `target/debug/clauth` fixture that writes a sentinel when run; the sentinel stays absent)
51c. `a_name_held_by_any_roster_refuses_through_validate_profile_name` (the message names the harness) · 51d. `the_guest_store_is_copied_into_the_global_store_and_rollback_deletes_only_created`
51e. `a_reversal_after_m5_started_exits_1_and_an_m4_refusal_exits_3`
Part 2 (`import_edits.rs`, `import_retire.rs`, `tests/import_cli.rs`, existing suites):
52. `g1_turns_the_upstream_plugin_off_inside_the_fence_and_rollback_restores_it` (G1 is an M7 entry; the rank stack shows fence item 8 held) · 53. `g2_runs_upstream_herdr_uninstall_scrubbed_before_any_lock_with_a_config_backup_and_the_plugin_record`
53a. `g2_undo_reinstalls_at_the_recorded_commit_after_the_fence_and_prints_the_command_on_failure` (fake `herdr` logs argv; offline stub fails; nothing is spawned while the fence is held)
54. `g3_rewrites_the_upstream_helper_and_permissions_allow_and_rollback_reverses_both` · 55. `g4_remaps_registry_paths_to_tollgate_then_the_twin_then_warns`
56. `a_refusal_after_m0_undoes_pre_and_records_aborted` (`pre` holds only G2; `settings.json` untouched) · 57. `an_m_minus_1_refusal_touches_no_global_file`
58. `non_tty_without_yes_exits_2_and_changes_nothing` · 59. `exit_codes_match_the_table` (binary)
60. `import_status_reports_every_journal_state` · 61. `an_interrupted_journal_warns_on_every_command`
62. `retire_steps_are_journaled_and_each_reverses` · 63. `rollback_after_retire_reverses_retire_first`
64. `health_and_status_routes_carry_the_import_state` (+ `dump_openapi` golden)
65. `a_guest_conversation_is_listed_and_resumable_after_import` (`tollgate sessions` lists it from `~/.claude/projects`; `--resume <id>` finds it with guest mode off; listed once) · 66. `guest_refusal_names_the_import_command`
67. `a_full_reference_import_then_rollback_leaves_every_file_tollgate_restores_byte_identical` (`.reference()`, both
    slot shapes; `~/.claude`, `~/.claude.json`, `~/.codex`, herdr `config.toml`, `installed_plugins.json`,
    `.bashrc`). Byte identity of the slot itself holds for the **symlink** slot only: a Same regular slot is
    captured (I4) and comes back as a link to its restored store, which then sits on the slot's old inode. An
    `--adopt-live` capture restores the regular slot byte for byte on immediate undo; after rotation the current store goes home and the superseded chain remains quarantined (tests in `import_slots.rs`). herdr's `plugins.json`, `.plugins.lock` and `plugins/` are **excluded**: herdr rewrites them on
    reinstall (install timestamps, a fresh checkout). The test asserts only that the reinstall argv names the
    recorded commit. This is the documented herdr residual.

## 8. Implementation slices (exactly two PRs, each green on nextest + clippy `-D warnings` + fmt)

**Part 1 — foundation (R3a + transaction core of R3b + R3c engine).** `src/import/{mod,inventory,journal,fence,
procs,fsops,slots,roster,rollback}.rs`; rank `ImportFence`; `identity::import_state()`; `claude::install_source_in`;
exit codes 3/4 (named `ExitCode` table from the prep PR, hot-swap spec §8); CLI `import clauth --dry-run [--json]
[--rename] [--adopt-live]` and `import status`. Lands after hot-swap Part 1 and before Hermes Part 1. The engine
(M3–M8 with F1–F3, plus rollback; without G1–G4) is complete and exercised by tests 1–51, but the CLI real run and
`import rollback` print `tollgate import clauth: only --dry-run is available in this build` and exit 2, so a
part-1 binary can never leave upstream's plugin hooks or herdr wiring pointing at an emptied `~/.clauth`.
**Part 2 — surfaces and docs (rest of R3b, R3c CLI, R3d).** `src/import/{edits,retire}.rs` (G1–G4, R1–R4); M0
`pre` section (G2 only) and its split undo; the real run, `--resume`, `import rollback`, `import retire` wired;
the interrupted warning in `main::dispatch`; local API `import` block + OpenAPI (golden changed only here, one
lane at a time); TUI guest footer; `GUEST_REFUSAL` text; completions (`src/completions.rs`, bash/zsh/fish:
`import clauth|rollback|status|retire`, `--dry-run --json --rename --adopt-live --resume --yes --step`); tests
52–67; docs: new `wiki/Import.md`, `wiki/Guest-Mode.md:7-9`, `wiki/Configuration.md:346`,
`docs/agent-api.md:116-119,201`, `CHANGELOG.md:93` (the "No import yet" known gap), plan §10 rows R3a–R3d moved
to Shipped.

## 9. Decisions taken now (defaults) and open questions

| Id | Decision | Why |
|---|---|---|
| I1 | Linux only; macOS/Windows → `unsupported_platform` | the macOS default Keychain item is a second live slot this spec does not move |
| I2 | Binary retire F1 is the **first** `main` step (plan: M8) | a crash anywhere after the first move then leaves no runnable upstream to re-adopt |
| I3 | Diverged regular claude slot needs `--adopt-live` (plan/D18: capture) | without a network identity probe a CC `/login` to another account is indistinguishable; the superseded store chain is parked in quarantine |
| I4 | Same regular slot: the live inode is renamed onto the store | moved, not copied; newest `mcpOAuth` kept; one inode per chain |
| I5 | No upstream active + slot chain equals a store → relink to it | it is a detached duplicate carrier |
| I6 | G2 runs upstream's own `clauth herdr uninstall --yes` (as the plan says), before any lock, config backed up and the plugin record journaled; its undo restores the config inside the fence and reinstalls the recorded commit **after** the fence, best effort | upstream's block matcher is not tollgate's; upstream's command takes its state lock itself; a reinstall is a GitHub fetch (`mommy:src/herdr.rs:35,439,780`) and must not run in the hold or pull a newer upstream commit |
| I7 | G4 remap order tollgate path → `~/.claude/plugins` twin → keep+warn | a bare prefix swap would point at skipped runtime trees |
| I8 | Roster: tollgate's set keys win; upstream's fill unset keys; `[serve]`/`[update]`/`[local_api]` never imported | the owner already configured tollgate; port/API collisions |
| I9 | Collisions refuse; `--rename` is the only escape; no merge into an existing profile | two stores per name would need a chain choice |
| I10 | `codex` processes block only with codex carriers in scope | today's independent `~/.codex` login is untouched |
| I11 | Any other tollgate process blocks (daemon, TUI, `api serve`, mcp, every `start` supervisor incl. Hermes), and so does a live `~/.tollgate/live_sessions` row or held `~/.tollgate` marker; the read-only subcommands `herdr tag`, `usage`, `__complete`, `which`, `status`, `list`, `import status` only warn | they cache the roster and hold tollgate's leases; the exempt ones hold no lock the fence does not, and `report-profile.sh:294` runs `herdr tag` on every pane refresh, which would otherwise abort M5 again and again |
| I12 | Per-session upstream trees are skipped; a regular-file carrier inside blocks | dormant copies are second carriers |
| I13 | No byte backup of `settings.json`/`.claude.json`; semantic reverse from recorded key paths | both can hold env secrets |
| I14 | Rollback never runs cargo; a missing retired binary is a printed manual step that builds the `v0.16.0` tag in a worktree | build/safety rule; `<src>` (`~/Work/clauth`) now builds tollgate, so `cargo install --path <src>` would install the wrong tool |
| I15 | Hold M3–M8 (and rollback's hold) has no prompt, subprocess or network; the G2 reinstall runs after release | upstream waiters' 25 s timeout; the 20 s subprocess budget armed by `StateLock` |
| I16 | Fence takes upstream rotation-lock files raw under one `ImportFence` rank, never `RotationGuard` | strict rank order forbids N nested rank-100 guards |
| I17 | Absent upstream lock files are created (real run only) and left | an absent file cannot be held against a later creator |
| I18 | D18: the import leaves the claude slot a symlink; tollgate's TUI-exit detach stays as today | R2 owns the start-time reconcile |
| I19 | `~/.clauth` and the guest store are kept after import; the guest store's transcripts are also copied into `~/.claude/projects` (M5.2) | rollback needs the former; the guest store is listed only in guest mode (`sessions.rs:804-807`), so without the copy guest conversations vanish from `sessions` and `--resume` |
| I20 | G1 moves from M0 (plan) into M7 inside the fence; `pre` holds only G2 | G1 is a pure JSON edit that upstream would race under `~/.clauth/.lock`; the fence holds that lock |
| I21 | A post-confirmation recheck (M1 + lock probes) runs before M0, and held fence files are M-1 blockers | the prompt is unbounded; a session started meanwhile hot-reloads `settings.json` (S1(c)) |
| I22 | A Same regular slot on a `session-token.json` profile is relinked, not captured | capture would change `sidecar_kind_of` and so the install source |
| I23 | `dev_build_exe` blocks; off-`PATH` upstream builds only warn | G3 must not point the global helper at a `target/` build; renaming another repo's build artefacts is out of scope, R2's reconcile closes the residual |
| I24 | Exit 3 only for refusals before M5 (M-1, recheck, M3, M4); any reversal after M5 started exits 1 | 3 promises "nothing changed" |

Open questions for the owner: **Q1** D18 — should a Diverged slot be captured without `--adopt-live` when its
`expiresAt` is later (plan default) instead of I3? **Q2** after a successful retire, should `tollgate import retire
--final` delete `clauth-0.16.0.retired` and print `cargo uninstall clauth`, or leave both manual (default)?
**Q3** accept I10 (codex processes allowed when no codex carrier is in scope)? **Q4** macOS import (Keychain slot)
in a later lane or never? **Q5** when to run it on this machine: today's dry-run would block on the live Claude
Code sessions and the 10 `clauth mcp` processes (`process_alive`), nothing else predicted.

## 10. Code anchors (all to change or to call; `mommy:` = upstream 0.16.0)

- `src/cli.rs:81` (`Command`; add `Import` after `Herdr` at `:501`), new `ImportCommand` enum next to `HerdrCommand` (`:760`).
- `src/main.rs:195-222` (`exit_code`: `ImportBlocked` → 3, `ImportNeedsAttention` → 4), `:224` (`dispatch`: interrupted warning; `Command::Import` arm near `:348`), module list `:1-59` (`mod import;`).
- `src/identity.rs:108-110` (journal doc: schema §3.2), `:114-119` (`GUEST_REFUSAL` text), `:135-147` (`upstream_active`, unchanged), `:153-167` (`import_completed`; add `import_state()`), `:98-102` (`UPSTREAM_HERDR_PLUGIN_ID` now used).
- `src/lockorder.rs:83-118` (add `ImportFence = 80` between `ApiSwitch` `:113` and `Rotation` `:117`), `:191` (`State`).
- `src/lock.rs:47` (25 s), `:313` (`acquire_with_timeout`), `:390` (`lock_file_with_timeout`, reused for fence item 8).
- `src/daemon/probe.rs:705-718` (`standby_waiting` pattern), `:789-812` (`UPSTREAM_REFRESHER_LOCKS`; export the names for the fence); `src/daemon/mod.rs:80-93` (tollgate lock names).
- `src/runtime.rs:2186-2190` (`rotation_lock_path`; add `upstream_rotation_lock_path(name)`), `:2516-2527` (`try_acquire`, not used under the fence), `:4859-4919` (guest store, kept).
- `src/claude.rs:890-898` (`install_source_path`; add `install_source_in(dir)`), `:943-976` (`classify_link_at`, reused on upstream paths), `:2114-2144` (`profile_name_from_helper`; add `upstream_helper_profile` for `__api-key`), `:2283-2305` (`build_api_key_helper_command`, `pub(crate)`), `:2651` (`force_link_profile_credentials`, not used by import), `:2703-2720` (`refuse_foreign_slot_link` message names the import), `:2726` (`detach_credentials_link`, unchanged, I18); `mommy:src/claude.rs:2637-2655` (the upstream detach F1 fences).
- `src/codex_auth.rs:474-476`, `:559-560` (codex carrier names), `src/codex_profiles.rs:212-214` (roster path).
- `src/profile.rs:1805` (`tollgate_dir`), `:1891` (`profiles_root`), `:1928` (`is_under_own_profiles_root`), `:2332` (`tmp_sibling`: stray-temp pattern), `:2364` (`atomic_write_600`; add `write_durable_600` beside it), `:2403` (`mkdir_700`), `:2423` (`open_state_file`), `:2534` (`claude_roster_names`), `:3870` (`load_config_read_only`, the only config load before the fence), `:667-691` (`HomeTab`; add `home_tab_alias`).
- `src/actions.rs:37` (`validate_name_chars`), `:59-76` (`validate_foreign_harness_free`, generalised over `Harness::ALL` in the prep PR), `:87-112` (`validate_profile_name`, the only uniqueness check the import calls). Lock-order precedent: `mommy:src/lockorder.rs:8-23,117` and `src/runtime.rs:2205-2220`.
- `src/claude.rs:226-240` (`sidecar_kind_of`: why a session-token profile's slot is relinked, not captured); `src/platform.rs:17` (`installed_exe_path`, `dev_build_exe`).
- `src/herdr.rs:673-760` (`plugin list --json` parser, G2 record), `:913` (the `plugin install … --ref … --yes` argv shape for the undo); `mommy:src/herdr.rs:32-35` (upstream `GITHUB_SOURCE`).
- `src/completions.rs` (hand-written; `import` subcommands and flags, bash/zsh/fish).
- `src/start.rs:258-261` (`seed_guest_passthrough_resume`, guest-only; unchanged, covered by the M5.2 copy).
- `src/plugin_host.rs:93-144` (`repoint_registry`), `:146-190` (`registry_remap`; extract a root-parameterised core for G4), `:44-48` (`install`, R2).
- `src/herdr.rs:56` (`install`, R3), `:331` (`config_path`, G2 backup), `:1688` (`uninstall`, R3 undo).
- `src/settings_sync.rs:148-150` and `src/runtime.rs:5464-5480` (`clauth@clauth` per-profile rule; after R1 the key is gone, rules stay).
- `src/claude_json.rs:94` (`mcpServers.clauth`, R1).
- `src/sessions.rs:804-807` (guest store listed only under `upstream_active()`; unchanged, covered by the M5.2 copy).
- `src/local_api/routes.rs:233,266,389-397` (`HealthBody`/status bodies gain `import`), `src/tui/mod.rs:52` (guest footer).
- `src/usage/upstream.rs:37-41` (turns off with guest mode; no change).
- `src/testutil.rs:22-70` (`HomeSandbox`; add `UpstreamTree`, `TreeSnapshot`, `LockHolder`), `tests/inline/guest_mode.rs:100-130` (journal contract; extend with every state).
- Docs: `wiki/Guest-Mode.md:7-9,30`, `wiki/Configuration.md:346`, `docs/agent-api.md:116-119,201`, `CHANGELOG.md:93`, `docs/multi-provider-redesign-plan.md:865,880`.

## Review log

Critique of 2026-09-29, each item verified against the code before editing. Nothing was rejected.

| # | Sev | Item | Outcome |
|---|---|---|---|
| 4 | IMPORTANT | G1/G2 after an unbounded prompt, no rescan, no lock | **Applied.** Post-confirmation recheck (§4.1); G1 moved into M7 inside the fence (upstream writes `settings.json` under `.lock`, `mommy:src/claude.rs:2196-2201`); `pre` = G2 only; §5 rows; tests 20d, 52, 56; I20, I21 |
| 5 | IMPORTANT | G2 undo is a GitHub fetch inside the fence; byte-identity unreachable | **Applied.** Confirmed `mommy:src/herdr.rs:35,439,780` and `~/.config/herdr/{plugins.json,plugins/github}`. Plugin record journaled; reinstall at `resolved_commit` after the fence, best effort; M-1 lock probes and the recheck gate; test 67 limited to files tollgate restores; tests 20c, 53a; I6, I15 |
| 6 | IMPORTANT | §6 claim about `sessions.rs:804` false | **Applied** (copy option). Confirmed `sessions.rs:804-807` and `start.rs:258-261` are guest-only. M5.2 `copy_tree` of the guest store into `~/.claude/projects`; §6 rewritten; tests 51d, 65; I19. The alternative (re-keying both sites on the store existing) was not taken: it would list transcripts twice after the copy, and outside guest mode `seed_guest_passthrough_resume` would seed into a store CC no longer reads |
| 7 | IMPORTANT | Cross-lane process/row rules, exempt short-lived runs, M1/M2 naming | **Applied.** `tollgate_live_session` blocker; exempt read-only subcommands warn; Hermes supervisors block; M1/M2 defined in §4.1; tests 19, 20, 20a, 20b; I11 |
| 8 | IMPORTANT | Two uniqueness implementations | **Applied.** §4.7 calls `actions::validate_profile_name` + `profile_dir().exists()`; `all_tollgate_names` removed; message names the harness; test 51c; the second lane adds `import_refuses_a_name_held_by_a_hermes_profile` |
| 14 | IMPORTANT | Capture onto a session-token source changes `sidecar_kind_of` | **Applied.** Confirmed `claude.rs:226-240` and `:890-898`. Capture only on a `credentials.json` source; session-token Same → `move_relink`; test 30a; I22 |
| 15 | IMPORTANT | Off-`PATH` upstream builds; wrong rollback cargo hint | **Applied.** Confirmed `target/{debug,release}/clauth` (nlink 2). `upstream_offpath_build` warning (metadata only), §6 residual tied to R2/D18, worktree-based manual step; test 51b; I14, I23 |
| 17 | IMPORTANT | Cross-lane merge hazards | **Applied.** Named `ExitCode` table (§2.2), landing order (§8), OpenAPI golden in Part 2 only, completions listed (§8, §10) |
| 23 | MINOR | Ranks outer to State inside the hold | **Applied.** raw `toml_edit` + `write_durable_600`; test 21 asserts no rank below 500 after item 9 |
| 24 | MINOR | Exit 3 used after changes | **Applied.** §2.2, §5; test 51e; I24 |
| 25 | MINOR | `move_relink` temp not journaled | **Applied.** `after.temp`; replay removes it; test 42 |
| 26 | MINOR | G3 with a dev-build `current_exe` | **Applied.** `dev_build_exe` blocker; test 51a |
| 27 | MINOR | "executor B's helper self-heal" does not exist | **Applied.** removed |
| 28 | MINOR | Wrong anchors | **Applied.** `actions.rs:1443-1447` replaced by `mommy:src/lockorder.rs:8-23,117` / `runtime.rs:2205-2220`; `CHANGELOG.md:93` |
