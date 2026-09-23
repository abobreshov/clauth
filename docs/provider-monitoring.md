# Native provider monitoring (working branch)

This branch adds opt-in subscription monitoring for the existing native Codex,
Grok and Antigravity (`agy`) logins alongside clauth's existing Claude profiles.
It does not rotate native refresh tokens or implement cross-tool handoff yet.

```sh
clauth providers init
clauth providers refresh
clauth providers --json
clauth providers start codex
clauth providers start grok --herdr
```

`init` creates `~/.clauth/providers.toml` privately and never overwrites it.
`clauth providers example` prints the schema. The scheduler that already polls
Claude usage (the daemon, or the TUI when it holds the usage-fetch lease) also
polls enabled targets. The Providers tab displays cached readings; `r` requests
a refresh. Agents can use the `provider_usage` MCP tool or `providers --json`.
The existing `status --json` contract gains additive `provider_accounts` data.

## Refresh cadence

Native targets refresh the same way Claude accounts do:

- The scheduler polls them on the Config tab `refresh` interval (90 s by
  default), floored at 30 s and capped at 1 h. `poll_interval_seconds` in
  `providers.toml` now paces only `clauth providers status`.
- After a failed check the next one waits the interval doubled per consecutive
  failure, up to 16 times and never more than 1 h. A forced refresh skips the
  wait, except after a 429: rate-limit backoff is always respected.
- In the TUI a reading goes stale when a Claude reading on the same interval
  would (`2 × max(interval, 5 min) + interval`), and never sooner than
  `stale_after_seconds`. `providers --json`, MCP and `status --json` keep
  `stale_after_seconds` alone.
- `r` on the Providers tab forces every enabled target. `r` on a Grok or
  Antigravity row of the Usage tab forces that target. `r` on Overview forces
  every target along with every Claude and Codex account. A forced refresh runs
  in the TUI process even while the daemon holds the usage-fetch lease; the
  per-target file lock keeps two processes from checking the same login at once.
  A request that arrives while a refresh pass is running is served by the pass
  after it.

Codex accounts that clauth manages (`clauth login <name> --codex`) are polled by
their own leg on the same interval, independently of any `codex` monitor target.
`r` on a Codex row queues that account for the scheduler's next tick; only the
lease holder polls, so a TUI standing down behind the daemon drops the request
and shows the daemon's readings. Each successful poll stamps the reading's
`fetched_at`, which dates it for the stale cue and the published feed.

## In the TUI

A Grok or Antigravity monitor is an account on Overview, Usage and Setup only
after `n` or `+ add account` lists it (`listed = true` in its target). The
Providers tab shows every enabled target, listed or not.

Overview rows use the Claude columns. A shared window of about five hours fills
the 5h column and a shared weekly window fills the 7d column; per-model and
monthly quotas stay off the row. The type cell is the provider's own plan word:
Codex's raw plan (`prolite`, `plus`, `pro`), Grok's `subscriptionTier`, and
Antigravity's paid tier name, otherwise its current tier. The `●` marks the
login the official tool is signed in as. For Codex this is the roster's
`active_profile`, or, when that marker is absent, the profile that owns the
operator `~/.codex/auth.json` symlink. For Grok and Antigravity it marks a
login only when it is the one listed login of its provider.

The Usage pane for these accounts has the Claude layout: a `plan` row, the
`status` block (spinner while queued or fetching, `refresh in`, `[ failed ]`,
`[ rate limited ]` with the retry ordinal, `[ stale ]`, and a pill for a dead
login), then one bar per shared window with its pace marker and reset time. A
provider with one pool per window gets `5h` and `7d` bars; Antigravity's
separate pools read `5h gemini`, `7d claude and gpt` and so on. Model-scope
buckets are not drawn as bars, because they are readings mapped onto those
pools rather than budgets of their own. Grok's product attribution is a
`products` row; a Codex account's spare limit resets are a `resets` row.

Setup opens these accounts like a Claude account, with rows for the actions
clauth can take on a login it does not own. A Codex account has `re-login`,
which captures the login `codex login` left in `~/.codex` (the same capture as
`clauth login <name> --codex`), and `delete account`. A Grok or Antigravity
login has `remove from overview`, which clears `listed` and keeps the monitor
target. Both delete rows arm on the first press.

## Interpretation and boundaries

- All quota windows actually returned by the provider are preserved. Not every
  provider returns a complete model catalog or an explicit model-to-pool mapping.
- Grok product contributions share one consumer allowance; they are not separate
  quotas. Antigravity's shared pools and model observations remain distinct.
- Unknown, stale, auth-required and rate-limited are distinct states. Missing
  quota does not mean unlimited. Percentages across providers are not comparable
  quantities of useful work. Warnings are account-wide, not routing verdicts.
- File-backed credentials are rechecked when cached reports are read. A changed
  login invalidates the previous observation. Native Antigravity keyring access
  stays off the render path: its identity is checked before/after a worker fetch,
  and cached reports explicitly flag observation-time-only identity validation.
- Duplicate model targets using the same configured store share cached results,
  polling and backoff. Aliased paths to the same underlying store are not resolved
  into a universal provider account identity yet.
- Codex honors an absolute `CODEX_HOME`; otherwise it uses `~/.codex/auth.json`.
  Grok uses `~/.grok/auth.json`; ambiguous logins need `auth_entry`. Antigravity
  uses the existing Linux Secret Service login or an explicit `auth_file`.
- Native clients own login renewal. Open the official client if monitoring says
  authentication is required. No keyring unlock or credential refresh is attempted.
- Custom auth-file/entry targets are monitoring-only for now. Direct launches
  inherit the caller environment; Herdr launches use the destination shell's
  environment. Launch success is not proof of account/model attribution, managed
  task ownership or a successful handoff. Herdr retains a failed-start pane for
  inspection and does not automatically answer startup approval dialogs.
- Provider quota transports are currently private client endpoints and may change.
  Unexpected payloads fail closed as unavailable/invalid, never as free capacity.

## Durable checkpoint foundation

`clauth tasks` stores a portable task, compacted brief and explicit resource
inventory across CLI processes. Every command returns JSON:

```sh
clauth tasks example task
clauth tasks register --from task.json
clauth tasks example resource
clauth tasks resource TASK_ID --session SOURCE_SESSION_ID --expected-generation 1 --from resource.json
clauth tasks example checkpoint
clauth tasks checkpoint TASK_ID --session SOURCE_SESSION_ID --expected-generation 2 --from checkpoint.json
clauth tasks show TASK_ID
```

Create the input files using the printed templates. Use the real native source
session ID, an existing workspace directory, and the latest generation returned
by each mutation. The checkpoint must preserve every original constraint and
cover every registered resource ID. Resources are append-only declarations in
this first version; their reconnect and cleanup text is never executed.

Records live under `~/.clauth/tasks/TASK_ID/`, with private directory/file modes
on Unix. Writes use a per-task lock, optimistic generation checks and atomic
publication; immutable checkpoint payloads are digest-checked on read. Malformed
input errors do not echo the supplied JSON. Do not include credentials or whole
environment dumps in task input; free text is not automatically secret-redacted.

This is **checkpoint storage, not a completed handoff**. Records explicitly say
`authority_mode: unmanaged_checkpoint_only` and `handoff_ready: false`. The session
argument is a consistency check, not authentication. Neither a session ID nor a
recorded workspace path fences native tools. Saving a checkpoint does not compact
the source automatically, stop a writer, switch provider, or adopt a pane/job.
Resource identities are declared, not live-verified. Workspace fingerprints,
destination sharing policy, native receipts and execution supervision must all
be checked before a managed transfer can be committed.

### Workspace and destination preflight

Add `--capture-workspace` to `tasks checkpoint` to bind the brief to an
observed-stable workspace fingerprint. No source file contents are stored in the
record: it contains a digest, counts, scope and exclusions. `tasks show` remains
a cached read even if the workspace has since moved or become unavailable.

```sh
clauth tasks example policy
clauth tasks policy TASK_ID --session SOURCE_SESSION_ID --expected-generation N --from policy.json
clauth tasks check-handoff TASK_ID --to grok --model ACTUAL_MODEL_NAME
```

The policy template denies all sharing. To explicitly allow a destination, add
an entry such as `{"tool":"grok","models":["ACTUAL_MODEL_NAME"]}` to
`destinations`, and enable the required `share_checkpoint`, `share_workspace`
and, for tasks with resources, `share_resource_metadata` fields. A sole `"*"`
model entry is an explicit all-model rule for that tool; preflight itself always
requires an actual model name. Empty destinations revoke all destination
permission. Policy changes use the same session/generation guards as checkpoints.
Do not treat an agent-authored policy as authenticated user consent: the report
explicitly emits `consent_authenticated: false`.

Preflight re-observes the workspace and returns `workspace_matches_checkpoint`
as true, false, or null (no fingerprint/unavailable), plus the observed task
generation and checkpoint digest. `sharing_allowed` means only that the recorded
policy matches this tool/model and required data scopes. It does not verify an
account, endpoint, native session, user approval or provider-side model access.
No data is sent to a model. `handoff_ready` remains false until native lifecycle
and receipt handling is implemented; command success means a report was produced,
not permission to launch a successor. Any future transfer must recheck the task
generation, destination and workspace immediately before commitment.

For Git workspaces, capture requires the repository root and covers HEAD/branch,
index entries, tracked files (including missing ones), and nonignored untracked
files. It does not stage, commit, stash, run hooks/filters or rewrite the index.
Git-ignored untracked files, Git metadata beyond HEAD/index, and symlink referent
contents are explicitly excluded. Global ignore configuration is disabled so it
cannot silently hide task files. Plain-directory capture includes directory
entries recursively. Symlinks contribute their target text without following it.
The report includes the scope and exclusions; a match is not a claim about excluded
content. Submodules, nested repositories, special files and a workspace containing
its own task journal are currently rejected rather than silently omitted.

Capture is bounded to 10,000 entries, 32 MiB per file, 256 MiB total content and
128 directory levels; Git output is capped at 8 MiB per command with a 5-second
command deadline. Two matching observations establish observed stability only,
not writer shutdown or protection against subsequent changes. Larger or unsupported
workspaces can still save a brief without a fingerprint, but cannot pass the
workspace preflight check.

### Recorded local process control (Linux)

The working branch adds an opt-in local execution scope for a registered task:

```sh
clauth tasks run TASK_ID --target codex --session SOURCE_SESSION_ID --expected-generation N -- NATIVE_ARGUMENTS
clauth tasks execution TASK_ID
clauth tasks stop-execution TASK_ID --execution-id EXECUTION_ID --session SOURCE_SESSION_ID --expected-generation N
```

`run` selects an enabled native target from `providers.toml`, which must match the
task's declared source tool. Custom monitored auth-file/entry targets remain
monitoring-only. It retains the native terminal streams and emits its final local
execution report on stderr; `execution` returns clean JSON separately. Pass any
native resume/session-selection options explicitly after `--`. The registered
session label does **not** force the CLI to resume that conversation, and this
wrapper does not yet verify native session/account identity.

On Linux with a reachable user systemd manager, the supervisor records a unique
scope intent before launch. A bounded startup worker waits until the host binds
the actual boot, unit invocation and cgroup identity and opens the execution gate.
Only then does it execute the configured native command without a shell. The
private execution record contains its arguments, but reports and errors do not
echo them; no environment dump is stored. Do not pass credentials as arguments.

`stop-execution` is an explicit stop request: it can interrupt unfinished work,
including local background writers, whether or not a checkpoint exists. Save and
verify the checkpoint first. The stop validates the recorded identity and targets
only that scope; it never uses a guessed PID/process-group kill or closes unrelated
Herdr panes. A foreground process exiting is not enough: remaining descendants
keep the execution active until the registered cgroup is empty. Resources intended
to survive must already be independently supervised outside that source scope.

Read `execution_id` from `tasks execution` and supply that exact value to stop;
an old request cannot stop a replacement run. Task revision checks are retained
through the launch commitment and shutdown operation. Completed run records are
archived before replacement. A proven spawn failure can be retried; a successfully
spawned but unbound/ambiguous launch stays unknown and requires manual investigation,
not an automatic retry that might duplicate work.

This is **local descendant containment, not full managed handoff**. Processes
started through a shared vendor leader, another Herdr pane, the user service bus,
remote MCP or a remote job may be outside the scope. They still need their own
resource reconciliation. A cgroup does not prevent those escapes or fence a later
arbitrary native resume. Native receipts, ownership transitions, read-only
successor acceptance and automatic quota-triggered transfers remain disabled.
Unsupported platforms/managers and ambiguous identities fail closed; they do not
fall back to an uncontrolled launch. `handoff_ready` remains false.

### Host-observed native session binding (Codex, Grok and agy, Linux)

The working branch can observe an existing registered Grok session:

```sh
clauth tasks bind-session TASK_ID --target grok --session NATIVE_SESSION_ID --expected-generation N
clauth tasks execution TASK_ID
```

The registered source tool must be Grok and its native session ID must already
exist. This command does not create a replacement conversation. It launches the
configured native executable as `agent --no-leader [--model MODEL] stdio` inside
the recorded user scope. Configured extra arguments and custom monitored auth
files/entries are rejected. Only protocol initialization and `session/load` are
sent; no prompt, acceptance request, or handoff notice is sent. Native stderr
is discarded and protocol errors are generic to avoid echoing private content.

Before persisting, clauth correlates the response ID, verifies the expected
session and local execution identity, checks native process membership in the
bound cgroup, and records its PID/start identity. `native_session_observation`
contains the source session ID, host-configured model (when reported), request
ID and observation time. ACP may omit sessionId in its load result, so the
session ID is the exact correlated request's ID, not a newly invented value.
Typed configOptions and legacy currentModelId must agree when both are present.

Protocol reads are bounded to 256 KiB per frame, 4 MiB/256 frames total and a
20-second exchange deadline. Session-history notifications are consumed but
not persisted. The persistence callback uses a separate bounded local lock;
direct-child cleanup waits one second, then kills only that child and checks
exit for up to two seconds. Descendants can remain in the recorded scope:
inspect `recorded_scope_empty` and use the exact `stop-execution` command when
appropriate. Session initialization can start configured MCP services; no
prompt and denied host callbacks do not mean read-only or side-effect-free.

This observation is historical evidence, not live-session ownership, effective
inference-model proof, account attestation, or a portable-context receipt. It
does not change the task generation, ownership epoch, declared owner or sharing
policy. `native_identity_verified` and `handoff_ready` deliberately remain
false. Direct `tasks run` records remain readable without these optional fields.
Native handoff receipt delivery is not yet exposed by this command.

For a registered agy source, use the same command with `--target agy` (or its
configured target ID). The adapter runs canonical
`--input-format=stream-json --output-format=stream-json --conversation ID`
with an optional configured `--model MODEL`. It holds stdin open without writing
any bytes and checks the first native init event against the registered
conversation ID and exact workspace. The same supervisor persists process/scope
identity while the child is checked live. Reads are bounded to one 256 KiB frame
and a 20-second deadline, followed by bounded direct-child cleanup.

agy's optional init model is a CLI-option echo: it is stored as `requested_model`,
never `configured_model`. Missing metadata remains unknown, even when a model was
requested. `request_id` is null because this is an unsolicited stream init event,
not an ACP response. Installed agy selects the requested ID in local state before
stream restoration finishes; init can echo that selection without confirming a
successful load. This observation does not prove saved conversation history
exists or was loaded, account identity, effective inference model, or ownership.
No prompt is sent, and native initialization is not a side-effect-free operation.

For Codex, register the native **thread ID**, not its session-tree root, then use
`--target codex` (or its configured target ID). The adapter runs direct
`app-server --listen stdio://`, initializes the connection and requests
`thread/resume` with the exact thread ID/workspace, `excludeTurns: true`,
`approvalPolicy: never` and the native `read-only` sandbox. It never sends
`turn/start`, thread creation, user input or a model prompt. A configured model
is encoded as a single native `--config` argument and resume override; other
configured arguments remain rejected.

The correlated resume response must report the same thread and workspace,
consistent configured model/provider metadata, and the requested sandbox and
approval policy. `native_session_id` stores the thread ID;
`codex.session_tree_id` separately stores the returned `thread.sessionId`.
`codex.model_provider` is provider configuration, not an authenticated account.
Requested and configured models remain distinct from per-turn inference proof.
The adapter uses the same bounded exchange and direct-child cleanup limits as
Grok. Old Grok/agy records remain readable without the new Codex metadata.

A successful resume observation is not source quiescence, exclusive ownership,
checkpoint acceptance or a whole-host read-only fence. Native configuration may
initialize MCP services or other independently controlled resources. Fresh empty
Codex threads may lack saved rollout history and fail resume; this command does
not silently create a replacement. It remains necessary to inspect and reconcile
the recorded scope and all external resources before transferring a task.

### Durable handoff proposals (working branch)

Once a task has a current workspace-captured checkpoint and a sharing policy,
record an exact transfer intention before any native delivery:

```sh
clauth tasks propose-handoff TASK_ID --request-id handoff-1 --to grok --model MODEL --session SOURCE_ID --expected-generation N
clauth tasks show TASK_ID
clauth tasks cancel-handoff TASK_ID --handoff-id handoff-1 --session SOURCE_ID --expected-generation N_PLUS_1
```

Use the generation returned by the preceding operation, not the literal
`N_PLUS_1`. Request IDs are 1–64 ASCII letters, digits, underscores or hyphens,
scoped to one task. Keep the same ID and original arguments when retrying an
ambiguous response. Exact retries return the current task without advancing its
generation or reopening an old proposal; a reused ID with different arguments
is rejected. Locate the requested entry by its `handoff_id`, not merely the last
entry in the returned history.

Each proposal binds its source identity/epoch, original task generation,
destination tool/model, immutable checkpoint generation/digest, resource revision
and sharing-policy digest. Creation re-observes the workspace and fails if it
differs from the checkpoint. Task locking and a single atomic record replacement
publish the proposal and journal event together. Only one proposal may be active.
History is bounded to 128 proposals; no old entries are silently discarded.

A checkpoint, resource or policy update automatically marks the active proposal
`superseded` at that first change. The source remains free to repair its brief;
a new transfer attempt needs a new request ID. `cancel-handoff` only cancels an
exact active proposal, with actor/generation checks. These are local consistency
checks, not caller authentication. Reads and exact replays return recorded state
without a fresh workspace scan; every future delivery/launch must revalidate it.

`proposed`, `cancelled` and `superseded` describe intentions only. They do not
launch a successor, freeze the source, adopt/release resources, authenticate user
consent, deliver native receipts, set `handed_off_to`/`continued_from`, or advance
ownership. `handoff_ready` stays false. Inert resource reconnect/cleanup text is
never executed. Existing records without proposals remain readable; older builds
that do not know the proposal field will reject a proposal-bearing record rather
than silently ignore its state.

### Read-only resource identities (working branch)

Resource declarations can be checked without stopping, adopting or releasing
anything. The identity capture commands emit just the `native_identity` object
for use in an explicit `tasks resource --from ...` declaration:

```sh
clauth tasks process-identity PID
clauth tasks herdr-identity --socket /absolute/path/to/herdr.sock --pane EXPLICIT_PANE_ID
clauth tasks inspect-resource TASK_ID --resource REGISTERED_RESOURCE_ID
```

These commands do not register resources automatically. Inspection reports the
task generation and resource revision it read, a timestamp, and `identity_state`:
`matched`, `mismatch`, `missing`, `unavailable` or `unsupported`. It does not write
the task journal, advance ownership, or alter an active transfer proposal.
Recheck immediately before a future control action: the report is a historical
observation, not a held lock or authority grant. `control_verified`,
`lifecycle_independence_verified` and `handoff_ready` remain false.

Linux process identities contain `pid`, `start_identity` (Linux starttime ticks)
and a versioned opaque `host` value. The host binds boot ID, the mounted procfs
PID view, and the observer's time namespace. The adapter requires the procfs
view to match the caller's PID namespace, verified through a single `NSpid`
entry equal to the caller's PID. A hidden, missing or multi-level view fails
closed; masked `/proc/1` metadata is not used as namespace proof. Namespace
symlink identities must match their regular-file metadata. Do not replace the
host value with a hostname or `localhost`. Both the process start fields and
host view are re-observed.
Dead/zombie processes are not matched. A `missing` result means not visible or
no longer live in that process view; it is not proof of termination, because
procfs can hide processes. Start ticks have limited resolution and survive
`exec`, so even a match is not an unforgeable lifetime token or permission to
signal a PID. Process arguments and environment are never read.

The Linux Herdr adapter uses an explicit local filesystem socket and pane ID;
there is no UI-focused-pane fallback. It checks the server peer credentials on
the same connection used for `pane.get`, brackets the server process identity
and socket identity, and retains only whitelisted identity fields. The socket
and peer must belong to the current effective user. A nonblocking connection,
bounded response and deadline-checked socket exchange fail closed if the endpoint
is unavailable or malformed. Filesystem resolution, metadata and procfs reads
are synchronous: a stalled filesystem can exceed the socket budget, so this is
not a hard total wall-clock guarantee. Terminal titles, pane tokens, argv and
transcript contents are neither returned nor saved.

For this adapter, `session` is the canonical socket endpoint, not a claimed
Herdr session display name. `instance` binds that local server/socket incarnation;
`socket_path`, `terminal_id`, `pane_id`, `workspace_id` and `tab_id` identify the
observed pane. A server restart, changed peer, socket replacement or pane move
invalidates the old observation. Herdr 0.9.0 has no verified server-incarnation
UUID or compare-and-swap ownership operation in this adapter; a same-user
malicious server, inherited listening socket, or unusual same-process reload is
outside this trust boundary. Kernel peer credentials identify the listener's
credential association, not necessarily the process handling every request.

Legacy free-form identities stay readable but are not guessed into verified
ones. External URIs are `unsupported`: inspection never follows a URI or runs
the declaration's `reconnect` or `cleanup` text. Non-Linux systems fail closed.
These identity checks do not prove successor access, successful reconnection,
survival after source shutdown, or that background writers have stopped.

### Stop-first source-scope release (working branch, not installed)

The approved v1 ordering stops the source before starting a write-capable
successor. The first coordinator boundary is now available for directly managed
Linux executions with **no declared resources**:

```sh
clauth tasks release-handoff-source TASK_ID --handoff-id HANDOFF_ID --execution-id EXECUTION_ID --session SOURCE_ID --expected-generation N
```

Run this from an external coordinator, not from inside the source scope. It
checks the caller's procfs PID view and actual membership at its resolved
cgroupfs path before comparing scope ancestry; namespace-rebased or unprovable
views fail closed rather than being mistaken for an outside coordinator. It
requires a non-threaded `domain` cgroup because threaded-domain membership can
include a subtree rather than identify the caller's exact group. It
requires an active proposal and a workspace-captured checkpoint. Before stopping
anything it atomically journals `source_stop_requested`, binding the exact
execution ID and a digest of the immutable launch/scope identity. This intent
blocks source `run`, `bind-session`, checkpoint/resource/policy mutations,
cancellation and new proposals, even after the coordinator crashes. The command
does not install a detached coordinator or automatically recover in the background.

Only the exact recorded scope is stopped. Positive empty-scope observation is
followed by a fresh workspace comparison. Success journals `source_scope_empty`;
uncertain stopping or changed/unreadable workspace journals `source_release_parked`
with a bounded reason. A parked result is emitted as JSON on stdout with a nonzero
exit status. No files are restored, no arbitrary cleanup text is executed, and
the old source is never automatically restarted.

Retry with the same handoff/execution IDs and either the original pre-intent
generation or the current generation. The coordinator re-observes the exact scope;
it never selects a replacement execution. Repeated identical outcomes do not add
journal events. An interrupted outcome write retains the stop intent for recovery.
Native session-binding observers are rejected: stopping an observer does not stop
the original agent. Declared resources must be classified before the stop intent.
A live Herdr pane has no verified close or ownership compare-and-swap, so it
cannot be released and cannot be given exclusive control. Adopt is only a
non-writing process or pane whose process is outside the source cgroup and whose
parent is outside that cgroup. Release of a live process is allowed only when
that process is inside the source scope, because stopping the scope is the
cleanup. Anything else refuses the stop and leaves the source running.

This is **not a completed handoff**: it does not launch a successor, deliver the
checkpoint, reconcile escaped/remote writers, adopt/release resources, increment
ownership, or establish native receipts. `owner` remains the registered source
identity for history, but managed source mutations/relaunch are frozen.
`handoff_ready` stays false. An empty local scope is not proof that all task writers
have stopped. Arbitrary native launches outside clauth are not fenced.

## Stop-first continuation after the source scope is empty

These commands do not prepare a read-only successor while the source can still
write. They run only after `release-handoff-source` has recorded `source_scope_empty`.

```sh
clauth tasks classify-resources TASK --handoff-id ID --source-cgroup PATH --binding-digest DIGEST --session SOURCE --expected-generation N
clauth tasks reconcile-resources TASK --handoff-id ID --session SOURCE --expected-generation N
clauth tasks continue-handoff TASK --handoff-id ID --target PROVIDER --session SOURCE --expected-generation N
clauth tasks recover-handoff TASK --handoff-id ID
```

`continue-handoff` launches Codex, Grok, or agy with a fixed argument list:
Grok uses `agent --no-leader`, agy uses stream-json without the source
conversation id, and Codex uses `app-server` `thread/start` with `read-only`
sandbox. `restore-code`, approval bypass, and credential switches are rejected.
The compact checkpoint is written and read back before it is sent. Session-linked
receipt sidecars are read back before the ownership epoch advances and before
`continued_from` / `handed_off_to` change the owner. A failed receipt leaves
`receipts_pending` and does not restart the source. `recover-handoff` will
not start a second successor when one was already released.

This is still not quota routing. Herdr cannot fence a later prompt to an adopted
pane. The successor pid is not a cgroup fence. Native transcript files are not
rewritten. A tool or disposition without a verified probe stays fail-closed.
