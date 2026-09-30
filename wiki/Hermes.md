# Hermes

tollgate launches Hermes Agent (Nous Research's agent CLI) under a named profile, the way it launches `claude` and `codex`. A Hermes profile is a whole Hermes home of its own, so several Nous, OpenRouter or Ollama Cloud accounts can be used from one machine, each isolated from `~/.hermes`, `~/.claude`, `~/.clauth` and `~/.codex`.

Requires Hermes Agent 0.19.x, installed with mise (`pipx:hermes-agent`) or pipx, on Linux or macOS. Every `hermes` verb refuses on Windows. The launch guards were verified against 0.19.0, and another version prints a note (or refuses, with `version_policy = "refuse"`).

tollgate never owns a Hermes credential. Hermes is the only writer of its `auth.json` and `config.yaml`, and the only one that refreshes a Nous login. tollgate writes one line of the home's `.env` (the key it bound) and runs Hermes' own `config set` and `auth` for everything else.

## Create a profile

```sh
tollgate hermes new nous-main                        # Nous, OAuth login (the default)
tollgate hermes new or-main --provider openrouter    # prompts for the key, input hidden
echo "$KEY" | tollgate hermes new oll --provider ollama-cloud --stdin
tollgate hermes new or-pool --provider openrouter --pool
```

| Flag | Meaning |
|---|---|
| `--provider nous\|openrouter\|ollama-cloud` | the provider this home is bound to (default `nous`); passed as `--provider` at every launch |
| `--model <id>` | the model, passed as `-m` at every launch |
| `--pool` | a credential-pool home: several keys of one provider, which Hermes fails over between |
| `--env-key` | Nous only: bind `NOUS_API_KEY` instead of the OAuth login |
| `--stdin` | read the key as one line from stdin instead of prompting |
| `--no-key` | create the home now and set the key later with `tollgate hermes key` |

The key is never taken on argv. It is written as one managed line of the home's `.env`, under a comment saying tollgate manages it, and every other line stays yours. The roster (`~/.tollgate/hermes-profiles.toml`) holds only Hermes' own fingerprint of the key (`sha256:` plus 16 hex digits), never the key.

`new` then pins all 15 of Hermes' auxiliary tasks (vision, compression, title generation and the rest) to the profile's provider with `hermes config set`. Hermes' `auto` choice for them can fall through to Anthropic, and a launch refuses while any is unpinned. Each pin checks the child home before starting the projector's Python process, then checks `auth.json` and projected env keys before starting Hermes. An install `.env` also refuses. When a pin fails, `new` prints a command to finish by hand, prefixed with the profile's `HOME=` and `HERMES_HOME=`; when the install cannot be resolved, the command uses `hermes` on PATH.

Outside [guest mode](Guest-Mode), when herdr is installed, `new` also runs `HERMES_HOME=<home> herdr integration install hermes` once, so herdr recognises this home's panes. In guest mode it prints that command instead of running it, because it is not yet confirmed that it leaves herdr's shared config alone.

The next step is printed on the second line: `tollgate start <name>` for an env-mode home with its key, `tollgate hermes auth <name> add nous --type oauth` for a Nous login, `tollgate hermes auth <name> add <provider> --type api-key --label <account>` for a pool.

## Log in and manage keys

```sh
tollgate hermes key or-main                          # replace the env-mode key (prompted, or --stdin)
tollgate hermes auth nous-main add nous --type oauth [--no-browser] [--timeout <s>]
tollgate hermes auth or-pool add openrouter --type api-key --label work
tollgate hermes auth or-pool remove openrouter <target>
tollgate hermes auth or-pool reset openrouter
tollgate hermes pool or-pool strategy round_robin   # fill_first, round_robin, random, least_used
```

`auth` hands off to Hermes' own `hermes auth` with your terminal, and Hermes prompts for a key itself. tollgate never passes a key, a portal URL or a CA bundle on its argv. An account home holds one account of one provider: a second credential, or another provider, is refused. `pool … strategy` runs the full home audit before `hermes config set credential_pool_strategies.<provider> <s>` and applies to pool homes only. Every writing verb waits for an idle home: a live session refuses it.

## Launch

```sh
tollgate start or-main
tollgate start or-main -- chat -q "summarise the diff"
tollgate start or-pool -- --resume <session-id>
tollgate start or-main --explain                     # the audit and the pick line, no launch
```

Everything after `--` goes to Hermes. `-p` / `--profile` (Hermes' own sub-profiles), `--provider`, and a `-m anthropic:…` style model are refused, wherever they sit. An OpenRouter slug such as `anthropic/claude-sonnet-4.5` is allowed, because the provider is pinned. `--isolated`, `--with-fallback` and `--auto` do not apply to a Hermes profile: the home is the account, and Hermes fails over inside its own pool.

One live session per home: a second `start` on the same profile is refused while the first runs. Parallel work uses separate account homes.

### What the launch isolates

The child runs with:

- `HERMES_HOME=~/.tollgate/profiles/<name>/hermes-home` and `HERMES_SHARED_AUTH_DIR` inside it;
- `HOME=~/.tollgate/profiles/<name>/child-home`, a directory that holds only links to `~/.gitconfig`, `~/.config/git` and `~/.ssh` (each only when it exists). Hermes resolves `~` from `HOME`, so its Claude Code credential reader, its `gh` source and its default root all land in a directory with nothing in them. This is what keeps an exhausted OpenRouter home's fallback chain from reading or rewriting `~/.claude/.credentials.json`. Spike S7(f) verified it against the real Hermes 0.19.0 (`docs/spikes/s7f-hermes-home.md`), and a start refuses on a Hermes series the spike has not passed;
- the environment scrubbed of every provider key and base URL Hermes knows (its registry, plus the names its provider plugins declare), `ANTHROPIC_*`, `NOUS_*`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, `GH_TOKEN`, `GITHUB_TOKEN` and the `XDG_*_HOME` directories.

On Linux the child gets `SIGTERM` if tollgate dies, so a killed supervisor never leaves an orphaned Hermes on the home.

### What the launch audits

Before anything starts, with no lock held, tollgate checks the home's shape, `active_profile`, sub-profiles, the pass-through arguments, a `.env` in Hermes' own install, a running session or gateway and the version. Then Hermes' own interpreter reads `config.yaml`, `.env`, `.op.env` and any managed scope with Hermes' own YAML and dotenv parsers, and prints names and hosts only. An anchor, alias or `<<:` merge resolves exactly as it does when Hermes starts. tollgate refuses:

- any route to Anthropic: `model.provider`, a fallback, an auxiliary task, `delegation`, a provider on `api.anthropic.com`, an anthropic entry in `auth.json`, Hermes' own `.anthropic_oauth.json`, or an `ANTHROPIC_*` key in the home `.env`;
- an unpinned (`auto`, empty or unset) auxiliary provider;
- a bulk secrets source (Bitwarden), or a 1Password mapping onto a scrubbed name;
- a `.op.env` that sets anything but `OP_SERVICE_ACCOUNT_TOKEN`;
- a managed scope (`/etc/hermes`, or `HERMES_MANAGED_DIR`) that sets a key or route the profile binds;
- a projector that fails, times out or prints anything but the exact schema: the audit fails closed.

Then, under the profile's lock, it re-checks that none of the four audited files changed since they were read (`home changed during audit; retry` otherwise). A key edited by hand in the home `.env` is accepted and re-attributed: `note — OPENROUTER_API_KEY changed outside tollgate; re-attributed`.

At teardown tollgate refreshes the usage figures, warns when a regular `state.db` shows a call billed to Anthropic during the session (the in-session `/model` picker can reach it, though it has no credentials to use), and warns when `state.db` is a symlink or other non-regular node and could not be checked. It also warns when the child home gained an entry or a non-symlink `~/.claude/.credentials.json` path appeared or replaced a symlink.

## See what you have

```sh
tollgate hermes list [--json]
tollgate hermes show or-main [--json]
tollgate hermes show or-main --check
```

`list` prints each profile's provider, mode, auth, model, a `●` while a session holds it, and this month's spend. `show` adds the home and child-home paths, the credential pool and the five latest sessions, whose ids feed `tollgate start <name> -- --resume <id>`:

```
or-pool  openrouter · pool home · auth pool
  home        ~/.tollgate/profiles/or-pool/hermes-home
  child HOME  ~/.tollgate/profiles/or-pool/child-home
  this month  $4.12 (hermes state.db: billed where known, else estimated)
  pool        openrouter
    #1 work  api_key/manual  ok  req 12  prio 0  fp …cdef
    #2 side  api_key/manual  exhausted until 2026-09-21T13:20:00+00:00  req 3  prio 1  fp …91a0
```

The pool view reads `auth.json` through a whitelist with no field for a token or key, so no secret can reach it, and it shows only the last four digits of each fingerprint. In an env-mode home, the entry that is the key tollgate bound is marked `(the key tollgate bound)`.

`--check` also resolves the Hermes install and runs every launch guard, printing each verdict (`ok`, `ok — <note>`, `REFUSED — <why>`) in launch order and stopping at the first refusal. It exits 1 when a guard refuses, and it writes nothing: a missing child home or a moved key is reported, not repaired. `show` without `--check`, `list`, the daemon, the TUI and herdr never run Hermes, its interpreter or mise.

## Spend

Each profile is a `hermes:<name>` account in `tollgate usage`, the TUI's Usage tab, the [local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md) and the MCP `usage` tool. Its origin is `hermes_profile` and its source `hermes`. The figure is a month-to-date estimate from the home's own `state.db`: Hermes' billed cost where it knows one, else Hermes' estimate, summed exactly per (billing provider, model) since the start of the UTC month. A pool home is exact per home, not per key: Hermes does not record which key served a call.

tollgate reads `state.db` with `sqlite3 -readonly -json` and never opens it any other way, so the `sqlite3` CLI must be on `PATH`: without it the account reads `install sqlite3 to read Hermes' local usage`. The daemon re-reads each home at most once a minute; `hermes list`, `hermes show` and a `tollgate start` teardown read it too. A read that fails keeps the last good figures for 7 days. A Nous cooldown Hermes recorded in `rate_limits/nous.json` shows as rate-limited until it lifts. A `state.db` schema other than the one tollgate knows (22), or a Hermes outside 0.19.x, marks the figures best effort.

## Switch

A Hermes home is the account, so switching is a relaunch:

- an account home: exit, then `tollgate start <other>`. It is a new session, since sessions live in each home's own `state.db`;
- a pool home: exit, optionally `tollgate hermes pool <name> strategy …`, then `tollgate start <name> -- --resume <id>` (or `-- -c`).

`tollgate <hermes-name>` is a usage error saying so, and so are the TUI's <kbd>⏎</kbd> and <kbd>s</kbd> on a Hermes row and the MCP `switch_profile` tool. `tollgate switch <session> …` refuses a Hermes session.

## Everywhere else

| Surface | Hermes |
|---|---|
| `tollgate list` | a `HERMES` section: name, provider, mode, this month's spend, `●` when live |
| `status.json` / `tollgate status --json` | `hermes_profiles[]` with `name`, `provider`, `model`, `mode`, `live`. No active slot: Hermes has none |
| `tollgate which` | inside a Hermes session (a tollgate `HERMES_HOME`), the profile, with `"harness": "hermes"` in `--json` |
| TUI Overview | <kbd>c</kbd> cycles all → claude → codex → hermes; Hermes rows are read-only ([Interface and keys](Interface-And-Keys)) |
| herdr | a started pane through its live session; a bare `hermes` pane through its `HERMES_HOME` ([herdr plugin](Herdr-Plugin)) |
| completions | the `hermes` verbs, and Hermes names after `start` and `delete` |
| `tollgate delete <name>` | resolves claude, then codex, then Hermes; `hermes delete` does the same for Hermes alone |

Names are unique across the claude, codex and Hermes rosters. That includes [`tollgate import clauth`](Import): an upstream profile whose name a Hermes profile holds is refused until you pass `--rename`. Stop every Hermes session before an import, since a live one blocks it. The import never reads, moves or rolls back a Hermes home.

## Delete

```sh
tollgate hermes delete or-main [--yes] [--force]
```

It removes `~/.tollgate/profiles/or-main/` and the roster entry. The child home's links are unlinked, never followed, so `~/.ssh` and `~/.gitconfig` are untouched. A live session refuses without `--force`; with it, the running Hermes keeps its open files while its home is gone.

## Files

| Path | What |
|---|---|
| `~/.tollgate/hermes-profiles.toml` | the roster (0600): provider, model, mode, auth, the key's variable and fingerprint; optional `[settings]` `bin` (the entrypoint) and `version_policy` (`warn` or `refuse`) |
| `~/.tollgate/profiles/<name>/hermes-home/` | `HERMES_HOME`; its contents are Hermes' own, except the one `.env` line tollgate manages |
| `~/.tollgate/profiles/<name>/child-home/` | the child's `HOME`: three allowlisted links, nothing else. A sibling of the Hermes home so `hermes backup` never zips through the links |
| `~/.tollgate/profiles/<name>/hermes_usage_cache.json` | the month's usage rows (0600) |
| `~/.tollgate/profiles/<name>/sessions-<sid>/<sid>` | the liveness marker a session or `auth` holds |

## Finding Hermes

tollgate resolves the Hermes entrypoint at every launch, never caching it, because `mise up` installs into a new directory:

1. `[settings] bin` in `hermes-profiles.toml`;
2. mise's install directory (`$MISE_DATA_DIR`, default `~/.local/share/mise`, `installs/pipx-hermes-agent/<version>/…`), the highest version, read without running mise; only when that holds nothing, `mise where pipx:hermes-agent` run from `~/.tollgate`, so no project `.mise.toml` loads;
3. pipx (`$PIPX_HOME`, default `~/.local/pipx`);
4. `hermes` on `PATH`.

A candidate must be a Python entrypoint of its own virtualenv. A shell launcher, such as Omarchy's self-installing `~/.local/bin/hermes`, is rejected by its first line and never executed: `… is a launcher script, not the Hermes entrypoint; tollgate will not run it (it can install software)`.

## Limitations

- macOS has no parent-death signal, so a Hermes whose supervisor was killed may keep running, and the next start cannot see it.
- A bare `hermes` you run yourself with `HERMES_HOME` set to a tollgate home is invisible to tollgate (unless it runs the gateway). `show --check` notes a `state.db-wal` written seconds ago with no session held.
- OpenSSH takes `~` from the password database, not `HOME`, so Hermes' ssh subprocesses read your own `~/.ssh` regardless of the child home. git reads `~/.gitconfig` through the child home's link.
