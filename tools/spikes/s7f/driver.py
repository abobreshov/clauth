#!/usr/bin/env python3
"""S7(f) driver — runs INSIDE bubblewrap (see run.sh). Phases: setup | main | control.

Layout under the run dir (HOME inside the sandbox = <run>/home, the OUTER fake home):
  home/.claude/.credentials.json   sentinel (fake, "S7F-SENTINEL" token) — the file
  home/.claude.json                sentinel     Hermes must never open
  home/.gitconfig, home/.config/git/ignore, home/.ssh/{config,id_ed25519}  fake git/ssh
  home/.tollgate/profiles/s7f/hermes-home/          = HERMES_HOME (0700)
  home/.tollgate/profiles/s7f/hermes-home/shared/   = HERMES_SHARED_AUTH_DIR
  home/.tollgate/profiles/s7f/child-home/           = the child's HOME (0700), holding
      only .gitconfig -> ~/.gitconfig, .config/git -> ~/.config/git, .ssh -> ~/.ssh
Results go to <run>/out/.
"""
import glob
import json
import os
import re
import subprocess
import sys
import time

RUN = os.path.dirname(os.path.abspath(__file__))
OUTER = os.path.join(RUN, "home")
OUT = os.path.join(RUN, "out")
PROFILE = os.path.join(OUTER, ".tollgate", "profiles", "s7f")
HH = os.path.join(PROFILE, "hermes-home")
CH = os.path.join(PROFILE, "child-home")
PORT = 18097
SENTINEL = {"claudeAiOauth": {
    "accessToken": "sk-ant-oat01-S7F-SENTINEL-NOT-A-REAL-TOKEN",
    "refreshToken": "sk-ant-ort01-S7F-SENTINEL-NOT-A-REAL-TOKEN",
    "expiresAt": 4102444800000, "scopes": ["user:inference"], "subscriptionType": "max"}}


def write(path, text, mode=0o600):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    os.chmod(path, mode)


def log(name, text):
    with open(os.path.join(OUT, name), "a") as f:
        f.write(text if text.endswith("\n") else text + "\n")


def semver(s):
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", s)
    return tuple(int(x) for x in m.groups()) if m else None


def resolve_entrypoint():
    """Spec §4.5 step 2: the mise install glob, highest semver dir, then the
    shebang check (`#!<venv>/bin/python…` in the same venv; a bash shim is
    rejected, never executed)."""
    base = os.environ["S7F_MISE_INSTALLS"]
    cands = []
    for p in glob.glob(os.path.join(base, "*", "hermes-agent", "bin", "hermes")):
        v = semver(p[len(base) + 1:].split("/", 1)[0])
        if v:
            cands.append((v, p))
    if not cands:
        sys.exit("no Hermes install found by the mise glob")
    _, entry = max(cands)
    venv = os.path.dirname(os.path.dirname(entry))
    with open(entry, "rb") as f:
        first = f.readline().decode().strip()
    if not first.startswith("#!" + os.path.join(venv, "bin", "python")):
        sys.exit(f"{entry}: shebang {first!r} is not the venv's python; refusing")
    hsp = glob.glob(os.path.join(venv, "lib", "python3.*", "site-packages", "hermes_cli", "main.py"))
    meta = glob.glob(os.path.join(venv, "lib", "python3.*", "site-packages",
                                  "hermes_agent-*.dist-info", "METADATA"))
    assert len(hsp) == 1 and len(meta) == 1, (hsp, meta)
    ver = next(l.split(":", 1)[1].strip() for l in open(meta[0]) if l.startswith("Version:"))
    return entry, os.path.join(venv, "bin", "python"), ver


def child_env(home=CH):
    """§4.4 step 4: a scrubbed env (bwrap --clearenv already removed everything
    from outside), HERMES_HOME + HERMES_SHARED_AUTH_DIR, HOME = the child home."""
    return {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "dumb", "NO_COLOR": "1",
            "HOME": home, "HERMES_HOME": HH, "HERMES_SHARED_AUTH_DIR": os.path.join(HH, "shared")}


def run(name, argv, env, timeout=120, cwd=None):
    t0 = time.time()
    try:
        p = subprocess.run(argv, env=env, cwd=cwd or RUN, stdin=subprocess.DEVNULL,
                           capture_output=True, text=True, timeout=timeout)
        rc, so, se = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        rc, so, se = "timeout", (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or ""), \
            (e.stderr or b"").decode() if isinstance(e.stderr, bytes) else (e.stderr or "")
    log(f"{name}.txt", f"$ {' '.join(argv)}\n# HOME={env.get('HOME')}\n# rc={rc} "
        f"elapsed={time.time() - t0:.1f}s\n--- stdout\n{so}\n--- stderr\n{se[-6000:]}")
    return rc, so, se


def tree(root, skip=()):
    rows = []
    for dp, dns, fns in os.walk(root, followlinks=False):
        dns[:] = sorted(d for d in dns if os.path.join(dp, d) not in skip)
        for n in sorted(dns) + sorted(fns):
            p = os.path.join(dp, n)
            st = os.lstat(p)
            kind = "l" if os.path.islink(p) else ("d" if os.path.isdir(p) else "f")
            extra = f" -> {os.readlink(p)}" if kind == "l" else ""
            rows.append(f"{kind} {oct(st.st_mode & 0o7777)} {os.path.relpath(p, root)}{extra}")
    return rows


def setup():
    os.makedirs(OUT, exist_ok=True)
    write(os.path.join(OUTER, ".claude", ".credentials.json"), json.dumps(SENTINEL))
    write(os.path.join(OUTER, ".claude.json"), json.dumps({"primaryApiKey": "S7F-SENTINEL"}))
    write(os.path.join(OUTER, ".gitconfig"),
          "[user]\n\tname = S7F Spike\n\temail = s7f@example.invalid\n", 0o644)
    write(os.path.join(OUTER, ".config", "git", "ignore"), "*.s7f-ignored\n", 0o644)
    write(os.path.join(OUTER, ".ssh", "config"),
          "Host s7f-git\n\tHostName git.example.invalid\n\tUser git\n"
          "\tIdentityFile ~/.ssh/id_ed25519\n")
    write(os.path.join(OUTER, ".ssh", "id_ed25519"), "not a key (S7F spike placeholder)\n")
    os.chmod(os.path.join(OUTER, ".ssh"), 0o700)
    for d in (PROFILE, HH, os.path.join(HH, "shared"), CH, os.path.join(CH, ".config")):
        os.makedirs(d, exist_ok=True)
        os.chmod(d, 0o700)
    os.symlink(os.path.join(OUTER, ".gitconfig"), os.path.join(CH, ".gitconfig"))
    os.symlink(os.path.join(OUTER, ".config", "git"), os.path.join(CH, ".config", "git"))
    os.symlink(os.path.join(OUTER, ".ssh"), os.path.join(CH, ".ssh"))
    # The stub "OpenRouter" as Hermes' custom provider; auxiliary tasks left at
    # their default (auto) on purpose: the worst case the redirect must cover.
    write(os.path.join(HH, "config.yaml"),
          "model:\n  provider: custom\n  default: stub-model\n"
          f"  base_url: http://127.0.0.1:{PORT}/v1\n  api_key: sk-s7f-stub\n")
    log("layout-before.txt", "\n".join(tree(OUTER)))


def main():
    entry, py, ver = resolve_entrypoint()
    log("entrypoint.txt", f"entry={entry}\npython={py}\nversion={ver}\n"
        f"real_home_listing={sorted(os.listdir(os.environ['S7F_REAL_HOME']))}\n"
        f"real ~/.claude visible: {os.path.exists(os.path.join(os.environ['S7F_REAL_HOME'], '.claude'))}\n"
        f"real ~/.local/bin/hermes visible: "
        f"{os.path.exists(os.path.join(os.environ['S7F_REAL_HOME'], '.local/bin/hermes'))}\n")
    env = child_env()
    stub = [None]

    def fresh_stub(ok, tag):
        """A new stub per scenario, so each starts from its own answer budget."""
        if stub[0]:
            stub[0].terminate()
            stub[0].wait()
        log("stub.jsonl", json.dumps({"scenario": tag, "ok_budget": ok}))
        stub[0] = subprocess.Popen([sys.executable, os.path.join(RUN, "stub.py"), "--port",
                                    str(PORT), "--log", os.path.join(OUT, "stub.jsonl"),
                                    "--ok", str(ok)], stdin=subprocess.DEVNULL)
        time.sleep(0.7)

    audit = os.path.join(OUT, "audit-%s.tsv")
    try:
        run("01-version", [py, os.path.join(RUN, "auditwrap.py"), audit % "version", entry,
                           "--version"], env)
        run("02-config-set", [py, os.path.join(RUN, "auditwrap.py"), audit % "config-set", entry,
                              "config", "set", "auxiliary.curator.provider", "custom"], env)
        fresh_stub(0, "03-probe-redirect")
        run("03-probe-redirect", [py, "-B", os.path.join(RUN, "probe.py"), "redirect"], env)
        fresh_stub(0, "04-probe-forcegate")
        run("04-probe-forcegate", [py, "-B", os.path.join(RUN, "probe.py"), "forcegate"], env)
        # The real entrypoint, a real chat turn: the first request is answered,
        # every later one (the auto title, any retry) is a 402.
        fresh_stub(1, "05-chat")
        run("05-chat", [py, os.path.join(RUN, "auditwrap.py"), audit % "chat", entry,
                        "chat", "-q", "S7F say ok", "--provider", "custom", "-m", "stub-model"],
            env, timeout=180)
        # §8: the allowlisted links must suffice for git-over-ssh tools.
        run("06-git", ["git", "config", "--global", "--show-origin", "--get", "user.email"], env)
        subprocess.run(["git", "init", "-q", "/tmp/s7frepo"], env=env, check=True)
        run("06-git-excludes", ["git", "check-ignore", "-v", "x.s7f-ignored"], env,
            cwd="/tmp/s7frepo")
        # OpenSSH takes ~ from the passwd entry, not $HOME (`pw_dir`), so a real
        # launch reads the operator's ~/.ssh whatever HOME says. Inside the
        # sandbox the passwd home is the tmpfs, and root-owned /etc/ssh files
        # show up as the overflow uid (ssh refuses them), so these runs name
        # the config through the child home's link with -F: what is being
        # shown is that the link resolves for a tool that honours $HOME.
        import pwd
        log("06-ssh-passwd-home.txt", f"pw_dir={pwd.getpwuid(os.getuid()).pw_dir}")
        run("06-ssh", ["ssh", "-F", os.path.join(CH, ".ssh", "config"), "-G", "s7f-git"], env)
        run("06-ssh-v", ["ssh", "-F", os.path.join(CH, ".ssh", "config"), "-v", "-o",
                         "BatchMode=yes", "-o", "ConnectTimeout=2", "s7f-git", "true"],
            env, timeout=30)
    finally:
        if stub[0]:
            stub[0].terminate()
    log("child-home-after.txt", "\n".join(tree(CH)))
    log("hermes-home-after.txt", "\n".join(tree(HH)))


def control():
    _, py, _ = resolve_entrypoint()
    run("07-probe-control", [py, "-B", os.path.join(RUN, "probe.py"), "control"],
        child_env(home=OUTER))


{"setup": setup, "main": main, "control": control}[sys.argv[1]]()
