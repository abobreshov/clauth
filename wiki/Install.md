# Install

Linux, macOS, and Windows (Git Bash / MSYS2). CI covers Linux and macOS only: Windows is untested, since tollgate's test suite does not compile there yet. tollgate needs `claude` on `PATH` for `tollgate start`, `resume`, the MCP `delegate` tool, and the Claude Code plugin's install and self-heal, both of which drive the `claude plugin` CLI. Everything else works without it, including `tollgate usage`, the monitors and the local agent API.

tollgate is a fork of [clauth](https://github.com/uwuclxdy/clauth) and installs beside it: a different binary (`tollgate`), a different data dir (`~/.tollgate`). If upstream clauth is installed, read [Guest mode](Guest-Mode) before you start.

## From source with cargo

The `tollgate` package lives on the `feat/tollgate` branch of the fork. The fork's default branch still carries upstream's `clauth` package, so an unpinned `--git` install fails.

```bash
cargo install --locked --git https://github.com/abobreshov/clauth --branch feat/tollgate tollgate
```

From a checkout:

```bash
git clone --branch feat/tollgate https://github.com/abobreshov/clauth tollgate
cd tollgate
cargo install --locked --path .
# or build without installing: cargo build --release  (binary at ./target/release/tollgate)
```

Do not run `cargo install tollgate` (crates.io): a crate by that name there is not this tool.

## Install script

`install.sh` on the `feat/tollgate` branch runs the same `cargo install --git … --branch feat/tollgate` when `cargo` is on `PATH`:

```bash
curl -fsSL https://raw.githubusercontent.com/abobreshov/clauth/feat/tollgate/install.sh | bash
```

`--nocargo` asks for a prebuilt binary instead. The fork has published no release yet, so that path has nothing to download today; use cargo. Unlike upstream's script, this one runs no post-install `self-heal`, because that writes Claude Code's plugin registry, which upstream clauth owns until an import.

## Updates

Self-update is compiled out of this build. Nothing checks GitHub, downloads or replaces the binary, and the herdr plugin is never reinstalled over the network either. The Config tab's `auto-update` row renders off and dimmed, and pressing it says `self-update is disabled in this build; reinstall from source`. A saved `[update] auto_update` value is kept in `profiles.toml` but has no effect.

To upgrade, re-run the cargo command above. A fork-signed updater (its own release API, its own minisign key) is planned; see [the plan](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/multi-provider-redesign-plan.md).

## Shell completions

The first TUI launch offers to install completions for your shell. bash and zsh get a `source` line appended to the rc file, asked for with `[Y/n]` first; fish writes straight into `~/.config/fish/completions/`. The answer is remembered in `~/.tollgate/.completions_installed`. The completion functions are tollgate's own, so they do not replace upstream clauth's. A declined prompt is not asked again, but the Plugin tab's `plugin` row keeps offering `install completions` without warning.

Install or refresh them any time:

```bash
tollgate completions install          # detects your shell from $SHELL
tollgate completions install zsh      # or name it
tollgate completions bash             # print the script to stdout instead
```

`TOLLGATE_NO_COMPLETIONS=1` skips the first-run prompt entirely.

`tollgate resume` and `tollgate info` complete session ids (and `latest`), and codex profile names complete beside the Claude Code ones after `tollgate`, `start`, `delete` and `switch`, and alone after `limit-reset`. An installed script never updates by itself: when the script at the path `tollgate completions install` writes (`~/.tollgate/completions/tollgate.<shell>`; fish `~/.config/fish/completions/tollgate.fish`) differs from what this tollgate prints, select the Plugin tab's `plugin` row and press <kbd>f</kbd> (its fix is `update completions` once the plugin itself needs nothing), or run `tollgate completions install`.

## Claude Code plugin

The plugin is a separate step, installed from the TUI's Plugin tab and covered on [Claude Code plugin](Claude-Code-Plugin). Because that install drives the `claude plugin` CLI, it needs a recent `claude`: an older one fails the install naming the version it wants. In guest mode the install still runs and writes only tollgate's own entries, since the plugin registry is upstream's until an import ([Guest mode](Guest-Mode#tollgates-own-entries)).

## Uninstall

```bash
cargo uninstall tollgate
rm -rf ~/.tollgate          # profiles, monitors, caches, the API token
```

`~/.tollgate` holds live credentials for any profile you created; delete it only when you mean to. Nothing under `~/.clauth` is touched by either command.
