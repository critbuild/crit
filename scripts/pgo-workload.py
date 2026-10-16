#!/usr/bin/env python3
"""Training run for scripts/pgo-build.sh: drives an instrumented crit through the work it
spends its CPU on (parsing agent output, drawing frames), so the profile-guided build is laid
out for those paths.

usage: pgo-workload.py CRIT_BINARY WORK_DIRECTORY

The main load is pgo-agent-output.bin: 60 KB of a real OMP pane's output (startup queries cut,
letters and digits scrambled, escape sequences, spacing and glyphs kept), replayed many times.
A profile has to come from output like this: one trained on synthetic frames measured slower
than no profile at all on real agent output. Repaints, wide characters and a split add the rest.
It runs in a throwaway home and state directory, so the user's workspace, agents and updaters
are never touched. Only the Python standard library is used.
"""
import fcntl
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

ROWS, COLS = 50, 180
SAMPLE = Path(__file__).with_name("pgo-agent-output.bin")
REPEAT = 120


def main() -> None:
    binary = str(Path(sys.argv[1]).resolve())
    # The shell runs the commands below from its own folder: every path in them is absolute.
    work = Path(sys.argv[2]).resolve()
    replay = work / "workload.bin"
    replay.write_bytes(SAMPLE.read_bytes() * REPEAT)
    root = work / "run"
    for name in ("home", "state/crit", "sandbox", "repo"):
        (root / name).mkdir(parents=True, mode=0o700, exist_ok=True)
    subprocess.run(["git", "init", "-q", str(root / "repo")], check=True)
    # Crit never runs the real updaters from this throwaway state directory.
    (root / "state/crit/updates.stamp").touch(mode=0o600)
    # XDG_RUNTIME_DIR stays the user's: Crit's socket goes in a folder of its own there, and a
    # throwaway one this deep would be too long a path for a socket.
    env = dict(
        os.environ,
        HOME=str(root / "home"),
        XDG_STATE_HOME=str(root / "state"),
        CRIT_SANDBOX=str(root / "sandbox"),
        TERM="xterm-256color",
        SHELL="/bin/sh",
        PS1="$ ",
    )
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(root / "repo")
        os.execve(binary, [binary, "--no-kitty", "--fresh", "--agent", "shell"], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def pump(seconds: float, done: Path | None = None) -> None:
        end = time.time() + seconds
        while time.time() < end:
            if select.select([fd], [], [], 0.05)[0]:
                try:
                    os.read(fd, 1 << 16)
                except OSError:
                    return
            if done and done.exists():
                return

    steps = iter(range(1, 100))

    def run(command: str) -> None:
        # Done once the shell has run the command: what crit draws is a diff of the screen,
        # in which no marker text can be relied on to appear whole.
        done = work / f"done-{next(steps)}"
        os.write(fd, f"{command}; touch {done}\r".encode())
        pump(180, done)
        # A profile of a crit that never ran the workload would lay the build out for startup
        # alone, which measured slower than no profile at all.
        if not done.exists():
            raise RuntimeError(f"crit did not finish training step {done.name}")

    try:
        pump(2.5)
        run(f"cat {replay}")
        run(
            "i=0; while [ $i -lt 120 ]; do printf '\\033[H'; j=0; while [ $j -lt 45 ]; do "
            "printf '\\033[3%dmrow %d frame %d \\033[1mbold\\033[0m %s\\n' $((j%8)) $j $i "
            "'text that fills a good part of the row with words and numbers 1234567890'; "
            "j=$((j+1)); done; i=$((i+1)); done"
        )
        run('i=0; while [ $i -lt 400 ]; do echo "日本語 $i 😀 café"; i=$((i+1)); done')
        # A side-by-side split, focus moves between its panes, the worktree pane on and off.
        for keys in (b"\x1b[44;6u", b"\x1b[B\r", b"\x1b[91;6u", b"\x1b[93;6u", b"\x1bOQ", b"\x1bOQ"):
            os.write(fd, keys)
            pump(0.6)
        run(f"cat {replay}")
        if os.waitpid(pid, os.WNOHANG)[0]:
            raise RuntimeError("crit exited before the training ended")
    finally:
        # SIGTERM lets crit exit normally, which is when the profile is written.
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        pump(3)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
