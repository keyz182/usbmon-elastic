#!/usr/bin/env bash
# =============================================================================
# install.sh — Install usbmon-elastic collector on Debian/Raspberry Pi OS
# =============================================================================
# Run as root:  sudo bash install.sh
# Uninstall:    sudo bash install.sh --uninstall
# =============================================================================

set -euo pipefail

INSTALL_DIR="/opt/usbtop-elastic"
LOG_DIR="/var/log/usbtop-metrics"
SYSTEMD_DIR="/etc/systemd/system"
LOGROTATE_DIR="/etc/logrotate.d"

BOLD="\033[1m"
GREEN="\033[32m"
YELLOW="\033[33m"
RED="\033[31m"
RESET="\033[0m"

info()    { echo -e "${GREEN}[INFO]${RESET}  $*"; }
warn()    { echo -e "${YELLOW}[WARN]${RESET}  $*"; }
error()   { echo -e "${RED}[ERROR]${RESET} $*" >&2; }
heading() { echo -e "\n${BOLD}==> $*${RESET}"; }

# ---------------------------------------------------------------------------
# Root check
# ---------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    error "This script must be run as root."
    echo "  Try: sudo bash install.sh"
    exit 1
fi

# ---------------------------------------------------------------------------
# Uninstall path
# ---------------------------------------------------------------------------
if [[ "${1:-}" == "--uninstall" ]]; then
    heading "Uninstalling usbmon-elastic"

    systemctl disable --now usbtop-collect.service 2>/dev/null || true

    rm -f "${SYSTEMD_DIR}/usbtop-collect.service"
    systemctl daemon-reload

    rm -f "${LOGROTATE_DIR}/usbtop-metrics"
    rm -rf "${INSTALL_DIR}"

    info "Uninstall complete."
    warn "Log directory ${LOG_DIR} has been left intact — remove manually if desired."
    exit 0
fi

# ---------------------------------------------------------------------------
# Detect script location
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ---------------------------------------------------------------------------
# Step 1 — System dependencies
# ---------------------------------------------------------------------------
heading "Checking system dependencies"

apt-get update -qq

MISSING=()
for pkg in libpcap-dev build-essential; do
    if ! dpkg -l "$pkg" &>/dev/null; then
        MISSING+=("$pkg")
    fi
done

if [[ ${#MISSING[@]} -gt 0 ]]; then
    info "Installing: ${MISSING[*]}"
    apt-get install -y -qq "${MISSING[@]}"
else
    info "System dependencies already installed"
fi

# ---------------------------------------------------------------------------
# Step 2 — Rust toolchain
# ---------------------------------------------------------------------------
heading "Checking Rust toolchain"

# Prefer a system cargo; fall back to the rustup-managed one.
CARGO_BIN=""
if command -v cargo &>/dev/null; then
    CARGO_BIN="$(command -v cargo)"
    info "Found cargo at ${CARGO_BIN} ($(cargo --version))"
elif [[ -f "${HOME}/.cargo/bin/cargo" ]]; then
    CARGO_BIN="${HOME}/.cargo/bin/cargo"
    info "Found cargo via rustup at ${CARGO_BIN}"
else
    info "Rust not found — installing via rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
    # shellcheck disable=SC1091
    source "${HOME}/.cargo/env"
    CARGO_BIN="${HOME}/.cargo/bin/cargo"
    info "Rust installed ($(${CARGO_BIN} --version))"
fi

# ---------------------------------------------------------------------------
# Step 3 — usbmon kernel module
# ---------------------------------------------------------------------------
heading "Configuring usbmon kernel module"

if ! lsmod | grep -q usbmon; then
    info "Loading usbmon"
    modprobe usbmon
else
    info "usbmon already loaded"
fi

MODULES_CONF="/etc/modules-load.d/usbmon.conf"
if [[ ! -f "$MODULES_CONF" ]]; then
    echo "usbmon" > "$MODULES_CONF"
    info "Created ${MODULES_CONF} (loads usbmon on boot)"
else
    info "${MODULES_CONF} already exists"
fi

# ---------------------------------------------------------------------------
# Step 4 — Build the collector
# ---------------------------------------------------------------------------
heading "Building usbmon-collector (release)"

CRATE_DIR="${SCRIPT_DIR}/collector-rs"
if [[ ! -f "${CRATE_DIR}/Cargo.toml" ]]; then
    error "Cannot find ${CRATE_DIR}/Cargo.toml — is the repo complete?"
    exit 1
fi

"${CARGO_BIN}" build --manifest-path "${CRATE_DIR}/Cargo.toml" --release
info "Build succeeded"

# ---------------------------------------------------------------------------
# Step 5 — Install binary
# ---------------------------------------------------------------------------
heading "Installing collector to ${INSTALL_DIR}/bin"

mkdir -p "${INSTALL_DIR}/bin"
install -m 755 \
    "${CRATE_DIR}/target/release/usbmon-collector" \
    "${INSTALL_DIR}/bin/usbmon-collector"
info "Binary installed at ${INSTALL_DIR}/bin/usbmon-collector"

# Keep the debug tool around — it's still useful for diagnosing usbmon output.
if [[ -f "${SCRIPT_DIR}/tools/usbtop_debug.py" ]]; then
    mkdir -p "${INSTALL_DIR}/tools"
    install -m 755 "${SCRIPT_DIR}/tools/usbtop_debug.py" \
                   "${INSTALL_DIR}/tools/usbtop_debug.py"
    info "Debug tool installed at ${INSTALL_DIR}/tools/usbtop_debug.py"
fi

# ---------------------------------------------------------------------------
# Step 6 — Log directory
# ---------------------------------------------------------------------------
heading "Creating log directory ${LOG_DIR}"

mkdir -p "${LOG_DIR}"
chmod 755 "${LOG_DIR}"
info "Log directory ready"

# ---------------------------------------------------------------------------
# Step 7 — systemd unit (daemon, no timer required)
# ---------------------------------------------------------------------------
heading "Installing systemd service"

install -m 644 \
    "${SCRIPT_DIR}/systemd/usbtop-collect.service" \
    "${SYSTEMD_DIR}/usbtop-collect.service"

# Remove the old timer unit if it's still installed from a previous version.
if systemctl is-enabled usbtop-collect.timer &>/dev/null; then
    info "Disabling legacy timer unit"
    systemctl disable --now usbtop-collect.timer 2>/dev/null || true
    rm -f "${SYSTEMD_DIR}/usbtop-collect.timer"
fi

systemctl daemon-reload
systemctl enable --now usbtop-collect.service
info "Service enabled and started"

# ---------------------------------------------------------------------------
# Step 8 — logrotate
# ---------------------------------------------------------------------------
heading "Installing logrotate config"

install -m 644 \
    "${SCRIPT_DIR}/elastic/logrotate-usbtop" \
    "${LOGROTATE_DIR}/usbtop-metrics"
info "Logrotate config installed"

# ---------------------------------------------------------------------------
# Step 9 — Smoke test
# ---------------------------------------------------------------------------
heading "Smoke test — waiting 5s for first output"

sleep 5
if [[ -f "${LOG_DIR}/usbtop.ndjson" ]] && [[ -s "${LOG_DIR}/usbtop.ndjson" ]]; then
    info "Output file has content — looks good"
    echo ""
    echo "  Last written line:"
    tail -n 1 "${LOG_DIR}/usbtop.ndjson" | python3 -m json.tool 2>/dev/null || \
        tail -n 1 "${LOG_DIR}/usbtop.ndjson"
    echo ""
else
    warn "No output yet (the daemon writes every USBMON_INTERVAL_SEC seconds, default 60)."
    warn "Check service status: systemctl status usbtop-collect.service"
    warn "Live logs:            journalctl -fu usbtop-collect.service"
fi

# ---------------------------------------------------------------------------
# Step 10 — Elastic Agent reminder
# ---------------------------------------------------------------------------
heading "Next step: configure Elastic Agent"
echo ""
echo "  Add the custom_logs input to your agent policy."
echo "  A ready-to-use snippet is at:"
echo ""
echo "    ${SCRIPT_DIR}/elastic/elastic-agent-input.yml"
echo ""
echo "  Fleet UI quick setup:"
echo "    1. Fleet → Agent Policies → your policy"
echo "    2. Add integration → Custom Logs"
echo "    3. Log file path: ${LOG_DIR}/usbtop.ndjson"
echo "    4. Enable 'Parse JSON messages', leave target field blank"
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
heading "Installation complete"
echo ""
echo "  Binary:       ${INSTALL_DIR}/bin/usbmon-collector"
echo "  Log output:   ${LOG_DIR}/usbtop.ndjson"
echo "  Service:      systemctl status usbtop-collect.service"
echo "  Live logs:    journalctl -fu usbtop-collect.service"
if [[ -f "${INSTALL_DIR}/tools/usbtop_debug.py" ]]; then
echo "  Debug tool:   sudo python3 ${INSTALL_DIR}/tools/usbtop_debug.py"
fi
echo ""
