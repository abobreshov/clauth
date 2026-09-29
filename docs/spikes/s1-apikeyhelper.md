# S1 spike: how the installed Claude Code handles `apiKeyHelper`

Date: 2026-09-29. Plan reference: `docs/multi-provider-redesign-plan.md` v3.1, §4.4 (executor B) and
§5 S1 (a)–(i). Harness: `tools/spikes/s1/`. Raw and condensed evidence for every scenario:
`tools/spikes/s1/evidence/<scenario>.{raw,summary}.txt`.

## Version under test

```
$ readlink -f "$(command -v claude)"
/home/abobreshov/.local/share/mise/installs/claude/2.1.283/claude
$ claude --version            # run inside the sandbox, evidence/claude-version.txt
2.1.283 (Claude Code)
```

The session model reported by `system/init` was `claude-opus-5-5[1m]` and `apiKeySource` was
`apiKeyHelper` in every scenario.

## Harness

| File | Role |
|---|---|
| `stub.py` | Stub Anthropic API on `127.0.0.1`. `POST /v1/messages` returns a minimal valid message `ok` (SSE when `stream: true`, JSON otherwise); `count_tokens` returns 12; any other path returns a harmless 404 (`HEAD` returns 200). One JSON log line per request: path, model, stream flag, `Authorization` (logged as `Bearer:<value>`) and `x-api-key` values, status. Control files re-read per request: `reject` (keys to refuse), `reject_status` (401 or 403), `delay` (spread the SSE events over N seconds). A request that carries no non-empty key always gets 401. Two instances run: `stub1` on :18081, `stub2` on :18082 |
| `helper.sh` | The `apiKeyHelper`. It reads its control files once at the start (`mode` ok / exit1 / empty, `sleep`, `readat` start / end, `key`), appends `<ts> BEGIN …` and then `<ts> <key>` (or `EXIT1` / `EMPTY`) to `ctl/counter`, and prints the key. The ctl dir is an argument baked into the helper command string |
| `driver.py` | Runs inside the sandbox. For each scenario it starts both stubs, writes `CLAUDE_CONFIG_DIR/settings.json`, starts **one** `claude -p --input-format stream-json --output-format stream-json --verbose` process and sends it several user turns over time, switching the key file, helper mode, stub reject list or `settings.json` between turns. It timestamps everything into `runs/<scenario>/` and merges the helper counter, stub logs and its own timeline into `merged.txt` |
| `run.sh` | Outer launcher: runs `driver.py` under bubblewrap |
| `summarize.py` | Condenses `merged.txt` into the excerpts below (`auth=Bearer X x-api-key=X` shows the label after `sk-test-`) |

### Isolation (exact command, from `run.sh`)

```
bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
  --tmpfs "$HOME" \
  --bind "$SPIKE" "$SPIKE" \
  --ro-bind "$CLAUDE_DIR" "$CLAUDE_DIR" \
  --unshare-net --die-with-parent \
  --clearenv \
  --setenv PATH /usr/bin:/bin \
  --setenv HOME "$SPIKE/home" \
  --setenv S1_CLAUDE "$CLAUDE_BIN" \
  --chdir "$SPIKE" \
  -- /usr/bin/python3 "$SPIKE/driver.py" "$@"
```

- A tmpfs over the real `$HOME` hides `~/.claude`, `~/.clauth`, `~/.codex` and `~/.hermes`.
  Inside the sandbox, `ls /home/abobreshov` shows only `Work`, and `ls ~/.claude` is ENOENT.
  Only the spike directory is writable, and the claude install dir is bound read-only because it
  lives under `$HOME`.
- `--clearenv` removes every outside `ANTHROPIC_*`, `CLAUDE_CODE_OAUTH_TOKEN` and
  `CLAUDE_CONFIG_DIR` (a stronger form of per-variable `--unsetenv`). The driver also asserts that none
  of them leaked in.
- `--unshare-net` gives the sandbox a private loopback. The two stubs are the only endpoints
  claude can reach, so nothing leaves the machine.
- Only fake keys (`sk-test-A` … `sk-test-D`, `sk-test-H2`) ever exist. Tollgate and upstream clauth
  were never run.

The environment claude gets (from `timeline.log`) is `CLAUDE_CODE_API_KEY_HELPER_TTL_MS` (only in
TTL scenarios), `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `CLAUDE_CONFIG_DIR=<run>/home/.claude`,
`DISABLE_AUTOUPDATER=1`, `DISABLE_ERROR_REPORTING=1`, `DISABLE_TELEMETRY=1`, `HOME=<run>/home`,
`LANG`, `PATH` and `TERM`. `ANTHROPIC_BASE_URL` comes only from `settings.json`:

```json
{
  "apiKeyHelper": "<spike>/helper.sh <spike>/runs/<scenario>/ctl",
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:18081",
    "DISABLE_TELEMETRY": "1",
    "DISABLE_ERROR_REPORTING": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
  }
}
```

The claude command line:

```
claude -p --input-format stream-json --output-format stream-json --verbose \
  --no-session-persistence --debug-file <run>/debug.log
```

To reproduce: `tools/spikes/s1/run.sh [scenario ...]`, then `tools/spikes/s1/summarize.py <scenario>`.
The scenarios are `a_ttl a_default b_401_same b_401_switch b_403_switch c_env c_helper c_touch
c_rewrite_same d_fail d_fail_401 e_span e_overlap f_inflight h_blank_key`.

## Results

| # | Question | Verdict | Finding |
|---|---|---|---|
| (a) | An **unchanged** helper re-executed after the TTL, and its new stdout sent | **PASS** | With `TTL_MS=3000` the unchanged command is re-run after the TTL and its new stdout is sent. The refresh is **lazy and stale-while-revalidate**: no timer runs the helper. The first request after expiry is still sent with the cached key and starts a background refresh, and requests from the next one on carry the new stdout. The default TTL is **5 min**: the key is still cached at 60 s, refreshed by the first request at 5 min 10 s, and the new key is sent on the next request |
| (b) | Behaviour on 401 | **PASS** | On **401 and on 403** CC re-runs the helper synchronously (about 1 s after the error) and retries with its stdout. If the new key works the turn succeeds transparently (one extra request). If the key stays bad, the helper is re-run before **every** retry: 10 retries (11 requests, 11 helper runs, exponential backoff capped near 40 s, about 181 s in all). The turn then ends `is_error: true`, `"Failed to authenticate. API Error: 401 <server message>"`. stream-json emits `system/api_retry` events (`attempt`, `max_retries: 10`, `error_status`). The next turn recovers once the key is accepted. This also covers S1(i): `ANTHROPIC_AUTH_TOKEN` was unset throughout |
| (c) | Does a `settings.json` edit take effect mid-session? | **PASS** (both hot-reload) | Editing `env.ANTHROPIC_BASE_URL` sends the next request to stub2. Editing the `apiKeyHelper` string runs the new helper synchronously before the next request, and its key is sent (the cached key is dropped). **Any** change to `settings.json` drops the cached key, and the next request re-runs the helper synchronously before it is sent. That includes an unrelated key, a byte-identical atomic rewrite (temp + rename) and a bare mtime touch |
| (d) | The helper exits 1, or prints nothing, mid-session | **PASS** (gate) | A failed TTL refresh writes `apiKeyHelper failed: exited 1: <helper stderr>` or `apiKeyHelper failed: did not return a value` to claude's **stderr**, keeps serving the **last good (stale) key** silently (the turn succeeds and no stream-json event appears), and retries the helper on the next request after the TTL. After recovery, the output of the next successful refresh is sent from the next request on. When the helper fails **and** the stale key gets a 401, CC drops the cached key and sends **empty credentials** (`Authorization: Bearer`, `x-api-key: ''`), runs the helper once per attempt, and stops after 4 requests (about 4 s) with `is_error: true`, `"Your apiKeyHelper script is failing · This usually means you need to re-authenticate with your provider · Run /status to see the script's error output"`. The empty credential stays cached until the helper succeeds (a 401 then re-runs it, and it retries with the new key). No failed invocation's output is ever served |
| (e) | A helper invocation that spans a key switch: which key is served? | **PASS** | CC serves exactly what the **completed** invocation printed. A helper that snapshots at start (`A`) while the key switches to `B` mid-run serves `A`, not `B`. A helper that re-reads at the end serves the newer `C`. Only one refresh runs at a time: requests made during a refresh keep the cached key and start no second helper. A 12 s helper (above the documented 10 s warning) is not killed or timed out; the old key serves meanwhile, and its output `D` is adopted when it exits. **Overlap:** an older slow invocation (snapshot `A`, 6 s) that finishes **after** a newer 401-triggered invocation (`B`) does **not** overwrite `B`, and later requests carry `B`. So a stale generation does not regress CC's key |
| (f) | Do requests in flight at the switch finish with the old key? | **PASS** | A 10 s streaming request sent with `A` completes on `A` (`completed=True`, no abort, no retry). This holds while the background refresh adopts `B` and the server starts rejecting `A` mid-stream. The next request carries `B` |
| (g) | Header form | **PASS** (stub half) | Every `/v1/messages` request carries the helper value in **both** `Authorization: Bearer <key>` and `x-api-key: <key>`. The startup `HEAD /api/hello` carries no key. Whether real OpenRouter and Ollama endpoints accept that pair is the owner-run half of S1(g) under the spike protocol, and was **not run** here |
| (h) | `ANTHROPIC_API_KEY=""` + helper | PASS (extra) | `apiKeySource=apiKeyHelper`, and the helper key is sent in both headers |

### What the gate needs, and what was observed

The plan's release gate for executor B requires three things:

- (a) passes: it does.
- (d) leaves the swap committed-not-served with a warning and never reports it as served. The
  tollgate helper acks only after exit 0 with a key on stdout. A failing or empty helper therefore
  never acks, while CC keeps the old key, or sends empty credentials after a 401. The swap stays
  committed-not-served, and nothing CC does can move the ack.
- (e) and (f) never regress an acknowledgement and never mark a stale generation served. The key
  CC sends from the next request on is exactly the stdout of the last completed invocation. A late
  stale invocation does not overwrite a newer key in CC, and the sidecar's newer-only rule covers
  the ack side. In-flight requests finish on the old key, as the plan already allows.

## Evidence excerpts

Raw (verbatim `merged.txt` lines: helper counter + stub log + driver timeline), `a_ttl`, t3-t4:

```
+   4.258 DRIVER TURN send 't3 after TTL'
+   4.269 STUB1 {"seq": 6, "method": "POST", "path": "/v1/messages?beta=true", "authorization": "Bearer:sk-test-A", "x-api-key": "sk-test-A", "model": "claude-opus-5-5", "stream": true, "status": 200, "phase": "stream-start", "delay": 0.0}
+   4.269 STUB1 {"seq": 7, "method": "POST", "path": "/v1/messages?beta=true", "authorization": "Bearer:sk-test-A", "x-api-key": "sk-test-A", "phase": "stream-end", "completed": true, "req_of": "msg_stub_5", "elapsed": 0.0}
+   4.272 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.272 DRIVER TURN done 't3 after TTL' after 0.0s
+   4.273 HELPER(ctl) BEGIN pid=3503964 mode=ok sleep=0 readat=start key_at_start=sk-test-B
+   4.274 HELPER(ctl) sk-test-B pid=3503964
+   4.472 DRIVER helper idle (0.2s)
+   4.472 DRIVER TURN send "t4 right after t3's refresh"
stub1.jsonl: {"seq": 6, "t": 1790705544.579, "stub": "stub1", "method": "POST", "path": "/v1/messages?beta=true", "authorization": "Bearer:sk-test-A", "x-api-key": "sk-test-A", "model": "claude-opus-5-5", "stream": true, "status": 200, "phase": "stream-start", "delay": 0.0}
stub1.jsonl: {"seq": 7, "t": 1790705544.579, "stub": "stub1", "method": "POST", "path": "/v1/messages?beta=true", "authorization": "Bearer:sk-test-A", "x-api-key": "sk-test-A", "phase": "stream-end", "completed": true, "req_of": "msg_stub_5", "elapsed": 0.0}
stub1.jsonl: {"seq": 8, "t": 1790705544.793, "stub": "stub1", "method": "POST", "path": "/v1/messages?beta=true", "authorization": "Bearer:sk-test-B", "x-api-key": "sk-test-B", "model": "claude-opus-5-5", "stream": true, "status": 200, "phase": "stream-start", "delay": 0.0}
counter: 3:1790705544.583 BEGIN pid=3503964 mode=ok sleep=0 readat=start key_at_start=sk-test-B
counter: 4:1790705544.584 sk-test-B pid=3503964
```

Condensed (`summarize.py`; pids and startup lines dropped):

### a_ttl

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.001 DRIVER TURN send 't1 expect A'
+   0.146 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.147 HELPER(ctl) sk-test-A
+   0.207 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.229 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.229 DRIVER helper key=sk-test-B
+   0.229 DRIVER TURN send 't2 within TTL, expect cached A'
+   0.254 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.258 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.258 DRIVER TURN send 't3 after TTL'
+   4.269 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   4.272 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.273 HELPER(ctl) BEGIN key_at_start=sk-test-B
+   4.274 HELPER(ctl) sk-test-B
+   4.472 DRIVER TURN send "t4 right after t3's refresh"
+   4.483 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   4.485 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.485 DRIVER helper key=sk-test-C
+   8.485 DRIVER TURN send 't5 after TTL'
+   8.495 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   8.497 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   8.498 HELPER(ctl) BEGIN key_at_start=sk-test-C
+   8.499 HELPER(ctl) sk-test-C
+   8.698 DRIVER TURN send "t6 right after t5's refresh"
+   8.707 STUB1 POST /v1/messages auth=Bearer C x-api-key=C -> 200
+   8.710 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### a_default

```
+   0.000 DRIVER TURN send 't1 expect A'
+   0.155 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.157 HELPER(ctl) sk-test-A
+   0.217 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.239 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.239 DRIVER helper key=sk-test-B
+  60.239 DRIVER TURN send 't2 at ~1 min'
+  60.252 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+  60.255 DRIVER claude RESULT is_error=False subtype=success result='ok'
+ 310.255 DRIVER TURN send 't3 at ~5m10s'
+ 310.270 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+ 310.273 DRIVER claude RESULT is_error=False subtype=success result='ok'
+ 310.274 HELPER(ctl) BEGIN key_at_start=sk-test-B
+ 310.275 HELPER(ctl) sk-test-B
+ 310.473 DRIVER TURN send "t4 right after t3's refresh"
+ 310.485 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+ 310.489 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### b_401_switch

```
+   0.001 DRIVER TURN send 't1 expect A ok'
+   0.162 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.163 HELPER(ctl) sk-test-A
+   0.239 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.263 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.263 DRIVER stub1 reject_status=401
+   0.263 DRIVER helper key=sk-test-B
+   0.264 DRIVER stub1 reject=sk-test-A
+   0.264 DRIVER TURN send 't2 cached A rejected, helper now prints B'
+   0.296 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   1.306 HELPER(ctl) BEGIN key_at_start=sk-test-B
+   1.307 HELPER(ctl) sk-test-B
+   1.310 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   1.314 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   1.314 DRIVER TURN send 't3 expect B'
+   1.324 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   1.327 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### b_401_same

```
+   0.000 DRIVER TURN send 't1 expect A ok'
+   0.156 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.157 HELPER(ctl) sk-test-A
+   0.224 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.246 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.246 DRIVER stub1 reject=sk-test-A
+   0.246 DRIVER TURN send 't2 A rejected, helper still prints A'
+   0.273 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   1.282 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   1.283 HELPER(ctl) sk-test-A
+   1.286 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   2.293 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   2.295 HELPER(ctl) sk-test-A
+   2.298 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   2.299 DRIVER claude api_retry attempt=3/10 status=401 delay_ms=2482
+   4.788 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   4.788 HELPER(ctl) sk-test-A
+   4.791 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   4.792 DRIVER claude api_retry attempt=4/10 status=401 delay_ms=4393
+   9.192 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   9.193 HELPER(ctl) sk-test-A
+   9.196 DRIVER claude api_retry attempt=5/10 status=401 delay_ms=9881
+   9.196 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+  19.085 HELPER(ctl) BEGIN key_at_start=sk-test-A
+  19.086 HELPER(ctl) sk-test-A
+  19.089 DRIVER claude api_retry attempt=6/10 status=401 delay_ms=19513
+  19.089 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+  38.609 HELPER(ctl) BEGIN key_at_start=sk-test-A
+  38.610 HELPER(ctl) sk-test-A
+  38.613 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+  38.614 DRIVER claude api_retry attempt=7/10 status=401 delay_ms=33812
+  72.437 HELPER(ctl) BEGIN key_at_start=sk-test-A
+  72.438 HELPER(ctl) sk-test-A
+  72.444 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+  72.445 DRIVER claude api_retry attempt=8/10 status=401 delay_ms=34957
+ 107.410 HELPER(ctl) BEGIN key_at_start=sk-test-A
+ 107.411 HELPER(ctl) sk-test-A
+ 107.415 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+ 107.416 DRIVER claude api_retry attempt=9/10 status=401 delay_ms=39648
+ 147.078 HELPER(ctl) BEGIN key_at_start=sk-test-A
+ 147.079 HELPER(ctl) sk-test-A
+ 147.082 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+ 147.084 DRIVER claude api_retry attempt=10/10 status=401 delay_ms=33814
+ 180.906 HELPER(ctl) BEGIN key_at_start=sk-test-A
+ 180.907 HELPER(ctl) sk-test-A
+ 180.910 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+ 180.916 DRIVER claude RESULT is_error=True subtype=success result='Failed to authenticate. API Error: 401 stub rejected this key'
+ 180.916 DRIVER stub1 reject=
+ 180.916 DRIVER TURN send 't3 reject lifted, expect recovery'
+ 180.935 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+ 180.940 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### c_env

```
+   0.000 DRIVER TURN send 't1 expect stub1'
+   0.147 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.148 HELPER(ctl) sk-test-A
+   0.214 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.236 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.236 DRIVER settings.json edited: env.ANTHROPIC_BASE_URL -> stub2
+   5.236 DRIVER TURN send 't2 after env edit: stub1 or stub2?'
+   5.256 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   5.256 HELPER(ctl) sk-test-A
+   5.260 STUB2 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   5.263 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  10.263 DRIVER TURN send 't3 after env edit: stub1 or stub2?'
+  10.273 STUB2 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+  10.275 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### c_helper

```
+   0.001 DRIVER TURN send 't1 expect A via ctl'
+   0.151 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.152 HELPER(ctl) sk-test-A
+   0.214 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.235 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.236 DRIVER settings.json edited: apiKeyHelper -> helper.sh ctl2 (prints sk-test-H2)
+   5.236 DRIVER TURN send 't2 after helper edit: A (cached) or H2?'
+   5.256 HELPER(ctl2) BEGIN key_at_start=sk-test-H2
+   5.257 HELPER(ctl2) sk-test-H2
+   5.260 STUB1 POST /v1/messages auth=Bearer H2 x-api-key=H2 -> 200
+   5.263 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  10.263 DRIVER TURN send 't3 after helper edit: A (cached) or H2?'
+  10.273 STUB1 POST /v1/messages auth=Bearer H2 x-api-key=H2 -> 200
+  10.277 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### c_rewrite_same

```
+   0.000 DRIVER TURN send 't1 expect A'
+   0.165 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.166 HELPER(ctl) sk-test-A
+   0.236 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.259 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.260 DRIVER helper key=sk-test-B
+   0.260 DRIVER settings.json edited: byte-identical atomic rewrite (temp + rename)
+   5.260 DRIVER TURN send 't2 after identical rewrite: A (cached) or B?'
+   5.281 HELPER(ctl) BEGIN key_at_start=sk-test-B
+   5.282 HELPER(ctl) sk-test-B
+   5.285 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   5.289 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   5.289 DRIVER settings.json mtime touched (content unchanged, no rename)
+  10.289 DRIVER TURN send 't3 after mtime-only touch: A (cached) or B?'
+  10.308 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  10.309 HELPER(ctl) sk-test-B
+  10.312 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  10.315 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### d_fail

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.001 DRIVER TURN send 't1 expect A ok'
+   0.143 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.144 HELPER(ctl) sk-test-A
+   0.211 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.235 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.235 DRIVER helper key=sk-test-B
+   0.235 DRIVER helper mode=exit1
+   4.235 DRIVER TURN send 't2 helper exits 1 (refresh)'
+   4.247 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   4.250 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.252 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   4.253 HELPER(ctl) EXIT1
+   4.450 DRIVER TURN send 't3 right after the failed refresh'
+   4.459 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   4.462 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.662 DRIVER helper mode=empty
+   8.662 DRIVER TURN send 't4 helper prints empty (refresh)'
+   8.673 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   8.675 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   8.678 HELPER(ctl) BEGIN mode=empty sleep=0 readat=start key_at_start=sk-test-B
+   8.679 HELPER(ctl) EMPTY
+   8.875 DRIVER TURN send 't5 right after the empty refresh'
+   8.884 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   8.887 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   9.087 DRIVER helper mode=ok
+  13.087 DRIVER TURN send 't6 helper recovered (refresh)'
+  13.097 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+  13.100 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  13.102 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  13.103 HELPER(ctl) sk-test-B
+  13.300 DRIVER TURN send 't7 right after the recovered refresh: expect B'
+  13.309 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  13.312 DRIVER claude RESULT is_error=False subtype=success result='ok'
-- claude stderr:
apiKeyHelper failed: exited 1: helper: simulated failure
apiKeyHelper failed: did not return a value
```

### d_fail_401

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.001 DRIVER TURN send 't1 expect A ok'
+   0.152 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.153 HELPER(ctl) sk-test-A
+   0.219 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.240 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.241 DRIVER helper key=sk-test-B
+   0.241 DRIVER helper mode=exit1
+   0.241 DRIVER stub1 reject=sk-test-A
+   4.241 DRIVER TURN send 't2 cached A rejected and helper failing'
+   4.253 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   4.258 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   4.259 HELPER(ctl) EXIT1
+   5.262 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   5.263 HELPER(ctl) EXIT1
+   5.266 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+   6.275 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   6.276 HELPER(ctl) EXIT1
+   6.279 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+   6.280 DRIVER claude api_retry attempt=3/10 status=401 delay_ms=2228
+   8.515 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   8.516 HELPER(ctl) EXIT1
+   8.519 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+   8.522 DRIVER claude RESULT is_error=True subtype=success result="Your apiKeyHelper script is failing · This usually means you need to re-authenticate with your provider · Run /status to see the script's error output"
+   8.722 DRIVER TURN send 't3 helper still failing'
+   8.733 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+   9.741 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+   9.742 HELPER(ctl) EXIT1
+   9.745 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+  10.754 HELPER(ctl) BEGIN mode=exit1 sleep=0 readat=start key_at_start=sk-test-B
+  10.755 HELPER(ctl) EXIT1
+  10.758 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+  10.760 DRIVER claude RESULT is_error=True subtype=success result="Your apiKeyHelper script is failing · This usually means you need to re-authenticate with your provider · Run /status to see the script's error output"
+  10.961 DRIVER helper mode=ok
+  14.961 DRIVER TURN send 't4 helper recovered with B'
+  14.972 STUB1 POST /v1/messages auth=Bearer '' x-api-key='' -> 401
+  14.977 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  14.978 HELPER(ctl) sk-test-B
+  15.980 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  15.981 HELPER(ctl) sk-test-B
+  15.984 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  15.987 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  16.187 DRIVER TURN send 't5 expect B'
+  16.201 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  16.204 DRIVER claude RESULT is_error=False subtype=success result='ok'
-- claude stderr:
apiKeyHelper failed: exited 1: helper: simulated failure
apiKeyHelper failed: exited 1: helper: simulated failure
apiKeyHelper failed: exited 1: helper: simulated failure
apiKeyHelper failed: exited 1: helper: simulated failure
apiKeyHelper failed: exited 1: helper: simulated failure
```

### e_span

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.001 DRIVER TURN send 't1 expect A ok'
+   0.156 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.157 HELPER(ctl) sk-test-A
+   0.222 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.244 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.244 DRIVER helper sleep=3
+   0.244 DRIVER helper readat=start
+   4.244 DRIVER TURN send 't2 triggers a 3 s refresh that snapshots A; key -> B 1 s in'
+   4.256 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   4.258 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.260 HELPER(ctl) BEGIN mode=ok sleep=3 readat=start key_at_start=sk-test-A
+   5.244 DRIVER helper key=sk-test-B
+   5.244 DRIVER TURN send 't3 while that refresh is still running (second helper started?)'
+   5.255 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   5.258 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   7.262 HELPER(ctl) sk-test-A
+   7.562 DRIVER TURN send 't4 after the refresh printed A: expect A, not B'
+   7.572 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   7.574 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   7.575 DRIVER helper readat=end
+  11.575 DRIVER TURN send 't5 triggers a 3 s refresh that starts on B and prints C'
+  11.585 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+  11.587 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  11.590 HELPER(ctl) BEGIN mode=ok sleep=3 readat=end key_at_start=sk-test-B
+  12.575 DRIVER helper key=sk-test-C
+  14.593 HELPER(ctl) sk-test-C
+  14.879 DRIVER TURN send 't6 expect C (what the helper printed)'
+  14.889 STUB1 POST /v1/messages auth=Bearer C x-api-key=C -> 200
+  14.891 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  14.892 DRIVER helper readat=start
+  14.892 DRIVER helper sleep=12
+  14.892 DRIVER helper key=sk-test-D
+  18.892 DRIVER TURN send 't7 triggers a 12 s refresh printing D'
+  18.902 STUB1 POST /v1/messages auth=Bearer C x-api-key=C -> 200
+  18.904 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  18.904 DRIVER helper sleep=0
+  18.906 HELPER(ctl) BEGIN mode=ok sleep=12 readat=start key_at_start=sk-test-D
+  21.904 DRIVER TURN send 't8 3 s into the slow refresh'
+  21.914 STUB1 POST /v1/messages auth=Bearer C x-api-key=C -> 200
+  21.917 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  25.917 DRIVER TURN send 't9 7 s into the slow refresh'
+  25.929 STUB1 POST /v1/messages auth=Bearer C x-api-key=C -> 200
+  25.932 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  30.909 HELPER(ctl) sk-test-D
+  31.142 DRIVER TURN send 't10 after the slow refresh printed D: expect D'
+  31.151 STUB1 POST /v1/messages auth=Bearer D x-api-key=D -> 200
+  31.153 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### e_overlap

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.000 DRIVER TURN send 't1 expect A ok'
+   0.151 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.152 HELPER(ctl) sk-test-A
+   0.214 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.235 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.236 DRIVER helper sleep=6
+   0.236 DRIVER helper readat=start
+   4.236 DRIVER TURN send 't2 triggers a slow (6 s) background refresh that snapshots A'
+   4.249 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   4.252 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   4.252 HELPER(ctl) BEGIN mode=ok sleep=6 readat=start key_at_start=sk-test-A
+   4.752 DRIVER helper sleep=0
+   4.752 DRIVER helper key=sk-test-B
+   4.752 DRIVER stub1 reject=sk-test-A
+   4.752 DRIVER TURN send 't3 A now rejected -> 401 re-run (fast, prints B) while the slow A refresh runs'
+   4.762 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 401
+   5.771 HELPER(ctl) BEGIN key_at_start=sk-test-B
+   5.772 HELPER(ctl) sk-test-B
+   5.775 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+   5.778 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  10.254 HELPER(ctl) sk-test-A
+  10.486 DRIVER slow A invocation has finished
+  10.486 DRIVER TURN send 't4 after the slow invocation printed A: B (newest) or A (stale overwrite)?'
+  10.496 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  10.499 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  10.499 DRIVER TURN send 't5 again'
+  10.500 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  10.500 HELPER(ctl) sk-test-B
+  10.507 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  10.509 DRIVER claude RESULT is_error=False subtype=success result='ok'
```

### f_inflight

```
+   0.000 DRIVER CLAUDE_CODE_API_KEY_HELPER_TTL_MS=3000
+   0.000 DRIVER TURN send 't1 expect A ok'
+   0.153 HELPER(ctl) BEGIN key_at_start=sk-test-A
+   0.154 HELPER(ctl) sk-test-A
+   0.213 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200
+   0.234 DRIVER claude RESULT is_error=False subtype=success result='ok'
+   0.234 DRIVER helper key=sk-test-B
+   4.234 DRIVER stub1 delay=10
+   4.234 DRIVER TURN send 't2 10 s stream sent with stale A; its refresh adopts B; A rejected 3 s in'
+   4.246 STUB1 POST /v1/messages auth=Bearer A x-api-key=A -> 200 (delay 10.0s)
+   4.252 HELPER(ctl) BEGIN key_at_start=sk-test-B
+   4.253 HELPER(ctl) sk-test-B
+   7.235 DRIVER stub1 reject=sk-test-A
+  14.247 STUB1   stream of msg_stub_3 ended completed=True after 10.0s (auth=A x-api-key=A)
+  14.250 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  14.250 DRIVER stub1 delay=0
+  14.250 DRIVER TURN send 't3 expect B'
+  14.260 STUB1 POST /v1/messages auth=Bearer B x-api-key=B -> 200
+  14.262 DRIVER claude RESULT is_error=False subtype=success result='ok'
+  14.264 HELPER(ctl) BEGIN key_at_start=sk-test-B
+  14.265 HELPER(ctl) sk-test-B
```

## Implications for executor B (P6a)

1. **"Served" means "every request started after the ack".** The ack is written when the helper
   prints, and CC adopts that stdout for requests that start after the helper exits. The request
   that *triggered* a TTL refresh still went out on the old key. That is a one-request lag, and it
   is not a stale ack.
2. **There is no timer.** An idle session never runs the helper, so a commit stays `swapping…` until
   the next request. The plan's "typed warning after 2 × TTL" must count from the first request
   after the commit (or accept false warnings on idle sessions).
3. **A settings touch forces a synchronous swap.** Any change to the session's
   `CLAUDE_CONFIG_DIR/settings.json` drops CC's cached key, even a byte-identical rewrite or an mtime
   touch. The next request then runs the helper *before* it is sent. Executor B can touch the
   runtime `settings.json` on commit to remove the one-request lag and stop depending on the TTL.
   Two cautions: `write_merged_settings` skips byte-identical writes, so the touch has to be
   explicit, and the touch also reloads `env`, which is harmless because the runtime file is
   tollgate-owned.
4. **A failure after a 401 sends empty credentials.** It ends the turn with the "Your apiKeyHelper
   script is failing" text. The typed warning can quote it. A persistently rejected key costs about
   3 min of retries and 11 helper runs per turn, so the helper must stay cheap (the plan's < 50 ms,
   lock-free target).
5. **A slow helper is not killed.** CC keeps serving the old key until the helper exits, and there
   is never more than one refresh in flight, so a hung helper stalls the swap but never corrupts
   it.

Not covered here: the real-endpoint half of S1(g) (owner-run) and the gateway / `forceLogin*`
precondition.

S1 RESULT: PASS
