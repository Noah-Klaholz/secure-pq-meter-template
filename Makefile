.PHONY: hooks lint test run-server stop-server restart-server run-client client stop-client kill-client build-client-local build-client deploy-client build-pinger deploy-pinger

# Installs the git hooks from .pre-commit-config.yaml. Needs the `pre-commit` tool:
# `pipx install pre-commit`, or your distribution's package.
hooks:
	@command -v pre-commit >/dev/null 2>&1 || { \
		echo "pre-commit is not installed. See the README section 'Git hooks and CI'."; \
		exit 1; \
	}
	pre-commit install
	@echo "Hooks installed. Run them over the whole tree with: pre-commit run --all-files"

# The checks CI runs, minus the test suite.
lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace

OS := $(shell uname -s)

ifeq ($(OS),Darwin)
	# macOS typically uses en0 for Wi-Fi
	WLAN_IP := $(shell ipconfig getifaddr en0 2>/dev/null)
else
	# Linux: find interface starting with 'wl' and extract its IPv4 address
	WLAN_IP := $(shell ip -o -4 addr list 2>/dev/null | awk '$$2 ~ /^wl/ {print $$4}' | cut -d/ -f1 | head -n1)
endif

# Default SERVER to current device IP; can be passed as an environment variable or make override
SERVER ?= $(WLAN_IP)

PI_USER ?= anapaya
PI_HOST ?= 10.175.8.48
PI_DEST ?= /home/anapaya

# This is not a good idea but I don't wanna do it with ssh key right now XD
PI_PASS ?= anapaya

run-server:
	@if [ -z "$(WLAN_IP)" ]; then \
		echo "No WLAN IP address found. Please ensure your Wi-Fi is connected."; \
		exit 1; \
	fi
	@echo "Starting server on WLAN IP: $(WLAN_IP)"
	cargo run -p pq-meter-server -- --bind-ip $(WLAN_IP)

stop-server:
	@echo "Stopping pq-meter-server..."
	@pkill -INT -x pq-meter-server 2>/dev/null || true
	@echo "Stopped."

restart-server: stop-server
	@sleep 1
	@$(MAKE) run-server

run-client:
	@if [ -z "$(SERVER)" ]; then \
		echo "No SERVER IP address found. Please specify SERVER=<ip> or ensure Wi-Fi is connected."; \
		exit 1; \
	fi
	@echo "Running client on $(PI_HOST) connecting to server: $(SERVER)"
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' ssh -t $(PI_USER)@$(PI_HOST) "cd $(PI_DEST) && ./pq-meter-client --server $(SERVER)"; \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		ssh -t $(PI_USER)@$(PI_HOST) "cd $(PI_DEST) && ./pq-meter-client --server $(SERVER)"; \
	fi

client: run-client

kill-client:
	@echo "Stopping pq-meter-client on $(PI_HOST)..."
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' ssh $(PI_USER)@$(PI_HOST) "pkill -INT -f pq-meter-client || killall -q -INT pq-meter-client || true"; \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		ssh $(PI_USER)@$(PI_HOST) "pkill -INT -f pq-meter-client || killall -q -INT pq-meter-client || true"; \
	fi
	@echo "Stopped."

build-client-local:
	@echo "Building client for the local machine..."
	cargo build -p pq-meter-client

# Cross compilation, in order of preference:
#   1. `cross` (cross-rs) - runs the build in a container, needs Docker or Podman.
#   2. `cargo cross` (the `cargo-cross` crate) - downloads its own toolchain, no
#      container runtime. This is the one that works out of the box on macOS.
#   3. the host toolchain - needs an aarch64 GCC and binutils (Debian/Ubuntu:
#      gcc-aarch64-linux-gnu, Arch: aarch64-linux-gnu-gcc) plus the Rust std for
#      the target. cc-rs looks for `aarch64-linux-gnu-ar`, which some distributions
#      do not ship, so point it at llvm-ar when that is the case.
AARCH64_ENV := \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
	CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
	CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
	AR_aarch64_unknown_linux_gnu=$(shell command -v aarch64-linux-gnu-ar 2>/dev/null || command -v llvm-ar) \
	RANLIB_aarch64_unknown_linux_gnu=$(shell command -v aarch64-linux-gnu-ranlib 2>/dev/null || command -v llvm-ranlib)

build-client:
	@echo "Cross-compiling client for Raspberry Pi (aarch64)..."
	@if command -v cross >/dev/null 2>&1; then \
		cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu; \
	elif cargo cross version >/dev/null 2>&1; then \
		cargo cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu; \
	else \
		echo "neither 'cross' nor 'cargo cross' found; building with the host toolchain."; \
		$(AARCH64_ENV) cargo build --release -p pq-meter-client --target aarch64-unknown-linux-gnu; \
	fi

# Copies next to the old binary and moves it into place: writing straight over the file
# fails while a client is still running from it, and the move leaves that process alone.
deploy-client: build-client
	@echo "Copying client binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' scp target/aarch64-unknown-linux-gnu/release/pq-meter-client $(PI_USER)@$(PI_HOST):$(PI_DEST)/pq-meter-client.new && \
		sshpass -p '$(PI_PASS)' ssh $(PI_USER)@$(PI_HOST) "chmod +x $(PI_DEST)/pq-meter-client.new && mv $(PI_DEST)/pq-meter-client.new $(PI_DEST)/pq-meter-client"; \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		scp target/aarch64-unknown-linux-gnu/release/pq-meter-client $(PI_USER)@$(PI_HOST):$(PI_DEST)/pq-meter-client.new && \
		ssh $(PI_USER)@$(PI_HOST) "chmod +x $(PI_DEST)/pq-meter-client.new && mv $(PI_DEST)/pq-meter-client.new $(PI_DEST)/pq-meter-client"; \
	fi
	@echo "Deployed. Restart the client to pick it up: make run-client"

build-pinger:
	@echo "Cross-compiling pinger for Raspberry Pi (aarch64)..."
	@if command -v cross >/dev/null 2>&1; then \
		cross build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu; \
	elif cargo cross version >/dev/null 2>&1; then \
		cargo cross build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu; \
	else \
		echo "neither 'cross' nor 'cargo cross' found; building with the host toolchain."; \
		$(AARCH64_ENV) cargo build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu; \
	fi

deploy-pinger: build-pinger
	@echo "Copying pinger binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' scp target/aarch64-unknown-linux-gnu/release/pinger $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		scp target/aarch64-unknown-linux-gnu/release/pinger $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	fi
