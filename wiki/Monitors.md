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

## API keys and opt-in reads

```sh
tollgate secret set OPENAI_API_KEY
tollgate monitor add openai
# Optional: a separate admin key enables hourly month-to-date costs.
tollgate secret set OPENAI_ADMIN_KEY
tollgate monitor add openai --id openai-costs --admin-key-env OPENAI_ADMIN_KEY

tollgate secret set GEMINI_API_KEY
tollgate monitor add google-ai

tollgate secret set NOUS_API_KEY
tollgate monitor add nous-key                 # local only; key health unknown
tollgate monitor add nous-key --id nous-probe --probe
# Or choose a particular free model:
tollgate monitor add nous-key --id nous-model --probe --probe-model MODEL:free
```

OpenAI reads `/v1/models` for health and uses rate-limit headers only when
present. It never makes a paid inference call. Only the separately configured
admin key reaches `/v1/organization/costs`; costs are read at most hourly,
summed exactly by currency, and labelled `reported spend (lags)`. A response
that would require more than three pages is unavailable rather than a partial
month total. No OpenAI balance endpoint exists.

Google AI Studio keys provide health only. Cards always explain that spend
and quota are available per project in Google AI Studio, rather than through
the key read. The credential travels in `x-goog-api-key`, never in the URL.

Nous keys make no request by default. `--probe` opts into one free-model call
with `max_tokens: 1`, a minimum 900-second polling interval, and no portal read.
Without `--probe-model`, tollgate chooses the lexicographically first `:free`
model from the public list and caches its name for a day. An auto model that
vanishes is picked again once. Published credit and rate-limit headers become
meters; a rejected key reports Nous's combined invalid/blocked/out-of-funds
verdict.

Codex native and profile reads also expose optional credit balances (in
`CREDITS`, never dollars), spend-control exhaustion, and additional quota
windows. The original output is unchanged when those fields are absent.

`monitor refresh [ID] --capture DIR` saves response shapes in a private
directory. Strings are masked except the specification's enum fields;
numbers and allowed rate/credit headers remain. Review captures before
sharing them. In guest mode, capture directories must stay inside
`~/.tollgate/`; paths that escape through parent traversal or symlinks are
refused. Legacy typed-provider responses are captured before normalization
through a scoped observer, without additional HTTP reads. The live agy CLI path stays disabled until the AGY-CLI owner
check is recorded; its version/help checks and timeout behavior are covered
with synthetic runners. Configuration refuses `via = "cli"` while that owner
gate is unrecorded.
