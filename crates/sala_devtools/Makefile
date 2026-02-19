# Sala DevTools — dc-* CLI targets
#
# Usage:
#   make dc-smoke                             # Rust LSP against playground-rust
#   make dc-smoke-python                      # Python LSP against playground-python
#   make dc-smoke DC_WORKSPACE=/my/proj       # custom workspace
#   make dc-smoke TIMEOUT=120                 # override build timeout
#   make dc-status                            # show all running DevContainers
#   make dc-status-json                       # JSON output
#   make dc-doctor                            # full diagnostic with next-action hints
#   make dc-doctor-json                       # JSON diagnostic output
#   make dc-build                             # build DevContainer for workspace
#   make dc-connect                           # fast-path connect to existing container
#
# Override workspace for any target:
#   DC_WORKSPACE=/some/path make dc-doctor

DC_WORKSPACE ?= /workspace/_JAGORA/playground-rust
TIMEOUT      ?= 600
LANGUAGE     ?= rust
DAEMON_SOCK  ?= /tmp/tais-devcontainerd.sock

.PHONY: dc-smoke dc-smoke-python dc-status dc-status-json \
        dc-doctor dc-doctor-json dc-build dc-connect \
        check-daemon build-all

# --- Prerequisites ---

check-daemon:
	@if [ ! -S "$(DAEMON_SOCK)" ]; then \
		echo ""; \
		echo "ERROR: tais-devcontainerd is not running (no socket at $(DAEMON_SOCK))."; \
		echo ""; \
		echo "Start it first:"; \
		echo "  cd /workspace/_TAIS_APPS_FOUNDATION/tais-devcontainerd && cargo run -p tais-devcontainerd-server --bin tais-devcontainerd"; \
		echo ""; \
		exit 1; \
	fi

build-all:
	@echo "--- Building all sala_devtools binaries ---"
	cargo build -p sala_devtools

# --- dc-smoke ---

dc-smoke: check-daemon
	@echo ""
	@echo "--- dc-smoke ($(LANGUAGE)) ---"
	@echo "    workspace: $(DC_WORKSPACE)"
	@echo "    language:  $(LANGUAGE)"
	@echo "    timeout:   $(TIMEOUT)s"
	@echo ""
	cargo run -p sala_devtools --bin dc-smoke -- \
		--workspace $(DC_WORKSPACE) \
		--language $(LANGUAGE) \
		--max-wait-seconds $(TIMEOUT)

dc-smoke-python: check-daemon
	@echo ""
	@echo "--- dc-smoke (python) ---"
	@echo "    workspace: /workspace/_JAGORA/playground-python"
	@echo "    timeout:   $(TIMEOUT)s"
	@echo ""
	cargo run -p sala_devtools --bin dc-smoke -- \
		--workspace /workspace/_JAGORA/playground-python \
		--language python \
		--max-wait-seconds $(TIMEOUT)

# --- dc-status ---

dc-status:
	cargo run -p sala_devtools --bin dc-status -- \
		$(if $(filter-out /workspace/_JAGORA/playground-rust,$(DC_WORKSPACE)),--workspace $(DC_WORKSPACE),)

dc-status-json:
	cargo run -p sala_devtools --bin dc-status -- --json \
		$(if $(filter-out /workspace/_JAGORA/playground-rust,$(DC_WORKSPACE)),--workspace $(DC_WORKSPACE),)

# --- dc-doctor ---

dc-doctor:
	cargo run -p sala_devtools --bin dc-doctor -- \
		--workspace $(DC_WORKSPACE)

dc-doctor-json:
	cargo run -p sala_devtools --bin dc-doctor -- --json \
		--workspace $(DC_WORKSPACE)

# --- dc-build ---

dc-build: check-daemon
	cargo run -p sala_devtools --bin dc-build -- \
		--workspace $(DC_WORKSPACE)

# --- dc-connect ---

dc-connect: check-daemon
	cargo run -p sala_devtools --bin dc-connect -- \
		--workspace $(DC_WORKSPACE)
