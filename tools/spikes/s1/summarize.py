#!/usr/bin/env python3
"""Condense runs/<scenario>/merged.txt into a short evidence excerpt:
turns, results, helper invocations, and each /v1/messages request with the
key it carried in each header. Usage: summarize.py <scenario> [...]"""
import json
import os
import sys

SPIKE = os.path.dirname(os.path.abspath(__file__))


def short(v):
    if v is None:
        return "-"
    return v.replace("sk-test-", "") if v else "''"


for name in sys.argv[1:]:
    print("== %s" % name)
    for line in open(os.path.join(SPIKE, "runs", name, "merged.txt")):
        t, rest = line[:9], line[10:].rstrip()
        if rest.startswith("STUB"):
            stub, js = rest.split(" ", 1)
            r = json.loads(js)
            if r.get("phase") == "stream-end":
                if r.get("elapsed", 0) > 1:
                    print("%s %s   stream of %s ended completed=%s after %.1fs (auth=%s x-api-key=%s)" % (
                        t, stub, r["req_of"], r["completed"], r["elapsed"],
                        short((r.get("authorization") or "").replace("Bearer:", "")), short(r.get("x-api-key"))))
                continue
            if r["method"] != "POST":
                print("%s %s %s %s -> %s" % (t, stub, r["method"], r["path"], r["status"]))
                continue
            auth = r.get("authorization")
            print("%s %s POST %s auth=%s x-api-key=%s -> %s%s" % (
                t, stub, r["path"].split("?")[0],
                "Bearer " + short(auth[7:]) if auth and auth.startswith("Bearer:") else short(auth),
                short(r.get("x-api-key")), r["status"],
                " (delay %ss)" % r["delay"] if r.get("delay") else ""))
        elif rest.startswith("HELPER"):
            print("%s %s" % (t, rest))
        elif rest.startswith("DRIVER"):
            m = rest[7:]
            if m.startswith(("env names", "exec ", "claude init", "claude assistant")):
                continue
            if m.startswith("claude system/api_retry"):
                ev = json.loads(m.split(" ", 2)[2]) if m.count("{") else {}
                print("%s DRIVER claude api_retry attempt=%s/%s status=%s delay_ms=%s" % (
                    t, ev.get("attempt"), ev.get("max_retries"), ev.get("error_status"), ev.get("retry_delay_ms")))
                continue
            print("%s DRIVER %s" % (t, m))
    err = open(os.path.join(SPIKE, "runs", name, "claude.stderr")).read().strip()
    if err:
        print("-- claude stderr:")
        print(err)
    print()
