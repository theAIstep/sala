# Sala DevTools — local development targets
#
# Usage:
#   make smoke                    # run smoke test against playground-rust
#   make smoke WORKSPACE=/my/proj # run against a custom workspace
#   make smoke TIMEOUT=120        # override build timeout

WORKSPACE ?= /workspace/_JAGORA/playground-rust
TIMEOUT   ?= 600
TALA_SOCK ?= /tmp/tala.sock

.PHONY: smoke smoke-build check-tala

smoke-build:
	@echo "--- Building smoke test binary ---"
	cargo build -p sala_devtools --bin devcontainer_smoke_test

check-tala:
	@if [ ! -S "$(TALA_SOCK)" ]; then \
		echo ""; \
		echo "ERROR: tala daemon is not running (no socket at $(TALA_SOCK))."; \
		echo ""; \
		echo "Start it first:"; \
		echo "  cd /workspace/_JAGORA/tala && cargo run -p tala-server --bin tala"; \
		echo ""; \
		exit 1; \
	fi

smoke: smoke-build check-tala
	@echo ""
	@echo "--- Running DevContainer Smoke Test ---"
	@echo "    workspace: $(WORKSPACE)"
	@echo "    timeout:   $(TIMEOUT)s"
	@echo ""
	cargo run -p sala_devtools --bin devcontainer_smoke_test -- \
		--max-wait-seconds $(TIMEOUT) \
		$(WORKSPACE)
