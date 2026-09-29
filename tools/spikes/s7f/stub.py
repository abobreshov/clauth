#!/usr/bin/env python3
"""S7(f) spike: a stub OpenAI-compatible endpoint on 127.0.0.1 that runs out of credit.

Stands in for an exhausted OpenRouter account. OpenRouter's base URL is a
hard-coded HTTPS constant in Hermes 0.19.0 (`hermes_constants.py:1259`), so the
spike points Hermes' `custom` provider (`model.base_url`) at this stub instead;
the auxiliary 402 fallback (`agent/auxiliary_client.py:3973`) treats any
provider's 402 the same way.

- `GET  /v1/models`            → one model, `stub-model`.
- `POST /v1/chat/completions`  → the first `--ok` requests get a valid answer
  ("ok"; SSE when `stream: true`), every later one gets HTTP 402 with an
  OpenRouter-shaped "insufficient credits" body.
- anything else                → 404.

One JSON line per request goes to `--log`: path, model, stream flag, the first
60 characters of the last user message, and the status sent. No header values
are logged (the only key in play is the fake `sk-s7f-stub`).
"""
import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None
LOCK = threading.Lock()
STATE = {"seq": 0, "chat": 0}


def log(rec):
    with LOCK:
        STATE["seq"] += 1
        rec = {"seq": STATE["seq"], "t": round(time.time(), 3), **rec}
        with open(ARGS.log, "a") as f:
            f.write(json.dumps(rec) + "\n")


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, code, obj, ctype="application/json"):
        data = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            log({"path": self.path, "status": 200})
            self._send(200, {"object": "list", "data": [
                {"id": "stub-model", "object": "model", "context_length": 200000}]})
            return
        log({"path": self.path, "status": 404})
        self._send(404, {"error": {"message": "not found"}})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw) if raw else {}
        except ValueError:
            body = {}
        if not self.path.rstrip("/").endswith("/chat/completions"):
            log({"path": self.path, "status": 404})
            self._send(404, {"error": {"message": "not found"}})
            return
        msgs = body.get("messages") or []
        last_user = ""
        for m in reversed(msgs):
            if m.get("role") == "user":
                c = m.get("content")
                last_user = c if isinstance(c, str) else json.dumps(c)
                break
        with LOCK:
            STATE["chat"] += 1
            nth = STATE["chat"]
        stream = bool(body.get("stream"))
        rec = {"path": self.path, "model": body.get("model"), "stream": stream,
               "n": nth, "user": last_user[:60]}
        if nth > ARGS.ok:
            log({**rec, "status": 402})
            self._send(402, {"error": {
                "message": "Insufficient credits. This account never purchased credits. "
                           "Make sure your key is on the correct account or org, and if so, "
                           "purchase more at https://openrouter.ai/settings/credits",
                "code": 402}})
            return
        log({**rec, "status": 200})
        created = int(time.time())
        if not stream:
            self._send(200, {
                "id": f"chatcmpl-s7f-{nth}", "object": "chat.completion", "created": created,
                "model": body.get("model") or "stub-model",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        base = {"id": f"chatcmpl-s7f-{nth}", "object": "chat.completion.chunk",
                "created": created, "model": body.get("model") or "stub-model"}
        for delta, fin in (({"role": "assistant", "content": ""}, None),
                           ({"content": "ok"}, None), ({}, "stop")):
            chunk = {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": fin}]}
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
        usage = {**base, "choices": [], "usage": {
            "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}}
        self.wfile.write(f"data: {json.dumps(usage)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--ok", type=int, default=1)
    ARGS = p.parse_args()
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), H).serve_forever()


if __name__ == "__main__":
    main()
