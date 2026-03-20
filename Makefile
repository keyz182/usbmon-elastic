# =============================================================================
# usbmon-elastic — build and install
#
#   make              — build the release binary
#   sudo make install — install binary, systemd unit, logrotate config
#   sudo make uninstall
#   make clean
# =============================================================================

BINARY        := usbmon-collector
CRATE_DIR     := collector-rs
RELEASE_BIN   := $(CRATE_DIR)/target/release/$(BINARY)

INSTALL_DIR   := /opt/usbtop-elastic
BIN_DIR       := $(INSTALL_DIR)/bin
TOOLS_DIR     := $(INSTALL_DIR)/tools
LOG_DIR       := /var/log/usbtop-metrics
SYSTEMD_DIR   := /etc/systemd/system
LOGROTATE_DIR := /etc/logrotate.d
MODULES_CONF  := /etc/modules-load.d/usbmon.conf

# Detect cargo — prefer rustup-managed binary if system cargo is absent
CARGO := $(shell command -v cargo 2>/dev/null || echo $(HOME)/.cargo/bin/cargo)

# ---------------------------------------------------------------------------

.PHONY: all build install uninstall clean help

all: build

## build: compile the release binary (runs as any user)
build: $(RELEASE_BIN)

$(RELEASE_BIN): $(CRATE_DIR)/Cargo.toml $(CRATE_DIR)/src/main.rs
	@echo "==> Building $(BINARY)"
	$(CARGO) build --manifest-path $(CRATE_DIR)/Cargo.toml --release

## install: install binary + config + systemd unit (requires root)
install: _require-root _require-libpcap $(RELEASE_BIN)
	@echo "==> Loading usbmon kernel module"
	modprobe usbmon
	@[ -f "$(MODULES_CONF)" ] || (echo "usbmon" > $(MODULES_CONF) && echo "    created $(MODULES_CONF)")

	@echo "==> Installing binary"
	install -Dm755 $(RELEASE_BIN) $(BIN_DIR)/$(BINARY)

	@echo "==> Installing debug tools"
	install -Dm755 tools/usbtop_debug.py   $(TOOLS_DIR)/usbtop_debug.py
	install -Dm755 tools/usbtop_collect.py $(TOOLS_DIR)/usbtop_collect.py

	@echo "==> Creating log directory"
	install -d -m755 $(LOG_DIR)

	@echo "==> Installing systemd service"
	install -Dm644 systemd/usbtop-collect.service $(SYSTEMD_DIR)/usbtop-collect.service
	@# Remove legacy timer unit if still present from an old install
	-systemctl disable --now usbtop-collect.timer 2>/dev/null
	-rm -f $(SYSTEMD_DIR)/usbtop-collect.timer
	systemctl daemon-reload
	systemctl enable --now usbtop-collect.service

	@echo "==> Installing logrotate config"
	install -Dm644 elastic/logrotate-usbtop $(LOGROTATE_DIR)/usbtop-metrics

	@echo ""
	@echo "Installation complete."
	@echo "  Status:  systemctl status usbtop-collect.service"
	@echo "  Logs:    journalctl -fu usbtop-collect.service"
	@echo "  Output:  tail -f $(LOG_DIR)/usbtop.ndjson"
	@echo ""

## uninstall: stop and remove everything installed by 'make install'
uninstall: _require-root
	@echo "==> Uninstalling"
	-systemctl disable --now usbtop-collect.service 2>/dev/null
	rm -f  $(SYSTEMD_DIR)/usbtop-collect.service
	rm -f  $(SYSTEMD_DIR)/usbtop-collect.timer
	systemctl daemon-reload
	rm -f  $(LOGROTATE_DIR)/usbtop-metrics
	rm -rf $(INSTALL_DIR)
	@echo "Done. Log directory $(LOG_DIR) has been left intact."

## clean: remove cargo build artefacts
clean:
	$(CARGO) clean --manifest-path $(CRATE_DIR)/Cargo.toml

## help: show this message
help:
	@grep -E '^## ' Makefile | sed 's/^## /  /'

# ---------------------------------------------------------------------------
# Internal guards

_require-root:
	@[ "$(shell id -u)" = "0" ] || \
		(echo "error: this target must be run as root — use 'sudo make install'" && false)

_require-libpcap:
	@dpkg -l libpcap-dev 2>/dev/null | grep -q '^ii' || \
		(echo "error: libpcap-dev not installed — run: apt-get install -y libpcap-dev" && false)
