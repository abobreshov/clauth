# Importing clauth

`tollgate import clauth` moves an upstream clauth 0.16.0 install (`~/.clauth`) into tollgate (`~/.tollgate`). Afterwards tollgate is the only tool that writes the refresh chains, the live credential slots and Claude Code's shared config, and [guest mode](Guest-Mode) is off. The design is in [the import spec](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/specs/import-clauth.md). This page covers using it.

Linux only. On macOS and Windows the import refuses (`unsupported_platform`), because the macOS default Keychain item is a second live slot the import does not move.

## The commands

| Command | What it does |
|---|---|
| `tollgate import clauth --dry-run [--json] [--rename OLD=NEW]… [--adopt-live]` | Prints what the import would do. Changes nothing and creates no file, not even a lock file |
| `tollgate import clauth [--yes] [--json] [--rename OLD=NEW]… [--adopt-live]` | The import itself: the same report, one confirmation, then the transaction |
| `tollgate import clauth --resume` | Continues an import that was interrupted |
| `tollgate import rollback [--yes] [--json] [--adopt-live]` | Undoes an import, or finishes undoing one that stopped part-way |
| `tollgate import status [--json]` | Reads the journal's state and says what to run next. Takes no lock |
| `tollgate import retire [--yes] [--step r1\|r2\|r3\|r4]…` | The checklist to run after a committed import |

`--json` prints one JSON document on stdout and nothing else there. Prompts and progress go to stderr. With no `--yes`, a real run, rollback or retire reads its answer from the terminal. On a non-interactive stdin it refuses instead, exits 2 and changes nothing:

```
tollgate import clauth: refusing to change files without --yes on a non-interactive stdin
```

## Before you start

Run the dry-run first:

```
tollgate import clauth --dry-run
```

The report lists:

- every entry under `~/.clauth` and what the import does with it: `move`, `copy`, `copy-0600`, `merge`, `skip`, `never` or `refuse`
- both live slots (`~/.claude/.credentials.json`, `~/.codex/auth.json`) and their verdict
- the profiles on each roster, and any renames
- the global edits (G1–G4, below)
- every lock file the import would hold, and whether it is free, held or absent
- the processes it found
- how many steps it would journal
- how many files and bytes it copies while it holds its locks
- every warning and blocker

It exits 0 when the import could run and 3 when something blocks it.

The import refuses while anything could write the files it moves. That means you close, first:

- every Claude Code session, including the ones that run `clauth mcp`
- every clauth process: the TUI, the daemon, `clauth mcp`
- every other tollgate process: the daemon, the TUI, `tollgate api serve`, `tollgate mcp`, and every `tollgate start` session, Hermes sessions included
- codex, but only when a codex store or the codex slot is being imported

A few read-only tollgate commands only warn: `herdr tag`, `usage`, `__complete`, `which`, `status`, `list` and `import status`. They wait on the import's locks and exit.

Other blockers name their own fix, for example:

- **`pending_rotation`**: a crashed rotation left a staged chain. Run `clauth list` once so upstream adopts it.
- **`name_collision`**: a tollgate profile already has that name, on the claude, codex or [Hermes](Hermes) roster; the message names which. Pass `--rename <old>=<new>`.
- **`claude_live_diverged`**: the live slot holds a different login than its profile's store. Pass `--adopt-live` to import the live one; the superseded stored chain is parked in the profile's `quarantine/`. An immediate rollback restores both original inodes. After a tollgate refresh, rollback returns the current chain to the upstream store and keeps the superseded chain in quarantine. A slot holding another profile's login (`live_is_other_profile`) or an older login than the store (`live_older_than_store`) is refused even with `--adopt-live`.
- **`dev_build_exe`**: you ran a `target/` build instead of the installed `tollgate` on `PATH`. Run the installed one, so the rewritten `apiKeyHelper` points at it.
- **`unknown_entry`**: `~/.clauth` holds a file the import does not know. It might carry a credential, so nothing is imported.

## What moves

Each profile's chain carriers are **moved by rename, never copied**, so every refresh chain keeps exactly one inode:

- claude: `credentials.json`, `session-token.json`, `session-token.static.json`, `mcp-logins.json`, `quarantine/`
- codex: `auth.json`, `auth.lkg.json`, `auth.quarantine.json`

A rename across filesystems is refused (`cross_device`) rather than falling back to a copy.

Everything else is copied or skipped:

- **Copied at 0600:** `config.toml` (it can hold an API key, so the journal records only its size), and the caches and histories.
- **Tree copies:** `conversations/` and `codex-home/`. The destination wins on a name that already exists.
- **Merged:** the rosters and `session_profiles.json`. On the rosters, tollgate's set keys win and upstream's fill the ones it leaves unset. `[serve]`, `[update]` and `[local_api]` are never imported.
- **Skipped:** logs, status feeds, caches, per-session trees, daemon-API secrets, and macOS-only state.
- **Guest transcripts:** the ones in `~/.tollgate/guest-claude/projects` are copied into `~/.claude/projects`, so `tollgate sessions` and `--resume` still find them once guest mode is off.

Live slots:

- **A slot that links into an upstream store** is repointed at the moved store in the same step.
- **A regular-file claude slot** that holds its active profile's login becomes the store. It is renamed onto the store, keeping its newer `mcpOAuth`, and then linked. On a `session-token.json` profile it is relinked and the copy dropped instead, so the install source does not change.
- **An independent login, or a missing slot**, is left alone.

## The global edits

One confirmation covers all of them, and each is journaled with its prior value:

| Edit | When | File | Change |
|---|---|---|---|
| G1 | inside the locks | `~/.claude/settings.json` | `enabledPlugins["clauth@clauth"]` `true` → `false` |
| G2 | before any lock | herdr's `config.toml` | runs upstream's own `clauth herdr uninstall --yes` |
| G3 | inside the locks | `~/.claude/settings.json` | upstream's `apiKeyHelper` becomes tollgate's for the same profile, and each allowed `mcp__plugin_clauth_clauth__*` tool is renamed `mcp__plugin_tollgate_tollgate__*` |
| G4 | inside the locks | `~/.claude/plugins/installed_plugins.json` | install paths under `~/.clauth/profiles/` are repointed: first to tollgate's copy, else to the `~/.claude/plugins` twin, else kept with a warning |

Before G2 runs, the import backs up herdr's config and records upstream's plugin: its source, owner, repo, commit, managed path and enabled flag. G2's child runs with its environment cleared, except the basics and upstream's `CLAUTH_NO_UPDATE`, `CLAUTH_NO_COMPLETIONS` and `CLAUTH_NO_API`. Its stdin is closed and it gets 20 seconds. With no herdr or no upstream binary, G2 is skipped with a warning.

`settings.json`'s `env` is never copied into the journal, a backup or the report.

## The transaction

After the confirmation:

1. The process scan and the lock probes run again, since the prompt had no time limit.
2. The journal is written in state `pre`, and G2 runs.
3. The import takes its locks, all at once and in a fixed order. These are upstream's and tollgate's daemon, standby and fetch leases, every upstream rotation lock, upstream's `~/.clauth/.lock`, and tollgate's state lock.
4. Under the locks it checks everything again, plans every step, and writes the journal `in_progress`.
5. It runs the steps:
   - upstream's binary is retired first: `clauth` is renamed `clauth-0.16.0.retired` and replaced by a shim
   - then the moves, the copies and the merges
   - then G1, G3 and G4
   - then `active_profile` is removed from upstream's rosters, and the `~/.clauth/MIGRATED` tombstone is written
6. The journal's last write is `complete`, and guest mode ends.

While the locks are held, nothing prompts, spawns a process or uses the network.

Every step is journaled before it runs and marked done after, each write synced to disk. A refusal before any store moved restores herdr's config, records the journal `aborted` and exits 3. Anything that goes wrong after a store moved reverses every step, records `aborted` and exits 1. In both cases guest mode stays on.

On success it prints:

```
tollgate: imported 3 claude and 0 codex profiles from ~/.clauth; guest mode is off. Next: tollgate import retire
```

## If it is interrupted

While the journal is `pre`, `in_progress` or `rolling_back`, every tollgate command prints this on stderr:

```
tollgate: an import of clauth was interrupted at step <n>; run 'tollgate import clauth --resume' or 'tollgate import rollback'
```

- **`tollgate import clauth --resume`** continues forward from the journal.
- **`tollgate import rollback`** undoes it.

Both check every step against disk first. A step that matches neither its before nor its after state stops everything with exit 4 and changes nothing more.

## Rolling back

`tollgate import rollback` undoes the import in reverse:

1. It undoes the retire steps first, before taking its locks, because they run `claude` and herdr.
2. Under the locks it puts the live slots back as links to the restored stores, never as copies. Each carrier goes back to its upstream path: whatever file tollgate holds now, so a rotation since the import is kept. It deletes only the copies the import made, and removes only the imported names from the rosters. Profiles created after the import stay.
3. It restores upstream's binary last, after the stores. If the retired binary is gone, it prints the manual reinstall steps and leaves the shim.
4. Once the locks are released, it tries to reinstall upstream's herdr plugin at the recorded commit, which needs the network:

   ```
   herdr plugin install <owner>/<repo>/herdr-plugin --ref <commit> --yes
   ```

   If that fails, it prints this command, and `clauth herdr install --yes --no-config` as a fallback.

Rollback refuses while any of these is true:

- tollgate, Claude Code, or codex (when codex was imported) is running
- an imported profile holds a staged chain
- the claude slot links to a profile created after the import (`tollgate switch` back to an imported one first)
- an upstream store path is occupied again

These checks run again after the confirmation, since the prompt has no time limit. Tollgate's own rosters are never copied back into `~/.clauth`.

## After the import: `import retire`

| Step | What it does | Undo (by `import rollback`) |
|---|---|---|
| `r1` | Removes upstream's wiring: `enabledPlugins["clauth@clauth"]` and `extraKnownMarketplaces.clauth` from `settings.json`, `clauth` from `known_marketplaces.json`, `plugins["clauth@clauth"]` from `installed_plugins.json`, `mcpServers.clauth` from `~/.claude.json`. A value that looks like it holds a secret is left in place and named | puts each value back where it was |
| `r2` | Installs `tollgate@tollgate` | removes only the plugin |
| `r3` | Installs tollgate's herdr plugin. With `--yes` there is no key to bind, so herdr's config is left alone | uninstalls it |
| `r4` | Replaces upstream's completion `source` line in `~/.bashrc` (and its `# clauth completions` comment) with tollgate's | puts the lines back |

Each step is journaled. A step already done is skipped. Retire refuses while Claude Code, clauth or another tollgate is running.

These stay yours to do by hand, when you are sure:

- delete `clauth-0.16.0.retired`
- run `cargo uninstall clauth`
- remove `~/.clauth`

## Exit codes

| Code | Meaning |
|---|---|
| 0 | dry-run clean, import committed, rollback finished, status printed |
| 1 | failed after stores moved; every step was reversed (journal `aborted`) |
| 2 | usage: a bad flag, a non-interactive stdin without `--yes`, `--resume` with nothing to resume |
| 3 | blocked before anything changed (at most, G2 was undone) |
| 4 | the journal needs you: an interrupted import, a journal that disagrees with disk, or a reversal that stopped part-way |

## Files

| Path | What |
|---|---|
| `~/.tollgate/import-journal.json` | the journal (0600). Its `state` ends guest mode when it is `complete` |
| `~/.tollgate/import-journal.<ms>.json` | a previous aborted or rolled-back journal, archived by the next import |
| `~/.tollgate/import-backup/` | non-secret byte backups: the rosters and herdr's config |
| `~/.clauth/MIGRATED` | the tombstone |
| `<bindir>/clauth-0.16.0.retired`, `<bindir>/clauth` | the retired upstream binary and the shim that replaces it |

`GET /v1/health` and `GET /v1/status` on the [local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md) carry `"import": {"state": …, "completed_at": …}`.

## Known limits

- **Off-`PATH` upstream builds.** A `target/*/clauth` build of upstream, or a `cargo run` on the `mommy` branch, is not retired. Its TUI exit would turn the slot link back into a regular file. The dry-run lists them as warnings. Do not run them until tollgate's start-time reconcile ships.
- **A regular-file slot comes back as a link.** Once the import captured or relinked a regular-file slot, a rollback restores a link to the store, never the original copy.
- **Edited JSON files are pretty-printed.** G1, G3 and `r1` write `settings.json`, `~/.claude.json` and the plugin registries back with two-space indentation, as Claude Code writes them, so a file you formatted differently changes its whitespace (G4 edits the registry in place, byte for byte). Its keys keep their order, and a rollback puts back each value it changed.
- **Retire holds no lock.** Claude Code takes none on its files, so retire refuses while a session runs, but one started during the retire itself could lose an update to `~/.claude.json`. Start no session until it finishes.
- **herdr's plugin state is herdr's.** The reinstall rewrites herdr's `plugins.json` and its checkout. A rollback restores herdr's `config.toml` byte for byte, but not those.
