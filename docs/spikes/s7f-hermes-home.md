# S7(f) spike: the child `HOME` redirect for Hermes

Date: 2026-09-29. Spec reference: `docs/specs/hermes-harness.md` §8 (Part 1 prerequisite), §3, §4.4
step 4, G10a, D-H15. Harness: `tools/spikes/s7f/`. The condensed evidence is in
`tools/spikes/s7f/evidence/`; `summary.txt` is the output of `summarize.py`.

**Result: PASS for Hermes 0.19.0.** A Hermes started with the §4.4 env (`HOME` = the child home,
`HERMES_HOME` = the tollgate home, a cleared environment) resolves `Path.home()` to the child home.
It opens nothing under the outer home except through the allowlisted links. The auxiliary 402
fallback makes no Anthropic call, even with Hermes' own anthropic gate forced open. The sentinel
`~/.claude/.credentials.json` is never opened. The links are enough for git; for ssh, see finding 4.

## Version under test

The entrypoint was resolved the way spec §4.5 step 2 resolves it: the mise install glob, the
highest semver dir, then the shebang check. `~/.local/bin/hermes`, the Omarchy shim, was not bound
into the sandbox and was never run.

```
entry=/home/abobreshov/.local/share/mise/installs/pipx-hermes-agent/0.19.0/hermes-agent/bin/hermes
python=/home/abobreshov/.local/share/mise/installs/pipx-hermes-agent/0.19.0/hermes-agent/bin/python
version=0.19.0                       # hermes_agent-0.19.0.dist-info/METADATA
$ hermes --version                   # evidence/01-version.txt
Hermes Agent v0.19.0 (2026.7.20)
```

## Harness

| File | Role |
|---|---|
| `run.sh` | Outer launcher. It runs three bubblewrap phases (setup, main, control) and keeps `inotifywait` on the sentinels from **outside** the sandbox, on the host path of the bound run dir. It also lists the outer home before and after the main phase, outside any inotify window |
| `driver.py` | Runs inside the sandbox. It builds the fake outer home and the tollgate layout, resolves the entrypoint, restarts the stub for each scenario, and runs the scenarios |
| `stub.py` | A stub OpenAI-compatible endpoint on `127.0.0.1:18097`. It answers the first `--ok` chat requests with `ok` and sends HTTP 402 "Insufficient credits" (OpenRouter's body) to every later one |
| `probe.py` | Runs under Hermes' **own** venv interpreter with the child env. It imports Hermes' real modules and reports where they look for Claude Code credentials, then drives the real `call_llm` 402 fallback. A spy on `_try_anthropic` calls straight through and records whether the chain reached that step. It prints booleans, paths and labels, never a token |
| `auditwrap.py` | Runs the real entrypoint script (`runpy.run_path`) with a path recorder in front of it. The recorder is `sys.addaudithook` for open, listdir, scandir, mkdir, rename, remove, symlink, chmod, Popen and connect, plus wrapped `os.stat` / `os.lstat`, because Hermes' credential reader starts with `Path.exists()`. The sandbox has no strace, so this stands in for it for the Python half of the process, and inotify covers C-level opens |
| `summarize.py` | Classifies every recorded path relative to the outer home, echoes the probes and inotify logs, prints `VERDICT`, and with `--export` copies the evidence |

### Isolation (from `run.sh`)

```
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /run \
  --tmpfs "$HOME" \
  --bind "$RUN" "$RUN" \
  --ro-bind "$VENV" "$VENV" --ro-bind "$PYHOME" "$PYHOME" \
  --unshare-all --die-with-parent \
  --clearenv --setenv PATH /usr/bin:/bin --setenv HOME "$RUN/home" --setenv LANG C.UTF-8 ...
```

- The tmpfs over the real `$HOME` hides `~/.claude`, `~/.clauth`, `~/.codex`, `~/.hermes` and
  `~/.config/herdr`. Inside the sandbox the real home lists only `.local`, the mount point of the
  read-only Hermes venv (`evidence/entrypoint.txt`).
- `--unshare-all` includes the network: the stub is the only reachable endpoint.
- The only credentials anywhere are the fake sentinels: `.claude/.credentials.json` with an
  `S7F-SENTINEL` token and `.claude.json`, both placed only in the **outer** fake home
  `<run>/home`. `<run>` is outside the repository. No real credential file was read, and tollgate and
  clauth were never run.

The layout matches spec §3. The outer fake home is `<run>/home`. `HERMES_HOME` is
`<run>/home/.tollgate/profiles/s7f/hermes-home`, with `shared/` as `HERMES_SHARED_AUTH_DIR`, and
`HOME` is `<run>/home/.tollgate/profiles/s7f/child-home` (0700). The child home holds only
`.gitconfig`, `.config/git` and `.ssh`, each a link to its outer-home twin. `config.yaml` sets
`model.provider: custom` at the stub and leaves every auxiliary task at its default (`auto`). That
is the worst case the redirect has to cover, because the pinned auxiliary providers (the second
layer of G10a) are absent.

## Scenarios and results

| # | Scenario | Result |
|---|---|---|
| 01 | `hermes --version` (real entrypoint, audited) | rc 0. It opened only files under `HERMES_HOME` |
| 02 | `hermes config set auxiliary.curator.provider custom` (the `new` pin path, audited) | rc 0. It wrote `HERMES_HOME/config.yaml` and nothing else |
| 03 | probe `redirect` | `Path.home()` and `expanduser("~")` are the child home. `get_hermes_home()` and `get_default_hermes_root()` are the tollgate home, so `active_profile` is read from the home itself (G2, G3). The Claude credential path is `<child-home>/.claude/.credentials.json`, which does not exist. `read_claude_code_credentials()` and `resolve_anthropic_token()` both find nothing. Neither `call_llm` 402 run reached `_try_anthropic`: 0.19.0's own gate (`auxiliary_client.py:1941-1951`, `is_provider_explicitly_configured("anthropic") == false`) skipped it |
| 04 | probe `forcegate`: the anthropic gate forced open, `local/custom` marked unhealthy (the exhausted-OpenRouter shape) | The 402 fallback walk **reached** `_try_anthropic` (once from `call_llm`, once from `_try_payment_fallback("openrouter")`). Both times it returned no client, because the resolver read `<child-home>/.claude/.credentials.json`, which is absent. The log reads `payment error on auto and no fallback available (tried: openrouter (unhealthy), nous (unhealthy), local/custom (unhealthy), api-key)`. The stub saw only its own requests, so no Anthropic call was made (the network is unshared, and no Anthropic client was built) |
| 05 | `hermes chat -q "S7F say ok" --provider custom -m stub-model` (real entrypoint, audited) | The turn was answered (`ok`). The auto title that followed got 402 twice and failed with `⚠ Auxiliary title generation failed: HTTP 402` (`evidence/stub.jsonl`, `05-chat.txt`). rc 0 |
| 06 | git and ssh with `HOME` = the child home | `git config --global --get user.email` was read from `<child-home>/.gitconfig`, via the link. The global excludes file was read from `<child-home>/.config/git/ignore`, via the link. `ssh -F <child-home>/.ssh/config -G s7f-git` resolved the host block through the link. See finding 4 |
| 07 | control: probe with `HOME` = the outer home (no redirect) | `resolve_anthropic_token()` returned the **sentinel**, and inotify recorded 4 OPEN / 4 ACCESS events on it. This proves that both the probe and the watch can see the route when it is open |

Evidence that nothing leaked (`evidence/summary.txt`):
- **inotify, main phase: 0 events** on `.claude/.credentials.json`, `.claude.json` and `.claude/`,
  across scenarios 01–06.
- **Audit logs: no `OUTSIDE` path.** In all three real-entrypoint runs, every recorded path under
  the outer home was one of four things: inside `HERMES_HOME`, inside the child home, an ancestor
  directory `lstat` (path resolution), or an `lstat` through the `.ssh` link.
- **The outer home is byte-identical** before and after the main phase, `HERMES_HOME` aside
  (`outer-before.txt` = `outer-after-main.txt`).
- **The child home is unchanged.** After a full chat session it still holds only the three links
  (`child-home-after.txt`). Hermes created no `.hermes`, `.claude`, `.cache` or `.local` in it, so
  G2a will not refuse the next launch.

## Findings for the implementation

1. **The redirect is decisive.** In 0.19.0 the anthropic step of the auto chain also sits behind
   `is_provider_explicitly_configured("anthropic")`. That gate reads `auth.json`
   `active_provider`, `config.yaml` `model.provider` and the anthropic env vars, and each of those
   is already refused or scrubbed by G10–G12. With the gate forced open, the child `HOME` alone
   still keeps the resolver away from the operator's file (scenario 04). The pinned auxiliary
   providers (M-AUX) stay as the third layer.
2. **Hermes probes `~/.hermes` under the child home**: 117 `lstat`s of `<child-home>/.hermes/...`
   in one chat, never a create. G2 is therefore computed against the child home, as the spec says.
3. **Hermes `lstat`s credential-bearing paths in `~`** as part of its sensitive-path scan:
   `.aws`, `.azure`, `.config/gcloud`, `.config/gh`, `.docker`, `.git-credentials`, `.gnupg`,
   `.kube`, `.netrc`, `.npmrc`, `.pgpass`, `.pypirc`, `.ssh/{authorized_keys,config,id_ed25519,id_rsa}`.
   These are `lstat`s only, never an open. Under the redirect they all land in the child home, and
   only `.ssh` reaches the operator's tree, through its allowlisted link. This is another reason
   the child home must never carry `.config/gh` or `.claude`.
4. **OpenSSH ignores `$HOME`.** It expands `~` from the passwd entry (`pw_dir`, logged in
   `06-ssh-passwd-home.txt`), so in a real launch Hermes' ssh subprocesses read the operator's own
   `~/.ssh` whatever `HOME` says. git honours `$HOME`, and the `.gitconfig` and `.config/git`
   links are what it needs. The `.ssh` link serves tools that take `~` from `$HOME`. Together they
   suffice for git-over-ssh, and the sandbox runs `ssh -F` through the link only because its
   passwd home is the tmpfs. Inside the user namespace, root-owned `/etc/ssh` files appear as the
   overflow uid, and ssh refuses them.
5. `hermes config set` (the `new` step 7 pin) writes `HERMES_HOME/config.yaml` by atomic rename, and
   touches nothing outside the home.

## Re-run before Part 2 (2026-09-29, branch `tg/hermes-p2` @ `85edc959`)

The same harness was run again, unchanged, before the Part 2 surfaces were built:
`tools/spikes/s7f/run.sh <scratch dir outside the repo>`, then `summarize.py`. Same bubblewrap
isolation (tmpfs over the real home, `--unshare-all`, `--clearenv`), the same real 0.19.0
entrypoint resolved through the mise glob and the shebang check; the `~/.local/bin/hermes` shim was
not bound and never ran. **Result: PASS again.**

- inotify, main phase: 0 events on the sentinels; control phase: 4 OPEN / 4 ACCESS on
  `.credentials.json` (the instruments still see an open route).
- The outer home listing before and after the main phase is identical (`HERMES_HOME` excluded).
- No `OUTSIDE` path in any audit log; the only outer-home paths are `lstat`s through the `.ssh` link
  (9 in the chat run: `.ssh`, `authorized_keys`, `config`, `id_ed25519`, `id_rsa`).
- Probe `redirect`: `Path.home()` = the child home, no credential found, `_try_anthropic` not
  reached. Probe `forcegate`: `_try_anthropic` reached once from `call_llm` and once from
  `_try_payment_fallback`, and returned no client both times.
- The chat turn answered `ok` (rc 0); the child home still holds only the three links.

The evidence in `tools/spikes/s7f/evidence/` is from the first run; this run's output matched it
line for line in the verdict-bearing sections and was not re-exported.

## Machine block

tollgate compiles this block in (`src/hermes/resolve.rs`). `tollgate start <hermes-profile>` refuses
with `the S7(f) HOME-redirect spike has not passed for Hermes <v>` unless `result = "pass"` and the
installed version's `major.minor` series matches one of the listed versions. The series match is
the same 0.19.x band that W-VERSION and D-H5 use.

```toml
# s7f-gate
result = "pass"
hermes = ["0.19.0"]
commit = "ea1d0c64"
```
