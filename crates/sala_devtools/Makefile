# Sala DevTools — local development targets
#
# Usage:
#   make smoke                          # Rust LSP against playground-rust
#   make smoke-python                   # Python LSP against playground-python
#   make smoke WORKSPACE=/my/proj       # custom workspace
#   make smoke TIMEOUT=120              # override build timeout
#   make smoke LANGUAGE=python          # override language

WORKSPACE  ?= /workspace/_JAGORA/playground-rust
TIMEOUT    ?= 600
LANGUAGE   ?= rust
TALA_SOCK  ?= /tmp/tala.sock

.PHONY: smoke smoke-python smoke-build check-tala

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
	@echo "--- Running DevContainer Smoke Test ($(LANGUAGE)) ---"
	@echo "    workspace: $(WORKSPACE)"
	@echo "    language:  $(LANGUAGE)"
	@echo "    timeout:   $(TIMEOUT)s"
	@echo ""
	cargo run -p sala_devtools --bin devcontainer_smoke_test -- \
		--language $(LANGUAGE) \
		--max-wait-seconds $(TIMEOUT) \
		$(WORKSPACE)

smoke-python: smoke-build check-tala
	@echo ""
	@echo "--- Running DevContainer Smoke Test (python) ---"
	@echo "    workspace: /workspace/_JAGORA/playground-python"
	@echo "    timeout:   $(TIMEOUT)s"
	@echo ""
	cargo run -p sala_devtools --bin devcontainer_smoke_test -- \
		--language python \
		--max-wait-seconds $(TIMEOUT) \
		/workspace/_JAGORA/playground-python
