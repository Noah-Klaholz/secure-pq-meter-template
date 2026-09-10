.PHONY: run-server

WLAN_IP := $(shell ip -o -4 addr list | awk '$$2 ~ /^wl/ {print $$4}' | cut -d/ -f1 | head -n1)


run-server:
	@if [ -z "$(WLAN_IP)" ]; then \
		echo "No WLAN IP address found."; \
		exit 1; \
	fi
	@echo "Starting server on WLAN IP: $(WLAN_IP)"
	cargo run -p pq-meter-server -- --bind-ip $(WLAN_IP)
