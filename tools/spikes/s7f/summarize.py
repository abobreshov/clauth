#!/usr/bin/env python3
"""S7(f): condense a run dir's out/ into the verdict lines the spike doc quotes.

  python3 tools/spikes/s7f/summarize.py <run dir>

For every audit log (one per real-entrypoint scenario) it classifies each
recorded path relative to the outer fake home:
  hermes-home   inside HERMES_HOME (Hermes' own tree, allowed)
  child-home    inside the child HOME (allowed: what Hermes sees as ~)
  via-link      a path under the outer home reached through an allowlisted
                child-home link (.ssh, .gitconfig, .config/git)
  OUTSIDE       anything else under the outer home — a failure
and prints the counts per class and event, plus every OUTSIDE and via-link
path. The probe JSONs and the inotify logs are echoed as they are.
"""
import collections
import glob
import json
import os
import sys

run = os.path.abspath(sys.argv[1])
out = os.path.join(run, "out")
home = os.path.join(run, "home")
hh = os.path.join(home, ".tollgate", "profiles", "s7f", "hermes-home")
ch = os.path.join(home, ".tollgate", "profiles", "s7f", "child-home")
links = [os.path.join(home, p) for p in (".ssh", ".gitconfig", os.path.join(".config", "git"))]
ancestors = {home, os.path.join(home, ".tollgate"), os.path.join(home, ".tollgate", "profiles"),
             os.path.dirname(hh)}


def under(p, root):
    return p == root or p.startswith(root + os.sep)


def cls(p):
    if under(p, hh):
        return "hermes-home"
    if under(p, ch):
        return "child-home"
    if any(under(p, l) for l in links):
        return "via-link"
    if p in ancestors:
        return "ancestor-dir"
    return "OUTSIDE"


verdict_ok = True
for f in sorted(glob.glob(os.path.join(out, "audit-*.tsv"))):
    counts = collections.Counter()
    special = collections.Counter()
    for line in open(f, encoding="utf-8", errors="replace"):
        ev, _, p = line.rstrip("\n").partition("\t")
        if not under(p, home):
            continue
        c = cls(p)
        counts[(c, ev)] += 1
        if c in ("OUTSIDE", "via-link"):
            special[(c, ev, os.path.relpath(p, home))] += 1
        if c == "OUTSIDE":
            verdict_ok = False
    print(f"## {os.path.basename(f)}")
    for (c, ev), n in sorted(counts.items()):
        print(f"  {c:12} {ev:18} {n}")
    for (c, ev, p), n in sorted(special.items()):
        print(f"    {c} {ev} ~/{p} x{n}")

for name in ("inotify-main.log", "inotify-control.log"):
    body = open(os.path.join(out, name)).read().strip()
    print(f"## {name}: {len(body.splitlines()) if body else 0} events")
    for line in body.splitlines():
        print("  " + line.replace(run, "<run>"))
    if name == "inotify-main.log" and body:
        verdict_ok = False

for f in sorted(glob.glob(os.path.join(out, "0[347]-probe-*.txt"))):
    txt = open(f).read()
    js = json.loads(txt[txt.index("--- stdout") + 10:txt.index("--- stderr")])
    keep = {k: (v.replace(run, "<run>") if isinstance(v, str) else v) for k, v in js.items()
            if k != "fallback_log"}
    print(f"## {os.path.basename(f)}")
    print("  " + json.dumps(keep, sort_keys=True))
print("VERDICT", "PASS" if verdict_ok else "FAIL")

# --export <dir>: copy the small evidence files there with the run dir
# replaced by <run> (the audit logs stay in the run dir: they are megabytes).
if len(sys.argv) > 3 and sys.argv[2] == "--export":
    dest = sys.argv[3]
    os.makedirs(dest, exist_ok=True)
    for f in sorted(os.listdir(out)):
        if f.startswith("audit-") or f == "layout-before.txt":
            continue
        with open(os.path.join(out, f), encoding="utf-8", errors="replace") as src:
            body = src.read().replace(run, "<run>")
        with open(os.path.join(dest, f), "w") as dst:
            dst.write(body)
