# tollgate herdr plugin

Opens [tollgate](https://github.com/abobreshov/clauth) in a [herdr](https://herdr.dev) popup: the account table, the usage windows, and the auto-switch chain, over whatever you were doing, without a pane of its own. It labels every herdr pane with the account that pane is spending, and it shows when a delegate runs inside a pane. The popup width, the pane tag, and the delegate state all tune from the dashboard's Plugin tab.

**The manual for all of it lives in the wiki: [herdr plugin](https://github.com/abobreshov/clauth/blob/feat/tollgate/wiki/Herdr-Plugin.md).** This file covers what the plugin itself is, for anyone reading it before letting herdr run it.

## Requires

- herdr 0.8.0 or newer. The manifest declares it, so an older herdr refuses to link with `plugin_requires_newer_herdr`.
- `tollgate` on `PATH`.
- Linux or macOS. The entrypoints are POSIX shell, and herdr's own Windows support is preview-only.

## Install

```sh
tollgate herdr install
```

Once the fork publishes a `tollgate-v*` release, that runs herdr's installer at the newest one, then writes the two things a herdr plugin cannot declare for itself: the key that opens the dashboard, and the sidebar row that renders the pane tag. `tollgate herdr uninstall` reverses both. Flags, the by-hand route, and everything the plugin does once installed are in the wiki page linked above.

While upstream clauth owns `~/.claude` on the machine (guest mode), `tollgate herdr install` refuses: herdr's `config.toml` is shared with upstream's own plugin until an import.

### From a local checkout (dev install)

The fork has published no `tollgate-v*` release tag yet, and its default branch still carries upstream's plugin, so `install` refuses rather than fetch it. Link the checkout instead:

```sh
tollgate herdr link                 # the working directory's checkout, else the one this binary was built from
tollgate herdr link --path ~/src/tollgate   # a repo root, its herdr-plugin/ dir, or the manifest itself
tollgate herdr unlink               # herdr plugin unlink tollgate; the files stay
```

`link` runs `herdr plugin link <dir>` on the checkout's `herdr-plugin/`, so an edit to a script is live on the next hook. Both commands refuse unless the manifest they act on has id `tollgate`: the fork's history carries upstream's tree (id `clauth`), and linking that would register over upstream's live plugin. `link` also refuses over a GitHub install or a link from another tree, and `unlink` refuses a GitHub install (`tollgate herdr uninstall` removes that). Neither writes herdr's `config.toml`, so they work in guest mode; paste the key and rows below by hand.

## What it runs as you

Three actions and two event hooks, all of them one of the shell scripts below, plus a per-pane background watcher:

- `tollgate.open` opens the dashboard in a popup.
- `tollgate.usage` opens the same dashboard straight on the usage view (the `usage` entrypoint runs `tollgate --tab usage`). Bind it like `tollgate.open`:

  ```toml
  [[keys.command]]
  key = "prefix+u"
  type = "plugin_action"
  command = "tollgate.usage"
  description = "tollgate usage"
  ```
- `tollgate.which` re-reads the account the focused pane burns and publishes it as pane metadata, and the same script runs on herdr's `pane.agent_detected` and `pane.agent_status_changed` events. It reads `herdr pane get` first so the pane's live agent outranks a hook event naming the agent that just exited. Reporting a live Claude Code or codex pane starts the watcher for that pane.

### The usage-aware pane tag

The `$tollgate` token names the account a pane burns and its lead metric, from tollgate's usage caches (never a network call): `leadtone 2%` (the 5h session window), `cx-work 23%w` (weekly), `nous-main 64% mo` (monthly), `or-main $13.67` (a balance, when the account has no windows), `or-alt $9.00 left` (a key cap), `ds $4.08/mo` (spend). A `⏸` marks figures past their staleness threshold; `⚠` marks a HIGH account (used share at 75%, a balance under $5, a window burning ahead of its clock) or one with a failure on record, and `‼` a CRITICAL one. The scripts ask the binary for the text (`tollgate herdr tag --agent <kind> -- <profile>`, a hidden subcommand), so they stay thin; a binary predating it answers nothing and the tag falls back to the bare account name. Tags carry only the name and numbers: other herdr clients read pane metadata, so no id, email or key fragment rides along.

The severity class rides beside it as a second token, `$tollgate_severity` (`ok`, `mid`, `high`, `critical`; cleared when nothing is graded), so a sidebar rule can colour the row. herdr rules match a token's own value, so either colour the tag by its flag or render the class itself:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", { token = "$tollgate", rules = [{ contains = "‼", fg = "#f38ba8", bold = true }, { contains = "⚠", fg = "#fab387" }, { contains = "⏸", dim = true }] }]]
agy = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate", { token = "$tollgate_severity", rules = [{ equals = "critical", fg = "#f38ba8", bold = true }, { equals = "high", fg = "#fab387" }] }]]
```

Native panes (`hermes`, `grok` and `agy`, harnesses tollgate does not launch) have no session row and no `tollgate which` answer. The binary tags one with the single enabled account its harness owns and answers nothing when there is none or more than one: an ambiguous pane is cleared rather than tagged with a guess. In 0.1.0 only a `hermes` pane can be tagged, from a Nous monitor that reads Hermes' own login; no Hermes-state, Grok or Antigravity reader ships yet, so `grok` and `agy` panes stay untagged. The pane still gets a watcher, so an account that appears later tags it on the next tick. A codex pane keeps its own rule (the session row, else the adopted login), since a bare codex burns the operator's own login.

The watcher re-reads the live pane agent and re-publishes the account every few seconds. An account or harness change fires no herdr event, so the timer is what keeps the tag from going stale. A codex pane names the profile its `tollgate start` session runs under, else the profile its own login is adopted into (`tollgate login <name> --codex` leaves `~/.codex/auth.json` a link onto that profile's store). An unadopted codex pane clears the tag but keeps its watcher, so a later adoption appears on the next tick and deleting one clears it then too. A pane with no live agent, or another agent, clears its token and border label and gets no watcher; an existing watcher clears and exits when it sees that state. A failed `pane get` with no event agent publishes nothing and starts nothing, preserving the last tag; a running watcher retries failures and exits after three without clearing the metadata. The scripts write only herdr's own pane metadata plus one pidfile per watched pane in the plugin state directory.

Seven knobs tune the plugin. Six live in `~/.tollgate/profiles.toml` under `[herdr]` and edit from the dashboard's Plugin tab (herdr row, options); `home_tab` is a top-level key picking the tab every launch opens on, from the Config tab's appearance band. The scripts read the six through `tollgate herdr config get <key>` and fall back to the shipped defaults when the binary predates the subcommand. The delegate state token (`tollgate_delegate`) reports on the pane JSON and, with the `delegate_row_text` knob on, beside the row.

## Paste these if you installed by hand

`tollgate herdr install` writes both. herdr does not let a plugin declare either one, so without them the key does nothing and the tag stays invisible. The installer writes the `claude` row below; add the `codex` and native rows by hand until it writes those too (swap in the styled token above to colour them).

```toml
[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "tollgate.open"
description = "tollgate accounts"
```

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
codex = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
hermes = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
grok = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
agy = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
```

## Files

| File | Role |
|------|------|
| `herdr-plugin.toml` | Manifest: two popup entrypoints (the dashboard, and the dashboard on its usage view), three actions, two event hooks |
| `open-pane.sh` | Opens an entrypoint in the placement the `popup_width` knob picks; "popup already open" is a no-op for the popup placements only |
| `report-profile.sh` | Resolves the account a pane burns and publishes its tag and severity as pane metadata |
| `watch-profile.sh` | Per-pane watcher re-publishing the account on a timer |
