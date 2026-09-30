# Spec: Hermes Agent as a third harness (H-1a..H-4)

Status: draft for implementation · 2026-09-29 · base `feat/tollgate` @ c53123a0 (tollgate 0.1.0)
Authority: `docs/multi-provider-redesign-plan.md` v3.1 §4.3, §4.6 (H2, H2h, H4), §4.8, §5 (H-1a..H-4), D1, D9, D11, D12b.
Where this spec deviates from the plan it says so and gives the reason (§9).
Hermes citations are relative to `$HSP` = `~/.local/share/mise/installs/pipx-hermes-agent/0.19.0/hermes-agent/lib/python3.13/site-packages/`,
and were read from the installed 0.19.0 (`hermes_agent-0.19.0.dist-info/METADATA:3`).

## 1. Goal and non-goals

Goal: `tollgate` launches Hermes Agent under a named profile, the same way it launches `claude` and `codex`.
Each profile gets its own isolated `HERMES_HOME` **and its own child `HOME`** (`profiles/<name>/child-home`, §4.4),
so neither tollgate nor the Hermes it launches can reach `~/.hermes`, `~/.claude`, `~/.clauth`, `~/.codex`,
`~/.qwen` or `~/.config/gh`. Hermes' own credential pool, `state.db` usage and rate-limit state show up as a
`hermes:<name>` observation. herdr tags Hermes panes by their home. A manual account switch is a relaunch (H-4).

In scope: `Harness::Hermes` and `HermesEngine`; the `hermes-profiles.toml` roster; the home layout and launch guards;
the env scrub and the env-layer audit; the child `HOME` redirect; the refusal of anthropic on every named route, and
the closing of the implicit one (§4.3 G10a); entrypoint resolution and the version read;
`tollgate hermes new|key|auth|list|show|delete|pool`; `tollgate start <hermes-profile>`; the read-only pool view;
the `hermes_local` estimate; `which`; the TUI filter; status, list and completions; the herdr join.

Non-goals (v1):
- Owning or refreshing any Hermes credential. Hermes is the only writer of `auth.json`, and the only one that refreshes Nous OAuth (`hermes_cli/auth.py:5224-5246`).
- Hot swap. Hermes' automatic pool failover (`agent/credential_pool.py:106-123`) is Hermes' own behaviour, not a tollgate swap.
- Automating stop-and-resume. That needs P6b.
- S7 spikes other than (f), including (d) two Nous OAuth entries in one pool. **S7(f), the `HOME` redirect, is a
  Part 1 prerequisite** (§8): it must show that a Hermes started with the §4.4 env reads and writes nothing
  under the real home.
- Provider-side meters for a Hermes key. Nous goes through the existing `monitors.toml` `kind = "nous"` with `hermes_home`, `src/usage/monitor/config.rs:112-181`.
- Windows. Every `hermes` verb refuses there with "Hermes profiles are supported on Linux and macOS only".

## 2. Surface

### 2.1 CLI (new `Command::Hermes { cmd: HermesCommand }`, `src/cli.rs:81`)

| Command | Flags | Effect | Exit |
|---|---|---|---|
| `tollgate hermes new <name>` | `--provider nous\|openrouter\|ollama-cloud` (default `nous`), `--model <id>`, `--pool`, `--env-key` (nous only: use `NOUS_API_KEY` instead of OAuth), `--stdin`, `--no-key` | Create the roster entry and the home (§4.1). Env mode prompts for the key with the input hidden (`rpassword`), or reads one line from stdin with `--stdin` (the same flag name as `hermes key`). The key is never taken on argv | 0 / 1 |
| `tollgate hermes key <name>` | `--stdin` | Set or replace the env-mode key (§4.2). Refused on a pool home or an OAuth home | 0 / 1 |
| `tollgate hermes auth <name> add <provider>` | `--type api-key\|oauth` (required; `api_key` accepted as an alias), `--label <l>`, `--no-browser`, `--timeout <s>` | Guards (§4.3), then hand off to `<hermes> auth add <provider> --type <t> [--label ..] [--no-browser] [--timeout ..]` with the terminal inherited. Hermes prompts for the key itself (`hermes_cli/auth_commands.py:197-200`). tollgate never passes `--api-key`, `--portal-url`, `--inference-url`, `--client-id`, `--scope`, `--insecure` or `--ca-bundle` | the child's code |
| `tollgate hermes auth <name> remove <provider> <target>` / `reset <provider>` | none | Hand off to `hermes auth remove\|reset`, idle homes only | the child's code |
| `tollgate hermes list` | `--json` | The roster with provider, mode, model, live state and month-to-date estimate. Reads files only | 0 |
| `tollgate hermes show <name>` | `--json`, `--check` | Home, binding, pool view (§4.7), estimate, the 5 latest sessions. `--check` also runs entrypoint resolution and the full guard audit (§4.3) and prints each verdict | 0; with `--check`, 1 when a guard refuses |
| `tollgate hermes pool <name> strategy <s>` | `s ∈ fill_first\|round_robin\|random\|least_used` | Idle homes only: `<hermes> config set credential_pool_strategies.<provider> <s>`. The pool is keyed by provider (`agent/credential_pool.py:470-483`) | the child's code |
| `tollgate hermes delete <name>` | `--yes`, `--force` | The same confirm gate as `delete` (`main.rs:1640-1660`). Removes `profiles/<name>/` and the roster entry | 0 / 1 |
| `tollgate start <hermes-profile> [-- <hermes args>]` | `--explain` works. `--isolated`, `--with-fallback` and `--auto` are refused by name | §4.4 | the child's code, or 128+signal |
| `tollgate delete <name>` | as today | The bare name resolves claude, then codex, then hermes. The hermes leg is the `hermes delete` body | 0 / 1 |
| `tollgate <name>` (switch) | none | Hermes name → usage error, exit 2 | 2 |

The resolution order is claude, then codex, then hermes, in `cmd_start`, `cmd_delete`, `cmd_switch` and `unknown_profile_error`.
Names are unique across the three rosters, so the order only matters for hand-edited state; a collision prints the existing "note — also names …" line.

Exit contract (`main.rs:195-221`, unchanged): 0 on success. 1 on a refusal or runtime failure; the message starts `tollgate: hermes '<name>': `.
2 on a usage error: an unknown profile, a bad flag, or a hermes name given to a claude-only verb.
`start` and `auth` propagate the child's code through `status_code` (`start.rs:424`).

Messages. Each is one line and exact; `{…}` is substituted.
- M-NAME: `a Hermes profile cannot be named 'profiles': Hermes treats a home whose parent dir is named profiles as one profile of a shared root`
- M-LIVE: `tollgate: hermes '{n}': a Hermes session is already running on this home (session {sid}); Hermes homes take one process at a time — start another account home instead`
- M-BUSY: `tollgate: hermes '{n}': the home is busy ({what}); try again when it finishes`
- M-ACTIVE: `tollgate: hermes '{n}': {home}/active_profile names '{v}', which would move Hermes into {home}/profiles/{v} and out of tollgate's isolation; remove the file (tollgate never writes it)`
- M-PROFILES: `tollgate: hermes '{n}': {home}/profiles exists; Hermes sub-profiles inside a tollgate home are not supported — remove it`
- M-ARGV-P: `tollgate: hermes '{n}': '{flag}' selects a Hermes profile and would leave this home; drop it (the tollgate profile is the account)`
- M-ARGV-PROVIDER: `tollgate: hermes '{n}': '--provider' is fixed by the profile ({p}); create another profile for another provider`
- M-ANTHROPIC: `tollgate: hermes '{n}': {route} routes to anthropic, which makes Hermes read and rewrite ~/.claude/.credentials.json outside tollgate and clauth; remove it (refused on every route in v1)`
- M-HSP-ENV: `tollgate: hermes '{n}': {hsp}/.env exists; Hermes loads it into every session and it can refill scrubbed keys — move it away`
- M-OPENV: `tollgate: hermes '{n}': {home}/.op.env sets {keys}; only OP_SERVICE_ACCOUNT_TOKEN is allowed there`
- M-MANAGED: `tollgate: hermes '{n}': the managed Hermes scope {dir} sets {what}, which outranks this profile's binding; ask whoever manages this machine, tollgate cannot override it`
- W-MANAGED (warning, stderr): `tollgate: note — managed Hermes scope {dir} applies to this session`
- M-SECRETS: `tollgate: hermes '{n}': secrets source '{s}' {why}; disable it in {home}/config.yaml or map only names this profile does not bind`
- M-NOKEY: `tollgate: hermes '{n}': no {VAR} in {home}/.env; run 'tollgate hermes key {n}'`
- M-BIN: `tollgate: hermes '{n}': cannot find the Hermes install (tried {list}); install it, or set [settings] bin in ~/.tollgate/hermes-profiles.toml` (never "run hermes": that runs the self-installing shim)
- M-SHIM: `tollgate: hermes '{n}': {path} is a launcher script, not the Hermes entrypoint; tollgate will not run it (it can install software)`
- W-VERSION: `tollgate: note — Hermes {v} is installed; tollgate's guards were verified against 0.19.x`
- M-SWITCH (usage error): `'{n}' is a Hermes profile; Hermes switches by relaunch: 'tollgate start {n}'`
- M-SESSION-SWITCH: `session '{sid}' is a Hermes session; switch by relaunch (tollgate start <profile>)`. This is the
  shared constant `sessions_cli::NON_CLAUDE_SWITCH` (prep PR, hot-swap spec §8); the hot-swap spec references it
  rather than carrying its own text.
- M-AUX: `tollgate: hermes '{n}': auxiliary.{task}.provider is {v}; Hermes' auto chain can fall through to Anthropic — run HOME='<child-home>' HERMES_HOME='<home>' '<hermes>' config set auxiliary.{task}.provider {p} or recreate the profile`
- M-CHILD-HOME: `tollgate: hermes '{n}': {child_home} holds '{entry}', which tollgate did not put there; remove it (the child home carries only .gitconfig, .config/git and .ssh links)`
- M-CHANGED: `tollgate: hermes '{n}': home changed during audit; retry`

### 2.2 TUI, MCP, local API, herdr

- TUI: `c` cycles `HarnessFilter` All → Claude → Codex → Hermes → All (`tui/app.rs:1637-1667`), and gains `shows_hermes`. Hermes rows are read-only: name, provider, mode, a live dot, the estimate. On a Hermes row, Enter and `s` raise the toast M-SWITCH.
- MCP: no new tool. `usage` returns `hermes:<name>` observations through `collect`. `switch_profile` on a Hermes name returns M-SWITCH as a refusal (`mcp/mod.rs:370-395`). `profiles` stays CC-only (D17).
- Local API: no new route. `/accounts` and `/usage` carry `hermes:<name>`. The OpenAPI `Origin` enum gains `hermes_profile`; the dump golden is regenerated.
- herdr: §6.3.

## 3. Data and files

| Path | Owner | Mode | Written by tollgate |
|---|---|---|---|
| `~/.tollgate/hermes-profiles.toml` | tollgate | 0600 | `atomic_write_600`, only inside `HermesState::update` (State lock) |
| `~/.tollgate/profiles/<name>/` | tollgate | 0700 | mkdir at `new` |
| `…/<name>/hermes-home/` (= `HERMES_HOME`) | Hermes, inside a dir tollgate created | 0700 node; contents are Hermes' own | mkdir; `.env` only |
| `…/hermes-home/.env` | shared: tollgate manages only the bound key line | 0600 | §4.2, idle only |
| `…/hermes-home/shared/` (= `HERMES_SHARED_AUTH_DIR`) | Hermes (`nous_auth.json`) | 0700 | mkdir only |
| `…/<name>/child-home/` (= the child's `HOME`) | tollgate | 0700 | mkdir; holds only the allowlisted symlinks `.gitconfig` → `~/.gitconfig`, `.config/git` → `~/.config/git`, `.ssh` → `~/.ssh` (each created only when its target exists). Never `.claude`, `.claude.json`, `.codex`, `.qwen`, `.config/gh`, `.hermes`. A sibling of `hermes-home/`, not inside it, because `hermes backup` zips the whole Hermes root (`hermes_cli/backup.py:1203`) and would read through the links |
| `…/hermes-home/{auth.json,config.yaml,state.db*,rate_limits/,plugins/,sessions/…}` | Hermes | Hermes' | never |
| `…/<name>/sessions-<sid>/<sid>` | tollgate liveness marker (flock) | 0600 | as for codex (`runtime.rs:472-487`) |
| `…/<name>/hermes_usage_cache.json` | tollgate | 0600 | §4.6 |
| `~/.tollgate/live_sessions/<sid>.json` | tollgate | 0600 | a row with `"harness":"hermes"`, `launch_store: null` |

`hermes-profiles.toml`, schema 1. A missing file is an empty roster. Unknown keys are tolerated on load and dropped on the next rewrite, which is the codex contract (`codex_profiles.rs:10-11`).

```toml
schema_version = 1
[settings]                      # optional
bin = "/abs/path/bin/hermes"    # entrypoint override (§4.5); must pass the shebang check
version_policy = "warn"         # "warn" | "refuse"
[[profiles]]
name = "or-main"
provider = "openrouter"         # nous | openrouter | ollama-cloud
model = "anthropic/claude-sonnet-4.5"   # optional; passed as -m at launch
mode = "account"                # account | pool
auth = "env"                    # env | oauth | pool
key_env = "OPENROUTER_API_KEY"  # auth = env only
key_fingerprint = "sha256:0123456789abcdef"  # auth = env only; Hermes' format
created_at = "2026-09-29T12:00:00Z"
```

- The binding: `openrouter → OPENROUTER_API_KEY`, `ollama-cloud → OLLAMA_API_KEY`, `nous → NOUS_API_KEY` when `--env-key` is given, else `auth = "oauth"`. The var names are what the providers declare (`plugins/model-providers/{openrouter,ollama-cloud,nous}/__init__.py`).
- The fingerprint is Hermes' own: `"sha256:" + hex(sha256(key))[:16]` (`agent/credential_persistence.py:123-131`). Env-mode attribution compares it with the `secret_fingerprint` of the pool's `env:<VAR>` entry.
- The key's value is never stored in the roster.
- `mode = "pool"` implies `auth = "pool"` and no `key_env`.

`hermes_usage_cache.json`, schema 1:

```json
{"schema_version":1,"read_at_ms":0,"db_schema_version":22,"period_start":"2026-09-01T00:00:00Z",
 "rows":[{"billing_provider":"openrouter","model":"…","api_calls":3,"input_tokens":0,"output_tokens":0,
          "cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"cost_usd":"0.012345"}],
 "anthropic_since_ms":null,"nous_reset_at":null,"error":null}
```

- `cost_usd` is summed per row as `actual_cost_usd` when that is > 0, else `estimated_cost_usd`, and printed with `printf('%.6f')`. It is parsed into `Amount` and never through `f64` formatting.
- `error` is one of `sqlite3_missing`, `schema_unknown`, `db_unreadable`, `timeout`.

## 4. Algorithms

Lock ranks: `RotationGuard` is `Rotation = 100` (`lockorder.rs:117`); `StateLock` / `with_state_lock` is `State = 500` (`lockorder.rs:191`).
Every Hermes verb that writes takes `RotationGuard::acquire_with_timeout(name, 25 s)` (`runtime.rs:2466`) first, then the State lock inside it.
It never takes them the other way round. It never holds either lock across a prompt, a child process (`mise where`, the projector, `hermes config set`, `sqlite3`), or HTTP: those run first, and the guard then re-stats what they read (§4.4 step 2).
tollgate never takes Hermes' `auth.lock` (`hermes_cli/auth.py:983-985`).

### 4.1 `hermes new <name>`

1. Parse the flags. Resolve the provider and the auth: nous → `oauth` (or `env` with `--env-key`), openrouter and ollama-cloud → `env`; `--pool` → `pool`.
2. Validate the name without locks: `validate_name_chars`, then refuse a case-insensitive `profiles` (M-NAME).
3. Env mode without `--no-key`: read the key **before any lock**. On a TTY, prompt `Paste your {Provider} API key (input hidden): `; with `--stdin`, read one line. Trim it. Refuse an empty key, or one with a byte outside `0x21..=0x7e`, `'`, `"`, `\` or `#`: the `.env` line is written unquoted and must parse identically in python-dotenv.
4. `RotationGuard(name)` [100].
5. `HermesState::update` [500]. Inside the closure:
   1. `validate_profile_name(name, Harness::Hermes, None)` against all three rosters.
   2. `profiles/<name>/` may exist only when it holds nothing except `hermes-home/`, `child-home/` and `sessions-*`. That is the leftover of a `new` that crashed, and it is adopted. Anything else refuses: `profiles/{n} exists and is not a leftover Hermes home; remove it or pick another name`.
   3. `mkdir_700` the profile dir, `hermes-home/`, `hermes-home/shared/` and `child-home/` (with `child-home/.config/` when `~/.config/git` exists); create the allowlisted symlinks (§3).
   4. Env mode with a key: write the `.env` (§4.2, steps 3–6) and compute the fingerprint.
   5. Push the roster entry last, so a crash before this point leaves an adoptable dir and no entry.
6. Release both locks. On the second line of output, print the next step, depending on the auth:
   - oauth: `tollgate hermes auth {n} add nous --type oauth`
   - pool: `tollgate hermes auth {n} add {p} --type api-key --label <account>`
   - `--no-key`: `tollgate hermes key {n}`
7. Pin the auxiliary providers (defence in depth for G10a). Resolve the entrypoint (§4.5), then for every task in
   `HERMES_AUX_TASKS` run `<hermes> config set auxiliary.<task>.provider <provider>` with the §4.4 step 4 env
   (child `HOME` included), stdin null, 10 s each, no lock held. Before each entrypoint run, audit G2a, G6, G11,
   and G12 through P; an unsafe leftover child home or `.env` refuses without starting Hermes. `HERMES_AUX_TASKS` is the pinned list of the 15
   task keys of 0.19.0's default config (`hermes_cli/config.py:1621-1800`: `vision`, `web_extract`,
   `compression`, `skills_hub`, `approval`, `mcp`, `title_generation`, `memory_query_rewrite`, `tts_audio_tags`,
   `triage_specifier`, `kanban_decomposer`, `profile_describer`, `goal_judge`, `curator`, `monitor`). A failure
   prints the exact command to finish by hand with `HOME=` and `HERMES_HOME=` prefixes; the next `start` refuses (M-AUX) until it is done.
8. H2h (herdr). Outside guest mode, and when `herdr` is on PATH, run `HERMES_HOME=<home> herdr integration install hermes` once; a failure is a warning. In guest mode, print the command instead (§9 D-H11).

### 4.2 The `.env` writer (`key`, `new`, and the normalisation at launch)

This runs only under `RotationGuard(name)` with `has_live_session(name) == false` (`runtime.rs:587-594`). Unknown counts as live.

1. `symlink_metadata(<home>/.env)`. A symlink, a non-regular file, or a foreign uid refuses (`.env is not a regular file tollgate can rewrite`).
2. Read the file, capped at 1 MiB. Split it into lines with a key-name scan, `^\s*(export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=`.
   Cross-check that set against the projector's dotenv key names (§4.3 P). If they disagree (multiline values), refuse with `cannot safely rewrite {home}/.env (multiline values); edit it by hand`.
3. Build the new content:
   - line 1 is `# tollgate: {VAR} is managed by 'tollgate hermes key {n}'; other lines are yours`;
   - then `{VAR}={key}`;
   - then every other line byte-for-byte, dropping every earlier line that sets `{VAR}`.
   The output is UTF-8 with LF endings, no BOM and no NUL, so Hermes' startup sanitize (`hermes_cli/env_loader.py:241-275`) leaves it alone.
4. If the new bytes equal the old ones and the mode is 0600, stop: no write and no mtime bump.
5. Remove any stale `<home>/.env.tollgate-*` sibling. No tollgate writer can be running: we hold its RotationGuard.
6. `atomic_write_600(<home>/.env)` (`profile.rs:2364`). This writes a same-dir temp created `O_CREAT|O_EXCL` at 0600, then `rename(2)`. It stays on one filesystem because it is the same directory.
7. Update `key_fingerprint` in the roster under `HermesState::update` [500], nested inside the held Rotation.

At every launch, step 2 also runs as an audit. The home `.env` must hold a non-blank `{VAR}` (else M-NOKEY). If its fingerprint differs from the roster's, the key was edited outside tollgate: print `note — {VAR} changed outside tollgate; re-attributed` and update the roster fingerprint (step 7). The file is not rewritten.

### 4.3 Guards (every `start`, `auth`, `key`, `pool` and `show --check`; a warning never passes a refusal)

Guards run in this order and stop at the first refusal:

- **G1 shape.** `home = tollgate_dir()/profiles/<name>/hermes-home`, as a literal non-canonical path. It is also what `HERMES_HOME` receives, because Hermes tests `Path(HERMES_HOME).parent.name` on the raw string (`hermes_cli/main.py:589-592`). Refuse when:
  - the parent dir name is `profiles`;
  - the home is missing (`home missing; delete and recreate the profile`);
  - the home is a symlink or foreign-owned.
  Tighten a looser mode to 0700.
- **G2 containment**, computed against the **child** `HOME`: Hermes resolves `Path.home()` from the redirected `HOME`, so its default root is `<child-home>/.hermes`. Refuse when `<child-home>/.hermes` exists (tollgate never creates it), or when `canonicalize(home)` is it or sits under it. Otherwise `get_default_hermes_root()` would return it and Hermes would read that tree's `active_profile` (`hermes_constants.py:154-191`). The same test against the operator's `~/.hermes` is kept as belt-and-braces; it is no longer the decisive one.
- **G2a child home.** `<child-home>` is a 0700 dir owned by the uid, not a symlink, and every entry in it is one of the allowlisted symlinks, pointing where §3 says. Anything else → M-CHILD-HOME. A missing child home (a profile made before this rule) is created as in §4.1 step 5.3, under the RotationGuard.
- **G3 `active_profile`.** Refuse (M-ACTIVE) when `<home>/active_profile` exists and its trimmed content is neither empty nor `default`, or when it is a symlink. This is Hermes' own rule (`main.py:607-613`), whose root is the home itself (G1 plus G2).
- **G4.** `<home>/profiles` exists, of any type → M-PROFILES.
- **G5 argv** (`start` pass-through only). Scan the whole vector, including after a `--`:
  - `-p`, a glued `-p<name>`, `--profile`, any argparse prefix such as `--prof`, or an `=<name>` form → M-ARGV-P (Hermes scans for these anywhere, `main.py:526-560`);
  - `--provider` or `--provider=*` → M-ARGV-PROVIDER;
  - a `-m` / `--model` value of the form `<alias>:<model>`, where the alias normalises to anthropic (`anthropic`, `claude`, `claude-code`, `hermes_cli/providers.py:291-292`) → M-ANTHROPIC with the route `-m`.
  An OpenRouter slug `anthropic/…` is allowed: the provider is pinned to the profile's.
- **G6.** `$HSP/.env` exists → M-HSP-ENV. `main.py:654` loads `PROJECT_ROOT/.env` with `PROJECT_ROOT = $HSP` (`main.py:461`), fill-only or override (`env_loader.py:326-327`).
- **G14 liveness.** A live tollgate marker on the profile → M-LIVE. A `<home>/gateway.pid` that names a live pid → M-BUSY (`a Hermes gateway`); Hermes' own marker (`hermes_cli/profiles.py:702-715`).
- **G15 version.** Resolve the entrypoint (§4.5) and read `Version:`. With `version_policy = "refuse"`, a version outside 0.19.x refuses; otherwise it prints W-VERSION.
- **P projector.** One run of the resolved interpreter: `<venv>/bin/python -I -B -c <PROJECTOR> <home> <managed_dir|"">`, with the child env of §4.4 step 4, stdin null, and a 10 s timeout. `PROJECTOR` is an embedded constant of about 60 lines. It runs **before** any lock (§4.4 step 2).
  - It parses with `utils.fast_safe_load` (`$HSP/utils.py:396-404`, Hermes' exact loader) and `dotenv.dotenv_values`.
  - It prints one JSON object with names and hosts only, never a value, and never any `api_key` or URL path:
    - `config`: `model.provider`, `model` as a string, `providers.*.{name, base_url_host}`, `custom_providers[*].{name, base_url_host}`, `fallback_providers[*]` (the provider string, or the dict's `provider`), `fallback_model` (the provider or providers), `auxiliary.<task>.{provider, base_url_host}`, `delegation.{provider, base_url_host}`, `credential_pool_strategies`, `secrets.<source>.{enabled, targets: [env map keys]}`, `plugins.enabled`;
    - `managed_config_top_keys`;
    - `env_keys: {home, op_env, managed}`, each a list of `{key, nonblank}`.
  - A non-mapping route, including `delegation: anthropic`, a non-zero exit, bad JSON or a timeout refuses: `cannot audit {home}/config.yaml ({why})`. The failure is closed.
  - The output is checked against a strict Rust schema (`ProjectionV1`, `#[serde(deny_unknown_fields)]`, every
    key required, lists possibly empty): a missing key, an unknown key or a wrong type refuses the same way, so a
    projector that silently drops a route cannot pass the guards.
- **G7.** `op_env` keys ⊄ {`OP_SERVICE_ACCOUNT_TOKEN`} → M-OPENV.
- **G8 managed.** The dir is `$HERMES_MANAGED_DIR` when it is set and non-empty: that dir if it is a dir, else none. Otherwise it is `/etc/hermes` when that is a dir (`managed_scope.py:52-71`); tests inject it through `MANAGED_DIR_OVERRIDE`. Refuse (M-MANAGED) when:
  - a `managed` env key is in `SCRUB ∪ {ANTHROPIC_*} ∪ {CLAUDE_CODE_OAUTH_TOKEN}`; or
  - `managed_config_top_keys ∩ {model, provider, providers, custom_providers, auxiliary, delegation, fallback_providers, fallback_model, credential_pool_strategies, secrets} ≠ ∅`.
  A managed dir that passes prints W-MANAGED. `HERMES_MANAGED_DIR` is passed through and never altered.
- **G9 secrets.** Refuse (M-SECRETS) when:
  - an enabled source other than `onepassword` is found (`bitwarden` is bulk, `agent/secret_sources/bitwarden.py:600`; plugin sources are unknown);
  - an enabled `onepassword` has a target in `SCRUB ∪ anthropic`, unless the target equals the profile's `key_env`.
- **G10 anthropic, config.** After alias normalisation, refuse M-ANTHROPIC on the first hit among:
  - `model.provider`;
  - any `fallback_providers` / `fallback_model` provider;
  - any `auxiliary.<task>.provider`;
  - `delegation.provider`;
  - any `providers` key named anthropic, or any `providers` / `custom_providers` / `auxiliary` / `delegation` `base_url_host == api.anthropic.com`.
  The route string names the key, e.g. `auxiliary.vision.provider`.
- **G10a anthropic, the implicit route.** Hermes 0.19.0 has a route to Anthropic that no config key names. The
  auxiliary `auto` chain includes Native Anthropic (`agent/auxiliary_client.py:7-22`), and the 402 / credit
  exhaustion fallback walks the same chain (`:36-37`, `:2925-3048`). That step calls `resolve_anthropic_token()`
  (`agent/anthropic_adapter.py:1298`, via `auxiliary_client.py:2799-2818`). It reads
  `Path.home()/.claude/.credentials.json` (`:958`) and writes refreshed credentials back to it (`:1160-1167`), and
  the pool's `claude_code` source reads the same file (`agent/credential_sources.py:6`). An exhausted OpenRouter
  profile could therefore rotate upstream's chain (guest mode) or tollgate's (after import). Closed in two
  layers:
  1. **Primary: the child `HOME`** (§4.4 step 4). `Path.home()` is `<child-home>`, which holds no `.claude`, so
     the token resolver finds nothing and the step is skipped. `ANTHROPIC_*` and `CLAUDE_CODE_OAUTH_TOKEN` are
     scrubbed, so the env sources are empty too.
  2. **Defence in depth: pinned auxiliary providers.** Refuse M-AUX when any task in `HERMES_AUX_TASKS`, or any
     other `auxiliary.<task>` table in `config.yaml`, has a `provider` that is `auto`, empty or unset. `new`
     pins them (§4.1 step 7).
  The post-session `state.db` check (§4.4 step 6.2) is evidence only. Auxiliary calls need not appear in
  `session_model_usage`, so a clean check does not prove the route was avoided.
- **G11 anthropic, auth.json.** Read `<home>/auth.json` with the whitelist struct `PoolAuthView` (§4.7), capped at 1 MiB. Refuse when:
  - `active_provider` normalises to anthropic;
  - `providers` has an anthropic key;
  - `credential_pool` has an anthropic key (any entries, including `claude_code`);
  - `<home>/.anthropic_oauth.json` exists (Hermes' own PKCE store, `agent/anthropic_adapter.py:1418`; route
    `.anthropic_oauth.json`).
  A missing `auth.json` passes. An unreadable or unparseable file refuses: `cannot audit auth.json; retry when Hermes is not writing it`.
- **G12 anthropic, env.** A non-blank `ANTHROPIC_API_KEY`, `ANTHROPIC_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` in `op_env` or `managed` refuses. So does any `ANTHROPIC_*` key or a non-blank `CLAUDE_CODE_OAUTH_TOKEN` in `home` (M-ANTHROPIC, route `.env`).
- **G13 binding.**
  - `auth = env`: the §4.2 audit (M-NOKEY, fingerprint).
  - `mode = account`, when `auth add` is given `<provider>`: refuse if it is not the profile's provider (`an account is one provider; this home is {p}`), and refuse if the home already holds a credential for it (the env key, or at least 1 pool entry): `account homes hold one account; create another with 'tollgate hermes new', or use a pool home`.
  - `mode = pool`: the provider must equal the profile's.
  - For `auth add`, an anthropic alias is refused before G1.

`SCRUB` (the child's env scrub, and the audit set):
- `HERMES_HOME`, `HERMES_SHARED_AUTH_DIR`, `HERMES_INFERENCE_PROVIDER`, `HERMES_MODEL`, `HERMES_S6_SUPERVISED_CHILD`, `HERMES_PORTAL_BASE_URL`, `PYTEST_CURRENT_TEST` (this last one flips Hermes' managed-scope and auth-store behaviour, `managed_scope.py:41-49`);
- every `NOUS_*` and every `ANTHROPIC_*`;
- `CLAUDE_CODE_OAUTH_TOKEN`, `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OLLAMA_API_KEY`, `OLLAMA_BASE_URL`;
- the static registry list `HERMES_REGISTRY_ENV_KEYS`: every `api_key_env_vars` and `base_url_env_var` of `hermes_cli/auth.py:177-445`, pinned as a const;
- the runtime plugin scan: every quoted name inside `env_vars=(…)` in `$HSP/plugins/model-providers/*/__init__.py` and `<home>/plugins/model-providers/*/__init__.py`, taken by regex without executing them (`providers/__init__.py:5-10`, `auth.py:447-477`);
- the existing `scrub_tollgate_homes` and `scrub_billing_env` sets;
- `CLAUDE_CONFIG_DIR` (set when `tollgate start <hermes>` runs from a Claude Code Bash tool), `GH_TOKEN`, `GH_CONFIG_DIR`,
  `GITHUB_TOKEN` (the pool's `gh_cli` source, `credential_sources.py:10`), and `XDG_CONFIG_HOME`,
  `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, which would otherwise point the child back into the real
  home past the `HOME` redirect.

### 4.4 `tollgate start <hermes-profile> [-- args]`

1. `cmd_start` resolves the name against claude, then codex, then hermes. For a hermes name:
   - `--isolated` refuses with `--isolated is not available on a Hermes profile: the home is the account`;
   - `--with-fallback` refuses with `… Hermes fails over inside its own pool; there is no tollgate chain`;
   - `--auto` never picks a hermes profile.
   `--explain` prints the pick line and stops. It runs after G1–G6 and a read-only G2a check, before the projector.
2. **Before any lock**: G1–G6 and G15 (entrypoint resolution and the version read, §4.5), then P. Record the
   `(ino, len, mtime_ns)` of `<home>/config.yaml`, `<home>/.env`, `<home>/.op.env` and `<home>/auth.json`
   (absent is a value) as they were when P read them. Then `RotationGuard(name)` [100]: re-stat the four files,
   and any difference refuses M-CHANGED. Inside the guard run G2a, G7–G13 (G11 reads `auth.json` here) and the
   §4.2 launch normalisation, which spawn nothing.
3. `with_state_lock` [500]:
   - re-check `has_live_session(name)` and refuse M-LIVE;
   - mint the sid (`SessionId::mint`, with the `SID_COLLISION_REMINTS` loop);
   - `mkdir_700(profiles/<name>/sessions-<sid>)`;
   - `open_pid_file` + `try_lock` the marker `…/sessions-<sid>/<sid>` (`runtime.rs:2535`);
   - `live_sessions::register(LiveSession::starting(sid, name, Harness::Hermes, isolated=false, follows_chain=false, launch_store=None))`.
   Release the State lock, then drop the RotationGuard. The marker flock now excludes every other tollgate writer (G14).
4. Build the command (`hermes_spawn_command`, the twin of `start.rs:607-622`):
   - `Command::new(<resolved entrypoint>)`;
   - `HermesEngine::scrub_env`: remove `SCRUB` and the active claude profile's custom env keys;
   - `.env("HERMES_HOME", home)` and `.env("HERMES_SHARED_AUTH_DIR", home/"shared")`;
   - `.env("HOME", <child-home>)`, the literal `profiles/<name>/child-home` path (§3, G2a). The spawn cwd is the
     caller's, unchanged;
   - args `--provider <provider>`, then `-m <model>` if set, then the user args verbatim. `--provider` and `-m` are top-level flags (`main.py:12876-12882`), and the CLI flag beats `config.yaml` (`runtime_provider.py:541-556`).
   - On Linux, `pre_exec` runs `prctl(PR_SET_PDEATHSIG, SIGTERM)`, then `if getppid() != <supervisor pid> { _exit(1) }`,
     so a SIGKILLed supervisor cannot leave an orphaned Hermes on the home, even when it died between `fork` and
     `prctl`. PDEATHSIG fires when the **forking thread** exits, so the spawn is made from `start::run_hermes`'s
     main thread, never from a helper thread. The supervisor pid is captured before `fork`, and the closure
     does only async-signal-safe calls.
5. Record `run_start`. Spawn under `SignalWatcher` / `wait_for_child` (`start.rs:488`).
6. Teardown, in order:
   1. Refresh the usage cache (§4.6), best effort.
   2. The post-session anthropic evidence check reads `state.db` only when `symlink_metadata` shows a regular file. When a `session_model_usage` row with `billing_provider` normalising to anthropic has `last_seen ≥ run_start`, print `tollgate: WARNING — this Hermes session called Anthropic (the in-session /model picker); it had no Claude credentials to use, but check the session` and a `logline!`. The check is evidence, not proof: auxiliary calls need not land in `session_model_usage` (G10a).
   3. Audit `<child-home>` as G2a does, and warn naming any new entry (a `.claude/` Hermes created, say). Also compare the `symlink_metadata` of the operator's `~/.claude/.credentials.json` from before and after the session. A regular file that appears from absence or replaces a symlink warns.
   4. Drop the marker and unregister the row.
   5. Exit with the child's code.

### 4.5 Entrypoint resolution (re-run at every user-initiated launch; never cached)

The order:
1. `[settings] bin`.
2. The mise install glob, read without running mise: `$MISE_DATA_DIR` (default `~/.local/share/mise`) `/installs/pipx-hermes-agent/*/hermes-agent/bin/hermes`, taking the highest version dir by semver (the layout the Omarchy shim tests, `~/.local/bin/hermes:14`). Only when that glob matches nothing: `mise where pipx:hermes-agent` with `current_dir(~/.tollgate)` (so no project `.mise.toml` env or hooks load), stdin null, a 5 s timeout, stdout's first line, then `<dir>/hermes-agent/bin/hermes`.
3. `$PIPX_HOME` (default `~/.local/pipx`) `/venvs/hermes-agent/bin/hermes`.
4. `which hermes`.

Each candidate must be a regular file, or a symlink to one, whose first line is `#!<abs>/bin/python…` in the same venv. A `#!/bin/bash` or `#!/usr/bin/env bash` file (the Omarchy shim) is rejected with M-SHIM, never executed.
Then `$HSP` = the single `<venv>/lib/python3.*/site-packages` that contains `hermes_cli/main.py`, and the version = `Version:` in the single `hermes_agent-*.dist-info/METADATA`. Zero or several matches refuse with M-BIN.

There is no cache, because `mise up` reinstalls into a new version dir (`~/.local/bin/hermes:9-13`) and a cached path would go stale.
The daemon, `detect`, `list`, `show` without `--check`, the collect hooks and herdr never run any of this.

### 4.6 `hermes_local` (the usage cache)

`refresh(name)` is called:
- from the daemon tick next to `poll_detached` (`daemon/tick.rs:68`), at most once per 60 s per profile;
- from `hermes list` and `show`;
- at start teardown.

It never runs Hermes or its interpreter. Steps:
1. Find `sqlite3` on PATH. If absent, write the cache with `error: sqlite3_missing`.
2. Run `sqlite3 -readonly -json -cmd '.timeout 2000' <home>/state.db "<Q1>;<Q2>;<Q3>"` with a 5 s kill timeout and stdin null. `-readonly` gives `SQLITE_OPEN_READONLY`; WAL reads work because the user owns the dir. The queries:
   - Q1 = `SELECT version FROM schema_version LIMIT 1`;
   - Q2 = `SELECT name FROM pragma_table_info('session_model_usage')`, which must include every column Q3 uses (`hermes_state.py:836-856`); otherwise `schema_unknown`;
   - Q3 = `SELECT billing_provider, model, SUM(api_call_count), SUM(input_tokens), SUM(output_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(reasoning_tokens), printf('%.6f', SUM(CASE WHEN actual_cost_usd > 0 THEN actual_cost_usd ELSE estimated_cost_usd END)), MAX(last_seen) FROM session_model_usage WHERE COALESCE(last_seen, first_seen, 0) >= {period_start_secs} GROUP BY 1, 2 ORDER BY 1, 2`.
   `period_start_secs` is a tollgate-computed integer (UTC month start), so nothing is interpolated from input.
   Parse the concatenated JSON arrays with `serde_json::StreamDeserializer`.
3. Read `<home>/rate_limits/nous.json` (≤ 64 KiB; `reset_at`, `agent/nous_rate_guard.py:25-36,100-113`) as `nous_reset_at`. A torn or unparseable file is ignored.
4. `atomic_write_600(profiles/<name>/hermes_usage_cache.json)`.

The collect hook `hermes_observations` is appended to `MONITOR_SOURCES` (`usage/collect.rs:95-98`) and reads caches only. It produces:
- `id = hermes:<name>`, `source = Hermes`, `auth = NativeLogin`, `origin = HermesProfile`, `label = name`, `plan = "<provider> · <mode> home"`;
- `freshness` from `read_at_ms`, at a 60 s cadence;
- `estimate = LocalEstimate{amount: Σ cost_usd, currency "USD", period Monthly [month start, now) derived, basis "hermes state.db (billed where known, else estimated)"}`;
- `failure`:
  - `RateLimited{retry_after}` while `nous_reset_at > now`;
  - `Unavailable` with the `error` text: `install sqlite3 to read Hermes' local usage`, `Hermes state.db schema not recognised`, …
- `best_effort = true` when `db_schema_version ≠ 22` or the installed version is outside 0.19.x. The last-launch version is recorded in the cache.

A pool home is exact per home, not per entry, as the plan says.

### 4.7 Pool view (read-only, H-3)

`PoolAuthView` deserialises only these keys:
- `version`, `active_provider`;
- `providers` as key names only (`BTreeMap<String, IgnoredAny>`);
- `credential_pool: BTreeMap<provider, Vec<PoolEntryView>>`, where `PoolEntryView` = {`id`, `label`, `source`, `auth_type`, `priority`, `last_status`, `last_status_at`, `last_error_code`, `last_error_reset_at`, `request_count`, `secret_fingerprint`, `expires_at`}.

`#[serde(deny_unknown_fields)]` is **not** set. The struct has no field that could hold `access_token`, `refresh_token`, `agent_key` or `api_key`; serde skips those values as `IgnoredAny`. The reader is an atomic `read` of the file, with no lock, capped at 1 MiB. `auth.json` `version` 1 is expected (`auth.py:71`); another version renders `best_effort`.

The display is `#<n> <label>  <auth_type>/<source>  <status>[ until <reset>]  req <count>  prio <p>  fp …<last 4 hex>`. Env-mode attribution marks the entry `source = env:<key_env>` whose fingerprint equals the roster's.

The strategy writer (`hermes pool <n> strategy <s>`):
1. Run the full lock-free preflight and projector. Then RotationGuard [100] and the in-guard audit, including G2a, G6, G11 and G12.
2. Claim a marker (as in §4.3 `auth`).
3. Release the locks. Run `<hermes> config set credential_pool_strategies.<provider> <s>` (`hermes_cli/subcommands/config.py:38`).
4. Drop the marker.

tollgate never writes `auth.json` or `config.yaml` itself.

### 4.8 `hermes auth` / `key` / `delete`

- `auth`:
  1. Without locks: G1–G6, G15 and P, recording the four stats (§4.4 step 2). Then RotationGuard [100]: re-stat (M-CHANGED), then G2a and G7–G13 (G5 is not applicable).
  2. `with_state_lock` [500]: claim a marker `sessions-<sid>/<sid>`, with no registry row.
  3. Release both locks. Spawn `<hermes> auth …` with the §4.4 step 4 env, without `--provider`.
  4. On exit, drop the marker. Env mode after `add`: nothing; the pool entry is Hermes' own.
- `key`: read the key without locks, then RotationGuard [100]; G1–G4, G14; then §4.2.
- `delete`:
  1. The confirm prompt without locks.
  2. RotationGuard [100]. Then `HermesState::update` [500]: re-check membership; refuse a live session without `--force`; `remove_dir_all(profiles/<name>)`; `remove_profile`. This is the pattern of `actions.rs:1305-1345`.
  3. With `--force` on a live session, print `the running Hermes keeps its open files; its home is gone`.
  `remove_dir_all` unlinks the child home's `.ssh` / `.gitconfig` / `.config/git` links without following them
  (std's `remove_dir_all` never traverses a symlink); test 36 asserts the link targets survive.

## 5. Failure modes and recovery

| Where | Crash / race | State after | Recovery |
|---|---|---|---|
| `new` step 5 before the roster push | dirs, perhaps `.env`, no roster entry | an orphan `profiles/<n>/` | `new <n>` adopts it (§4.1 5.2); the key is rewritten |
| `.env` write (§4.2 step 6) | the temp is left over | `.env` is old or new: `rename` is atomic | the next write removes `.env.tollgate-*` (step 5) |
| `start` after the marker, before spawn | marker flock released by exit | a stale row | `gc_live_session_rows` (`runtime.rs:1697-1706`) through `session_row_is_live`, which probes `sessions-<sid>/<sid>`; the orphan-marker GC collects the dir (`runtime.rs:1783-1787`) |
| supervisor SIGKILLed mid-session | Hermes gets SIGTERM (PDEATHSIG, Linux) | as above | the same. macOS: Hermes may survive; the next start cannot see it (documented residual) |
| user runs a bare `hermes` with `HERMES_HOME` = a tollgate home | invisible to tollgate unless `gateway.pid` | two processes on one home | documented; `show --check` warns when `state.db-wal` mtime is under 5 s old and no marker is held |
| Hermes rewrites `.env` (sanitize, `hermes setup`) during a session | it is Hermes' own atomic replace | the next launch's audit re-attributes | fingerprint note (§4.2) |
| Hermes mid-write of `auth.json` at G11 | read sees old or new (atomic replace, `auth.py:1151-1190`) | none | a parse failure refuses the launch; retry |
| `delete` crash inside `remove_dir_all` | partial dir, roster entry kept | `start` refuses (G1 "home missing") | re-run `delete` |
| upstream clauth 0.16.0 live | never reads `~/.tollgate`; its rotations of `~/.claude` could race only with a Hermes anthropic route. The named routes are refused (G5, G10–G12); the implicit auxiliary/402 route finds no `~/.claude` under the child `HOME` (G10a) | none | the post-session check (§4.4 6.2–6.3) is evidence only |
| config or `auth.json` rewritten between the projector and the guard | the audit saw the old bytes | refused (M-CHANGED) | retry |
| another tollgate process on the same profile | blocks on RotationGuard (25 s) or sees the marker | M-LIVE / M-BUSY | none needed |
| cross-fs | every write is a same-dir temp + rename; no move across dirs | none | none |
| `mise up` swaps the version mid-session | the running Hermes keeps its files; the next launch re-resolves | none | W-VERSION if outside 0.19.x |
| roster unreadable | `HermesState::load` errors | hermes verbs exit 1; `collect` shows no hermes accounts (`unwrap_or_default`) | fix or delete the file |
| sqlite3 slow or locked | killed at 5 s | `error: timeout`; the last good cache is kept for 7 days | automatic |

## 6. Interactions

### 6.1 Guest mode (`identity::upstream_active`, `identity.rs:135-147`)
Hermes profiles are allowed in guest mode (plan §4.0 guest row). Nothing here writes an upstream-owned file, and
the Hermes child cannot either:
- homes, child homes, rosters and caches live under `~/.tollgate`;
- the child runs with `HOME = <child-home>` (§4.4 step 4), which holds no `.claude`, `.claude.json`, `.codex`,
  `.qwen` or `.config/gh`, so Hermes' Claude Code credential reader and writer (`anthropic_adapter.py:958,1160-1167`)
  and its `claude_code` pool source find nothing. Without this, G5 and G10–G12 all pass while the auxiliary
  auto / 402 route still reaches `~/.claude/.credentials.json` (G10a);
- tollgate itself only `symlink_metadata`s `~/.claude/.credentials.json` (§4.4 6.3);
- the herdr integration install is skipped in guest mode (§4.1 step 8).
`GUEST_REFUSAL` is never raised by a hermes verb.

### 6.2 The other two lanes
- **Import clauth (R3).** Import's M2 uniqueness check calls `actions::validate_profile_name`, which Part 1 generalises: `validate_foreign_harness_free` iterates `Harness::ALL` minus self, so every roster (claude, codex, hermes) refuses a name held by any other, and the import needs no Hermes-specific code. Hermes homes are never moved, copied or rolled back. **Stop Hermes sessions before import**: a `tollgate start <hermes>` supervisor is a tollgate process, and its live row and held marker are M1 blockers (`tollgate_live_session`, import spec §4.3). A `hermes auth` run holds a marker and blocks the same way. Whichever of the two lanes lands second adds `import_refuses_a_name_held_by_a_hermes_profile`.
- **Executor B / hot swap.** Hermes rows are `follows_chain = false` (filtered by `row_follows_chain_live`, `usage/scheduler.rs:1693-1700`), and their `executor` derives to `none` (hot-swap spec §3.1), so `SwapView`, `LiveSessionView` and the local API report `executor: null`. `sessions switch` refuses them with the shared `NON_CLAUDE_SWITCH` (M-SESSION-SWITCH; `sessions_cli.rs:219`, `== Codex` becomes `!= Claude`); hot-swap rewrites `run_switch` first, and this lane only supplies the constant's text. `swap_eligible` is untouched.
- **H-4 relaunch.**
  - An account home is switched by exiting and running `tollgate start <other>`: a new session, no resume, because sessions live in that home's `state.db` (`hermes_state.py:153`).
  - A pool home: exit; optionally `tollgate hermes pool <n> strategy …` while idle; then `tollgate start <n> -- --resume <id>` or `-- -c`.
  - `hermes show` lists the latest 5 session ids from `state.db` (`sessions.id, title, started_at, billing_provider`, a read-only sqlite3) to feed `--resume`.
  - Automated stop and restart waits for P6b.

### 6.3 herdr
- A tollgate-started Hermes pane resolves through the existing live-row join (`herdr-plugin/report-profile.sh:65-104`): the row has `start_profile = <name>`.
- `herdr tag --agent hermes -- <name>` must look up `Origin::HermesProfile` (`herdr/tag.rs:104-108`: pick the origin by `PaneAgent`).
- A bare `hermes` pane with no row: the script extracts only `HERMES_HOME` from `/proc/<fg pid>/environ` (Linux; same uid), with `tr '\0' '\n' <"/proc/$pid/environ" | sed -n 's/^HERMES_HOME=//p' | head -1`, so no other variable ever enters a shell variable, and passes `--hermes-home <path>`. The binary maps a `profiles/<n>/hermes-home` path to `hermes:<n>`; otherwise it falls back to `native_match`.
- `native_match` (`herdr/tag.rs:140-152`) excludes `Origin::HermesProfile`. Otherwise a lone tollgate Hermes profile would tag the operator's own `~/.hermes` pane with the wrong account.
- Tags carry only the name and numbers (H1).

### 6.4 Local API and MCP
These are read-only projections of `collect`, and nothing new executes. `delegate` stays CC-only.

## 7. Test plan (hermetic: `testutil::HomeSandbox`; no network; no real `hermes`, `mise` or `python`)

Fixtures live in `tests/fixtures/hermes/`:
- `venv/` — `bin/hermes` with the shebang `#!{venv}/bin/python`, rewritten at test time; `bin/python`, a shell stub that prints `$FIXTURE_PROJECTION` and records argv and env to `$REC`; `lib/python3.13/site-packages/hermes_cli/main.py` (empty); `hermes_agent-0.19.0.dist-info/METADATA`; `plugins/model-providers/openrouter/__init__.py` (the `env_vars` line only).
- `projection/*.json` — one per route or env layer.
- `auth/*.json` — `pool-env-openrouter.json`, `pool-anthropic-claude_code.json`, `active-anthropic.json`, `torn.json`, `secrets-bearing.json` (with `access_token` / `refresh_token` sentinels).
- `state-v22.sql` — schema from `hermes_state.py:758-906` plus rows, built into a db by `sqlite3` at test time. A test that needs it is marked `#[ignore = "needs sqlite3"]` (it never passes silently by returning early); a CI job with `sqlite3` installed runs `cargo nextest run --run-ignored all -E 'test(/sqlite/) | test(post_session_check)'`.
- `rate_limits_nous.json`.
- `omarchy-shim.sh` — a copy of the shim's text.
- `sqlite-out/*.json` — sqlite3 `-json` output samples.
- `projector-real/` — for the `hermes_projector_real` tier below: `config.yaml` / `.env` / `.op.env` fixtures for every G7–G12 route plus YAML anchors and aliases, `<<:` merge keys (a route hidden behind a merge), dotenv `export KEY=`, quoted and multiline values, and a per-fixture expected `ProjectionV1` JSON.

Part 1:
1. `harness_hermes_roundtrips_lowercase_and_old_rows_stay_readable` — the serde value is `"hermes"`; an old claude row still parses as the default. 2. `hermes_engine_install_credentials_bails` — both install methods error, and the message names Hermes.
3. `hermes_state_roundtrip_drops_unknown_keys_and_skips_noop_save` — the codex contract. 4. `names_are_unique_across_three_rosters` — each harness refuses the other two, case-insensitively.
5. `a_hermes_profile_cannot_be_named_profiles`. 6. `new_creates_home_env_0600_and_fingerprint_in_hermes_format` — `sha256:` plus 16 hex, equal to a Python-computed vector.
7. `new_adopts_a_crashed_leftover_and_refuses_a_foreign_dir`. 8. `new_reads_the_key_before_taking_any_lock` — a lock-rank recorder sees no rank held during the prompt callback.
9. `env_writer_keeps_foreign_lines_and_is_idempotent` — the second run leaves bytes and mtime unchanged. 10. `env_writer_refuses_symlink_and_multiline_disagreement`.
11. `env_writer_never_writes_under_a_live_marker` — a held marker makes it refuse. 12. `guard_parent_named_profiles_refuses` (G1) and `guard_home_under_dot_hermes_refuses` (G2).
13. `guard_active_profile_refuses_non_default_and_allows_empty_or_default` (G3). 14. `guard_profiles_subdir_refuses` (G4).
15. `argv_scan_refuses_profile_flags_anywhere_including_after_dashdash` (G5) — `-p x`, `--profile=x`, `chat -p x`, `-- -p x`. 16. `argv_scan_refuses_provider_and_anthropic_colon_model_but_allows_openrouter_slug`.
17. `guard_hsp_dotenv_present_refuses` (G6). 18. `guard_op_env_foreign_key_refuses` (G7).
19. `managed_dir_env_override_and_non_dir_disable_match_hermes` — `HERMES_MANAGED_DIR` set to a file means no managed scope; unset means the injected `/etc/hermes`. 20. `managed_scope_refuses_each_key_class_and_otherwise_only_warns` (G8). A warning alone never passes a refusal fixture.
21. `bulk_secret_source_and_scrubbed_mapped_target_refuse` (G9). 21a. `the_child_home_has_no_claude_codex_qwen_or_gh_entries` — the stub python records `HOME`, which is `profiles/<n>/child-home`; that dir holds only the allowlisted links; a planted `.claude` refuses M-CHILD-HOME. 21b. `the_child_env_scrubs_xdg_claude_config_dir_and_gh_tokens`.
21c. `g2_is_computed_against_the_child_home` — a `<child-home>/.hermes` refuses; the operator `~/.hermes` alone does not decide. 21d. `auto_auxiliary_provider_refuses` (M-AUX for `auto`, empty and unset, on a pinned task and on an extra task table).
21e. `new_pins_every_auxiliary_task` — the stub records one `config set auxiliary.<task>.provider <p>` per `HERMES_AUX_TASKS` entry, run with no rank held. 21f. `anthropic_oauth_json_in_the_home_refuses` (G11).
22. `anthropic_refused_on_every_config_route` (G10) — one sub-case per route: `model.provider`, `claude-code` alias, `fallback_providers` string and dict, `fallback_model` list, `auxiliary.vision`, `delegation`, a `providers.x.base_url` on `api.anthropic.com`, `custom_providers`.
23. `anthropic_refused_from_auth_json` (G11) — `active_provider`, `providers.anthropic`, `credential_pool.anthropic` holding `claude_code`, a torn file. 24. `anthropic_refused_from_env_layers` (G12) — home `.env` with `ANTHROPIC_BASE_URL`, `.op.env`, managed `.env`.
25. `projector_failure_refuses_closed` — exit 1, garbage output, a timeout stub, and output with a missing key, an unknown key or a wrong type (the `ProjectionV1` schema). 25a. `audit_refuses_when_the_home_changes_between_projector_and_guard` (a stub that rewrites `config.yaml` after printing; M-CHANGED). 25b. `no_child_process_runs_under_a_tollgate_lock` (lock-rank recorder around every spawn in `start`, `auth`, `new`, `pool strategy`). 26. `scrub_covers_registry_and_plugin_scan_and_passes_managed_dir` — every `api_key_env_vars` of the pinned list, plus a plugin fixture var, is removed from the spawned env; `HERMES_MANAGED_DIR` is unchanged.
27. `spawn_command_pins_home_shared_dir_provider_and_model_before_user_args` — the recorded argv and env of the stub (`HOME` included). 28. `resolver_prefers_override_then_mise_glob_then_mise_where_then_pipx_then_path` — `mise` is a stub on PATH that records its cwd: it is not run when the glob matches, and runs in `~/.tollgate` with stdin null when it does not.
28a. `pdeathsig_child_exits_when_the_parent_is_already_gone` (fork seam: `getppid` mismatch → `_exit(1)` before exec).
29. `resolver_rejects_the_omarchy_shim_and_never_executes_it` — the shim fixture writes a sentinel file if it is run; the file must stay absent. 30. `version_read_from_metadata_warns_or_refuses_per_policy`.
31. `start_registers_hermes_row_and_marker_then_tears_down` — a live row has `harness = hermes`; `has_live_session` is true during the run; GC leaves the live row and marker. 32. `second_start_on_same_profile_refuses_live` (G14), and `gateway_pid_live_refuses`.
33. `start_refuses_isolated_with_fallback_and_auto_never_picks_hermes`. 34. `post_session_check_flags_anthropic_billing_rows` — `#[ignore = "needs sqlite3"]`, run by the sqlite CI job.
35. `sessions_switch_refuses_hermes_rows` and `bare_name_switch_on_hermes_is_usage_error_exit_2`. 36. `delete_resolves_hermes_after_codex_and_removes_home` (the child home's link targets survive the delete).
37. `guest_mode_allows_hermes_verbs_and_operator_trees_stay_byte_identical` — `~/.clauth` is present; `~/.claude`, `~/.claude.json`, `~/.codex`, `~/.hermes` and the herdr config hash are unchanged across `new`, `key`, `auth` (stub), `start` (stub) and `delete`. 38. `perms_sweep_stops_at_hermes_home_threshold` — an exec-bit file inside `hermes-home/plugins` keeps its mode.

Part 2:
39. `pool_view_holds_no_secret_fields_and_sentinels_never_reach_output` — against `secrets-bearing.json`: the struct, `show --json`, the logs, `usage --json`, the MCP `usage` tool and the herdr tag. 40. `pool_view_marks_env_entry_by_fingerprint`.
41. `strategy_writer_runs_hermes_config_set_idle_only_and_never_writes_auth_json`. 42. `hermes_local_parses_sqlite_json_streams_into_decimal_estimate` — from `sqlite-out/` fixtures, without running sqlite3.
43. `hermes_local_reads_a_wal_db_readonly` — `#[ignore = "needs sqlite3"]`, run by the sqlite CI job; the db's mtime is unchanged. 44. `hermes_local_missing_sqlite3_and_unknown_schema_are_typed_unavailable`.
45. `nous_rate_limit_file_maps_to_rate_limited_and_torn_file_is_ignored`. 46. `collect_emits_hermes_origin_ids_and_openapi_origin_enum_updated` — the dump golden.
47. `which_answers_from_a_tollgate_hermes_home_and_scrub_tollgate_homes_drops_it`. 48. `herdr_tag_uses_hermes_origin_and_hermes_home_join` (the script's `/proc` read yields only `HERMES_HOME`: a fixture environ with a sentinel key never reaches the tag or a log).
49. `native_match_ignores_hermes_profiles`. 50. `tui_filter_cycles_four_states_and_hermes_rows_toast_relaunch`.
51. `status_json_lists_hermes_profiles` and `list_prints_hermes_section`. 52. `completions_offer_hermes_names_for_start_delete_and_hermes_subcommands` — bash, zsh and fish.
53. `herdr_integration_install_skipped_in_guest_mode` — the `herdr` stub records nothing. 54. `daemon_never_executes_hermes_python_or_mise` — the recording stubs stay empty across a daemon tick, `collect`, `list`, `show` without `--check`, and `herdr tag`.

**CI-gated tier `hermes_projector_real`** (feature `hermes-projector-real`, off by default; its own CI job). A
hermetic venv built from pinned wheels (`PyYAML`, `python-dotenv` at the versions 0.19.0 pins) plus a vendored
copy of `utils.fast_safe_load` (`$HSP/utils.py:396-404`, license-checked). The **real** `PROJECTOR` runs
against every `projector-real/` fixture, and its output must equal the expected `ProjectionV1` exactly: anchors,
aliases and `<<:` merges resolve to the routes they hide, and each dotenv form yields the right key set. A
guard verdict test then runs G7–G12 on each real projection. This is the only place the security-critical
projector meets real parsers; tests 22–26 keep using the stub. The job never runs Hermes, mise or the network
(wheels come from a CI cache).

## 8. Implementation slices (two parts; each green on `cargo nextest`, clippy `-D warnings` and fmt)

**Prerequisites.** (1) The shared prep PR (hot-swap spec §8) has landed: `Harness::Hermes`, `Harness::ALL`, the
three-roster validate, the named `ExitCode` table, `NON_CLAUDE_SWITCH`. This lane lands after hot-swap Part 1 and
import Part 1. (2) **Spike S7(f), the `HOME` redirect**, has passed and is recorded in
`docs/spikes/s7f-hermes-home.md` with a machine block like S1's (`result`, `hermes = ["0.19.0"]`, `commit`). The
protocol follows S1: bubblewrap with a tmpfs over the real home, `--unshare-net` with a stub OpenRouter that returns
402, a sentinel `.claude/.credentials.json` placed only in the *outer* fake home, and the real 0.19.0 entrypoint
(never the `~/.local/bin/hermes` shim). It must show: `Path.home()` resolves to the child home; the auxiliary 402
fallback reaches no Anthropic call; the sentinel is never opened (checked with `strace -e trace=openat` or inotify);
and the allowlisted links suffice for git-over-ssh tools. Until it passes, `start` refuses with
`the S7(f) HOME-redirect spike has not passed for Hermes <v>`.

**Part 1 — foundation (H-1a + H-1b + the `start` path; tests 1–38 and their lettered additions).**
- `Harness::Hermes` and `HermesEngine` (the variant itself comes from the prep PR).
- New `src/hermes/{mod,profiles,home,guards,env_file,projector,resolve}.rs`.
- `validate_profile_name` over three rosters: `validate_foreign_harness_free` iterates `Harness::ALL` minus self.
- The child home (§3, G2a), the `HOME` redirect and the extended scrub; G10a and M-AUX; `new`'s auxiliary pinning.
- The `tollgate hermes new|key|auth|delete` CLI.
- `start` / `delete` / `switch` resolution.
- `sessions_cli` refusal; the perms threshold.
- The G11 whitelist parser `PoolAuthView`, in `src/hermes/pool.rs`: needed by G11, and reused in part 2.
- `hermes list` in plain form, roster only, with no estimate yet.
- Docs: `CHANGELOG.md` "Hermes profiles (foundation)" and plan §10 "Shipped: H-1a/H-1b".
- Every exhaustive `match Harness` compiles with a Hermes arm.
- No observation changes.

**Part 2 — surfaces (H-1c + H-1d-lite + H-2 + H-3 + H-4 semantics; tests 39–54).**
- `Origin::HermesProfile`, `usage/hermes_local.rs` + the cache + the collect hook + the daemon tick call + the card lines (`cards.rs:488,527`).
- The pool view and the strategy writer; `hermes show`, `pool`, and `list` with the estimate.
- The `which` arm; `scrub_tollgate_homes`.
- The TUI filter and rows.
- The `status.json` `hermes_profiles[]`; the list section; completions.
- herdr: the tag origin, the `--hermes-home` join, `native_match`, the script's `/proc` read, H2h.
- The post-session check (§4.4 6.2–6.3); the MCP `switch_profile` refusal.
- The `hermes_projector_real` CI tier and the sqlite CI job.
- The OpenAPI golden (`tests/dump_openapi.rs`) changes here only, after the other lanes' Part 2s.
- Docs: README "Hermes profiles", `docs/agent-api.md` origin, plan §10 update, CHANGELOG.

## 9. Decisions taken now (defaults), and open questions

| # | Decision | Default and why |
|---|---|---|
| D-H1 | Home location | `~/.tollgate/profiles/<name>/hermes-home`, not the plan's `~/.local/share/<tool>/hermes/<name>`. The parent is `<name>`, never `profiles` (the name is refused), and the home is not under `~/.hermes`. So `get_default_hermes_root() == home` (`hermes_constants.py:154-191`), `_global_auth_file_path()` is None (`auth.py:916-940`), and `active_profile` is read from the home only, where G3 guards it. It reuses `profile_dir`, rotation locks, markers, `has_live_session`, delete and GC, and sits on the same filesystem and data root as the rest of tollgate. A second root would leak outside `identity::DATA_DIR` |
| D-H2 | YAML and dotenv parsing | Hermes' own interpreter (`python -I -B -c PROJECTOR`, using `utils.fast_safe_load` and python-dotenv) at user-initiated verbs only. This gives exact parser parity, no new Rust dependency, and fails closed. Rejected: a Rust YAML crate (a parser different from Hermes' own, and a new dependency) |
| D-H3 | `state.db` access | The `sqlite3` CLI with `-readonly -json`; when it is missing, the reading is typed `Unavailable`. Rejected: `rusqlite` bundled (it builds C SQLite, which puts a C toolchain in every build, the cost `Cargo.toml` avoids for rustls) |
| D-H4 | Entrypoint | Re-resolved every launch, never cached (plan §4.8 said "cache"; `mise up` moves the dir). The shim is never executed |
| D-H5 | Version gate | `warn` by default for anything outside 0.19.x; `refuse` is opt-in. Hermes ships several times a week |
| D-H6 | Concurrency | One live tollgate session per Hermes profile; parallel work uses separate account homes (D11) |
| D-H7 | Provider and model | Passed as `--provider` / `-m` at every launch; the roster is the authority. tollgate never writes `config.yaml` itself; the strategy goes through `hermes config set` |
| D-H8 | Defaults for `new` | Provider `nous`, OAuth; openrouter and ollama-cloud use env mode; `--pool` is opt-in (D11) |
| D-H9 | `active_profile` | Hermes' own rule: an empty or `default` file passes (plan §4.8), rather than refusing any file at all |
| D-H10 | Estimate | UTC month to date; billed cost where it is > 0, else Hermes' estimate; `Decimal` via `Amount` |
| D-H11 | H2h in guest mode | Skipped, with the command printed. Whether `herdr integration install` touches herdr's own config is unconfirmed |
| D-H12 | Status field | `hermes_profiles[]` with `{name, provider, model, mode, live}`. There is no `active_hermes_profile`: Hermes has no global slot, and tollgate never writes `~/.hermes` |
| D-H13 | `.env` ownership | tollgate manages one line, the bound key; other lines stay the user's; `ANTHROPIC_*` refuses |
| D-H14 | Orphan protection | `PR_SET_PDEATHSIG(SIGTERM)` on Linux |
| D-H15 | The implicit and `/model` Anthropic routes | **Prevented** by the child `HOME` redirect (no `.claude` for Hermes to read or rewrite) plus the scrub of `ANTHROPIC_*` / `CLAUDE_CODE_OAUTH_TOKEN`, with pinned auxiliary providers as defence in depth (G10a). S7(f) is a Part 1 prerequisite, not a later spike. The post-session `state.db` check stays as evidence and is not claimed as proof |
| D-H16 | Scrub | A pinned registry list plus a regex scan of the provider plugins; never an import of Hermes code |
| D-H17 | Plan H-1d's daemon live-slot watcher (watch `~/.claude/.credentials.json` during Hermes sessions) is dropped | With the child `HOME` redirect, a Hermes child has no path to the operator's live slot, so a watcher would guard a route that no longer exists. The teardown `symlink_metadata` compare and the child-home audit (§4.4 6.3) remain as cheap evidence. Deviation from plan §5 H-1d, recorded here |
| D-H18 | Child home location | `profiles/<name>/child-home`, a **sibling** of `hermes-home`, not the review's suggested `hermes-home/.tg-home`: `hermes backup` zips the whole Hermes root (`hermes_cli/backup.py:1203`) and would follow the `.gitconfig` link into the archive, and the home's contents are Hermes' own |
| D-H19 | Entrypoint via mise | Read the mise install glob directly; run `mise where` only as a fallback, from `~/.tollgate` with stdin null, so a project `.mise.toml` never loads its env or hooks |

Open questions for the owner:
1. Allow two concurrent sessions on one account home (Hermes' `state.db` is multi-process, `hermes_state.py:994-1011`)? Default: no.
2. Should `version_policy` default to `refuse` once S7(c) records a schema-drift history?
3. Run H2h in guest mode after verifying it writes only `HERMES_HOME`?
4. Poll OpenRouter `/key` for a Hermes env-mode key, reading `.env` under the configured-endpoint policy? Default: no; use `monitors.toml`.
5. Automate H-4 (stop, strategy, `--resume`) once P6b lands, and the herdr `tollgate.swap` leg?
6. Default `new` provider: `nous`, or require `--provider`?

## 10. Code anchors (sites to change)

| Site | Change |
|---|---|
| `src/harness.rs:19-47` | `Hermes` variant, `as_str`, `engine` |
| `src/harness.rs:116-188` | add `HermesEngine` after `CodexEngine`: install bails; `command` = the resolved entrypoint; `home_env_key` = `HERMES_HOME`; `scrub_env` = `SCRUB` + `scrub_tollgate_homes` + `scrub_billing_env` |
| `src/codex_profiles.rs:61-214` | pattern for the new `src/hermes/profiles.rs` (`HermesState`, `update`, `save`) |
| `src/actions.rs:59-76,87-112` | `validate_foreign_harness_free` iterates `Harness::ALL` minus self (prep PR); `validate_profile_name` gains the Hermes own-roster arm. The import lane calls the same function and has no roster reader of its own |
| `src/actions.rs:1305-1345` | pattern for `delete_hermes_profile` |
| `src/cli.rs:81-574` | `Command::Hermes`, `HermesCommand`, `HermesAuthAction`, `HermesPoolAction` |
| `src/main.rs:72-90` | `unknown_profile_error` lists `hermes:` names |
| `src/main.rs:111-129` | `resolve_or_bail` names a hermes profile |
| `src/main.rs:224-348` | dispatch `Command::Hermes` |
| `src/main.rs:508-530` | `cmd_start` hermes branch → `start::run_hermes` |
| `src/main.rs:1624-1631,1682-1700` | the hermes delete leg |
| `src/main.rs:1780-1791` | switch → M-SWITCH usage error |
| `src/start.rs:607-622,643-700,771-835` | patterns for `hermes_spawn_command`, `MANAGED_DIR_OVERRIDE`, `run_hermes` |
| `src/runtime.rs:373-382` | `scrub_tollgate_homes` drops a tollgate `HERMES_HOME`; add `is_hermes_home_path` next to `is_codex_home_path` (`runtime.rs:347-360`) |
| `src/runtime.rs:472-509,552-594` | markers reused unchanged (`sessions-<sid>`); `has_live_session` covers hermes by the `sessions*` scan |
| `src/runtime.rs:7341-7470` | pattern for `HermesRuntime::acquire` (Rotation → State → marker → row) |
| `src/profile.rs:2441-2471` | perms sweep stops at the `hermes-home` and `child-home` thresholds (never chmods through the child home's links) |
| `src/testutil.rs:1401` | the same threshold in the test perms twin |
| `src/live_sessions.rs:43-55,108-120` | doc: a Hermes row, `launch_store: None` |
| `src/sessions_cli.rs:217-221` | `!= Harness::Claude` → `NON_CLAUDE_SWITCH` (M-SESSION-SWITCH) / codex text; the constant is defined in the prep PR, and `run_switch` is rewritten by hot-swap first |
| `src/which.rs:59-107,109-180` | Hermes claim arm after the codex arm (`harness: "hermes"`, `source: "hermes_home"`) |
| `src/usage/observation.rs:606-628` | `Origin::HermesProfile` (`hermes:`) |
| `src/usage/collect.rs:95-98` | append `crate::usage::hermes_local::hermes_observations` |
| `src/usage/cards.rs:488-489,527-528` | relogin hint and `(hermes)` suffix; render `estimate` |
| `src/daemon/tick.rs:68` | `hermes_local::refresh_detached()` next to `poll_detached` |
| `src/daemon/status_json.rs:579,693` | `hermes_profiles[]` |
| `src/list.rs:96,211-222` | a hermes section in `render_table` |
| `src/completions.rs:10-294,525-540` | hermes subcommands; `__complete --hermes` names; start/delete include hermes |
| `src/tui/app.rs:1637-1667,1695,3823` | `HarnessFilter::Hermes`, `shows_hermes`, hermes rows |
| `src/herdr/tag.rs:73-80,96-134,140-152` | Hermes origin lookup, `--hermes-home` arg, `native_match` exclusion |
| `herdr-plugin/report-profile.sh:190-199,245-254` | `HERMES_HOME`-only extraction from `/proc/<pid>/environ` (`tr '\0' '\n' … \| sed -n 's/^HERMES_HOME=//p' \| head -1`) for a bare hermes pane |
| `src/runtime.rs:373-382` (scrub) and `src/harness.rs` `HermesEngine::scrub_env` | add `CLAUDE_CONFIG_DIR`, `GH_TOKEN`, `GITHUB_TOKEN`, `XDG_{CONFIG,DATA,STATE,CACHE}_HOME` to the Hermes child scrub; set `HOME` to the child home |
| `src/start.rs` (`run_hermes`) | spawn from the main thread; `pre_exec` PDEATHSIG + `getppid` check; projector and resolver before the RotationGuard, re-stat inside |
| new `docs/spikes/s7f-hermes-home.md`, `tools/spikes/s7f/` | the S7(f) prerequisite (§8) |
| `.github/workflows` (or the CI config in use) | `hermes_projector_real` tier; the sqlite job running the `#[ignore = "needs sqlite3"]` tests |
| `src/mcp/mod.rs:370-395` | `switch_profile` refusal for hermes names |

Unchanged, used as-is: `src/lockorder.rs:117,191` (ranks); `src/usage/observation.rs:1015-1021` (`LocalEstimate`); `src/local_api/routes.rs:304-340` (Hermes source already listed; OpenAPI golden via `tests/dump_openapi.rs`); `src/usage/monitor/config.rs:50-51,177-181` (the nous hint names `hermes_home`); `src/tui/app.rs:8402,9172` (already route through `validate_profile_name`); `src/cli.rs:575-632` (StartArgs doc gains one line on hermes pass-through).

## Review log

Critique of 2026-09-29, each item verified against the code and the installed Hermes 0.19.0 source (read only,
never executed) before editing. Nothing was rejected outright; one fix was applied in a different shape (item 3,
child-home location, D-H18).

| # | Sev | Item | Outcome |
|---|---|---|---|
| 3 | BLOCKING | Implicit anthropic route (auxiliary `auto` / 402 fallback → `resolve_anthropic_token` → `~/.claude/.credentials.json`) | **Applied.** Confirmed at `agent/auxiliary_client.py:7-22,36-37,2799-2818`, `agent/anthropic_adapter.py:958,1160-1167,1298`, `agent/credential_sources.py:6`. Child `HOME` redirect (§3, G2a, §4.4 step 4); G2 recomputed against the child home; S7(f) moved into Part 1 as a prerequisite (§1, §8); G10a with pinned auxiliary providers set by `new` (§4.1 step 7) and M-AUX; §6.1 and D-H15 rewritten; tests 21a, 21c, 21d, 21e. **Shape changed:** the child home is `profiles/<n>/child-home`, a sibling of `hermes-home`, not `hermes-home/.tg-home`, because `hermes backup` zips the Hermes root (`hermes_cli/backup.py:1203`) and would read through the links (D-H18). Added in the same pass: `.anthropic_oauth.json` refusal (G11, test 21f), and scrubbing `XDG_*_HOME`, `CLAUDE_CONFIG_DIR`, `GH_TOKEN`, `GITHUB_TOKEN`, which would otherwise route around the redirect (test 21b) |
| 7 | IMPORTANT | "Hermes need not stop" contradicts import; M1/M2 naming | **Applied.** §6.2 says stop Hermes sessions before import; the import spec now defines M1/M2 |
| 8 | IMPORTANT | Two uniqueness implementations | **Applied.** `validate_foreign_harness_free` over `Harness::ALL` minus self (§8, §10); import calls the same function; the second lane adds `import_refuses_a_name_held_by_a_hermes_profile` |
| 13 | IMPORTANT | Hermes rows would read as `executor: oauth` | **Applied** (owned by the hot-swap spec §3.1); §6.2 states `executor = none` / `null` |
| 16 | IMPORTANT | Projector only ever a stub | **Applied.** `hermes_projector_real` CI tier with real PyYAML / python-dotenv and anchor/alias/merge-key/`export`/multiline fixtures; strict `ProjectionV1` schema (test 25) |
| 17 | IMPORTANT | Cross-lane merge hazards | **Applied.** §8 prerequisites (prep PR, landing order), OpenAPI golden only in Part 2 |
| 18 | MINOR | Shared switch-refusal text | **Applied.** M-SESSION-SWITCH is `sessions_cli::NON_CLAUDE_SWITCH` |
| 29 | MINOR | `mise where` and projector under RotationGuard | **Applied.** They run before the guard; the guard re-stats four files (M-CHANGED); tests 25a, 25b |
| 30 | MINOR | `mise where` in the caller's cwd | **Applied.** Install glob first; `mise where` fallback from `~/.tollgate`, stdin null (D-H19, test 28) |
| 31 | MINOR | Whole `environ` read into the shell | **Applied.** `HERMES_HOME`-only extraction (§6.3, test 48) |
| 32 | MINOR | `--key-stdin` vs `--stdin` | **Applied.** `--stdin` on both |
| 33 | MINOR | sqlite tests pass silently | **Applied.** `#[ignore = "needs sqlite3"]` + CI job with `--run-ignored` |
| 34 | MINOR | PDEATHSIG thread and race | **Applied.** main-thread spawn, `getppid` check after `prctl` (test 28a) |
| 35 | MINOR | H-1d watcher dropped silently | **Applied.** D-H17 |
