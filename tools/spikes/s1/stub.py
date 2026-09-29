#!/usr/bin/env python3
"""S1 spike: minimal stub of the Anthropic Messages API on 127.0.0.1.

Answers POST /v1/messages (streaming SSE and non-streaming) with the text
"ok", logs one JSON line per request (path, which key headers were present
and their values -- fake labels such as sk-test-A only), and can be told
through control files (re-read on every request) to 401 a key or to stream
slowly. Anything else Claude Code calls gets a harmless 200 / 404 and is
logged too.

Control files in --ctl:
  reject   one key value per line; a request carrying any of them gets 401
           (a request carrying no non-empty key always gets 401)
  reject_status  401 (default) or 403 for rejected keys
  delay    float seconds; streaming responses spread their events over it
"""
import argparse
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
SEQ = [0]


def ctl_read(name, default=""):
    try:
        with open(os.path.join(ARGS.ctl, name)) as f:
            return f.read()
    except OSError:
        return default


def log(rec):
    with LOCK:
        SEQ[0] += 1
        rec = {"seq": SEQ[0], "t": round(time.time(), 3), "stub": ARGS.name, **rec}
        with open(ARGS.log, "a") as f:
            f.write(json.dumps(rec) + "\n")
            f.flush()


def key_headers(h):
    # Logged as "Bearer:<value>"; an empty value ("Bearer" / "Bearer ") logs as "Bearer:".
    auth = h.get("Authorization")
    if auth is not None and auth.startswith("Bearer"):
        auth = "Bearer:" + auth[len("Bearer"):].strip()
    return {"authorization": auth, "x-api-key": h.get("x-api-key")}


def presented_keys(kh):
    """Non-empty key values carried by either header."""
    out = set()
    a = kh["authorization"]
    if a and a.startswith("Bearer:") and a[len("Bearer:"):]:
        out.add(a[len("Bearer:"):])
    if kh["x-api-key"]:
        out.add(kh["x-api-key"])
    return out


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw) if raw else {}
        except ValueError:
            return {}

    def _json(self, code, obj, extra=None):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("request-id", "req_stub_%d" % SEQ[0])
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        kh = key_headers(self.headers)
        log({"method": "GET", "path": self.path, **kh, "status": 404})
        self._json(404, {"type": "error", "error": {"type": "not_found_error", "message": "stub"}})

    def do_HEAD(self):
        log({"method": "HEAD", "path": self.path, "status": 200})
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        body = self._body()
        kh = key_headers(self.headers)
        path = self.path.split("?")[0]
        rejected = {l.strip() for l in ctl_read("reject").splitlines() if l.strip()}
        keys = presented_keys(kh)
        base = {"method": "POST", "path": self.path, **kh,
                "model": body.get("model"), "stream": bool(body.get("stream"))}
        if path.endswith("/v1/messages/count_tokens"):
            log({**base, "status": 200})
            return self._json(200, {"input_tokens": 12})
        if not path.endswith("/v1/messages"):
            log({**base, "status": 404})
            return self._json(404, {"type": "error", "error": {"type": "not_found_error", "message": "stub"}})
        if keys & rejected or not keys:
            code = int((ctl_read("reject_status", "401").strip() or "401"))
            log({**base, "status": code})
            etype = "authentication_error" if code == 401 else "permission_error"
            return self._json(code, {"type": "error", "error": {
                "type": etype, "message": "stub rejected this key"}})
        mid = "msg_stub_%d" % SEQ[0]
        model = body.get("model") or "stub"
        if not body.get("stream"):
            log({**base, "status": 200})
            return self._json(200, {
                "id": mid, "type": "message", "role": "assistant", "model": model,
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 12, "output_tokens": 1}})
        try:
            delay = float(ctl_read("delay", "0").strip() or 0)
        except ValueError:
            delay = 0.0
        t0 = time.time()
        log({**base, "status": 200, "phase": "stream-start", "delay": delay})
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": mid, "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 12, "output_tokens": 0}}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": "ok"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                               "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.send_header("request-id", "req_" + mid)
        self.end_headers()
        self.close_connection = True
        step = delay / len(events) if delay else 0
        ok = True
        for name, data in events:
            try:
                self.wfile.write(("event: %s\ndata: %s\n\n" % (name, json.dumps(data))).encode())
                self.wfile.flush()
            except OSError:
                ok = False
                break
            if step:
                time.sleep(step)
        log({"method": "POST", "path": self.path, **kh, "phase": "stream-end",
             "completed": ok, "req_of": mid, "elapsed": round(time.time() - t0, 3)})


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--name", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--ctl", required=True)
    ARGS = p.parse_args()
    os.makedirs(ARGS.ctl, exist_ok=True)
    srv = ThreadingHTTPServer(("127.0.0.1", ARGS.port), H)
    srv.daemon_threads = True
    srv.serve_forever()


if __name__ == "__main__":
    main()
