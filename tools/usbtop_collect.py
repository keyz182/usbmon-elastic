#!/usr/bin/env python3
"""
usbtop-elastic collector
Captures usbtop USB bandwidth metrics via pty and emits NDJSON
for ingestion by the Elastic Agent custom_logs input.
"""

import errno
import json
import logging
import os
import pty
import re
import select
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

# ---------------------------------------------------------------------------
# Configuration — override via environment variables
# ---------------------------------------------------------------------------

OUTPUT_FILE    = os.environ.get("USBTOP_OUTPUT_FILE",    "/var/log/usbtop-metrics/usbtop.ndjson")
SETTLE_SECONDS = int(os.environ.get("USBTOP_SETTLE_SEC", "8"))
READ_TIMEOUT   = int(os.environ.get("USBTOP_READ_TIMEOUT", "2"))
LOG_LEVEL      = os.environ.get("USBTOP_LOG_LEVEL",      "INFO")
DEBUG_RAW_FILE = os.environ.get("USBTOP_DEBUG_RAW",      "")  # e.g. /tmp/usbtop-raw.txt

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

logging.basicConfig(
    level=getattr(logging, LOG_LEVEL.upper(), logging.INFO),
    format="%(asctime)s [%(levelname)s] %(message)s",
    stream=sys.stderr,
)
log = logging.getLogger("usbtop-collect")


# ---------------------------------------------------------------------------
# pty capture
# ---------------------------------------------------------------------------

def read_usbtop_snapshot() -> str:
    """Spawn usbtop on a pty, wait for data to settle, return captured output."""
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
    log.debug("usbtop pid=%d, settling for %ds", proc.pid, SETTLE_SECONDS)

    buf = b""
    settle_deadline  = time.monotonic() + SETTLE_SECONDS
    overall_deadline = settle_deadline + READ_TIMEOUT

    try:
        while time.monotonic() < overall_deadline:
            timeout = 0.2
            ready, _, _ = select.select([master_fd], [], [], timeout)
            if ready:
                try:
                    chunk = os.read(master_fd, 4096)
                    if chunk:
                        buf += chunk
                except OSError as exc:
                    if exc.errno in (errno.EIO, errno.EBADF):
                        log.debug("pty closed by usbtop")
                        break
                    raise
            else:
                # No data in this poll — if past settle period, we're done
                if time.monotonic() > settle_deadline:
                    break
    finally:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            log.warning("usbtop did not exit cleanly; sending SIGKILL")
            proc.kill()
            proc.wait()
        try:
            os.close(master_fd)
        except OSError:
            pass

    # Strip ANSI / VT100 escape sequences and carriage returns
    ansi_re = re.compile(rb"\x1b\[[0-9;]*[A-Za-z]|\x1b[()][A-B]|\r")
    cleaned = ansi_re.sub(b"", buf).decode("utf-8", errors="replace")

    if DEBUG_RAW_FILE:
        try:
            Path(DEBUG_RAW_FILE).write_text(cleaned)
            log.debug("raw output written to %s", DEBUG_RAW_FILE)
        except OSError as exc:
            log.warning("could not write debug file: %s", exc)

    return cleaned


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

def to_kbps(value: float, unit: str) -> float:
    """Normalise any rate to KB/s."""
    u = unit.upper().rstrip("B")  # "KB" → "K", "MB" → "M", "B" → ""
    if u == "":
        return value / 1024
    if u in ("K", ""):
        return value
    if u == "M":
        return value * 1024
    if u == "G":
        return value * 1024 * 1024
    return value  # unknown unit — return as-is


# Pattern A: "ID bus:dev [  1.23 KB/s] [  0.00 KB/s]  Device Name"
_PAT_A = re.compile(
    r"ID\s+(?P<bus>\d+):(?P<dev>\d+)"
    r"\s+\[\s*(?P<in_rate>[\d.]+)\s*(?P<in_unit>\w+)/s\]"
    r"\s+\[\s*(?P<out_rate>[\d.]+)\s*(?P<out_unit>\w+)/s\]"
    r"\s*(?P<name>.*)"
)

# Pattern B: "Bus N Device N [1.23 KB/s in] [0.00 KB/s out] Device Name"
_PAT_B = re.compile(
    r"Bus\s+(?P<bus>\d+)\s+Device\s+(?P<dev>\d+)"
    r".*?\[\s*(?P<in_rate>[\d.]+)\s*(?P<in_unit>\w+)/s\s+in\]"
    r"\s*\[\s*(?P<out_rate>[\d.]+)\s*(?P<out_unit>\w+)/s\s+out\]"
    r"\s*(?P<name>.*)",
    re.IGNORECASE,
)


def parse_usbtop(raw: str) -> list[dict]:
    events = []
    for line in raw.splitlines():
        for pat in (_PAT_A, _PAT_B):
            m = pat.search(line)
            if m:
                events.append({
                    "bus":         int(m.group("bus")),
                    "device":      int(m.group("dev")),
                    "device_name": m.group("name").strip(),
                    "in_kbps":     to_kbps(float(m.group("in_rate")),  m.group("in_unit")),
                    "out_kbps":    to_kbps(float(m.group("out_rate")), m.group("out_unit")),
                })
                log.debug("parsed device: bus=%s dev=%s in=%.2f out=%.2f name=%s",
                          m.group("bus"), m.group("dev"),
                          events[-1]["in_kbps"], events[-1]["out_kbps"],
                          events[-1]["device_name"])
                break
    return events


# ---------------------------------------------------------------------------
# Hostname helper
# ---------------------------------------------------------------------------

def hostname() -> str:
    try:
        return Path("/etc/hostname").read_text().strip()
    except OSError:
        return "unknown"


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    log.info("collecting usbtop snapshot (settle=%ds)", SETTLE_SECONDS)

    try:
        raw = read_usbtop_snapshot()
    except FileNotFoundError:
        log.error("usbtop not found — install with: apt install usbtop")
        sys.exit(1)

    devices = parse_usbtop(raw)
    log.info("parsed %d device(s)", len(devices))

    timestamp = datetime.now(timezone.utc).isoformat()
    host      = hostname()

    Path(OUTPUT_FILE).parent.mkdir(parents=True, exist_ok=True)

    with open(OUTPUT_FILE, "a") as fh:
        if devices:
            for dev in devices:
                doc = {
                    "@timestamp": timestamp,
                    "host":  {"name": host},
                    "usbtop": dev,
                    "event": {"dataset": "usbtop.metrics", "module": "usbtop"},
                }
                fh.write(json.dumps(doc) + "\n")
        else:
            # Heartbeat so you can tell the collector is running
            fh.write(json.dumps({
                "@timestamp": timestamp,
                "host":  {"name": host},
                "usbtop": {"no_activity": True},
                "event": {"dataset": "usbtop.metrics", "module": "usbtop"},
            }) + "\n")
            log.info("no USB activity detected — heartbeat written")

    log.info("output written to %s", OUTPUT_FILE)


if __name__ == "__main__":
    main()
