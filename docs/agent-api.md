# The local agent API

tollgate exposes what it observes (every account's quota windows, money
meters and freshness, plus the status feed and a provider catalog) through a
**read-only** API that agents on this machine can call. You can reach it three
ways:

| Door | Address | Auth |
|---|---|---|
| Loopback HTTP | `http://127.0.0.1:8454` (default) | `Authorization: Bearer <~/.tollgate/api-token>` |
| Unix socket | `~/.tollgate/api.sock` (0600, Unix only) | none: only your user can open it |
| MCP | the `usage` tool of `tollgate mcp` | none: stdio |

All three read tollgate's on-disk caches and **never call a provider**, so
polling them costs no quota. No route can change anything, and credentials are
never returned: endpoints lose their userinfo, query and any key-shaped path
segment, and free-text fields (failure messages, labels, plan names) have
token-like words replaced with `[redacted]`.

## Running it

- **`tollgate daemon`** hosts the API by default. If the port is taken, the
  daemon logs it and runs without the API.
- **`tollgate api serve [--listen ADDR]`** runs it in the foreground on a
  machine that has no daemon.
- **`tollgate api token`** prints the token file's path and creates the token
  on first use. **`tollgate api token --show`** prints the token itself, alone
  on one line.
- **`tollgate api url`** prints the base URL and a working `curl` line for each
  door.

Configuration lives in `~/.tollgate/profiles.toml`:

```toml
local_api = { enabled = true, listen = "127.0.0.1:8454" }
```

- **`enabled`** only controls whether the daemon hosts the API.
  `tollgate api serve` always serves.
- **`listen`** must be a loopback address (`127.0.0.0/8`, `::1` or
  `localhost:<port>`). Anything else, `0.0.0.0` included, is refused because
  there is no TLS on this API. For access from another machine, use the TLS
  REST API (`tollgate daemon --listen`).
- **`TOLLGATE_NO_LOCAL_API=1`** in the daemon's environment turns the
  daemon-hosted API off without editing the file.

The token is 64 hex characters, generated with the OS CSPRNG, and stored at
`~/.tollgate/api-token` with mode 0600. The server re-reads the file on every
request, so to rotate the token, delete the file and run `tollgate api token`.

## Calling it

With the token:

```sh
TOKEN=$(cat ~/.tollgate/api-token)        # or: TOKEN=$(tollgate api token --show)
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8454/v1/usage
```

Through the unix socket, with no token:

```sh
curl -s --unix-socket ~/.tollgate/api.sock http://localhost/v1/usage
```

From Python, with only the standard library:

```python
import json, pathlib, urllib.request
token = (pathlib.Path.home() / ".tollgate/api-token").read_text().strip()
req = urllib.request.Request("http://127.0.0.1:8454/v1/accounts",
                             headers={"Authorization": f"Bearer {token}"})
accounts = json.load(urllib.request.urlopen(req))["accounts"]
for a in accounts:
    lead = next(iter(a["windows"]), None)
    print(a["id"], a["provider"], lead and lead["used_pct"])
```

### From an MCP client

Claude Code, and any other MCP client running `tollgate mcp`, gets a
read-only `usage` tool. It returns the same envelope as `/v1/usage` and takes
these optional filters:

| Argument | Meaning |
|---|---|
| `account` | an account id (`claude:work`) or name |
| `provider` | a source (`openrouter`) or provider name (`OpenRouter`) |
| `all` | `true` to include disabled profiles |

## Endpoints

- **Methods:** every route answers `GET` only. Any other method gets
  `405 {"ok":false,"error":"method_not_allowed"}`.
- **Errors:** an unknown path gets `404 {"ok":false,"error":"not_found"}`.
  Over TCP, a missing or wrong token gets `401` with
  `WWW-Authenticate: Bearer`, and this check comes before routing. Before
  that, the `Host` header must name loopback (`localhost`, `127.0.0.1` or
  `[::1]`, any port): any other name gets
  `421 {"ok":false,"error":"misdirected_request"}` whatever token it carries,
  which is what stops a DNS-rebinding web page, and no `Host` at all gets
  `400 {"ok":false,"error":"host_required"}`. The unix socket takes any
  `Host`, or none.
- **Headers:** responses are `application/json` with `Cache-Control: no-store`.
  No CORS headers are sent, because the API is not meant for browsers.
- **Limits:**
  - The request head can be at most 8 KiB (`431` if larger), and the body at
    most 64 KiB (`413` if larger).
  - A request has 10 seconds to arrive.
  - A connection stays open (keep-alive) for at most 60 seconds or 100
    requests. Only successful answers keep it open.
  - At most 16 connections are served at once.

| Route | Body |
|---|---|
| `GET /v1/health` | `{ok, version, schema_version, guest_mode, import}` |
| `GET /v1/accounts` | `{schema_version, accounts: [AccountObservation…], live_sessions: [LiveSessionView…]}` |
| `GET /v1/accounts/{id}` | `{schema_version, account: AccountObservation, live_sessions: [LiveSessionView…]}` (only the sessions whose committed or served member is this account), or `404 account_not_found` |
| `GET /v1/usage` | `{schema_version, generated_at, guest_mode, accounts}`, the same envelope as `tollgate usage --json` |
| `GET /v1/providers` | `{schema_version, providers: [{source, display_name, auth_kinds, configured, accounts}]}` |
| `GET /v1/status` | the `~/.tollgate/status.json` feed, parsed and redacted (never the file's raw bytes), plus `import`, with an `ETag` of the body served; built on the spot when no daemon has written a parseable one. It carries `hermes_profiles[]` (`name`, `provider`, `model`, `mode`, `live`) beside `profiles[]` |
| `GET /v1/openapi.json` | the OpenAPI 3.1 document for all of the above |

**Filters.** `/v1/accounts` and `/v1/usage` take these query parameters. Values
are percent-decoded, and an empty value means no filter.

| Parameter | Keeps |
|---|---|
| `all=1` | disabled profiles too |
| `account=<id or name>` | only that account |
| `provider=<source or name>` | only that provider, case-insensitive |

**Account ids.** An id has the form `<namespace>:<name>`, with these
namespaces:

| Namespace | Account |
|---|---|
| `claude:` | a `profiles.toml` profile, whatever its provider |
| `codex:` | a codex profile |
| `monitor:` | a monitoring-only key |
| `upstream:` | upstream clauth's accounts (read-only) |
| `hermes:` | a Hermes profile tollgate launches (`hermes-profiles.toml`): its month-to-date spend from the home's own `state.db` |

In a path, the colon can be sent as is or percent-encoded
(`/v1/accounts/claude%3Awork`). A profile name works too (`/v1/accounts/work`).
Disabled accounts are always found by direct lookup.

### An AccountObservation

The schema is `src/usage/observation.rs`, `schema_version` 1. Every key is
always present: an absent value is `null`, an empty list is `[]`.

```json
{
  "id": "claude:work",
  "source": "anthropic_oauth",
  "auth": "subscription",
  "label": "work",
  "provider": "Anthropic",
  "plan": "Max 20x",
  "active": true,
  "disabled": false,
  "origin": "profile",
  "endpoint": null,
  "freshness": {"state": "fresh"},
  "failure": null,
  "windows": [
    {"id": "session", "label": "5h", "used_pct": 42.0, "exhausted": false,
     "resets_at": "2026-09-29T14:32:00Z", "window_secs": 18000,
     "scope": {"kind": "shared"}, "chain_eligible": true,
     "used": null, "limit": null, "breakdown": []}
  ],
  "money": [],
  "estimate": null,
  "banked_resets": null,
  "best_effort": false,
  "observed_at": "2026-09-29T11:20:04Z",
  "checked_at": null
}
```

**Reading it:**

- **Money** amounts are exact decimal **strings** (`"12.50"`); parse them as
  decimals, not floats. For a `limit` meter, `amount` is what is left and
  `limit` is the cap.
- **`used_pct`** is not clamped, so it can exceed 100.
- **`freshness`** is one of:
  - `fresh`
  - `stale`, with `since` giving when the figures were read
  - `not_fetched`

  Stale figures are still shown; treat them as last known values.
- **`failure.kind`** is one of:
  - `auth_required`
  - `rate_limited`
  - `quota_exhausted`
  - `unavailable`
  - `invalid_response`
  - `console_expired`
  - `subscription_inactive`
- **`origin`** is where the account is defined, and the id's namespace:
  - `profile` (`claude:`)
  - `codex_profile` (`codex:`)
  - `monitor` (`monitor:`)
  - `upstream` (`upstream:`)
  - `hermes_profile` (`hermes:`): a Hermes home tollgate launches. It carries
    no windows or meters; `estimate` is the UTC month to date from the home's
    own `state.db` (`basis` names it), `failure` is `rate_limited` during a
    Nous cooldown and `unavailable` when the ledger cannot be read (for
    example, no `sqlite3`), and `best_effort` marks an unknown `state.db`
    schema or a Hermes outside 0.19.x.
- **`guest_mode: true`** means upstream clauth owns `~/.claude` on this machine,
  and tollgate is only watching.
- **`import`** is `{state, completed_at}`: where a `tollgate import clauth` of
  upstream's accounts stands. `state` is one of `none`, `pre`, `in_progress`,
  `complete`, `rolling_back`, `rolled_back`, `aborted`, or `unreadable` for a
  journal that does not parse; `completed_at` is the RFC 3339 instant it
  committed, else `null`. There is no write route: the import runs only from
  the CLI.

### A LiveSessionView

`live_sessions` lists every running `tollgate start` session, oldest first,
with where it stands in a switch. It is read from the session's registry row
and its key-helper ack, with no lock and no write. A row whose supervisor
process is gone is left out.

```json
{"session_id":"4242-0","harness":"claude","start_profile":"or-main","executor":"api_key",
 "relaunch_reason":null,"requested_member":null,
 "committed":{"member":"or-alt","generation":2,"at_ms":1759140000000},
 "served":{"member":"or-main","generation":1,"at_ms":1759139990000},
 "state":"swapping","idle":true}
```

- **`executor`** is how the session moves between accounts: `oauth` (its
  credential link is repointed), `api_key` (an in-class hot swap: only the key
  its key helper prints changes), or `relaunch_only` (only
  `tollgate switch <sid> <p> --relaunch` moves it; `relaunch_reason` says
  why). It is `null` for a codex session, whose `committed` and `served` are
  `null` too.
- **`committed`** is the member the session has switched to, and **`served`**
  the member its requests authenticate as. For an API-key session, `served` is
  the member its key helper last printed a key for. The two differ while a
  hot swap is in flight, and attribution uses `served`.
- **`state`** is `requested` (a switch was asked for and not committed yet),
  `swapping` (committed, not served yet), `stalled` (the key helper ran for
  the commit and failed; Claude Code keeps the previous key until it is
  rejected) or `served`. The state depends only on recorded helper runs, not
  on elapsed time: a session that has made no request since its commit stays
  `swapping` and carries `"idle": true`.

**Versioning.** Adding a field does not bump `schema_version`, so ignore keys
you do not know. A breaking change bumps the version. Refuse any
`schema_version` you do not know.

## Examples

Which account has the most session headroom:

```sh
curl -s --unix-socket ~/.tollgate/api.sock http://localhost/v1/accounts |
  jq -r '.accounts[] | select(.disabled|not)
         | [.id, ((.windows[] | select(.id=="session") | .used_pct) // "n/a")] | @tsv'
```

Every balance meter, with its currency:

```sh
curl -s -H "Authorization: Bearer $(cat ~/.tollgate/api-token)" \
  'http://127.0.0.1:8454/v1/usage?provider=openrouter' |
  jq -r '.accounts[].money[] | select(.kind=="balance") | "\(.label) \(.amount) \(.currency)"'
```

Check that the API is up and which schema it speaks:

```sh
curl -s --unix-socket ~/.tollgate/api.sock http://localhost/v1/health
# {"ok":true,"version":"…","schema_version":1,"guest_mode":false}
```
