"""S7(f): run the REAL Hermes entrypoint with a path recorder in front of it.

The sandbox has no strace, so this is the openat substitute for the Python
half of the process: a `sys.addaudithook` hook records every `open`,
`os.listdir`, `os.scandir`, `os.mkdir`, `os.rename`, `os.remove`,
`os.symlink`, `shutil.*`, `subprocess.Popen` and `socket.connect` event, and
`os.stat` / `os.lstat` are wrapped (stat is not an audit event, and Hermes'
credential reader starts with `Path.exists()`). The recorder only logs; it
changes no behaviour. C-level opens (sqlite, OpenSSL) are covered by the
inotify watch run.sh keeps on the outer home.

Argv: <log file> <entrypoint> [hermes args...]. The entrypoint is executed with
`runpy.run_path(..., run_name="__main__")`, i.e. the pip-generated script
itself, under the interpreter its shebang names.
"""
import os
import runpy
import sys

log_path, entry, *args = sys.argv[1:]
fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
busy = [False]
EVENTS = {"open", "os.listdir", "os.scandir", "os.mkdir", "os.rename", "os.remove",
          "os.rmdir", "os.symlink", "os.chmod", "os.truncate", "subprocess.Popen",
          "socket.connect", "os.exec", "os.posix_spawn"}


def rec(kind, path):
    if busy[0]:
        return
    busy[0] = True
    try:
        if isinstance(path, bytes):
            path = path.decode("utf-8", "replace")
        if isinstance(path, int):
            return
        os.write(fd, f"{kind}\t{path}\n".encode("utf-8", "replace"))
    except Exception:
        pass
    finally:
        busy[0] = False


def hook(event, a):
    if event in EVENTS or event.startswith("shutil."):
        first = a[0] if a else ""
        if event == "socket.connect":
            first = a[1] if len(a) > 1 else ""
        if isinstance(first, os.PathLike):
            first = os.fspath(first)
        rec(event, first if isinstance(first, (str, bytes)) else repr(first)[:200])


sys.addaudithook(hook)
_stat, _lstat = os.stat, os.lstat


def stat(p, *a, **k):
    if not isinstance(p, int):
        rec("os.stat", os.fspath(p))
    return _stat(p, *a, **k)


def lstat(p, *a, **k):
    if not isinstance(p, int):
        rec("os.lstat", os.fspath(p))
    return _lstat(p, *a, **k)


os.stat, os.lstat = stat, lstat
sys.argv = [entry, *args]
runpy.run_path(entry, run_name="__main__")
