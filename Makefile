.PHONY: run-server build-client deploy-client build-pinger deploy-pinger

OS := $(shell uname -s)

ifeq ($(OS),Darwin)
	# macOS typically uses en0 for Wi-Fi
	WLAN_IP := $(shell ipconfig getifaddr en0 2>/dev/null)
else
	# Linux: find interface starting with 'wl' and extract its IPv4 address
	WLAN_IP := $(shell ip -o -4 addr list 2>/dev/null | awk '$$2 ~ /^wl/ {print $$4}' | cut -d/ -f1 | head -n1)
endif

# Raspberry Pi deployment configuration (override via command line: make deploy-client PI_USER=myuser PI_HOST=192.168.1.10)
PI_USER ?= anapaya
PI_HOST ?= 10.175.8.48
PI_DEST ?= /home/anapaya

run-server:
	@if [ -z "$(WLAN_IP)" ]; then \
		echo "No WLAN IP address found. Please ensure your Wi-Fi is connected."; \
		exit 1; \
	fi
	@echo "Starting server on WLAN IP: $(WLAN_IP)"
	cargo run -p pq-meter-server -- --bind-ip $(WLAN_IP)

build-client:
	@echo "Cross-compiling client for Raspberry Pi (aarch64)..."
	cargo cross build --release -p pq-meter-client --target aarch64-unknown-linux-gnu

deploy-client: build-client
	@echo "Copying client binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	scp target/aarch64-unknown-linux-gnu/release/pq-meter-client $(PI_USER)@$(PI_HOST):$(PI_DEST)

build-pinger:
	@echo "Cross-compiling pinger for Raspberry Pi (aarch64)..."
	cargo cross build --release -p umg605-modbus-client --bin pinger --target aarch64-unknown-linux-gnu

deploy-pinger: build-pinger
	@echo "Copying pinger binary to $(PI_USER)@$(PI_HOST):$(PI_DEST) ..."
	scp target/aarch64-unknown-linux-gnu/release/pinger $(PI_USER)@$(PI_HOST):$(PI_DEST)