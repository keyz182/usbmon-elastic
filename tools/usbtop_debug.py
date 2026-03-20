#!/usr/bin/env python3
"""
usbtop-debug — dump raw pty output from usbtop for 10 seconds.
Run as root to verify usbtop is working and check the output format.

Usage:
    sudo python3 usbtop_debug.py
    sudo python3 usbtop_debug.py 15    # custom duration in seconds
"""

import errno
import os
import pty
import re
import select
import signal
import subprocess
import sys
import time

DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 10


def main():
    print(f"Capturing usbtop output for {DURATION}s — make sure usbmon is loaded.")
    print("  If you see no output, try:  sudo modprobe usbmon\n")

    master_fd, slave_fd = pty.openpty()
    proc = subprocess.Popen(
        ["usbtop"],
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,
        close_fds=True,
        preexec_fn=os.setsid,
    )
    os.close(slave_fd)

    buf = b""
    deadline = time.monotonic() + DURATION

    while time.monotonic() < deadline:
        r, _, _ = select.select([master_fd], [], [], 0.3)
        if r:
            try:
                buf += os.read(master_fd, 4096)
            except OSError as exc:
                if exc.errno in (errno.EIO, errno.EBADF):
                    break
                raise

    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    except ProcessLookupError:
        pass
    proc.wait()
    os.close(master_fd)

    ansi_re = re.compile(rb"\x1b\[[0-9;]*[A-Za-z]|\x1b[()][A-B]|\r")
    cleaned = ansi_re.sub(b"", buf).decode("utf-8", errors="replace")

    print("=" * 60)
    print("RAW OUTPUT (ANSI stripped):")
    print("=" * 60)
    print(cleaned)
    print("=" * 60)
    print("\nIf the lines above show USB device rates, paste a few into")
    print("the project issue tracker so the regex can be verified.")


if __name__ == "__main__":
    main()
