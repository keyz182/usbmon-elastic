# usbmon-elastic

> [!WARNING]
> I have been experimenting with LLMs - this repo is 100% LLM code. Use at your own risk!

Captures live USB bandwidth metrics directly from the Linux kernel's `usbmon` subsystem and ships them to Elasticsearch via the Elastic Agent `custom_logs` input.

## How it works

```
usbmon (kernel) → libpcap → usbmon-collector (Rust daemon) → /var/log/usbtop-metrics/usbtop.ndjson → Elastic Agent → Elasticsearch
```

A long-running Rust daemon opens every `usbmonN` pcap interface, captures USB Request Blocks in real time, accumulates per-device byte counts, and flushes a snapshot as NDJSON every 60 seconds (configurable). The Elastic Agent tails that file and ships it to Elasticsearch.

There is no dependency on `usbtop`, no pseudo-terminal, and no polling — the daemon reads directly from the same kernel interface that `usbtop` itself uses.

---

## Requirements

- Debian 11+ or Raspberry Pi OS (Bullseye / Bookworm)
- `libpcap-dev` and `build-essential`
- Rust toolchain (`cargo`)
- `usbmon` kernel module
- Elastic Agent installed and enrolled in Fleet (or running standalone)

---

## Install

```bash
# Install build dependencies (once)
apt-get install -y libpcap-dev build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source ~/.cargo/env

# Clone, build, and install
git clone https://github.com/keyz182/usbmon-elastic
cd usbmon-elastic
make
sudo make install
```

`make` compiles the release binary as your normal user. `sudo make install` then loads usbmon, installs the binary, enables the systemd service, and sets up logrotate.

### Uninstall

```bash
sudo make uninstall
```

---

## Configuration

The collector is configured via environment variables in the systemd service file. To change a value, edit `/etc/systemd/system/usbtop-collect.service`, uncomment the relevant `Environment=` line, then reload:

```bash
sudo systemctl daemon-reload && sudo systemctl restart usbtop-collect.service
```

| Variable | Default | Description |
|---|---|---|
| `USBMON_OUTPUT_FILE` | `/var/log/usbtop-metrics/usbtop.ndjson` | Output log path |
| `USBMON_INTERVAL_SEC` | `60` | Seconds between NDJSON flushes |
| `USBMON_LOG_LEVEL` | `INFO` | Set to `DEBUG` for per-packet logging |
| `USBTOP_OUTPUT_FILE` | *(same as above)* | Legacy alias — accepted for compatibility |

---

## Elastic Agent setup (Fleet / Kibana UI)

These steps configure the Elastic Agent already running on your Pi to tail the NDJSON log and ship it to Elasticsearch.

### Step 1 — Open Fleet

In Kibana, go to **Management → Fleet**.

If your Pi's agent does not appear in the **Agents** list, ensure the agent is installed and enrolled:

```bash
# On the Pi — check agent status
sudo systemctl status elastic-agent
sudo elastic-agent status
```

### Step 2 — Add the Custom Logs integration to your agent policy

1. In Fleet, click **Agent Policies** in the left sidebar.
2. Click the policy your Pi's agent is enrolled in (e.g. `Raspberry Pi`).
3. Click **Add integration** (top-right).
4. Search for **Custom Logs** and select it (it is published by Elastic, not a third-party).
5. Click **Add Custom Logs**.

### Step 3 — Configure the integration

Fill in the form as follows:

| Field | Value |
|---|---|
| **Integration name** | `usbtop-metrics` (or any descriptive name) |
| **Log file path** | `/var/log/usbtop-metrics/usbtop.ndjson` |
| **Datastream type** | `Metrics` |
| **Dataset name** | `usbtop.metrics` |

In the **Parsers** YAML field, replace the default content with:

```yaml
- ndjson:
    target: ""
    add_error_key: true
```

`target: ""` merges all JSON keys into the root document rather than nesting them under a field. `add_error_key: true` surfaces any NDJSON parse failures as `event.error` so they are visible in Discover.

Leave all other fields at their defaults.

Click **Save and deploy changes**. Fleet will push the updated policy to your agent within a few seconds.

### Step 4 — Verify data is flowing

Allow one full interval (default 60 s) for the first document to be written, then:

1. In Kibana, go to **Discover**.
2. Select the **`metrics-*`** data view (or create one if it does not exist).
3. In the search bar, filter by: `event.dataset : "usbtop.metrics"`
4. You should see documents with `usbtop.bus`, `usbtop.device`, `usbtop.in_kbps`, and `usbtop.out_kbps` fields.

If no documents appear after two intervals, work through the troubleshooting section below.

### Step 5 — (Optional) Create a data view for USB metrics only

1. Go to **Management → Data Views**.
2. Click **Create data view**.
3. Set **Index pattern** to `metrics-usbtop.metrics-*`.
4. Set **Timestamp field** to `@timestamp`.
5. Save. This gives you a clean view scoped to USB metrics in Discover and Lens.

### Step 6 — (Optional) Build a Lens dashboard

1. Go to **Dashboards → Create dashboard**.
2. Click **Create visualisation**.
3. Select the `logs-usbtop.metrics-*` data view.
4. In Lens, drag **`usbtop.in_kbps`** onto the canvas for a quick time-series chart.
5. Add a breakdown by **`usbtop.device_name`** to see per-device bandwidth.
6. Repeat for **`usbtop.out_kbps`**.

### Standalone agent (no Fleet)

If you manage `elastic-agent.yml` directly, add the block from `elastic/elastic-agent-input.yml` under the `inputs:` section, then restart the agent:

```bash
sudo systemctl restart elastic-agent
```

---

## Output document shape

Each line in the NDJSON log produces a document like this:

```json
{
  "@timestamp": "2025-06-01T12:00:00Z",
  "host": { "name": "raspberrypi" },
  "event": { "dataset": "usbtop.metrics", "module": "usbtop" },
  "usbtop": {
    "bus": 1,
    "device": 3,
    "device_name": "USB Audio Device",
    "in_kbps": 12.500,
    "out_kbps": 0.000
  }
}
```

When no USB activity is detected during an interval, a heartbeat document is written instead — this lets you distinguish "collector is running but idle" from "collector has stopped":

```json
{
  "@timestamp": "2025-06-01T12:01:00Z",
  "host": { "name": "raspberrypi" },
  "event": { "dataset": "usbtop.metrics", "module": "usbtop" },
  "usbtop": { "no_activity": true }
}
```

---

## Troubleshooting

### Check the service is running

```bash
systemctl status usbtop-collect.service
journalctl -fu usbtop-collect.service
```

### usbmon not loaded

```bash
sudo modprobe usbmon
# Verify the interfaces appear as pcap devices:
tcpdump -D | grep usbmon
```

### No output file being written

```bash
# Watch the file grow in real time
tail -f /var/log/usbtop-metrics/usbtop.ndjson

# If the file does not exist, check the service logs for errors
journalctl -u usbtop-collect.service -n 50
```

### Elastic Agent not picking up the file

```bash
# Check the agent is running
systemctl status elastic-agent

# Stream agent logs — look for errors relating to the logfile input
journalctl -fu elastic-agent | grep usbtop

# Confirm the agent can see the file
sudo ls -lh /var/log/usbtop-metrics/
```

### No data in Elasticsearch

Run this in Kibana **Dev Tools → Console** to check whether the index exists and has documents:

```
GET metrics-usbtop.metrics-default/_count
```

If the count is 0 or the index is missing:

1. Confirm the NDJSON file is being written and contains valid JSON: `tail -1 /var/log/usbtop-metrics/usbtop.ndjson | python3 -m json.tool`
2. In Fleet → Agent Policies, check the integration is saved and deployed (green tick next to your policy).
3. Check the data stream exists: `GET _data_stream/metrics-usbtop.metrics-*`
4. If the data stream is missing, the integration may not have been saved correctly — re-add it and ensure **Datastream type** is `Metrics` and **Dataset name** is exactly `usbtop.metrics`.

### Enable debug logging

Set `USBMON_LOG_LEVEL=DEBUG` in the service file for per-packet output:

```bash
sudo systemctl edit usbtop-collect.service
# Add:
# [Service]
# Environment=USBMON_LOG_LEVEL=DEBUG

sudo systemctl restart usbtop-collect.service
journalctl -fu usbtop-collect.service
```

---

## Project structure

```
usbmon-elastic/
├── collector-rs/
│   ├── Cargo.toml              # Rust crate (pcap + libc only)
│   └── src/
│       └── main.rs             # Daemon — pcap capture + NDJSON output
├── systemd/
│   └── usbtop-collect.service  # Long-running daemon unit (Type=simple)
├── elastic/
│   ├── elastic-agent-input.yml # Standalone agent YAML snippet
│   └── logrotate-usbtop        # Log rotation config
├── tools/
│   ├── usbtop_debug.py         # Debug helper — dumps raw usbmon text output
│   └── usbtop_collect.py       # Legacy Python collector (reference only)
├── Makefile
└── README.md
```
