#!/usr/bin/env python3
"""S1 spike driver. Runs INSIDE the bubblewrap sandbox started by run.sh.

For one scenario it: starts two stub Anthropic APIs on 127.0.0.1:18081/18082
(private network namespace), writes CLAUDE_CONFIG_DIR/settings.json with only
apiKeyHelper + env, starts ONE real `claude -p --input-format stream-json
--output-format stream-json --verbose` process, and feeds it several user
turns over time while switching the helper's key file, the stub's reject
list, the helper's failure mode, or settings.json. Everything is timestamped
into runs/<scenario>/ (timeline.log, counter, stub*.jsonl, claude.jsonl,
debug.log) and merged into runs/<scenario>/merged.txt.
"""
import json
import os
import shutil
import subprocess
import sys
import threading
import time

SPIKE = os.path.dirname(os.path.abspath(__file__))
CLAUDE = os.environ["S1_CLAUDE"]
PORT1, PORT2 = 18081, 18082


def now():
    return round(time.time(), 3)


class Run:
    def __init__(self, name, ttl_ms=None, extra_env=None):
        self.name = name
        self.dir = os.path.join(SPIKE, "runs", name)
        shutil.rmtree(self.dir, ignore_errors=True)
        self.home = os.path.join(self.dir, "home")
        self.cfg = os.path.join(self.home, ".claude")
        self.ctl = os.path.join(self.dir, "ctl")
        self.ctl2 = os.path.join(self.dir, "ctl2")
        self.work = os.path.join(self.dir, "work")
        for d in (self.cfg, self.ctl, self.ctl2, self.work):
            os.makedirs(d)
        self.timeline = open(os.path.join(self.dir, "timeline.log"), "a")
        self.results = []
        self.lock = threading.Condition()
        self.ttl_ms = ttl_ms
        self.extra_env = extra_env or {}
        self.put(self.ctl, "key", "sk-test-A")
        self.put(self.ctl2, "key", "sk-test-H2")
        self.stubs = []
        for n, port in (("stub1", PORT1), ("stub2", PORT2)):
            sctl = os.path.join(self.dir, n + "-ctl")
            os.makedirs(sctl)
            self.stubs.append(subprocess.Popen(
                [sys.executable, os.path.join(SPIKE, "stub.py"), "--port", str(port),
                 "--name", n, "--log", os.path.join(self.dir, n + ".jsonl"), "--ctl", sctl]))
        time.sleep(0.5)
        self.settings = {
            "apiKeyHelper": "%s/helper.sh %s" % (SPIKE, self.ctl),
            "env": {
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:%d" % PORT1,
                "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            },
        }
        self.write_settings()

    # ---- control -------------------------------------------------------
    def put(self, d, name, value):
        tmp = os.path.join(d, "." + name + ".tmp")
        with open(tmp, "w") as f:
            f.write(value + "\n")
        os.replace(tmp, os.path.join(d, name))

    def stubctl(self, stub, name, value):
        self.put(os.path.join(self.dir, stub + "-ctl"), name, value)
        self.mark("%s %s=%s" % (stub, name, value.replace("\n", ",")))

    def helperctl(self, name, value, ctl=None):
        self.put(ctl or self.ctl, name, value)
        self.mark("helper %s=%s" % (name, value))

    def write_settings(self):
        path = os.path.join(self.cfg, "settings.json")
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self.settings, f, indent=2)
        os.replace(tmp, path)

    def edit_settings(self, fn, what):
        fn(self.settings)
        self.write_settings()
        self.mark("settings.json edited: " + what)

    def mark(self, msg):
        line = "%.3f DRIVER %s" % (now(), msg)
        self.timeline.write(line + "\n")
        self.timeline.flush()
        print("[%s] %s" % (self.name, line), flush=True)

    # ---- claude --------------------------------------------------------
    def start(self):
        env = {
            "PATH": "/usr/bin:/bin:" + os.path.dirname(CLAUDE),
            "HOME": self.home,
            "LANG": "C.UTF-8",
            "TERM": "dumb",
            "CLAUDE_CONFIG_DIR": self.cfg,
            "DISABLE_TELEMETRY": "1",
            "DISABLE_ERROR_REPORTING": "1",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_AUTOUPDATER": "1",
        }
        if self.ttl_ms is not None:
            env["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"] = str(self.ttl_ms)
        env.update(self.extra_env)
        leaked = [k for k in os.environ if k.startswith("ANTHROPIC_") or k in (
            "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR")]
        assert not leaked, "outer env leaked into sandbox: %r" % leaked
        self.mark("env names: " + " ".join(sorted(env)))
        if self.ttl_ms is not None:
            self.mark("CLAUDE_CODE_API_KEY_HELPER_TTL_MS=%d" % self.ttl_ms)
        for k, v in self.extra_env.items():
            self.mark("extra env %s=%r" % (k, v))
        cmd = [CLAUDE, "-p", "--input-format", "stream-json", "--output-format", "stream-json",
               "--verbose", "--no-session-persistence",
               "--debug-file", os.path.join(self.dir, "debug.log")]
        self.mark("exec " + " ".join(cmd))
        self.out = open(os.path.join(self.dir, "claude.jsonl"), "a")
        self.proc = subprocess.Popen(cmd, cwd=self.work, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=open(os.path.join(self.dir, "claude.stderr"), "w"),
                                     text=True, bufsize=1)
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.proc.stdout:
            t = now()
            self.out.write("%.3f %s" % (t, line))
            self.out.flush()
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            typ = ev.get("type")
            if typ == "system" and ev.get("subtype") == "init":
                self.mark("claude init apiKeySource=%s model=%s version=%s" % (
                    ev.get("apiKeySource"), ev.get("model"), ev.get("claude_code_version")))
            elif typ == "system":
                self.mark("claude system/%s %s" % (ev.get("subtype"), json.dumps(ev)[:400]))
            elif typ == "result":
                self.mark("claude RESULT is_error=%s subtype=%s result=%r" % (
                    ev.get("is_error"), ev.get("subtype"), (ev.get("result") or "")[:300]))
                with self.lock:
                    self.results.append(ev)
                    self.lock.notify_all()
            elif typ == "assistant":
                txt = "".join(c.get("text", "") for c in ev.get("message", {}).get("content", [])
                              if isinstance(c, dict))
                self.mark("claude assistant text=%r" % txt[:300])
        self.mark("claude stdout closed rc=%s" % self.proc.poll())
        with self.lock:
            self.lock.notify_all()

    def turn(self, text, timeout=120):
        with self.lock:
            n = len(self.results)
        self.mark("TURN send %r" % text)
        msg = {"type": "user", "message": {"role": "user", "content": text}}
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        t0 = time.time()
        with self.lock:
            while len(self.results) <= n and time.time() - t0 < timeout and self.proc.poll() is None:
                self.lock.wait(0.5)
            ok = len(self.results) > n
        self.mark("TURN done %r after %.1fs%s" % (text, time.time() - t0, "" if ok else " (TIMEOUT)"))
        return self.results[n] if ok else None

    def helper_idle(self, timeout=20):
        """Wait until every helper invocation that began has also ended."""
        t0 = time.time()
        while time.time() - t0 < timeout:
            busy = 0
            for c in (self.ctl, self.ctl2):
                p = os.path.join(c, "counter")
                if os.path.exists(p):
                    lines = open(p).read().splitlines()
                    busy += sum(1 for l in lines if " BEGIN " in l) - sum(1 for l in lines if " BEGIN " not in l)
            if busy <= 0:
                time.sleep(0.2)
                self.mark("helper idle (%.1fs)" % (time.time() - t0))
                return
            time.sleep(0.1)
        self.mark("helper still busy after %ds" % timeout)

    def wait(self, s, why=""):
        self.mark("sleep %.1fs %s" % (s, why))
        time.sleep(s)

    def finish(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(20)
        except Exception:
            self.proc.kill()
        for s in self.stubs:
            s.terminate()
        self.mark("done")
        self.timeline.close()
        merge(self.dir)


def merge(d):
    rows = []
    for line in open(os.path.join(d, "timeline.log")):
        t, rest = line.split(" ", 1)
        rows.append((float(t), rest.rstrip()))
    for c in ("ctl", "ctl2"):
        p = os.path.join(d, c, "counter")
        if os.path.exists(p):
            for line in open(p):
                t, rest = line.split(" ", 1)
                rows.append((float(t), "HELPER(%s) %s" % (c, rest.rstrip())))
    for n in ("stub1", "stub2"):
        p = os.path.join(d, n + ".jsonl")
        if os.path.exists(p):
            for line in open(p):
                r = json.loads(line)
                t = r.pop("t")
                r.pop("stub")
                rows.append((t, "%s %s" % (n.upper(), json.dumps(r))))
    rows.sort(key=lambda r: r[0])
    t0 = rows[0][0] if rows else 0
    with open(os.path.join(d, "merged.txt"), "w") as f:
        for t, rest in rows:
            f.write("+%8.3f %s\n" % (t - t0, rest))


# ---- scenarios ---------------------------------------------------------------
def s_a_ttl():
    r = Run("a_ttl", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A")
    r.helperctl("key", "sk-test-B")
    r.turn("t2 within TTL, expect cached A")
    r.wait(4, "past the 3 s TTL")
    r.turn("t3 after TTL")
    r.helper_idle()
    r.turn("t4 right after t3's refresh")
    r.helperctl("key", "sk-test-C")
    r.wait(4, "past the 3 s TTL, no requests meanwhile")
    r.turn("t5 after TTL")
    r.helper_idle()
    r.turn("t6 right after t5's refresh")
    r.finish()


def s_a_default():
    r = Run("a_default")
    r.start()
    r.turn("t1 expect A")
    r.helperctl("key", "sk-test-B")
    r.wait(60, "1 min, inside a 5 min default")
    r.turn("t2 at ~1 min")
    r.wait(250, "until ~5m10s after t1")
    r.turn("t3 at ~5m10s")
    r.helper_idle()
    r.turn("t4 right after t3's refresh")
    r.finish()


def s_b_401_same():
    r = Run("b_401_same")
    r.start()
    r.turn("t1 expect A ok")
    r.stubctl("stub1", "reject", "sk-test-A")
    r.turn("t2 A rejected, helper still prints A", timeout=300)
    r.stubctl("stub1", "reject", "")
    r.turn("t3 reject lifted, expect recovery")
    r.finish()


def s_b_401_switch(status="401"):
    r = Run("b_%s_switch" % status)
    r.start()
    r.turn("t1 expect A ok")
    r.stubctl("stub1", "reject_status", status)
    r.helperctl("key", "sk-test-B")
    r.stubctl("stub1", "reject", "sk-test-A")
    r.turn("t2 cached A rejected, helper now prints B", timeout=300)
    r.turn("t3 expect B")
    r.finish()


def s_b_403_switch():
    s_b_401_switch("403")


def s_c_env():
    r = Run("c_env")
    r.start()
    r.turn("t1 expect stub1")
    r.edit_settings(lambda s: s["env"].__setitem__("ANTHROPIC_BASE_URL", "http://127.0.0.1:%d" % PORT2),
                    "env.ANTHROPIC_BASE_URL -> stub2")
    r.wait(5, "file watcher")
    r.turn("t2 after env edit: stub1 or stub2?")
    r.wait(5)
    r.turn("t3 after env edit: stub1 or stub2?")
    r.finish()


def s_c_helper():
    r = Run("c_helper")
    r.start()
    r.turn("t1 expect A via ctl")
    r.edit_settings(lambda s: s.__setitem__("apiKeyHelper", "%s/helper.sh %s" % (SPIKE, r.ctl2)),
                    "apiKeyHelper -> helper.sh ctl2 (prints sk-test-H2)")
    r.wait(5, "file watcher")
    r.turn("t2 after helper edit: A (cached) or H2?")
    r.wait(5)
    r.turn("t3 after helper edit: A (cached) or H2?")
    r.finish()


def s_c_touch():
    """Does an unrelated settings.json edit (same helper, same env) drop the cached key?"""
    r = Run("c_touch")
    r.start()
    r.turn("t1 expect A")
    r.helperctl("key", "sk-test-B")
    r.edit_settings(lambda s: s.__setitem__("cleanupPeriodDays", 30), "add cleanupPeriodDays (helper/env unchanged)")
    r.wait(5, "file watcher")
    r.turn("t2 after unrelated edit: A (cached) or B?")
    r.finish()


def s_c_rewrite_same():
    """Does a byte-identical atomic rewrite of settings.json drop the cached key?"""
    r = Run("c_rewrite_same")
    r.start()
    r.turn("t1 expect A")
    r.helperctl("key", "sk-test-B")
    r.edit_settings(lambda s: None, "byte-identical atomic rewrite (temp + rename)")
    r.wait(5, "file watcher")
    r.turn("t2 after identical rewrite: A (cached) or B?")
    os.utime(os.path.join(r.cfg, "settings.json"))
    r.mark("settings.json mtime touched (content unchanged, no rename)")
    r.wait(5, "file watcher")
    r.turn("t3 after mtime-only touch: A (cached) or B?")
    r.finish()


def s_d_fail():
    r = Run("d_fail", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A ok")
    r.helperctl("key", "sk-test-B")
    r.helperctl("mode", "exit1")
    r.wait(4, "past TTL")
    r.turn("t2 helper exits 1 (refresh)", timeout=300)
    r.helper_idle()
    r.turn("t3 right after the failed refresh", timeout=300)
    r.helper_idle()
    r.helperctl("mode", "empty")
    r.wait(4, "past TTL")
    r.turn("t4 helper prints empty (refresh)", timeout=300)
    r.helper_idle()
    r.turn("t5 right after the empty refresh", timeout=300)
    r.helper_idle()
    r.helperctl("mode", "ok")
    r.wait(4, "past TTL")
    r.turn("t6 helper recovered (refresh)", timeout=300)
    r.helper_idle()
    r.turn("t7 right after the recovered refresh: expect B", timeout=300)
    r.finish()


def s_d_fail_401():
    r = Run("d_fail_401", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A ok")
    r.helperctl("key", "sk-test-B")
    r.helperctl("mode", "exit1")
    r.stubctl("stub1", "reject", "sk-test-A")
    r.wait(4, "past TTL")
    r.turn("t2 cached A rejected and helper failing", timeout=400)
    r.helper_idle()
    r.turn("t3 helper still failing", timeout=400)
    r.helper_idle()
    r.helperctl("mode", "ok")
    r.wait(4, "past TTL")
    r.turn("t4 helper recovered with B", timeout=300)
    r.helper_idle()
    r.turn("t5 expect B", timeout=300)
    r.finish()


def s_e_span():
    r = Run("e_span", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A ok")
    # (e1) helper snapshots the key at start; the switch to B lands mid-invocation
    r.helperctl("sleep", "3")
    r.helperctl("readat", "start")
    r.wait(4, "past TTL")
    th = threading.Timer(1.0, lambda: r.helperctl("key", "sk-test-B"))
    th.start()
    r.turn("t2 triggers a 3 s refresh that snapshots A; key -> B 1 s in")
    th.join()
    r.turn("t3 while that refresh is still running (second helper started?)")
    r.helper_idle()
    r.turn("t4 after the refresh printed A: expect A, not B")
    # (e2) helper re-reads at the end; the switch to C lands mid-invocation
    r.helperctl("readat", "end")
    r.wait(4, "past TTL")
    th = threading.Timer(1.0, lambda: r.helperctl("key", "sk-test-C"))
    th.start()
    r.turn("t5 triggers a 3 s refresh that starts on B and prints C")
    th.join()
    r.helper_idle()
    r.turn("t6 expect C (what the helper printed)")
    # (e3) a slow helper (12 s > the 10 s warning) printing D; turns keep coming
    r.helperctl("readat", "start")
    r.helperctl("sleep", "12")
    r.helperctl("key", "sk-test-D")
    r.wait(4, "past TTL")
    r.turn("t7 triggers a 12 s refresh printing D")
    r.helperctl("sleep", "0")
    r.wait(3)
    r.turn("t8 3 s into the slow refresh")
    r.wait(4)
    r.turn("t9 7 s into the slow refresh")
    r.helper_idle(30)
    r.turn("t10 after the slow refresh printed D: expect D")
    r.finish()


def s_e_overlap():
    """An older, slow invocation finishing AFTER a newer, fast one: does CC let
    the older (stale) stdout overwrite the newer key?"""
    r = Run("e_overlap", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A ok")
    r.helperctl("sleep", "6")
    r.helperctl("readat", "start")
    r.wait(4, "past TTL")
    r.turn("t2 triggers a slow (6 s) background refresh that snapshots A")
    r.wait(0.5)
    r.helperctl("sleep", "0")
    r.helperctl("key", "sk-test-B")
    r.stubctl("stub1", "reject", "sk-test-A")
    r.turn("t3 A now rejected -> 401 re-run (fast, prints B) while the slow A refresh runs")
    r.helper_idle(30)
    r.mark("slow A invocation has finished")
    r.turn("t4 after the slow invocation printed A: B (newest) or A (stale overwrite)?")
    r.turn("t5 again")
    r.finish()


def s_f_inflight():
    r = Run("f_inflight", ttl_ms=3000)
    r.start()
    r.turn("t1 expect A ok")
    r.helperctl("key", "sk-test-B")
    r.wait(4, "past TTL")
    r.stubctl("stub1", "delay", "10")
    th = threading.Timer(3.0, lambda: r.stubctl("stub1", "reject", "sk-test-A"))
    th.start()
    r.turn("t2 10 s stream sent with stale A; its refresh adopts B; A rejected 3 s in", timeout=300)
    th.join()
    r.stubctl("stub1", "delay", "0")
    r.turn("t3 expect B")
    r.finish()


def s_h_blank_key():
    r = Run("h_blank_key", extra_env={"ANTHROPIC_API_KEY": ""})
    r.start()
    r.turn("t1 ANTHROPIC_API_KEY blank + helper")
    r.finish()


SCENARIOS = {k[2:]: v for k, v in globals().items() if k.startswith("s_")}

if __name__ == "__main__":
    names = sys.argv[1:] or list(SCENARIOS)
    os.makedirs(os.path.join(SPIKE, "runs"), exist_ok=True)
    with open(os.path.join(SPIKE, "runs", "claude-version.txt"), "w") as f:
        subprocess.run([CLAUDE, "--version"], stdout=f, env={"PATH": "/usr/bin:/bin", "HOME": os.path.join(SPIKE, "home")})
    for n in names:
        SCENARIOS[n]()
