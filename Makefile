.PHONY: test run-server stop-server restart-server run-client client stop-client kill-client build-client-local build-client deploy-client build-pinger deploy-pinger

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

build-client:
	@echo "Cross-compiling client for Raspberry Pi (aarch64)..."
	cargo cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu

deploy-client: build-client
	@echo "Copying client binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' scp target/aarch64-unknown-linux-gnu/release/pq-meter-client $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		scp target/aarch64-unknown-linux-gnu/release/pq-meter-client $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	fi
	@echo "should be done"

build-pinger:
	@echo "Cross-compiling pinger for Raspberry Pi (aarch64)..."
	cargo cross build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu

deploy-pinger: build-pinger
	@echo "Copying pinger binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	@if [ -n "$(PI_PASS)" ] && command -v sshpass >/dev/null 2>&1; then \
		sshpass -p '$(PI_PASS)' scp target/aarch64-unknown-linux-gnu/release/pinger $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	else \
		if [ -n "$(PI_PASS)" ]; then echo "Warning: PI_PASS is set but 'sshpass' is not installed. Asking interactively..."; fi; \
		scp target/aarch64-unknown-linux-gnu/release/pinger $(PI_USER)@$(PI_HOST):$(PI_DEST); \
	fi
