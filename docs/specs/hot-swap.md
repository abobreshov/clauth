# Spec: same-provider hot swap for API-key accounts (P6a) + relaunch in place (P6b)

Draft, 2026-09-29, base `feat/tollgate` @ `c53123a0`. Authority: plan v3.1 §4.4, §5 (S1/P6a/P6b), §10;
`docs/tollgate-code-review-0.1.0.md` (guest rules B1/B2/I6 stand). Paths repo-relative; `~` = test home.

## 1. Goal and non-goals

**Goals**
1. A live `tollgate start` Claude Code session on an API-key account moves to another account of the
   **same transport class** without a restart (**executor B**: only the key-helper output changes).
2. Every surface separates **requested → committed → served**; `swapping…` until the helper serves.
3. Where B is not allowed, and for every cross-class switch, `tollgate switch <sid> <p> --relaunch`
   stops the session gracefully and resumes the same conversation under `<p>` in the same terminal (**P6b**).
4. Executor A (OAuth: `swap_eligible`, `swap_to`, `poll_converge`, `RotationGuard`) is unchanged.

**Gate (named): `S1-APIKEYHELPER`.** B is chosen at spawn only when the machine block in
`docs/spikes/s1-apikeyhelper.md` says `result = "PASS"` for the installed Claude Code version (§3.6);
otherwise the session registers `relaunch_only(s1_gate_pending | s1_gate_version)` and in-class
switches use `--relaunch`. The spike ran and passed for CC 2.1.283 (commit `60e4aee5`), but that
committed doc has **no machine block yet**, and a missing block reads PENDING. Part 1 therefore
appends the block (§3.6, §8) and changes nothing else in the doc. After that, B is live on 2.1.283 and
nowhere else. S1(g) against real endpoints stays owner-run, and per the plan it gates only the P4-OR
helper preset, not B.

**Non-goals.** P6c chain rotation (the daemon never writes `intended_member` for a B row); the
OpenRouter "same account / different account" picker label (needs P4-OR key binding); Hermes
switching (H-4 reuses §4.9 later); codex sessions; model or preset changes (relaunch or CC `/model`,
D1); isolated-session hot swap; delegates (never B, never relaunched); a `tollgate.swap` herdr
action (P7 H4).

## 2. Surface

### 2.1 CLI

```
tollgate switch <sid> <profile> [--wait] [--relaunch [--yes] [--conversation <id>]]
```
All four flags require the two-name form; with one name: usage error, exit 2,
`tollgate: --wait, --relaunch, --yes and --conversation need a session id and a profile`.
`--yes`/`--conversation` without `--relaunch`: exit 2, `tollgate: --yes and --conversation need --relaunch`.

Exit codes (extends the `sessions_cli.rs:25-33` contract): **0** done (committed, served, relaunched,
already on, or A intent recorded); **1** refusal or error; **2** usage error; **3** `--wait` timed out
committed-not-served.

| Case | Output (stdout unless marked) | Exit |
|---|---|---|
| Row executor `oauth` (A) | today's `switch_receipt` (`sessions_cli.rs:255`), unchanged | 0 |
| Already on `<p>` | today's `already_on_line`, unchanged | 0 |
| B, committed within 5 s | `tollgate: session '<sid>' committed to '<p>' (api-key hot swap, key generation <N>)` then `tollgate: swapping… Claude Code picks the new key up on its next request` | 0 |
| B, no commit within 5 s | `tollgate: requested '<p>' for session '<sid>'; the session has not committed it yet (swapping…)` | 0 |
| `--wait`, served | the committed lines, then `tollgate: session '<sid>' is served by '<p>' (key generation <N>)` | 0 |
| `--wait`, 65 s passed | stderr `tollgate: session '<sid>' is committed to '<p>' but its key helper has not served it yet (still swapping…; the session has made no request since the commit)` | 3 |
| `--wait`, helper failed (`stalled`) | stderr `tollgate: session '<sid>' is committed to '<p>' but its key helper failed (<code>); Claude Code keeps the previous key until it is rejected` | 3 |
| B class pre-check fails, or executor refusal read back | stderr `tollgate: session '<sid>' stays on '<cur>': '<p>' is not hot-swappable (<reason text>)` + `relaunch instead: tollgate switch <sid> <p> --relaunch` | 1 |
| Row `relaunch_only` | stderr `tollgate: session '<sid>' cannot hot-swap (<reason text>)` + the relaunch line | 1 |
| Codex row | today's refusal; Hermes row: `sessions_cli::NON_CLAUDE_SWITCH` (the text is owned by the Hermes spec, M-SESSION-SWITCH: `session '<sid>' is a Hermes session; switch by relaunch (tollgate start <profile>)`) | 1 |
| Row without `relaunch_capable`, `--relaunch` | stderr `tollgate: cannot relaunch session '<sid>': the session predates relaunch; exit and 'tollgate start <p> -- --resume <conv>'` | 1 |
| `--relaunch` done | `tollgate: relaunched session '<sid>' as '<newsid>' on '<p>' (conversation <conv>)` | 0 |
| `--relaunch` non-TTY without `--yes` | stderr `tollgate: --relaunch stops a live session; confirm on a terminal or pass --yes` | 2 |
| `--relaunch` declined at prompt | `tollgate: relaunch cancelled; session '<sid>' is unchanged` | 1 |
| `--relaunch` refused (§4.9) | stderr `tollgate: cannot relaunch session '<sid>': <reason>`; session untouched | 1 |

Prompt (TTY, stderr): `Relaunch session '<sid>' (conversation <conv>) as '<p>'? Claude Code exits and resumes the conversation. [y/N] `.
B and A are not asked to confirm (a hot swap keeps the conversation running).

Reason texts (`relaunch_only` codes recorded at spawn, §3.1; `class_differs:*` from §4.5):

| Code | Text |
|---|---|
| `s1_gate_pending` / `s1_gate_version` | `the S1 key-helper spike has not passed` / `the S1 spike has not passed for Claude Code <v>; re-run tools/spikes/s1/run.sh and add <v> to the gate block` (CC updates through mise are frequent) |
| `cc_version_unknown` / `kill_switch` / `registry` | `the installed Claude Code version could not be read` / `TOLLGATE_HOT_SWAP=off` / `its registry row could not be written` |
| `fake_links` / `isolated` / `delegate` | `this host copies runtime trees` / `an isolated session` / `a delegate run` |
| `hybrid_oauth_store` | `the account also stores an OAuth login` |
| `auth_env` / `cloud_env` | `its env sets an auth token` / `its env selects a cloud provider` |
| `loopback_endpoint` / `endpoint_userinfo` / `no_endpoint` | `a local gateway or daemon endpoint` / `an endpoint with credentials in the URL` / `no custom endpoint` |
| `gateway_policy` | `a forceLogin* setting is in effect` |
| `class_differs:endpoint\|models\|env\|auth_env` | `a different endpoint` / `different model routing` / `different custom env` / `its env sets an auth token` |
| `class_differs:oauth_store\|no_api_key\|workspace\|harness` | `it also stores an OAuth login` / `it has no usable api key` / `a different OpenRouter workspace` / `another harness` |
| `disabled` / `not_configured` / `marker_held` / `shutting_down` | `it is disabled` / `it is not configured` / `its liveness marker is held by another process` / `the session is shutting down` |

Helper subcommand (hidden, `cli.rs:540`): `tollgate __tollgate-api-key <profile>` (unchanged) **or**
`tollgate __tollgate-api-key --session <sid>` (new; `profile` and `--session` conflict, one required).
Exit 0 with the bare key on stdout (no newline, `main.rs:2227`); exit 1 with nothing on stdout.

### 2.2 TUI
- Overview `live` cell (`tui/render/overview.rs:720`): trailing marker `…` when ≥ 1 session on the row
  is committed-not-served (`MemberSessions.swapping > 0`), else `⇄` as today.
- New key `m` on an Overview row ("move a session here"): modal of live claude sessions
  (`sid · now-on · executor · state`). Enter calls the §4.4 request core with `Surface::Tui`; toast
  `session <sid>: swapping… onto <p>` / `session <sid> stays on <cur>: <reason text>` /
  `session <sid>: relaunch with 'tollgate switch <sid> <p> --relaunch'`. The TUI never relaunches.
  Guest mode: allowed (it writes only `~/.tollgate/live_sessions/`).

### 2.3 MCP `switch_profile`
`SwitchArgs` (`mcp/mod.rs:721`) gains `session: Option<String>`: `"self"` (sid from
`CLAUDE_CONFIG_DIR` via `sid_of_runtime_dir_name`, `runtime.rs:336`) or a sid. Omitted: the global
relink, unchanged. With `session`: request core with `Surface::Mcp` (5 s commit wait, never
`--wait`, never relaunch). Payload `{ok, session, executor, state, requested_member,
committed_member, served_member, key_generation, reason}`; `state ∈ requested|swapping|served|
refused|relaunch_required`. `relaunch_required` carries `command: "tollgate switch <sid> <p> --relaunch"`.
Allowed in guest mode (the global form keeps its guest refusal).

### 2.4 Local API (read-only, `local_api/routes.rs`)
`AccountsBody` (`:243`) and `AccountBody` (`:252`) gain `live_sessions: [LiveSessionView]`
(additive; `SCHEMA_VERSION` stays 1; `/v1/accounts/{id}` filters to rows whose committed or served
member is that account):
```json
{"session_id":"4242-0","harness":"claude","start_profile":"or-main","executor":"api_key",
 "relaunch_reason":null,"requested_member":null,
 "committed":{"member":"or-alt","generation":2,"at_ms":1759140000000},
 "served":{"member":"or-main","generation":1,"at_ms":1759139990000},"state":"swapping"}
```
Read lock-free (row + sidecar); no write, no lock, no `load_config` (I3 stands). `state ∈
requested|swapping|stalled|served`; a `swapping` view also carries `"idle": true` while no helper run
has been recorded since the commit. For a codex or Hermes row `executor`, `committed` and `served` are
`null` (§3.1). No field carries the claude argv (§3.1 `launch_args` is gone).

### 2.5 herdr
`tollgate herdr tag --agent <k> [--session <sid>] [--] [<profile>]` (`cli.rs:818`). With `--session`
and state `swapping|stalled`, line 1 is `<served> → <committed> swapping…` (or `… stalled`) and no
severity line; the account the tag grades is the served member (§4.7). The
`report-profile.sh` claude arm passes `--session "$(basename "$row" .json)"` (`:294`). Tags carry only
profile names (H1 redaction).

## 3. Data and files

All under `~/.tollgate` (dirs 0700 `mkdir_700`, files 0600 `atomic_write_600`/`open_state_file`); nothing under `~/.claude`, `~/.clauth`, `~/.codex`, `~/.hermes`.

### 3.1 Registry row `live_sessions/<sid>.json` (additive, all `#[serde(default)]`, no version bump)
| Field | Type | Writer | Meaning |
|---|---|---|---|
| `executor` | `{"kind":"oauth"}` \| `{"kind":"api_key"}` \| `{"kind":"relaunch_only","reason":"<code>"}` \| `{"kind":"none"}` | session, at registration | absent = derived from `harness` by `LiveSession::executor()`: Claude → `oauth` (legacy), Codex / Hermes → `none`. Nothing reads the raw field |
| `launch_class` | `LaunchClass` \| null | session, at registration | set for every API-key-shaped launch |
| `key_generation` | u64 \| null | session (`SessionFields`) | B: 0 at registration, +1 per commit |
| `committed_at` | ms \| null | session | last B commit |
| `swap_refusal` | `{member, code, text, at_ms}` \| null | session | last refusal, cleared on commit |
| `relaunch_capable` | bool | session, at registration | supervisor polls `.relaunch` (§4.9) |
| `relaunched_from` | sid \| null | session, at registration | from a nonce-verified `TOLLGATE_RELAUNCHED_FROM` (§4.9 step 5) |

The claude argv is **not** a row field. `--mcp-config` / `--settings` JSON can carry tokens, and
`live_sessions::list` is read by panes (`daemon/api/panes.rs:210`), the MCP server, herdr and the local
API. The supervisor keeps its own argv in memory for relaunch (§4.9), and no surface serialises it.
`SwapView`, `LiveSessionView` and pane attribution report `executor = null` for a `none` row.

B rows register `current_member = start_profile`; `cwd` becomes the spawn cwd (resume workspace or
process cwd). `DaemonFields` is unchanged.

### 3.2 `LaunchClass` (persisted; no secrets)
```json
{"version":1,"endpoint":"https://openrouter.ai/api","models":{…ModelSettings…},
 "env_sha256":"<hex>","link_mode":"real","provider":"openrouter","workspace_id":null}
```
`endpoint` = `transport_key(routing_endpoint())`: scheme and host lower-cased, default port
(`:443` https, `:80` http) dropped, trailing `/` dropped from the path, query kept verbatim, fragment
dropped; `None` (not B) when the authority has userinfo or there is no `://`. `env_sha256` = SHA-256
(`sha2`) of `serde_json::to_vec(BTreeMap)` of `profile.env` minus `ANTHROPIC_BASE_URL` and minus a
blank-trimmed `ANTHROPIC_API_KEY`. `workspace_id` stays `null` until P4-OR (None-tolerant).

### 3.3 Helper ack `live_sessions/<sid>.helper`
`{"version":1,"generation":N,"member":"<p>","served_at_ms":T,"last_failure":null|{"generation":M,"code":"<code>","at_ms":T2}}`,
replaced only by rename of `<sid>.helper.tmp.<pid>`; torn, foreign or other-version reads as "no ack".
`generation`/`member`/`served_at_ms` describe the last **successful** run and never regress.
`last_failure` records the newest failed run (codes: `no_row`, `config_unreadable`, `no_key`,
`invalid_key`, `stdout_write`) and is cleared by a success at a generation ≥ its own. Lock
`<sid>.helper.lock`: empty, `open_state_file`, **never renamed**, removed only by teardown or GC (§4.8).

### 3.4 Relaunch files (§4.9)
`<sid>.relaunch` request `{"version":1,"target","conversation","cwd","follows_chain",
"requested_at_ms","requester_pid"}` (no argv: the supervisor uses its own in-memory claude args, so a
request file can never inject arguments); claimed by rename to `<sid>.relaunch.taken`, which the
supervisor then rewrites (atomic 0600) with an added `"nonce":"<32 hex>"` (§4.9); cancelled by rename to
`<sid>.relaunch.cancel`; `<sid>.relaunch.result`
`{"version":1,"outcome":"refused"|"relaunching","reason":str|null}`.

### 3.5 Other
- Runtime `profiles/<p>/runtime-<sid>/settings.json`: B writes `apiKeyHelper =
  "<exe> __tollgate-api-key --session <sid>"`; `env` never carries `CLAUDE_CODE_API_KEY_HELPER_TTL_MS`.
- `conversations/<conv>.json` (`hook_note::NoteRecord`, `hook_note.rs:196`): new
  `runtime_sid: Option<String>`, written by every main-scope hook fire from `CLAUDE_CONFIG_DIR`.
- `cc-version.json` `{"version":1,"path","mtime_ns","len","cc_version":"2.1.283"}`: cache of
  `plugin_probe::cc_version()` (`plugin_probe.rs:285`) keyed on the resolved `claude` binary.

### 3.6 Gate file `docs/spikes/s1-apikeyhelper.md`
Compiled in with `include_str!` by `src/hot_swap.rs`. The doc is the committed spike evidence
(`60e4aee5`); Part 1 **appends** one machine block after its final `S1 RESULT: PASS` line and changes
nothing else (anything outside the block is prose):
```
<!-- tollgate:s1-gate
result = "PASS"                     # PASS | FAIL | PENDING
claude_code = ["2.1.283"]           # exact versions the harness passed on
date = "2026-09-29"
commit = "60e4aee5"                 # the spike commit whose evidence this block summarises
g_real_endpoints = "not_run"        # S1(g) owner-run half; gates the P4-OR preset only
gateway_precondition = "not_run"    # forceLogin* / apps-gateway precondition
ttl_tested_ms = [3000, 300000]      # TTLs the harness exercised (a_ttl, a_default)
-->
```
Parsed as TOML; a missing or malformed block reads PENDING. B is on iff `result == "PASS"`, the
installed version's first token is in `claude_code`, and `TOLLGATE_HOT_SWAP` is not `off`.
`g_real_endpoints` and `gateway_precondition` are recorded for the owner and never read by the B
decision (the `gateway_policy` refusal in §4.1 covers the gateway case). A new CC version is enabled by
re-running `tools/spikes/s1/run.sh` against it and adding the version to `claude_code`.

## 4. Algorithms

Lock ranks (`src/lockorder.rs`): `Rotation` 100 (`:117`), `State` 500 (`:191`), `SwapCell` 550 (`:205`),
**new `HelperAck` 1950** (after `GatewayPublished` `:256`; a leaf only the helper process takes). B never
takes `Rotation`. `with_state_lock` is re-entrant (`lock.rs:440`): `update_as_*` inside a hold take no
second flock; waits are bounded at 25 s (`lock.rs:47`).

### 4.1 Executor choice at spawn (inside `acquire_synced`'s state hold, `runtime.rs:3981`)
`ProfileRuntime::acquire` gains `launch: LaunchInfo { claude_args, spawn_cwd, hot_swap:
HotSwapPolicy::{Allowed, Never} }`; `start::run` passes `Allowed`, the MCP delegate
(`mcp/mod.rs:3651`) `Never`, codex (`runtime.rs:7353`) nothing.
1. After `fresh = load_profile(name)` and `mode = detect_link_mode(..)`:
   `choice = hot_swap::choose(&fresh, mode, isolation, policy, child_env_snapshot)`:
   - `fresh.is_oauth()` → `oauth`.
   - Not API-key shaped (`!has_usable_api_key`) → `oauth` (today's refusals apply).
   - Else first failing check → `relaunch_only(code)`, in this order: `delegate`, `kill_switch`,
     `isolated`, `fake_links`, `hybrid_oauth_store` (any of `credential_fingerprint`'s three files,
     `claude.rs:161`), `no_endpoint`, `endpoint_userinfo`, `loopback_endpoint` (host `localhost`,
     `127.0.0.0/8`, `::1`: covers the shunt `127.0.0.1:3001` and the Ollama daemon), `auth_env`
     (non-blank `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY` in `profile.env`), `cloud_env`
     (non-empty `CLAUDE_CODE_USE_BEDROCK|VERTEX|FOUNDRY` in profile env or the inherited env),
     `gateway_policy` (`forceLoginMethod|OrgUUID|GatewayUrl` in the base `settings.json` or the
     managed settings file), then the gate (`cc_version_unknown`, `s1_gate_pending`,
     `s1_gate_version`).
   - All pass → `api_key` with `LaunchClass`.
2. `build_runtime_dir_with_active_env` passes `HelperForm::Session(&session)` for `api_key`, else
   `HelperForm::Profile` (today's bytes).
3. `LiveSession::starting` records executor, class, `key_generation = Some(0)` and
   `current_member = Some(start)` for B, `relaunch_capable = true` (start only), and
   `relaunched_from`. The claude args stay in `start::run`'s memory (§3.1).
4. If `register` fails and the choice was `api_key`: rewrite settings with `HelperForm::Profile`
   (same `write_merged_settings`, still in the hold) and downgrade the in-memory executor to
   `relaunch_only("registry")`: a session helper with no row would print nothing.
5. `SessionSwap::new` (`runtime.rs:3018`) stores `executor` and `launch_class`.

`GateStatus` is computed in `acquire` before the rotation guard, outside every lock, and passed in:
stat the resolved `claude`, reuse `cc-version.json` on an equal `(path, mtime_ns, len)`, else run
`cc_version()` once and rewrite the cache (only for an API-key-shaped `Allowed` launch).

### 4.2 Child env (`start.rs:327-340`)
`scrub_profile_env` (`runtime.rs:4470`) also removes `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` (new
`SESSION_SCOPED_ENV_KEYS`, not added to `MANAGED_ENV_KEYS`, so settings sync is unaffected). After the
scrub and before spawn: if `runtime.executor() == api_key`, `command.env(TTL_KEY, "30000")`.

### 4.3 Settings writer
`claude::build_claude_settings_json` (`claude.rs:2444`) delegates to a new
`build_claude_settings_json_with(base, profile, prev, HelperForm)`; `build_api_key_helper_command`
(`:2297`) gains the session form `format!("{} {} --session {}", q(exe), q(SUBCMD), q(sid))`.
`write_merged_settings` (`runtime.rs:5425`) and `build_runtime_dir_with_active_env` (`:4736`) take
`HelperForm`. The global `apply_profile_to_claude_settings` stays profile form. `apiKeyHelper`
stays in `PER_PROFILE_TOP_FIELDS` (`settings_sync.rs:74`). `claude::helper_target(&str) ->
Option<HelperTarget::{Profile(name), Session(sid)}>` replaces `profile_name_from_helper`
(`claude.rs:2114`) as the parser (exe check unchanged; `--session` must be followed by exactly one
`is_session_id` token and nothing else); the old name remains a wrapper resolving `Session` via a
lock-free `live_sessions::get` (`current_member` or `start_profile`).

### 4.4 Request core `sessions_cli::request_session_switch(sid, p, surface, wait) -> RequestOutcome`
Shared by CLI, TUI and MCP.
1. Row lock-free (`live_sessions::get`); missing / dead / codex / hermes as today (`sessions_cli.rs:198-243`).
2. Resolve `p` (`resolve_profile_name`). Already current → `AlreadyOn`.
3. Branch on `row.executor`:
   - `oauth`/absent: `update_as_daemon(set_intended_member)` exactly as today → `IntentRecorded`.
   - `relaunch_only(code)` → `RelaunchRequired(code)`; nothing written.
   - `api_key`: pre-check with `load_profile_read_only(p)` (new `pub(crate)` wrapper over
     `LoadMode::ReadOnly`, `profile.rs:3385`) and `hot_swap::class_matches(&row.launch_class, &target)`;
     a mismatch → `Refused(code)`, nothing written. Else `update_as_daemon(set_intended_member(p))`
     (State, one row rename), `t0 = now`.
4. Poll the row every 100 ms for 5 s: `current_member == p && key_generation > g0` → `Committed(N)`;
   `swap_refusal.member == p && at_ms ≥ t0` → `Refused(code)`. Timeout → `Requested`.
5. `wait`: poll `SwapView` every 250 ms until `served.generation ≥ N` (`Served`) or 65 s (`exit 3`).

### 4.5 Executor B (`SessionSwap::poll`, `runtime.rs:3113`)
Dispatch is the first statement: `if let Executor::ApiKey(class) = &self.executor { return
self.poll_api_key(class); }`. The rest of `poll`, `poll_converge`, `precondition`, `swap_to`,
`converge_in_place` is untouched and unreachable for B; B code never calls them, and A never calls
B (the arm is keyed on the spawn-time value, never re-derived).

`poll_api_key`:
1. `row = live_sessions::get(sid)`; `intended = row.intended_member` filtered `!= self.member()`;
   none → return (**no converge leg**).
2. Pre-check outside locks (`load_profile_read_only`, `class_matches`, usable key, not disabled):
   a refusal → `refuse(intended, code)` (below) and return.
3. `with_state_lock` (State 500):
   a. `shutdown.is_begun()` → refuse `shutting_down`. Re-read the row; `intended_member` changed →
      return `Ok` (next tick decides).
   b. Revalidate from disk in the hold: `profile::is_configured(intended)`,
      `load_profile_read_only(intended)`, not disabled, `class_matches`, `has_usable_api_key`, no
      OAuth store (`credential_fingerprint`), `isolation == Shared`, `swap_support(mode)` Ok
      (`runtime.rs:2562`). Then read the session's runtime `settings.json`
      (`profiles/<start>/runtime-<sid>/settings.json`) and compare its `env.ANTHROPIC_BASE_URL`
      (through `transport_key`) and its model env keys with `launch_class`. CC hot-reloads that `env`
      (S1(c)), so a drift means the session no longer runs the class it launched with: refuse
      `class_differs:endpoint` (or `:models`). An unreadable file refuses the same way.
   c. `paths = SessionPaths::resolve(intended, Shared, sid, mode)`;
      `claim = self.claim_markers(&paths)` (`runtime.rs:3074`; takes `SwapCell` 550 for the
      ownership check only, released before the `open_pid_file` + `try_lock` stamp of
      `profiles/<p>/sessions-<sid>/<sid>`). `Foreign` → refuse `marker_held`.
   d. Commit, fresh row, re-entrant State: `update_as_session(sid, |f| { f.set_current_member(p);
      let n = f.bump_key_generation(); f.set_committed_at(now); f.set_last_swap_at(now);
      f.clear_swap_refusal(); })` → one `atomic_write_600` rename of `<sid>.json`. `Err` → drop the
      stamped marker (release fd, `remove_file`, prune the dir if empty), log, return; nothing is
      published and the intent retries next tick.
      Still inside the State hold, after the row rename: **touch** the runtime `settings.json`
      (`OpenOptions::new().write(true).open(..)?.set_modified(SystemTime::now())`; no content
      write, because `write_merged_settings` skips identical bytes, `runtime.rs:5447-5453`). Any
      `settings.json` change drops CC's cached key, so the next request runs the helper
      synchronously before it is sent (S1(c), spike implication 3). This removes the one-request
      lag of the lazy TTL refresh (S1(a)). A failed touch is logged and does not undo the commit:
      the 30 s TTL stays as the backstop.
   e. Publish, holding `SwapCell` 550 **only here**: `cell.member = p`; push `Stamped` markers onto
      `held` (every marker kept for life, like A, `runtime.rs:2926`); `last_refusal = None`;
      `cell.canonical` untouched. No `RotationGuard`, no store touch, no relink.
4. Log `tollgate: session <sid> committed onto <p> (key generation <n>); swapping… Claude Code picks the new key up on its next request`.

`refuse(member, code)`: `should_announce` dedupe as A; on news, log once and
`update_as_session(set_swap_refusal{member, code, text, now})` (one State hold outside step 3).

### 4.6 Session helper (`main.rs:2213`, `__tollgate-api-key --session <sid>`)
No `load_profile`, no state flock, no network; target < 50 ms.
1. `is_session_id(sid)` (else exit 1, nothing written). `row = live_sessions::get(sid)` (rename-atomic):
   `member = row.current_member.unwrap_or(start_profile)`; `gen = row.key_generation.unwrap_or(0)`.
   **Row missing** (for example `gc_live_session_rows`, `runtime.rs:1697`, reaped it because the
   supervisor was SIGKILLed while its orphan CC keeps running): serve the `member` of the last
   successful ack in `<sid>.helper` at its `generation`. No usable ack: parse `CLAUDE_CONFIG_DIR`,
   which must be exactly `<tollgate_dir>/profiles/<start>/runtime-<sid'>` with a valid profile name and
   `sid' == --session`, and serve `<start>` at generation 0. Neither → failure `no_row`. Without this,
   a helper failure makes CC run on the stale key until a 401 and then send empty credentials (S1(d)),
   which today's profile-form helper never does.
2. Read `profiles/<member>/config.toml` once, `toml::from_str` into `{api_key: Option<String>}`;
   unreadable → failure `config_unreadable`; trim; empty → `no_key`; `validate_api_key`
   (`claude.rs:285`) else `invalid_key`.
3. `write_api_key` (write + flush, `main.rs:2227`); error → failure `stdout_write`.
4. Record (HelperAck 1950): `open_state_file(<sid>.helper.lock)`; `try_lock` up to 10 × 20 ms, else
   skip the record (the next run records). Read `<sid>.helper`.
   - Success: if it parses at `version 1` with `generation ≥ gen` and no `last_failure` at a
     generation ≤ `gen`, write nothing. Else write `{generation: gen, member, served_at_ms: now,
     last_failure: null}`.
   - Failure (steps 1–3): keep the parsed success fields as they are (or `generation 0`, no member,
     when there is no ack) and set `last_failure = {generation: gen, code, at_ms: now}`. An existing
     `last_failure` at a higher generation is not overwritten.
   Writes go to `<sid>.helper.tmp.<pid>` (0600) and `rename` over `<sid>.helper`. Unlock.
5. Exit 0 after a success whatever step 4 did; exit 1 with nothing on stdout after a failure. The
   profile form and `api_key_for_profile` (`main.rs:2247`) are unchanged.

### 4.7 `SwapView::of(row, ack, now)` (one function for every surface)
`committed = (member, key_generation, committed_at)`; `served = ack` success fields (or, when
`key_generation` is 0 and there is no ack, `committed` itself). The refresh is lazy with no timer
(S1(a)), so the state is keyed on **recorded helper runs**, never on elapsed time:
- `requested` if `intended_member` is set and differs from `committed.member`;
- else `served` if `served.generation ≥ committed.generation`;
- else `stalled` if `ack.last_failure.generation ≥ committed.generation` (the helper ran for this
  commit and failed). The watchdog logs once per `(sid, generation)`:
  `tollgate: session <sid> committed to <p> but its key helper failed (<code>); Claude Code keeps the previous key until it is rejected, then reports "Your apiKeyHelper script is failing"` (CC's text, S1(d));
- else `swapping`, with `idle = true` when no helper run has been recorded since `committed_at`. An
  idle session never runs the helper (spike implication 2), so `swapping (idle)` gets **no** warning,
  however long it lasts.

A and `none` rows: `served = committed`, never `stalled`. `LiveTally` (`live_sessions.rs:257`) adds
`MemberSessions.swapping`. **Every attribution surface uses `served.member`**, never
`committed.member`: until the helper serves, CC's requests still carry the previous member's key
(S1(d)), so pane attribution (`daemon/api/panes.rs:296`), `LiveTally` member counts, the herdr tag's
account line and `which` inside a B session (`which.rs:327`) all name the served member. `committed`
is shown only as the `→ <committed> swapping…` part.

### 4.8 Teardown and GC
`Drop for ProfileRuntime` (`runtime.rs:4388`), inside its single hold: after `unregister`, remove
`<sid>.helper`, `<sid>.helper.lock`, `<sid>.relaunch*` (NotFound ignored; on the relaunch exit path
`<sid>.relaunch.taken` is kept for the new process's nonce check, §4.9); `release_swapped_markers`
releases B's markers as A's. `gc_live_session_rows` (`runtime.rs:1697`) also removes sidecars whose
stem has no row or whose row is dead, with two exceptions: `<sid>.helper` / `.helper.lock` stay while
a `profiles/*/runtime-<sid>` dir still exists (an orphan CC may still run the helper, §4.6 step 1), and
`<sid>.relaunch.taken` goes only when older than 5 min. `list()` today skips sidecars only because they
fail to parse as a row; it now filters explicitly to `*.json` entries whose stem passes
`is_session_id` (`live_sessions.rs:331`).

### 4.9 Relaunch in place (P6b)
**CLI side** (`--relaunch`), before touching the session:
1. Any executor (A, B or relaunch-only). Row checks as §4.4.1; `harness == Claude`; `isolated` → refuse `isolated sessions relaunch empty; resume it with 'tollgate resume'`; `!relaunch_capable` → refuse `the session predates relaunch; exit and 'tollgate start <p> -- --resume <conv>'` (no herdr fallback).
2. `admit(config, p, Shared, row.follows_chain)` (`start.rs:149`): its message is the refusal.
3. Conversation: `--conversation` (must exist in the session's store) else the `conversations/*.json`
   main-scope records with `runtime_sid == sid`; none → transcript scan of
   `<store>/projects/<encoded cwd>/*.jsonl` with mtime ≥ `started_at` (store = guest store in guest
   mode, else `~/.claude/projects`). Exactly one → conv; 0 → `no conversation found for the session`;
   > 1 → `<n> conversations match; pass --conversation <id>`.
4. `cwd` = `row.cwd` must be a dir. The CLI never sees or sends the claude args.
5. Confirm (§2.1). Write `<sid>.relaunch` (atomic 0600).
6. Wait ≤ 30 s for `.relaunch.result`: `refused` → reason, exit 1; unclaimed → rename to `.relaunch.cancel`
   (success = nobody claimed), `the session did not answer; it is unchanged`, exit 1.
7. `relaunching` → wait ≤ 60 s for a live row with `relaunched_from == sid` → success line; else exit 1,
   `the session stopped but its relaunch has not registered yet; check 'tollgate sessions'`.
**Supervisor side** (`start::run`, `wait_for_child` `start.rs:488`). At the top of `start::run`,
`TOLLGATE_RELAUNCHED_FROM`, `TOLLGATE_RELAUNCH_FALLBACK` and `TOLLGATE_RELAUNCH_NONCE` are read into
locals, and all three are `env_remove`d from every child command (added to the `scrub_tollgate_homes`
set, `runtime.rs:373`). A CC child therefore never inherits them, and a nested `tollgate start` from
that CC's Bash tool sees none. Before spawning claude, `start::run` saves the terminal state
(`tcgetattr` on the controlling tty, when stdin is a tty). Every 10th 50 ms iteration it tries
`rename(<sid>.relaunch, <sid>.relaunch.taken)`; on success:
1. Parse and re-validate (version, target configured and admitted, cwd, transcript present). Fail →
   write `.relaunch.result {refused, reason}`; keep running. Pass → rewrite `.relaunch.taken` (atomic
   0600) with a fresh 128-bit `nonce`.
2. Write `{relaunching}`; forward `SIGTERM` to the child (`forward_signal_or_warn`, `start.rs:550`);
   wait 20 s, then `SIGKILL`.
3. Transcript flush: poll the transcript's `(mtime, len)` every 250 ms until two equal reads (≤ 2 s).
4. Stamp sessions and `drop(runtime)` exactly as today's exit path (`start.rs:370-396`), keeping
   `<sid>.relaunch.taken` (§4.8). Restore the saved termios (`tcsetattr`) and write
   `\x1b[?1049l\x1b[?25h` to the tty, so a SIGKILLed CC cannot leave it raw or in the alt screen.
5. Unix `exec` (Windows: spawn and wait, propagating the code) of `current_exe() start
   [--with-fallback] <target> -- <in-memory claude args minus --resume/-r/--continue/-c and their
   values> --resume <conv>` in `cwd`, with env `TOLLGATE_RELAUNCHED_FROM=<sid>`,
   `TOLLGATE_RELAUNCH_FALLBACK=<orig>` and `TOLLGATE_RELAUNCH_NONCE=<nonce>`. The new process
   honours these only when `<sid>.relaunch.taken` parses and its `nonce` equals the variable; it then
   removes `.taken` and records `relaunched_from`. A mismatch or a missing file ignores all three
   (logged).
6. `exec` error → `exec` the same with `<orig>`. The new process, on any error before the child
   spawns, with a nonce-verified `TOLLGATE_RELAUNCH_FALLBACK`: print the error and `exec` the
   original form once, without the relaunch variables. Both failing: stderr `tollgate: relaunch failed; resume with: tollgate start <orig> -- --resume <conv>`, exit 1.
Guest mode: the new start is a guest start; the passthrough `--resume` seeds the guest store
(`start.rs:255`).

## 5. Failure modes and recovery

| Where | What happens | Recovery |
|---|---|---|
| Intent written, session dies | Row stays until GC | GC reaps row + sidecars (§4.8) |
| Crash between marker stamp (3c) and commit (3d) | Process death drops the flock; marker file remains | `prune_stale_sessions`; nothing was committed |
| Commit rename fails (disk full, EIO) | Claim released, nothing published | Next tick retries while intent differs |
| Two `tollgate switch` writers, or intent changed mid-hold | `update_as_daemon` under State, last wins; 3a re-read aborts | Next tick commits the newest intent |
| Daemon decision leg | Never writes a B row (`follows_chain` is false: `start.rs:102`) | New guard in `row_follows_chain_live` skips `api_key` rows anyway |
| Helper exits ≠ 0 / empty | CC keeps the last good key silently, then sends empty credentials after a 401 (S1(d)); the helper records `last_failure` | `stalled` with the code at once (not after a timeout); switch back or relaunch |
| Session idle after a commit | No request, so no helper run (S1(a)) | `swapping (idle)`, no warning; the commit's settings touch makes the next request run the helper first |
| Settings touch fails at commit | Commit stands; CC refreshes lazily | Served within one request after the TTL (30 s backstop) |
| Row reaped while an orphan CC runs (supervisor SIGKILLed) | Helper finds no row | §4.6 step 1: last acked member, else the start profile from `CLAUDE_CONFIG_DIR` |
| Helper killed after flush, before ack | Served, not acked | Next run (≤ TTL) acks; UI conservative |
| Helper killed after ack, before exit | Acked, possibly not served (narrow) | Documented residual; the next run serves the same generation; acks never regress |
| N−1 helper finishes after N; torn/foreign `.helper` | Lock + `generation ≥` check: no write; a torn file reads as no ack | Next helper rewrites a torn file |
| Lock file removed mid-run | Two inodes could let an N−1 write win | Only teardown/GC remove it, and only once the child is gone or the row dead |
| Target's `config.toml` key edited after commit | Helper serves the new key | Intended: config is the source of truth |
| Target endpoint edited after commit | The runtime `settings.json` still carries the launch endpoint, built from the start profile at spawn. CC hot-reloads its `env` (S1(c)), so any rewrite of that file (the commit touch, a settings sync after import) re-applies whatever it then holds | The helper still serves the target's key. The next commit's 3b refuses `class_differs:endpoint` if the runtime env drifted from `launch_class`. Relaunch to change the endpoint |
| Target `--force` deleted | Helper records `no_key` / `config_unreadable` → `stalled`; CC keeps the old key until a 401 | Switch or relaunch; the marker blocks non-force delete/disable |
| Nested `tollgate start` inside a relaunched CC | Relaunch variables were scrubbed from the child env | Starts as a normal session |
| Upstream clauth 0.16.0 running | B reads/writes only `~/.tollgate/live_sessions` and `profiles/<p>` markers; upstream's helper token `__api-key` never parses as ours | none needed |
| `~/.tollgate` on exFAT/SMB; cross-fs | `LinkMode::Fake` → `relaunch_only(fake_links)`; every rename stays in one dir, so no EXDEV | relaunch |
| NFS home | flock advisory only | `TOLLGATE_HOT_SWAP=off` documented |
| Row predates the fields; register failed | Helper uses gen 0 + `start_profile`; §4.1.4 downgrades to the profile helper | session works; relaunch-only |
| Relaunch: supervisor dies after claim | Session gone; request `.taken` left | GC removes; user resumes by printed hint |
| Relaunch: SIGKILL needed | Last message may be partial in the transcript; the tty may be raw or in the alt screen | CC resumes from the last complete line; step 4 restores termios and leaves the alt screen |
| Relaunch: new start refused | `TOLLGATE_RELAUNCH_FALLBACK` restarts the original | printed hint if both fail |
| Relaunch: CLI killed while waiting | Request claimed or not; `.cancel` never written | Supervisor still acts on a claimed request; an unclaimed one is removed at teardown |

## 6. Interactions

- **Guest mode.** API-key profiles are allowed; B and relaunch write only under `~/.tollgate`.
  Settings sync stays off (`settings_sync`; I6 test stands), so the session form never reaches
  `~/.claude/settings.json`. `tollgate switch <sid> <p>` is not a global mutation and is not
  refused; the one-name form keeps `GUEST_REFUSAL` (`identity.rs:114`). Relaunch resumes in the guest
  store. The version probe runs `claude --version` only; `hermes` is never executed.
- **Import lane (R3).** Import's M1 (the process and marker scan, import spec §4.3) refuses while a
  `~/.tollgate/live_sessions` row has a live pid or a `~/.tollgate/profiles/*/sessions-*` marker is
  held (`tollgate_live_session`), and every `tollgate start` supervisor is a blocking tollgate
  process. So no B session spans an import. M5 copies `conversations/` + `session_profiles.json`:
  imported upstream records lack `runtime_sid` and fall back to the transcript scan. M5.2 also copies
  the guest transcript store into `~/.claude/projects`, so a guest conversation stays resumable by
  relaunch after import. The new sidecars live only in `~/.tollgate` and are never import inputs.
  After import (guest off) nothing here changes, except that a settings sync rewrite of a runtime
  `settings.json` is covered by the 3b drift check.
- **Hermes lane (H-*).** Hermes rows are refused by switch with `NON_CLAUDE_SWITCH` until H-4; H-4
  reuses §4.9 with a harness command builder (`hermes --resume <id> --provider <p> -m <model>`, same
  home). Hermes and codex rows derive `executor = none` (§3.1) and never get executor B.
- **herdr.** Tag reads committed + served (§2.5); pane attribution (`daemon/api/panes.rs:296`) moves
  to the **served** member (§4.7) and gains `state`. P7 H4 `tollgate.swap` will call
  `tollgate switch <sid> <p>` inside the popup (it has a TTY for relaunch confirmation).
- **Local API / MCP.** §2.3, §2.4. The MCP `profiles` session scope reports the served member.

## 7. Test plan

Fixtures: `testutil::HomeSandbox` (every test), `ConfigDirSandbox` (MCP `"self"`),
`runtime::with_fake_home`, `set_link_mode_override`, `hold_session_row_marker`; new:
`ApiKeyProfile::write(home, name, base_url, key, env, models)` (roster + `config.toml`),
`S1GateOverride` (cfg(test) replacement of the parsed block), `CcVersionStub` (writes
`cc-version.json` + a stat-matching fake `claude` on PATH), `HelperAckFile` (reads/writes the
sidecar), `FakeSupervisorChild` (`sh -c 'trap "exit 0" TERM; while :; do sleep 0.05; done'`),
`TranscriptFixture` (a project dir with N `.jsonl`). No test runs the real `claude`, `hermes` or
network.

**Part 1** — `tests/inline/hot_swap.rs` (new): 1. `transport_key_drops_default_port_trailing_slash_and_case`
(`HTTPS://OpenRouter.ai:443/api/` → `https://openrouter.ai/api`); 2. `transport_key_keeps_query_drops_fragment_and_refuses_userinfo`;
3. `an_openrouter_helper_profile_on_real_links_chooses_executor_b` (gate PASS stub); 4. `each_relaunch_only_reason_is_recorded_in_check_order`
(table over §4.1); 5. `loopback_endpoints_are_relaunch_only` (`127.0.0.1:3001`, `localhost:11434`, `[::1]`);
6. `a_blank_api_key_env_is_not_auth_env_and_leaves_the_env_hash_alone`; 7. `the_env_hash_ignores_the_endpoint_key_only`;
8. `class_matches_rejects_each_axis_and_tolerates_a_missing_workspace`; 9. `a_pending_or_malformed_gate_block_keeps_b_off`;
10. `a_pass_gate_enables_only_listed_versions`; 11. `the_committed_spike_doc_block_parses` (real `include_str!`: `result == PASS`, `claude_code` contains `2.1.283`, `g_real_endpoints == "not_run"`, and every prose line above the block is byte-identical to `60e4aee5`);
12. `the_cc_version_cache_is_reused_only_on_equal_path_mtime_and_len`; 13. `tollgate_hot_swap_off_forces_relaunch_only`;
14. `swap_view_states_follow_generation_and_recorded_helper_runs` (requested/swapping/idle/stalled/served; gen 0 is served; no state depends on elapsed time);
14a. `an_idle_committed_session_never_reports_stalled` (clock seam advanced 1 h, no helper run); 14b. `a_failed_helper_run_marks_stalled_with_its_code`;
14c. `a_hermes_or_codex_row_has_no_executor` (a legacy row without `executor` reads `oauth` for claude, `none` for codex/hermes; `LiveSessionView.executor` is null).
`tests/inline/live_sessions.rs`: 15. `a_row_predating_the_fields_reads_as_oauth_generation_zero`;
16. `bump_key_generation_is_monotonic_across_fresh_loads`; 17. `a_daemon_write_preserves_every_session_owned_hot_swap_field`;
18. `list_reads_only_session_id_json_stems` (`.helper`, `.helper.lock`, `.relaunch*`, a stray `x.json` all skipped by name, not by parse failure).
`tests/inline/runtime.rs`: 19. `an_api_key_start_registers_executor_b_current_member_and_generation_zero`;
20. `an_oauth_start_writes_byte_identical_settings_and_executor_oauth`; 21. `a_b_session_settings_carry_the_session_helper_and_no_ttl_env`;
22. `a_failed_register_downgrades_b_to_the_profile_helper`; 23. `poll_never_reaches_swap_to_or_converge_for_a_b_session` (cfg(test) leg counters);
24. `poll_for_an_oauth_session_is_unchanged` (every existing A pin passes unmodified);
25. `b_commit_writes_member_generation_and_committed_at_and_publishes_under_the_state_flock` (`lockorder::holds` asserts);
26. `b_claims_the_target_marker_for_life_and_a_swap_back_is_already_ours`; 27. `b_refuses_a_class_mismatch_publishes_nothing_and_records_the_refusal_once`;
28. `b_refuses_a_foreign_marker`; 29. `b_revalidates_in_the_hold_after_a_profile_edit` (seam between pre-check and hold);
30. `a_failed_commit_releases_the_claim_and_leaves_the_row`; 31. `teardown_removes_sidecars_and_releases_b_markers`;
32. `gc_removes_orphan_sidecars_only` (and keeps `.helper` while a `runtime-<sid>` dir exists); 33. `the_ttl_env_is_scrubbed_then_set_only_for_b` (the built `Command`'s env);
33a. `commit_touches_runtime_settings_even_when_bytes_equal` (mtime advances, bytes unchanged, inside the State hold); 33b. `b_refuses_when_the_runtime_settings_env_drifted_from_the_launch_class`;
33c. `no_surface_serialises_launch_args` (a sentinel claude arg never appears in the row, `SwapView`, `LiveSessionView`, pane JSON, MCP payloads or the herdr tag); 33d. `pane_attribution_live_tally_and_the_tag_use_the_served_member`.
`tests/inline/claude.rs`: 34. `the_session_helper_form_parses_to_a_session_target`; 35. `a_session_helper_with_extra_tokens_or_a_bad_sid_is_not_ours`;
36. `profile_name_from_helper_resolves_a_session_target_through_the_row`. `settings_sync.rs`: 37. `a_session_helper_never_syncs_into_the_base_or_a_sibling`.
`tests/inline/cli.rs`: 38. `the_session_helper_prints_the_committed_members_key_and_acks_its_generation`; 39. `an_older_generation_never_overwrites_a_newer_ack`;
40. `a_failed_print_writes_no_ack_but_records_last_failure`; 40a. `a_reaped_row_still_serves_the_last_acked_member` (then the start profile from `CLAUDE_CONFIG_DIR`; a mismatched sid there fails `no_row`); 41. `the_session_helper_takes_no_state_flock_and_calls_no_load_profile` (no `.lock` created; cfg(test) probe);
42. `the_helper_flags_conflict_and_one_is_required`.
`tests/inline/sessions_cli.rs`: 43. `switch_on_a_b_session_prints_committed_then_swapping`; 44. `switch_pre_check_refuses_a_different_class_without_writing_intent`;
45. `switch_on_a_relaunch_only_session_names_the_reason_and_the_relaunch_command`; 46. `switch_wait_exits_3_when_never_served` (and on `stalled`, naming the code);
47. `session_flags_with_one_name_are_usage_errors`. `scheduler.rs`: 48. `the_decision_leg_never_writes_a_b_row`.
`tests/inline/start.rs` (relaunch): 49. `relaunch_refuses_ambiguous_or_missing_conversations`; 50. `relaunch_resolves_preconditions_before_any_signal`;
51. `a_non_tty_relaunch_without_yes_is_a_usage_error`; 52. `the_supervisor_claims_once_stops_gracefully_and_execs_the_resume_form` (exec seam);
53. `a_failed_relaunch_restarts_the_original_profile`; 54. `an_unclaimed_request_is_cancelled_after_30_s` (clock seam);
54a. `relaunch_env_is_scrubbed_from_the_child_and_ignored_without_a_matching_nonce` (the spawned CC env lacks all three; a nested start with forged vars records no `relaunched_from` and never execs a fallback); 54b. `a_relaunch_request_cannot_inject_claude_args` (a `claude_args` key in `.relaunch` is ignored; the exec argv is the supervisor's own);
54c. `a_sigkilled_child_leaves_the_tty_restored` (pty seam: termios equal to the pre-spawn snapshot, `?1049l?25h` written); 54d. `a_row_without_relaunch_capable_refuses_with_the_manual_resume_line`.
`hook_note.rs`: 55. `the_hook_record_carries_the_runtime_sid`. `guest_mode.rs`: 56. `a_guest_b_swap_and_relaunch_leave_every_operator_tree_byte_identical`
(`~/.claude`, `~/.claude.json`, `~/.clauth`, `~/.codex`, `~/.hermes`). `lockorder.rs`: 57. `helper_ack_is_a_leaf_above_every_production_rank`.

**Part 2** — `mcp_switch_tool.rs`: 58. `switch_profile_session_self_drives_b_and_reports_swapping`; 59. `switch_profile_session_on_a_relaunch_only_row_returns_the_command`;
60. `switch_profile_without_session_is_unchanged`. `local_api_routes.rs`: 61. `accounts_list_live_sessions_committed_and_served`;
62. `account_by_id_filters_live_sessions`. `local_api.rs`: 63. `every_get_leaves_the_home_byte_identical_with_hot_swap_sidecars` (extends I3).
`herdr_tag.rs`: 64. `a_swapping_session_tags_the_served_member_then_the_committed_one_swapping`; `herdr.rs`: 65. `the_reporter_passes_the_row_session_to_the_tag`.
`tui_render_overview.rs`: 66. `the_live_cell_marks_a_swapping_session`; `tui_app.rs`: 67. `m_moves_a_live_session_through_the_request_core`.
`tests/dump_openapi.rs`: golden updated for `live_sessions`.

## 8. Implementation slices (each green on nextest + clippy `-D warnings` + fmt)

**Landing order across the three lanes.** A shared prep PR lands first (owned by whichever lane
starts first, and reviewed against all three specs): `Harness::Hermes` + `Harness::ALL`; the
three-roster `validate_profile_name` / `validate_foreign_harness_free`; a named `ExitCode` table in
`main.rs` (`exit_code` maps typed errors to named codes; per-command meanings documented in the wiki,
so `switch --wait` 3 = not served and `import` 3 = blocked can coexist without a clash); the
`claude::helper_target` parser skeleton at `claude.rs:2114` (which both this lane's `Session` form and
import's `upstream_helper_profile` extend); and the shared refusal constant
`sessions_cli::NON_CLAUDE_SWITCH`. Then this lane's Part 1, then import Part 1, then Hermes Part 1.
Each lane's exhaustive `match Harness` sites therefore see `Hermes` from the start, and
`sessions_cli::run_switch` is rewritten once here, with Hermes only swapping its refusal constant. The
`tests/dump_openapi.rs` golden changes only in the Part 2s, one lane at a time.

**Part 1 — foundation** (tests 1–57 and their lettered additions): `src/hot_swap.rs` (class,
transport key, gate + version cache, `SwapView`, refusal codes, `HELPER_TTL_MS`); **append the gate
block (§3.6) to the committed `docs/spikes/s1-apikeyhelper.md`, changing nothing else in it**; row
fields, `SessionFields` setters, executor choice in `acquire`, B in `SessionSwap` (commit touch, 3b
drift check), helper session form + ack/failure record + row-missing fallback, settings writer, env
scrub/TTL, rank `HelperAck`, request core + CLI flags + exit codes, daemon guard, teardown/GC,
`src/relaunch.rs` (CLI and supervisor, relaunch-env scrub + nonce, termios restore, hook
`runtime_sid`). With the block at PASS for 2.1.283, B is live on that version; every other version
registers `relaunch_only(s1_gate_version)` and in-class switches there use `--relaunch`.

**Part 2 — surfaces and docs** (tests 58–67 + golden): MCP `session` arg, local API
`live_sessions`, herdr `--session` + `report-profile.sh`, TUI `…` marker + `m` modal, `which`
served member, `wiki/{Auto-Switch,Guest-Mode,Interface-And-Keys,Herdr-Plugin,Claude-Code-Plugin}.md`,
`docs/agent-api.md`, `CHANGELOG.md`, plan §10 rows P6a/P6b. Completions (`src/completions.rs`, hand
written, all three shells): `switch` gains `--wait`, `--relaunch`, `--yes`, `--conversation`, and its
first positional completes live session ids.

## 9. Decisions (defaults) and open questions

Decided now: TTL 30 000 ms, on the child env only, as a backstop to the commit's settings touch.
`stalled` only on a recorded helper failure for the committed generation; an idle `swapping` never
warns (the plan's "typed warning after 2 × TTL" would fire on every idle session, S1(a)). CLI waits 5 s
for commit; `--wait` waits 65 s. Exit 3 = committed-not-served at timeout or stalled. B requires `Real` +
`Shared`; isolated, delegate and codex are never B. Kill switch `TOLLGATE_HOT_SWAP=off` (no force-on).
Gate = exact version list in the compiled spike doc; version cached by `(path, mtime_ns, len)`.
Transport key as §3.2; loopback endpoints excluded; `workspace_id` None-tolerant. B loads profiles
read-only and never writes a profile. Refusals published to the row (`swap_refusal`). Helper ack lock
wait 200 ms, then skip. New rank `HelperAck = 1950`. Row fields additive, no schema bump; sidecars
`version: 1`; local API additive under `SCHEMA_VERSION = 1`. Relaunch: confirmation always (TTY or
`--yes`), MCP/TUI never relaunch, isolated refused, `follows_chain` kept, SIGTERM then SIGKILL at
20 s, transcript stable ≤ 2 s, claim by rename, 30 s claim / 60 s re-register deadlines, fallback to the
original profile once. Conversation id: hook record, then transcript window, then refuse. No herdr
`pane run` fallback for pre-relaunch rows: they refuse with the manual resume line. Attribution uses
the served member everywhere.

| Deviation from the plan | Why |
|---|---|
| P6b ships in Part 1, before R3b (plan §5: P6b depends on P6a **and** R3b) | R3b supplies imported upstream `conversations/` records. P6b resolves the conversation from tollgate's own hook records (`runtime_sid`) and falls back to the transcript scan, so pre-import and guest sessions relaunch without them. Imported records only add a faster lookup for sessions that predate the import |
| Stall warning is failure-driven, not 2 × TTL | S1(a): lazy refresh, no timer (above) |
| Commit touches the runtime `settings.json` | S1(c) and spike implication 3: turns the lazy refresh into a synchronous one on the next request |
| Review item 11, rejected in part: no `<sid>.args` sidecar | The supervisor's in-memory argv is the only relaunch input, and `.relaunch` no longer carries argv, so no reader of a sidecar would remain. Writing the argv (which can carry `--mcp-config` / `--settings` tokens) to disk with no reader only adds exposure |

Open questions for the owner:
1. Allow B for isolated sessions (per-session tree under `Real`; the plan refuses today)?
2. Is `forceLogin*` presence enough to detect a Claude apps gateway session, or should S1 add (j)?
3. Should a TUI/MCP relaunch exist later (it kills the calling MCP server when targeting `"self"`)?
4. Windows relaunch keeps the old supervisor as parent (no `exec`): acceptable?
5. After P4-OR, may a different-account OpenRouter swap stay B, or must it confirm like relaunch?

## 10. Code anchors to change

| File:line | Change |
|---|---|
| `src/lockorder.rs:256` | add `HelperAck = 1950` after `GatewayPublished` |
| `src/live_sessions.rs:40-131` | row fields (§3.1, no argv); `starting` takes executor, class, cwd, relaunch fields; `LiveSession::executor()` derives a missing value from `harness` |
| `src/live_sessions.rs:150-181,191,257,319,331` | `SessionFields::{bump_key_generation, key_generation, set_committed_at, set_swap_refusal, clear_swap_refusal}`; `MemberSessions.swapping` and served-member counts; `unregister` removes sidecars; `read_helper_ack`; `list()` filters to `is_session_id` `*.json` stems |
| `src/runtime.rs:336,1697,2562` | `sid_of_runtime_dir_name` reused (MCP `"self"`, `which`); GC sidecars; `swap_support` reused unchanged |
| `src/runtime.rs:2926,2992,3018,3113` | `SessionSwap.{executor, launch_class}`; dispatch line; new `poll_api_key` / `commit_api_key` (row rename, then runtime `settings.json` touch) / `refuse_api_key` |
| `src/runtime.rs:2704-2708` | fix the stale `LaunchTransport` doc: runtime `settings.json` `env` hot-reloads (S1(c)); it is not applied at startup only |
| `src/runtime.rs:373` | `scrub_tollgate_homes` also removes `TOLLGATE_RELAUNCHED_FROM`, `TOLLGATE_RELAUNCH_FALLBACK`, `TOLLGATE_RELAUNCH_NONCE` |
| `src/runtime.rs:3887,3950,3981-4120,4184` | `LaunchInfo`; executor choice; `HelperForm`; register downgrade; `ProfileRuntime::executor()` |
| `src/runtime.rs:4388-4430,4452-4477,4736,4815,5425` | teardown sidecars; `SESSION_SCOPED_ENV_KEYS`; `HelperForm` through the build and `write_merged_settings` |
| `src/claude.rs:2114,2297,2444-2541` | `helper_target`; session helper form; `build_claude_settings_json_with` |
| `src/cli.rs:292-297,533-543,818-825`; `src/main.rs:277-280,346,2213-2270` | switch flags; helper `--session`; herdr tag `--session`; session helper body + ack |
| `src/sessions_cli.rs:25-33,198-266`; `src/start.rs:271-340,370-396,488-550` | exit-code doc gains 3 (via the named `ExitCode` table); request core, messages, `NON_CLAUDE_SWITCH`; `LaunchInfo`, TTL env, relaunch env read-and-scrub at the top of `run`, termios save/restore, relaunch claim/nonce/stop/exec, fallback |
| `src/mcp/mod.rs:721,1034-1130,3651`; `src/mcp/render.rs:1145` | `session` arg and arm; delegate `HotSwapPolicy::Never`; prose |
| `src/profile.rs:3385-3397`; `src/usage/scheduler.rs:1693` | `load_profile_read_only`; skip `api_key` rows in the decision leg |
| `src/hook_note.rs:196,1405-1430`; `src/which.rs:327` | `runtime_sid`; served member for a B runtime |
| `src/local_api/routes.rs:243-258,416,437`; `src/daemon/api/panes.rs:165,296` | `live_sessions` in both bodies (`executor` null for non-claude rows); `PaneSession.state`; attribution by `SwapView.served.member` |
| `src/completions.rs` | `switch` flags `--wait --relaunch --yes --conversation` (bash, zsh, fish) |
| `src/main.rs:195-222` | `exit_code` through the named `ExitCode` table (prep PR) |
| `src/herdr/tag.rs:96,272`; `herdr-plugin/report-profile.sh:141,294` | swapping text and `--session`; the reporter passes the row's sid |
| `src/tui/render/overview.rs:720`; `src/tui/app.rs` key map | `…` marker; `m` modal |
| new `src/hot_swap.rs`, `src/relaunch.rs`, `tests/inline/hot_swap.rs` | as §8 |
| `docs/spikes/s1-apikeyhelper.md` (end of file) | append the §3.6 gate block only |

## Review log

Critique of 2026-09-29, each item verified against the code and the committed spike before editing.

| # | Sev | Item | Outcome |
|---|---|---|---|
| 1 | BLOCKING | Gate not wired to the S1 PASS; Part 1 would overwrite the PASS doc | **Applied.** Confirmed: `docs/spikes/s1-apikeyhelper.md` (60e4aee5) has no `tollgate:s1-gate` block. §1, §3.6 (PASS block with `commit`, `g_real_endpoints`, `gateway_precondition`, `ttl_tested_ms`), §8 (append only), test 11, `s1_gate_version` text |
| 2 | BLOCKING | Stall model and copy contradict S1(a)/(d) | **Applied.** Commit touches the runtime `settings.json` (`write_merged_settings` skips equal bytes, `runtime.rs:5447-5453`); new copy; helper records `last_failure`; `stalled` only on a recorded failure; `swapping (idle)` never warns; TTL kept as backstop; tests 14a, 14b, 33a |
| 9 | IMPORTANT | "Endpoint baked into the child env" is false | **Applied.** Confirmed: `build_claude_settings_json` writes `env.ANTHROPIC_BASE_URL` (`claude.rs:2470`), and the `LaunchTransport` doc (`runtime.rs:2704-2708`) is stale. §5 row fixed, doc fix in §10, 3b drift check, test 33b |
| 10 | IMPORTANT | Session helper fails when GC reaps the row | **Applied.** Confirmed: `gc_live_session_rows` reaps on an unheld marker (`runtime.rs:1697-1706`). §4.6 step 1 fallback; GC keeps `.helper` while `runtime-<sid>` exists; test 40a |
| 11 | IMPORTANT | Claude argv in the row / `.relaunch` | **Applied in part.** `launch_args` dropped from the row and `claude_args` from `.relaunch`; the supervisor uses its in-memory args; test 33c, 54b. The proposed `<sid>.args` sidecar is **not** added: once `.relaunch` carries no argv, nothing would read it, and an unread 0600 file of possibly token-bearing args is only extra exposure |
| 12 | IMPORTANT | Relaunch env inherited by the child | **Applied.** Read into locals and scrubbed from every child (`scrub_tollgate_homes`); honoured only with a nonce matching `.relaunch.taken`; test 54a |
| 13 | IMPORTANT | Hermes/codex rows read as `executor: oauth` | **Applied.** Missing `executor` derived from `harness`; `none` kind; `LiveSessionView.executor = null`; test 14c |
| 7 | IMPORTANT | Cross-lane process/row rules and M1/M2 naming | **Applied.** §6 import bullet now cites import's M1 and its new `tollgate_live_session` blocker (the import spec now defines M1/M2) |
| 17 | IMPORTANT | Cross-lane merge hazards | **Applied.** §8 landing order and prep PR (named `ExitCode` table, `Harness::ALL`, `helper_target` skeleton, shared refusal constant); completions added to §8 and §10 |
| 18 | MINOR | Hermes-row switch text | **Applied.** `sessions_cli::NON_CLAUDE_SWITCH`, text owned by the Hermes spec |
| 19 | MINOR | P6b before R3b unrecorded; herdr `pane run` fallback | **Applied.** §9 deviation row; step 8 dropped, pre-relaunch rows refuse with the manual resume line; test 54d |
| 20 | MINOR | Attribution uses the committed member | **Applied.** §4.7, §2.5, §6: every attribution uses `served.member`; test 33d, 64 |
| 21 | MINOR | `list()` skips sidecars only by parse failure | **Applied.** explicit stem filter; test 18 |
| 22 | MINOR | SIGKILL leaves the tty raw | **Applied.** termios save/restore plus `\x1b[?1049l\x1b[?25h`; test 54c |
