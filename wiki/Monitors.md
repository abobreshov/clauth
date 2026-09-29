# Monitors

Monitors borrow native CLI logins read-only or read API keys by environment variable name. They never refresh borrowed tokens, lock a CLI store, or write another CLI's login.

```
tollgate monitor add grok
tollgate monitor add antigravity       # alias: agy; Linux Secret Service only
tollgate monitor add codex-native
tollgate monitor add nous             # alias: hermes; Hermes Nous login
tollgate monitor add openrouter
tollgate monitor add openai
tollgate monitor add google-ai        # alias: gemini
tollgate monitor add nous-key         # no network probe by default
```

Use `--id NAME` for another account. Explicit flags override defaults; the existing `monitor add ID --kind KIND` form remains supported. Native Grok and Codex homes can be set with `--tool-home`; multiple Grok logins require `--auth-entry issuer::client`. Expired or undated native tokens report authentication required without making a network call. Open the owning CLI to renew its login. A Codex auth symlink into a profile store is refused.

`monitor detect [--json] [--explain]` discovers local login files and key names without a network call or spawning a CLI. Secret Service discovery checks attributes and locked state only; it never reads an agy secret. Explain output reports field names and expiry formats, never credential values. `--apply` asks once before adding proposed rows; noninteractive use requires `--apply --yes`.

```
tollgate secret set OPENROUTER_API_KEY
printf '%s\n' "$KEY" | tollgate secret set OPENAI_API_KEY --stdin
```

Values are prompted with echo off or read from one stdin line, never accepted as a positional argument. `secret list [--json]` shows names only; `secret rm NAME [--yes]` removes one. The private `~/.tollgate/secrets.env` store is loaded by monitor and provider fetches only. Exported variables win by default; root `--prefer-store` or `TOLLGATE_PREFER_STORE=1` reverses that order. Stored names join the environment scrub lists for children. The daemon observes updates within its 10-second scan.

`monitor refresh [ID] [--json] [--capture DIR]` writes private shape captures: strings are masked by length, numbers remain, selected enum fields and permitted rate-limit headers remain visible. Review each capture before sharing it. Native Codex captures contain its mapped usage structure because that monitor reuses the existing read-only usage fetch.

Grok shows one shared window and optional per-product attribution. Monetary Grok fields remain hidden until their units have been confirmed. agy's CLI print path remains gated pending the owner-run AGY-CLI check. API-key monitor behavior is documented in [the lane 4 specification](../docs/specs/providers-lane4.md).

The Linux keyring reader uses zbus with an existing service owner and a local plain session. It never activates a wallet, unlocks an item, or invokes a prompt. Detection reads attributes and lock state only.
