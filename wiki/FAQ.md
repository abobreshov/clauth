# FAQ

## Using it

**What is the difference between tollgate and clauth?** tollgate is a hard fork of [clauth](https://github.com/uwuclxdy/clauth). It keeps clauth's Claude Code and codex account management and adds usage and spend monitoring across providers (Ollama Cloud, OpenRouter, Nous through Hermes, DeepSeek, Z.ai, MiniMax, Alibaba, generic endpoints), monitoring-only accounts, a local agent API, a Waybar output and the Omarchy palette. It has its own binary, data dir (`~/.tollgate`), plugin ids and daemon port, so both can be installed at once.

**I already use clauth. Will tollgate break it?** No. With `~/.clauth` present tollgate runs in [guest mode](Guest-Mode): it shows clauth's accounts read-only and refuses anything that writes `~/.claude`, `~/.codex`, the plugin registry or herdr's config. There is no import from clauth yet, so tollgate cannot take over those files; use `tollgate start <profile>` for its own accounts.

**How do I watch an account I never launch from tollgate?** Add a monitor: `tollgate monitor add <id> --kind <nous|ollama_cloud|openrouter|provider> …` with the key named by environment variable. [Providers](Providers#monitors).

**How does an agent read my usage?** `tollgate usage --json`, the local API on `127.0.0.1:8454` or `~/.tollgate/api.sock`, or the MCP `usage` tool. All three read caches and spend no quota. [Local agent API](https://github.com/abobreshov/clauth/blob/feat/tollgate/docs/agent-api.md).

**How do I put usage in Waybar?** `tollgate usage --waybar` prints one `{text, tooltip, class, percentage}` line; point a `custom` module with `"return-type": "json"` at it. The [README](https://github.com/abobreshov/clauth/blob/feat/tollgate/README.md#waybar-and-omarchy) has a snippet.

**How do I switch between multiple Claude Code accounts without logging out?** Save each logged-in session as a profile once, then switch with `tollgate <name>` or one keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with two accounts at the same time?** Yes. `tollgate start <profile>` launches `claude` in its own `CLAUDE_CONFIG_DIR`, so parallel sessions share no identity, settings, or billing caches.

**How do I run Claude Code without my global `CLAUDE.md`, plugins, or hooks?** `tollgate start --isolated <profile>` keeps the account's auth and drops the rest. Run it in an empty directory to skip project memory too. The MCP `delegate` tool takes `isolated: true` for the same thing.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?** Put the accounts in the fallback chain. tollgate switches to the next member with headroom the moment the active one crosses its threshold, from the TUI or from `tollgate daemon` with the TUI closed. [Auto-switch](Auto-Switch).

**Does it work with Pro, Max, Team, and Enterprise?** Yes, plan tier detected automatically, Max 5x and 20x included. Endpoint profiles cover the Anthropic API and any compatible proxy.

**Where does tollgate store my credentials?** Under `~/.tollgate/`, owner-only on Unix. Claude tokens go to Anthropic and codex tokens to OpenAI, nowhere else. [Security](Security).

**Can tollgate run codex accounts?** Yes. `tollgate login <name> --codex` adopts the ChatGPT login your own `codex` holds (or `--codex --browser` mints a fresh one), `tollgate start <name>` then runs `codex` under that profile's own `CODEX_HOME`, and a separate codex chain rotates accounts between sessions. [Codex](Codex).

**Can I add an account without logging out of the one I am using?** `tollgate login <name>` opens a browser, runs Claude Code's OAuth flow, and writes the tokens into a new profile. The session you are in is untouched.

**Can I log in on a server with no browser, over ssh?** Yes. `tollgate login <name>` prints the link under `Browser didn't open? Use the url below to sign in` and prompts `Paste code here if prompted:`: open the link on any device, sign in, and paste back the code the page shows. Same credential as a browser login, no flag. The Setup tab's login modal has the same: <kbd>c</kbd> copies the link to your local clipboard through the terminal, <kbd>p</kbd> turns its row into a code field (type or paste the code, <kbd>⏎</kbd> submits, <kbd>esc</kbd> brings the row back). [Quickstart](Quickstart#commands).

**Is there an MCP server for switching accounts from inside a chat?** Yes. Install the plugin, then a live session can call `profiles`, `switch_profile`, or `delegate` a whole prompt to another account. [Claude Code plugin](Claude-Code-Plugin).

**How do I stop tollgate updating itself?** Nothing to stop: self-update is compiled out of this build. Upgrade by re-running the `cargo install` command from [Install](Install).

## When something looks wrong

**An account shows 0% but I have been using it.** The 5h window opens on a real inference call, and a usage poll does not trip it. Either the window genuinely has not started, or the reading is cached. The refresh countdown on the Overview tab turns yellow on last-known numbers and red when the last fetch failed.

**Usage numbers are stuck.** Only one tollgate instance fetches at a time. If a daemon holds the lease, an open TUI reads its results instead of polling itself, and picks the lease back up within a tick of the daemon exiting. `tollgate daemon --status` says whether one is up.

**An account has a `×` next to it.** Its login was rejected for good, so it is quarantined and excluded from the chain. Run `tollgate login <name>` to re-authenticate it. On an account that authenticates by api key, what died is the stored subscription login its usage figures came from, so `tollgate login <name> --api-key <key>` is the one that clears the quarantine against the credential that account actually runs on. A bare browser login clears it too and leaves the endpoint and key standing. On a codex row the `×` means the chain is quarantined; only a new login clears it: `tollgate login <name> --codex --browser` ([Codex](Codex#when-a-chain-dies)).

**The chain will not switch to an account that looks fine.** Check the reason on its row. Weekly windows, per-model weekly windows, a spend ceiling, a canceled subscription, or a `disabled` flag all take a member out of rotation independently of its 5h number. The [exclusion table](Auto-Switch#excluded-members) lists all of them. If what excludes it is a per-model week and the sessions you run do not use that model, `start --auto` judges that week against the models a session will run instead of excluding the account outright ([Auto-switch](Auto-Switch#choosing-where-a-session-starts)).

**Auto-switch does nothing at all.** The active account has to be a chain member for the walk to start. An account outside the chain is never switched away from.

**On macOS a switch does not reach my running session.** Claude Code reads the Keychain first there, and it deletes the credentials file once it migrates. tollgate mirrors each fresh login into the Keychain for exactly this reason, but an account holding a live `tollgate start` session is skipped by force-rotate, since its Keychain item belongs to that session's own config dir. [Security](Security#per-platform-behavior).

**Claude Code does not show tollgate's tools.** Open the Plugin tab. It checks `tollgate` on `PATH`, the `mcpServers` entry, the plugin install record, and whether `tollgate mcp` actually answers a handshake. <kbd>f</kbd> on a row applies that row's fix: install the plugin at user scope, write the `mcpServers` entry into `~/.claude.json`, repair or relink the active account's credentials, or add tollgate's keybinding and sidebar row to herdr's config.

**A `delegate` run did nothing and the tree is unchanged.** A delegate spawns with the permission gate armed and nobody to answer it. Pass the permission flag through `args` for a delegate that writes files, and read the `permission_denials` array in the envelope. [Claude Code plugin](Claude-Code-Plugin#delegate).

**My custom endpoint shows no usage bars.** Only DeepSeek, Z.ai, OpenRouter, MiniMax, Ollama Cloud and Alibaba Model Studio have typed panels ([Providers](Providers)). Everything else gets a best-effort scan of the usual usage paths, which can come back empty; the scan retries at most once every five minutes (or once per refresh interval, whichever is longer), and <kbd>r</kbd> forces one immediately. An Alibaba account is the one case where an api key is not enough: run `tollgate login <account>` to capture the console session its quota is read with ([Configuration](Configuration#the-alibaba-console-session)).

**The Tokens tab shows `$X+` instead of a figure.** Some model in that period has no published price, or the period reaches into days that carry no cache split. The number is a floor. [Tokens and cost](Tokens-And-Cost#period-lens).

**Two daemons, or none.** A second `tollgate daemon` exits immediately by default. `--standby` parks one that takes over when the first dies; `--replace` terminates the running one and takes its place. [Daemon](Daemon).

**Every switch says `upstream clauth manages ~/.claude on this machine (guest mode)`.** That is guest mode: `~/.clauth` exists and no import has run. Run the account with `tollgate start <profile>` instead. [Guest mode](Guest-Mode).

**A monitor reads `$NAME is not set in the environment tollgate runs in`.** The daemon (or the shell running `monitor refresh`) does not have that variable. Export it where the daemon starts; `tollgate monitor list` marks each unset name `MISSING`.

**A Nous monitor reads `run hermes to refresh`.** Hermes' access token has expired and tollgate never refreshes it. Run `hermes` once; the next poll picks the fresh token up.

**`tollgate herdr install` refuses with "has id `clauth`, not `tollgate`".** The fork has published no `tollgate-v*` release yet, so the only plugin on GitHub is upstream's. Use `tollgate herdr link` from a checkout. [herdr plugin](Herdr-Plugin#install).
