# Sala DevContainer CLI API (`dc-*`)

Canonical, IDE-agnostic CLI surface for Tala DevContainer orchestration.
All commands work from VS Code, Zed, plain terminal, Jagora, scripts, and CI.

## Common Flags

All `dc-*` commands accept these flags:

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--workspace <PATH>` | `-w` | Current working directory | Workspace root path |
| `--json` | `-j` | off | Machine-readable JSON to stdout (no logs) |
| `--verbose` | `-v` | off | Show extra detail |
| `--help` | `-h` | — | Show help |

---

## Commands

### `dc-smoke`

End-to-end smoke test: build container, verify terminal, test LSP.

**Extra flags:**
| Flag | Default | Description |
|------|---------|-------------|
| `--language <rust\|python>` | `rust` | Language LSP to test |
| `--max-wait-seconds <N>` | `600` | Build timeout in seconds |

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | All stages passed |
| 1 | Daemon connection / health failed |
| 2 | Preflight failed (Docker issue) |
| 3 | DevContainer config not found |
| 4 | DevContainer build/connect failed |
| 5 | Terminal stage failed |
| 6 | LSP stage failed |

**JSON schema (`DcSmokeJson`):**
```json
{
  "workspace": "/workspace/project",
  "overall": "pass",
  "steps": [
    { "name": "devcontainer", "status": "ok", "message": "container abc123def456" },
    { "name": "terminal", "status": "ok", "message": "echo SMOKE_OK received" },
    { "name": "lsp", "status": "ok", "message": "initialize/shutdown ok" }
  ]
}
```

Step `status` values: `"ok"`, `"fail"`, `"skip"`.

---

### `dc-status`

Show running DevContainers discovered via Docker (direct Docker view, not through Tala).

**JSON schema:** Array of `DcStatusJson` objects (via `sala_hud::format_status_json`):
```json
[
  {
    "workspace_root": "/workspace/project",
    "project_name": "project",
    "container_id": "abc123def456789...",
    "phase": "Running",
    "capabilities": {
      "terminal_ok": true,
      "lsp_rust_ok": true,
      "lsp_python_ok": false
    }
  }
]
```

---

### `dc-doctor`

Comprehensive diagnostic: Tala daemon, Docker, config, container status, capabilities.
Interpreted view through Tala + HUD + Docker.

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | HEALTHY or DEGRADED |
| 1 | BROKEN |

**JSON schema (`DcDoctorJson`):**
```json
{
  "workspace": "/workspace/project",
  "overall_status": "HEALTHY",
  "checks": [
    {
      "name": "Tala daemon",
      "status": "OK",
      "message": "Tala daemon reachable at /tmp/tala.sock (version 0.1.0)",
      "suggestion": null,
      "details": { "socket": "/tmp/tala.sock", "version": "0.1.0", "healthy": true }
    }
  ]
}
```

Check `status` values: `"OK"`, `"WARN"`, `"ERROR"`, `"SKIP"`.
`overall_status` values: `"HEALTHY"`, `"DEGRADED"`, `"BROKEN"`.

---

### `dc-build`

Build or rebuild a DevContainer via Tala's `BuildContainer` RPC.

**Extra flags:**
| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--force` | `-f` | off | Force rebuild (remove existing container first) |
| `--max-wait-seconds <N>` | — | `600` | Build timeout in seconds |

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Build succeeded |
| 1 | Build failed or error |

**JSON schema (`DcBuildJson`):**
```json
{
  "workspace": "/workspace/project",
  "action": "build",
  "status": "success",
  "duration_ms": 12345,
  "container_id": "abc123def456789..."
}
```

On failure:
```json
{
  "workspace": "/workspace/project",
  "action": "build",
  "status": "failed",
  "duration_ms": 500,
  "error_message": "Docker not running"
}
```

`action` values: `"build"`, `"rebuild"`.
`status` values: `"success"`, `"failed"`.

---

### `dc-connect`

Fast-path connection to an existing DevContainer via Tala's `ConnectContainer` RPC.

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Connected successfully |
| 1 | Connection failed or error |

**JSON schema (`DcConnectJson`):**
```json
{
  "workspace": "/workspace/project",
  "container_id": "abc123def456789...",
  "workspace_folder": "/workspace",
  "config_changed": false
}
```

When `config_changed` is `true`, a rebuild is recommended.

---

## Makefile Targets

All targets accept `DC_WORKSPACE` override:

```sh
DC_WORKSPACE=/some/path make dc-doctor
```

| Target | Description |
|--------|-------------|
| `make dc-smoke` | Smoke test (Rust LSP, default workspace) |
| `make dc-smoke-python` | Smoke test (Python LSP) |
| `make dc-status` | Show all running DevContainers |
| `make dc-status-json` | JSON status output |
| `make dc-doctor` | Full diagnostic |
| `make dc-doctor-json` | JSON diagnostic output |
| `make dc-build` | Build DevContainer |
| `make dc-connect` | Fast-path connect |
| `make build-all` | Build all binaries |

**Variables:**

| Variable | Default | Description |
|----------|---------|-------------|
| `DC_WORKSPACE` | `/workspace/_JAGORA/playground-rust` | Workspace path |
| `TIMEOUT` | `600` | Build timeout (seconds) |
| `LANGUAGE` | `rust` | LSP language for smoke test |
| `TALA_SOCK` | `/tmp/tala.sock` | Tala daemon socket path |

---

## Architecture Notes

- **dc-status** queries Docker directly (`docker ps` + `docker inspect`) — "what Docker sees"
- **dc-doctor** queries through Tala (gRPC) + Docker — "interpreted Tala + HUD + Docker view"
- **dc-build**, **dc-connect**, **dc-smoke** all go through Tala's gRPC API
- All commands are thin wrappers — no business logic re-implementation
