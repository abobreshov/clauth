# herdr plugin

tollgate ships a plugin for [herdr](https://herdr.dev) that does four things: it opens the tollgate dashboard in a popup over whatever you were doing (or straight on the Usage tab), it labels every herdr pane with the account that pane is spending and that account's lead usage figure, it publishes a severity token a sidebar rule can colour, and it shows when a delegate runs inside a pane.

The plugin's id is `tollgate`, so it installs beside upstream clauth's `clauth` plugin; its actions are `tollgate.*` and its tokens `$tollgate`, `$tollgate_severity` and `$tollgate_delegate`.

Requires herdr 0.8.0 or newer, `tollgate` on `PATH`, and Linux or macOS. The entrypoints are POSIX shell scripts, so the manifest declares those two platforms; herdr on Windows is preview-only anyway.

## Install

The fork has published no `tollgate-v*` release tag yet, and its default branch still carries upstream's plugin (id `clauth`). `tollgate herdr install` checks the manifest id at the ref it would install before herdr runs, so today it refuses and says why. Use the dev link below until a release exists. In [guest mode](Guest-Mode) `install` and `uninstall` run too. They write only tollgate's own blocks, never upstream's `# clauth herdr plugin` block, under upstream's `~/.clauth/.lock`.

### From a local checkout

```sh
tollgate herdr link                          # the working directory's checkout, else the one this binary was built from
tollgate herdr link --path ~/src/tollgate    # a repo root, its herdr-plugin/ dir, or the manifest
tollgate herdr unlink                        # herdr plugin unlink tollgate; the files stay
```

`link` runs `herdr plugin link <dir>` on the checkout's `herdr-plugin/`, so a script edit is live on the next hook. Both refuse unless the manifest has id `tollgate` (the fork's history also carries upstream's tree), `link` refuses over a GitHub install or a link from another tree, and `unlink` refuses a GitHub install (`tollgate herdr uninstall` removes that). Neither writes herdr's `config.toml`; paste the key and sidebar rows from [below](Herdr-Plugin#the-key) by hand, or let the Plugin tab's herdr fix write them (guest mode too).

### From GitHub, once a release exists

```sh
tollgate herdr install
```

One command for the whole setup. It runs herdr's own installer, passing herdr's preview of every command the plugin would run as you straight through, then adds the two things a herdr plugin cannot declare for itself: the key that opens the dashboard, and the sidebar row that renders the pane tag. Both land in your herdr `config.toml`, appended after a diff and a `[y/N]`, and herdr validates the result before anything is written.

Run it a second time and it adds nothing. `--key` picks the keybinding, and a re-run with a new key re-binds an existing tollgate binding to it; the Plugin tab's heal keeps the installed key instead. If the plugin is already linked from a local checkout (a development setup), install refuses rather than replace your live tree, and names the checkout plus the two ways out: `herdr plugin link` relinks it, `tollgate herdr uninstall` first switches you to the GitHub install.

| Flag | Effect |
|------|--------|
| `--key <spec>` | the key that opens the dashboard, in herdr's own binding syntax; default `prefix+t` |
| `--no-config` | install the plugin, leave `config.toml` alone, and print the blocks to paste |
| `--yes` | skip both prompts, herdr's install preview included; required on a non-TTY stdin |

`HERDR_CONFIG_PATH` overrides which config file gets written, matching how herdr itself reads that variable. A key another `[[keys.command]]` block already binds (upstream's `prefix+a` → `clauth.open`, say) is never bound a second time.

To install by hand once a release exists, run `herdr plugin install abobreshov/clauth/herdr-plugin --ref tollgate-v<version>` and paste the two blocks from the [plugin README](https://github.com/abobreshov/clauth/tree/feat/tollgate/herdr-plugin#readme). Never install that source without `--ref`: the default branch is upstream's tree and would register upstream's `clauth` plugin.

## Updates

Nothing updates the plugin in the background: self-update is compiled out of this build, and the heal that used to reinstall the plugin at the newest release is a no-op with it. A linked checkout is always current, since herdr runs the files in place. Once releases exist, re-running `tollgate herdr install` is the refresh path.

## Uninstall

```sh
tollgate herdr uninstall
```

Removes the plugin from herdr and drops the config blocks tollgate added, as one operation behind one confirm. Declining leaves both halves exactly as they were. `--no-config` removes only the plugin; `--yes` skips the prompt.

It only removes blocks tollgate marked as its own, so anything you wrote elsewhere stays. If the plugin is already gone it says so and still cleans the config, which is the state a half-finished install leaves behind. In guest mode it removes the plugin alone and leaves `config.toml` untouched.

## Actions

| Action | Qualified id | What it does |
|--------|--------------|--------------|
| Open tollgate | `tollgate.open` | the tollgate dashboard in a popup; quit it with <kbd>q</kbd>, same as anywhere else |
| Open tollgate usage | `tollgate.usage` | the same dashboard opened straight on the Usage tab (the `usage` entrypoint runs `tollgate --tab usage`, which outranks `home_tab` and the first-launch landing) |
| Show this pane's tollgate account | `tollgate.which` | re-reads the account the focused pane burns and republishes it, with its lead metric, as pane metadata |

There is no account picker. Switching is a keystroke inside the dashboard, so a second switch surface would only know less than the first.

herdr allows one popup per session, so pressing the open key with tollgate already up does nothing rather than reporting an error.

## The key

A herdr plugin cannot declare a keybinding, so that line lives in your own herdr `config.toml` and nothing happens until it is there. `tollgate herdr install` writes it:

```toml
[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "tollgate.open"
description = "tollgate accounts"
```

The installer binds only `tollgate.open`. To bind `tollgate.usage` too, add a second block by hand:

```toml
[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "tollgate.usage"
description = "tollgate usage"
```

`command` takes the qualified action id from the table above. `prefix+` is herdr's own leader. Without a binding the actions are still reachable through `herdr plugin action invoke tollgate.open`. herdr 0.8.2 has no menu that lists plugin actions; that is an upstream ask tollgate has drafted, not something this plugin can add.

## The pane tag

Every herdr pane currently running Claude Code or codex may spend a tollgate account, and which one is invisible from the pane itself. The plugin hooks agent detection, reads `herdr pane get` for the pane's live agent, publishes the answer as pane metadata under the name `tollgate`, and starts a per-pane watcher for a live agent.

### What the tag says

The `$tollgate` token is the account name plus its lead figure, from tollgate's usage caches (never a network call). The scripts ask the binary for it through the hidden `tollgate herdr tag --agent <kind> -- <profile>`:

| Example | Lead figure |
|---------|-------------|
| `leadtone 2%` | the 5h session window |
| `cx-work 23%w` | the weekly window |
| `nous-main 64% mo` | a monthly window |
| `or-main $13.67` | a balance, when the account has no windows |
| `or-alt $9.00 left` | what is left of a key cap |
| `ds $4.08/mo` | spend |

A `⏸` marks figures past their staleness threshold, `⚠` a HIGH account (a used share at 75 %, a balance under $5, a window burning ahead of its clock) or one with a failure on record, and `‼` a CRITICAL one. The token is capped at herdr's 80-character limit. Tags carry only the name and numbers: other herdr clients can read pane metadata, so no id, email or key fragment rides along.

The severity class rides beside it as a second token, `$tollgate_severity` (`ok`, `mid`, `high`, `critical`; cleared when nothing is graded), so a sidebar rule can colour the row. The [plugin README](https://github.com/abobreshov/clauth/tree/feat/tollgate/herdr-plugin#the-usage-aware-pane-tag) has a styled row template for both.

A claude pane whose `tollgate start` session the script found also passes that session: `tollgate herdr tag --agent claude --session <sid> -- <profile>`. The tag then grades the account the session's requests authenticate as. For an API-key session that has just been hot-swapped ([Auto-switch](Auto-Switch#moving-a-live-session-by-hand)), that is still the previous account until the session's key helper has served the new key. While the swap is in flight the tag names both accounts and carries no severity: `or-main → or-alt swapping…`, or `or-main → or-alt stalled` when the key helper failed. The account the script prints, and the border label, follow the same rule: the member the helper last served, read off `~/.tollgate/live_sessions/<sid>.helper`.

### Native panes

`hermes`, `grok` and `agy` panes run harnesses tollgate does not launch, so they have no session row. The binary tags one with the single enabled account its harness owns (for `hermes`, a Nous monitor that reads Hermes' own login) and tags nothing when there is none or more than one: an ambiguous pane is cleared rather than tagged with a guess. No Grok or Antigravity reader ships in 0.1.0, so `grok` and `agy` panes stay untagged for now. The pane still gets a watcher, so an account that appears later tags it on the next tick.

### How it follows the pane

The live pane agent outranks the hook event: a release event names the agent that just exited, while `pane get` names what runs now. A failed `pane get` falls back to the event's agent; with no event agent it publishes nothing and starts nothing, preserving the last tag rather than treating a read failure as an empty pane. An explicit no-agent result or another agent clears both the account token and border label and gets no watcher, whether the report came from a hook, `tollgate.which`, or a TUI knob push.

The watcher is what keeps the tag right across an account or harness change, which fires no herdr event. Each tick re-reads the live agent and resolves as that agent, so a watcher spawned under codex can follow the pane to Claude Code and vice versa. A failed read leaves the tag untouched and retries; a persistent failure ends the watcher without publishing a clear. A `tollgate start --with-fallback` session that moves onto the next chain member, or a bare `claude` that follows a `tollgate switch`, both repoint the account invisibly. A codex pane answers the profile its `tollgate start` session runs under, else the profile its own login is adopted into (`tollgate login <name> --codex` leaves `~/.codex/auth.json` a link onto that profile's store). A codex login that is not adopted clears the tag but still keeps a watcher, so an adoption created later appears on its next tick; deleting an adopted profile clears it on the next tick too. Once a pane runs neither agent, the watcher clears the metadata and exits.

herdr renders a reported value only where your own agent-row template asks for it, so **the tag stays invisible until `$tollgate` is in a row**. `tollgate herdr install` adds the `claude` one; agent panes take the `rows_by_agent` template rather than the generic `rows`, so add the other agents' rows yourself until the installer writes them too:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
codex = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
hermes = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$tollgate"]]
```

That reads `claude · D1 42%` in the sidebar for a pane started as `tollgate start D1`. A pane running Claude Code some other way reports whichever account owns the global credentials. Point `CLAUDE_CONFIG_DIR` somewhere else yourself and the tag stops matching what that pane spends.

## Delegate state

When a `claude` delegate runs in a pane, the pane's `tollgate mcp` server publishes a second token: `tollgate_delegate` reads `working` for the run's duration and `idle` once the last in-flight delegate ends. The token self-clears a minute after its last report, so a dead server never leaves a stale `working` behind.

The agent-panel status dot does not follow it. The dot is herdr's own lifecycle signal; a metadata token renders only as text where a row template names it. The row above names `$tollgate` alone, so delegate state reads on `herdr pane get` / `herdr pane list`. Turn the `delegate row text` knob on and `tollgate herdr install` appends `$tollgate_delegate` to the row.

## Herdr mode

A tollgate TUI opened inside a herdr pane (`HERDR_ENV=1`) adds one thing: the header carries a dim `[ herdr ]` tag. The first launch opens the Plugin tab with the `herdr` row selected and its detail pane open; every later launch opens the `home tab` like a standalone TUI. Everything else is the same TUI.

## Herdr options

Seven knobs tune the plugin. Six live in the dashboard's Plugin tab: select `herdr`, press <kbd>⏎</kbd>, and an `options` section at the bottom of the detail lists them as form rows. The seventh, `home tab`, sits in the Config tab's appearance band and picks the tab every launch opens on; the first herdr launch lands on the Plugin tab with the herdr row open instead. The six form knobs persist in `~/.tollgate/profiles.toml` under `[herdr]`, never in herdr's own `config.toml`; `home tab` persists as the top-level `home_tab` key. The plugin scripts read them through `tollgate herdr config get <key>`, which prints one line (`fit`, `on`, `off`, or a count). Knob changes apply immediately: moving `pane tag` or `border label` re-reports every pane at once (from the plugin-pane launch only; a standalone TUI has no panes to reach, and a bare pane lacks the plugin root).

| Knob | Default | What it does |
|------|---------|--------------|
| `popup width` | `fit` | the placement `tollgate.open` uses: `fit` opens a popup sized against the focused pane (full width up to 540 columns, then a centered 540), `half` is herdr's default half-size popup, `split-right` opens a real pane right of the focused one, `split-top` opens a real pane directly above it (a downward split of the pane above; no pane above splits the focused pane instead). `full` folded into `fit`, so a saved `full` loads as `fit`. If the snapshot herdr serves cannot be read, the popup opens without sizing flags |
| `pane tag` | on | publish the `$tollgate` tag and `$tollgate_severity` token; off clears both on every pane |
| `tag refresh` | 5 | seconds between the per-pane watcher's re-publishes |
| `border label` | off | publish `--display-agent "$profile"` so split-pane borders name the account; off clears the stale label. Whether herdr scopes that label to the one pane or to the agent is not verified against herdr 0.9 |
| `delegate dot` | on | the `tollgate mcp` server reports `tollgate_delegate=working\|idle` during delegate runs; off disables the reporting entirely |
| `delegate row text` | off | the sidebar row `install` writes gains the `$tollgate_delegate` token, so a running delegate reads as text beside the row; toggling it in the TUI rewrites only the blocks tollgate itself wrote (a block you edited by hand is kept whole), behind a confirm that defaults to cancel |
| `home tab` | `overview` | the tab every launch opens on (`overview`, `usage`, `tokens`, `setup`, `fallback`, `config`, `status`, `plugin`); the first herdr launch lands on the Plugin tab with the herdr row open instead. edited from the Config tab's appearance band, not the herdr detail |

The options render whether the TUI runs inside herdr or standalone. herdr mode differs only in the header tag and the first-launch landing: the first herdr launch opens the Plugin tab with the herdr row selected and its detail open; every later launch opens the `home tab`.

## Checking it from the TUI

The dashboard's [Plugin tab](Interface-And-Keys#plugin-tab) carries a `herdr` row, shown only if herdr is installed. It reports the herdr version, whether the plugin is linked or installed and whether it is enabled, the key you bound and its spelling, and whether the sidebar row is templated. A registry entry whose checkout has been moved or deleted reads as danger, since herdr keeps the entry and the plugin cannot run.

<kbd>f</kbd> on that row appends whichever of the keybinding and the sidebar row is missing, behind a confirm that defaults to cancel. It is the same write `tollgate herdr install` performs, so it is the repair for a config edited by hand since. If your config spells one of those tables in a way tollgate cannot extend by appending, it says so and leaves that half to you rather than guessing.

## What this plugin cannot do, by design of herdr's plugin v1

Plugin UI is pane-scoped. herdr documents runtime action registration and native non-terminal plugin UI as outside plugin v1, so none of this is a missing feature here:

- no button or row beside the sidebar spaces list, and no status-bar item
- no menu outside a pane, and no menu that lists plugin actions
- no mouse binding of any kind: herdr's key parser rejects mouse tokens, and the only click routed to a plugin is a Control-click on a URL matching a `link_handlers` pattern
- no click-outside dismiss; a popup holds every keystroke, <kbd>Esc</kbd> included, until its command exits

Two more are drafted as upstream asks and not tollgate-side work: popups have no position control on herdr 0.8.2, and the agents panel title has no config key to hide it.
