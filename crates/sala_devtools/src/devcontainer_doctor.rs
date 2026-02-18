// dc-doctor — Diagnoses why a workspace's DevContainer is not "green".
//
// Runs a sequence of checks against the Tala daemon, Docker, and the
// filesystem, then prints a human-readable report (or JSON) with
// per-check status and concrete next-action hints.
//
// # Usage
//
// ```sh
// dc-doctor --workspace /workspace/_JAGORA/sala
// dc-doctor --json
// dc-doctor --verbose
// ```
//
// # Exit codes
//
// | Code | Meaning    |
// |------|------------|
// | 0    | HEALTHY or DEGRADED |
// | 1    | BROKEN     |

use sala_devtools::ipc::{self, devcontainer, DevContainerServiceClient};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct CliArgs {
    workspace: PathBuf,
    json: bool,
    verbose: bool,
}

fn parse_args() -> CliArgs {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let common = sala_devtools::cli::parse_common_args(&mut args);

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: dc-doctor [OPTIONS]");
        println!();
        println!("Diagnoses DevContainer issues for a workspace and suggests fixes.");
        println!();
        println!("Options:");
        println!("  -w, --workspace <PATH>  Workspace to diagnose (default: cwd)");
        println!("  -j, --json              Machine-readable JSON output");
        println!("  -v, --verbose           Show detailed internal check info");
        println!("  -h, --help              Show this help");
        std::process::exit(0);
    }

    // Check for unexpected arguments
    for arg in &args {
        if arg.starts_with('-') && arg != "--help" && arg != "-h" {
            eprintln!("error: unexpected argument: {arg}");
            std::process::exit(1);
        }
    }

    CliArgs {
        workspace: common.workspace,
        json: common.json,
        verbose: common.verbose,
    }
}

// ---------------------------------------------------------------------------
// Check model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Healthy,
    Degraded,
    Broken,
}

// ---------------------------------------------------------------------------
// Interpretation functions (pure, testable without I/O)
// ---------------------------------------------------------------------------

pub fn interpret_tala_health(
    result: Result<(String, bool), String>,
    socket_path: &str,
) -> CheckResult {
    match result {
        Ok((version, true)) => CheckResult {
            name: "Tala daemon".to_string(),
            status: CheckStatus::Ok,
            message: format!(
                "Tala daemon reachable at {} (version {})",
                socket_path, version
            ),
            suggestion: None,
            details: Some(serde_json::json!({
                "socket": socket_path,
                "version": version,
                "healthy": true,
            })),
        },
        Ok((version, false)) => CheckResult {
            name: "Tala daemon".to_string(),
            status: CheckStatus::Error,
            message: format!(
                "Tala daemon at {} reports unhealthy (version {})",
                socket_path, version
            ),
            suggestion: Some(
                "Restart the tala daemon: cd /workspace/_JAGORA/tala && cargo run -p tala-server --bin tala"
                    .to_string(),
            ),
            details: Some(serde_json::json!({
                "socket": socket_path,
                "version": version,
                "healthy": false,
            })),
        },
        Err(error) => CheckResult {
            name: "Tala daemon".to_string(),
            status: CheckStatus::Error,
            message: format!("Tala daemon not reachable at {}", socket_path),
            suggestion: Some(format!(
                "Start tala in a separate terminal:\n  cd /workspace/_JAGORA/tala && cargo run -p tala-server --bin tala\n\nError: {}",
                error
            )),
            details: Some(serde_json::json!({
                "socket": socket_path,
                "error": error,
            })),
        },
    }
}

pub fn interpret_devcontainer_config(
    workspace: &Path,
    config_path: Option<PathBuf>,
) -> CheckResult {
    match config_path {
        Some(path) => {
            let relative = path
                .strip_prefix(workspace)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            CheckResult {
                name: "DevContainer config".to_string(),
                status: CheckStatus::Ok,
                message: format!("DevContainer config detected ({})", relative),
                suggestion: None,
                details: Some(serde_json::json!({
                    "config_path": path.display().to_string(),
                })),
            }
        }
        None => CheckResult {
            name: "DevContainer config".to_string(),
            status: CheckStatus::Warn,
            message: "No DevContainer config found for this workspace".to_string(),
            suggestion: Some(
                "Create .devcontainer/devcontainer.json manually, or use Flow 0 scaffold from Sala."
                    .to_string(),
            ),
            details: None,
        },
    }
}

pub fn interpret_docker_preflight(
    preflight: Option<Result<PrefightData, String>>,
    direct_docker_ok: Option<bool>,
) -> CheckResult {
    match preflight {
        Some(Ok(data)) => {
            if data.docker_available && data.docker_permissions_ok {
                CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Ok,
                    message: "Docker daemon reachable with correct permissions".to_string(),
                    suggestion: None,
                    details: Some(serde_json::json!({
                        "docker_available": true,
                        "permissions_ok": true,
                        "source": "tala_preflight",
                    })),
                }
            } else if data.docker_available && !data.docker_permissions_ok {
                CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Error,
                    message: "Docker daemon reachable but permissions denied".to_string(),
                    suggestion: Some(
                        "Add your user to the docker group: sudo usermod -aG docker $USER && newgrp docker"
                            .to_string(),
                    ),
                    details: Some(serde_json::json!({
                        "docker_available": true,
                        "permissions_ok": false,
                        "error_message": data.error_message,
                    })),
                }
            } else {
                CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Error,
                    message: "Docker daemon not available".to_string(),
                    suggestion: Some(format!(
                        "Start Docker Desktop or the Docker service.\n{}",
                        if data.error_message.is_empty() {
                            String::new()
                        } else {
                            format!("Detail: {}", data.error_message)
                        }
                    )),
                    details: Some(serde_json::json!({
                        "docker_available": false,
                        "error_message": data.error_message,
                    })),
                }
            }
        }
        Some(Err(error)) => CheckResult {
            name: "Docker daemon".to_string(),
            status: CheckStatus::Error,
            message: "Failed to check Docker via Tala preflight".to_string(),
            suggestion: Some(format!("Error: {error}")),
            details: None,
        },
        None => {
            match direct_docker_ok {
                Some(true) => CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Ok,
                    message: "Docker daemon reachable (direct check, Tala unavailable)".to_string(),
                    suggestion: None,
                    details: Some(serde_json::json!({ "source": "direct" })),
                },
                Some(false) => CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Error,
                    message: "Docker daemon not reachable".to_string(),
                    suggestion: Some(
                        "Start Docker Desktop or the Docker service.".to_string(),
                    ),
                    details: Some(serde_json::json!({ "source": "direct" })),
                },
                None => CheckResult {
                    name: "Docker daemon".to_string(),
                    status: CheckStatus::Skip,
                    message: "Docker check skipped (Tala unavailable, direct check failed)"
                        .to_string(),
                    suggestion: None,
                    details: None,
                },
            }
        }
    }
}

pub fn interpret_workspace_status(
    status: Option<Result<StatusData, String>>,
) -> (CheckResult, Option<String>) {
    match status {
        Some(Ok(data)) => {
            if data.running {
                let short_id = if data.container_id.len() > 12 {
                    &data.container_id[..12]
                } else {
                    &data.container_id
                };
                (
                    CheckResult {
                        name: "Container status".to_string(),
                        status: CheckStatus::Ok,
                        message: format!(
                            "Container running ({}, status: {})",
                            short_id, data.status
                        ),
                        suggestion: None,
                        details: Some(serde_json::json!({
                            "container_id": data.container_id,
                            "status": data.status,
                            "running": true,
                        })),
                    },
                    Some(data.container_id),
                )
            } else if !data.container_id.is_empty() {
                (
                    CheckResult {
                        name: "Container status".to_string(),
                        status: CheckStatus::Warn,
                        message: format!(
                            "Container exists but not running (status: {})",
                            data.status
                        ),
                        suggestion: Some(
                            "Trigger reconnect from Sala, or run Flow 1 build to restart."
                                .to_string(),
                        ),
                        details: Some(serde_json::json!({
                            "container_id": data.container_id,
                            "status": data.status,
                            "running": false,
                        })),
                    },
                    None,
                )
            } else {
                (
                    CheckResult {
                        name: "Container status".to_string(),
                        status: CheckStatus::Warn,
                        message: "No DevContainer has been built for this workspace".to_string(),
                        suggestion: Some(
                            "Run Flow 1 build from Sala, or use: make dc-build"
                                .to_string(),
                        ),
                        details: None,
                    },
                    None,
                )
            }
        }
        Some(Err(error)) => (
            CheckResult {
                name: "Container status".to_string(),
                status: CheckStatus::Warn,
                message: "Could not query workspace status from Tala".to_string(),
                suggestion: Some(format!("Error: {error}")),
                details: None,
            },
            None,
        ),
        None => (
            CheckResult {
                name: "Container status".to_string(),
                status: CheckStatus::Skip,
                message: "Container status check skipped (Tala unavailable)".to_string(),
                suggestion: None,
                details: None,
            },
            None,
        ),
    }
}

pub fn interpret_capabilities(
    container_id: Option<&str>,
    caps: Option<(bool, bool, bool)>,
) -> CheckResult {
    match (container_id, caps) {
        (Some(cid), Some((terminal_ok, rust_ok, python_ok))) => {
            let all_ok = terminal_ok && rust_ok && python_ok;
            let short_id = if cid.len() > 12 { &cid[..12] } else { cid };

            let mut problems = Vec::new();
            if !terminal_ok {
                problems.push("terminal");
            }
            if !rust_ok {
                problems.push("rust-analyzer");
            }
            if !python_ok {
                problems.push("pyright");
            }

            if all_ok {
                CheckResult {
                    name: "Capabilities".to_string(),
                    status: CheckStatus::Ok,
                    message: format!(
                        "Container {} — terminal=OK, rust_lsp=OK, py_lsp=OK",
                        short_id
                    ),
                    suggestion: None,
                    details: Some(serde_json::json!({
                        "terminal_ok": terminal_ok,
                        "lsp_rust_ok": rust_ok,
                        "lsp_python_ok": python_ok,
                    })),
                }
            } else {
                let mut hints = Vec::new();
                if !terminal_ok {
                    hints.push("Check that the container allows docker exec.");
                }
                if !rust_ok {
                    hints.push(
                        "Install rust-analyzer in the container (rustup component add rust-analyzer).",
                    );
                }
                if !python_ok {
                    hints.push(
                        "Install pyright in the container (npm install -g pyright).",
                    );
                }

                CheckResult {
                    name: "Capabilities".to_string(),
                    status: CheckStatus::Warn,
                    message: format!(
                        "Container {} — terminal={}, rust_lsp={}, py_lsp={}. Missing: {}",
                        short_id,
                        if terminal_ok { "OK" } else { "FAIL" },
                        if rust_ok { "OK" } else { "FAIL" },
                        if python_ok { "OK" } else { "FAIL" },
                        problems.join(", "),
                    ),
                    suggestion: Some(hints.join("\n")),
                    details: Some(serde_json::json!({
                        "terminal_ok": terminal_ok,
                        "lsp_rust_ok": rust_ok,
                        "lsp_python_ok": python_ok,
                    })),
                }
            }
        }
        (None, _) => CheckResult {
            name: "Capabilities".to_string(),
            status: CheckStatus::Skip,
            message: "Capability checks skipped (no running container)".to_string(),
            suggestion: None,
            details: None,
        },
        (Some(_), None) => CheckResult {
            name: "Capabilities".to_string(),
            status: CheckStatus::Skip,
            message: "Capability checks could not be performed".to_string(),
            suggestion: None,
            details: None,
        },
    }
}

pub fn compute_verdict(checks: &[CheckResult]) -> Verdict {
    let has_error = checks.iter().any(|c| c.status == CheckStatus::Error);
    let has_warn = checks.iter().any(|c| c.status == CheckStatus::Warn);
    if has_error {
        Verdict::Broken
    } else if has_warn {
        Verdict::Degraded
    } else {
        Verdict::Healthy
    }
}

// ---------------------------------------------------------------------------
// Data transfer types
// ---------------------------------------------------------------------------

pub struct PrefightData {
    pub docker_available: bool,
    pub docker_permissions_ok: bool,
    pub error_message: String,
}

pub struct StatusData {
    pub container_id: String,
    pub status: String,
    pub running: bool,
}

// ---------------------------------------------------------------------------
// I/O functions
// ---------------------------------------------------------------------------

const GRPC_TIMEOUT: Duration = Duration::from_secs(5);

async fn connect_tala() -> anyhow::Result<DevContainerServiceClient<tonic::transport::Channel>> {
    ipc::connect_tala_with_timeout(ipc::TALA_CONNECT_TIMEOUT).await
}

async fn tala_health(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
) -> anyhow::Result<(String, bool)> {
    let resp = tokio::time::timeout(
        GRPC_TIMEOUT,
        client.health_check(tonic::Request::new(devcontainer::HealthCheckRequest {})),
    )
    .await
    .map_err(|_| anyhow::anyhow!("health check timed out"))??;
    let inner = resp.into_inner();
    Ok((inner.version, inner.healthy))
}

async fn tala_preflight(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
    workspace: &str,
) -> anyhow::Result<PrefightData> {
    let resp = tokio::time::timeout(
        GRPC_TIMEOUT,
        client.preflight_check(tonic::Request::new(devcontainer::PreflightRequest {
            workspace_path: workspace.to_string(),
        })),
    )
    .await
    .map_err(|_| anyhow::anyhow!("preflight check timed out"))??;
    let inner = resp.into_inner();
    Ok(PrefightData {
        docker_available: inner.docker_available,
        docker_permissions_ok: inner.docker_permissions_ok,
        error_message: inner.error_message,
    })
}

async fn tala_get_status(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
    workspace: &str,
) -> anyhow::Result<StatusData> {
    let resp = tokio::time::timeout(
        GRPC_TIMEOUT,
        client.get_status(tonic::Request::new(devcontainer::StatusRequest {
            workspace_path: workspace.to_string(),
        })),
    )
    .await
    .map_err(|_| anyhow::anyhow!("get_status timed out"))??;
    let inner = resp.into_inner();
    Ok(StatusData {
        container_id: inner.container_id,
        status: inner.status,
        running: inner.running,
    })
}

async fn direct_docker_check() -> bool {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output(),
    )
    .await;
    match output {
        Ok(Ok(out)) => out.status.success(),
        _ => false,
    }
}

async fn check_terminal(container_id: &str) -> bool {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("docker")
            .args(["exec", container_id, "echo", "SMOKE_OK"])
            .output(),
    )
    .await;
    match output {
        Ok(Ok(out)) => {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("SMOKE_OK")
        }
        _ => false,
    }
}

async fn check_binary_in_container(container_id: &str, binary: &str) -> bool {
    let check_cmd = format!("command -v {binary}");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("docker")
            .args(["exec", container_id, "sh", "-c", &check_cmd])
            .output(),
    )
    .await;
    match output {
        Ok(Ok(out)) => out.status.success(),
        _ => false,
    }
}

async fn check_capabilities_io(container_id: &str) -> (bool, bool, bool) {
    let (terminal, rust, python) = tokio::join!(
        check_terminal(container_id),
        check_binary_in_container(container_id, "rust-analyzer"),
        check_binary_in_container(container_id, "pyright-langserver"),
    );
    (terminal, rust, python)
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn status_badge(status: CheckStatus, use_color: bool) -> &'static str {
    match (status, use_color) {
        (CheckStatus::Ok, true) => "\x1b[32m[OK]\x1b[0m   ",
        (CheckStatus::Ok, false) => "[OK]   ",
        (CheckStatus::Warn, true) => "\x1b[33m[WARN]\x1b[0m ",
        (CheckStatus::Warn, false) => "[WARN] ",
        (CheckStatus::Error, true) => "\x1b[31m[ERROR]\x1b[0m",
        (CheckStatus::Error, false) => "[ERROR]",
        (CheckStatus::Skip, true) => "\x1b[2m[SKIP]\x1b[0m ",
        (CheckStatus::Skip, false) => "[SKIP] ",
    }
}

fn verdict_line(verdict: Verdict, use_color: bool) -> String {
    match (verdict, use_color) {
        (Verdict::Healthy, true) => format!(
            "\n{BOLD}Overall: {GREEN}HEALTHY{RESET}{BOLD}.{RESET}"
        ),
        (Verdict::Healthy, false) => "\nOverall: HEALTHY.".to_string(),
        (Verdict::Degraded, true) => format!(
            "\n{BOLD}Overall: {YELLOW}DEGRADED{RESET}{BOLD} \u{2014} follow the WARN checks above.{RESET}"
        ),
        (Verdict::Degraded, false) => {
            "\nOverall: DEGRADED \u{2014} follow the WARN checks above.".to_string()
        }
        (Verdict::Broken, true) => format!(
            "\n{BOLD}Overall: {RED}BROKEN{RESET}{BOLD} \u{2014} fix the ERROR checks first.{RESET}"
        ),
        (Verdict::Broken, false) => {
            "\nOverall: BROKEN \u{2014} fix the ERROR checks first.".to_string()
        }
    }
}

pub fn format_text_report(
    workspace: &Path,
    checks: &[CheckResult],
    verdict: Verdict,
    verbose: bool,
    use_color: bool,
) -> String {
    let mut out = String::new();

    if use_color {
        out.push_str(&format!(
            "\n{BOLD}dc-doctor \u{2014} {}{RESET}\n\n",
            workspace.display()
        ));
    } else {
        out.push_str(&format!(
            "\ndc-doctor \u{2014} {}\n\n",
            workspace.display()
        ));
    }

    for check in checks {
        let badge = status_badge(check.status, use_color);
        out.push_str(&format!("{} {}\n", badge, check.message));

        if let Some(ref suggestion) = check.suggestion {
            for line in suggestion.lines() {
                if use_color {
                    out.push_str(&format!("        {DIM}{line}{RESET}\n"));
                } else {
                    out.push_str(&format!("        {line}\n"));
                }
            }
        }

        if verbose {
            if let Some(ref details) = check.details {
                let json = serde_json::to_string(details).unwrap_or_default();
                if use_color {
                    out.push_str(&format!("        {DIM}details: {json}{RESET}\n"));
                } else {
                    out.push_str(&format!("        details: {json}\n"));
                }
            }
        }
    }

    out.push_str(&verdict_line(verdict, use_color));
    out.push('\n');
    out
}

pub fn format_json_report(
    workspace: &Path,
    checks: &[CheckResult],
    verdict: Verdict,
) -> String {
    let report = serde_json::json!({
        "workspace": workspace.display().to_string(),
        "overall_status": verdict,
        "checks": checks,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let workspace = std::fs::canonicalize(&args.workspace).unwrap_or(args.workspace.clone());
    let workspace_str = workspace.to_string_lossy().to_string();
    let use_color = !args.json && std::io::stdout().is_terminal();

    let mut checks: Vec<CheckResult> = Vec::new();

    // --- Check 1: Tala daemon connectivity ---
    let tala_client = connect_tala().await;
    let health_result = match tala_client {
        Ok(mut client) => match tala_health(&mut client).await {
            Ok(pair) => Ok(pair),
            Err(err) => Err(err.to_string()),
        },
        Err(err) => Err(err.to_string()),
    };
    checks.push(interpret_tala_health(health_result, ipc::TALA_SOCKET));

    // Reconnect for further RPCs
    let mut tala_client = connect_tala().await.ok();

    // --- Check 2: DevContainer config ---
    let config_path = ipc::detect_devcontainer_config(&workspace);
    checks.push(interpret_devcontainer_config(&workspace, config_path));

    // --- Check 3: Docker (via Tala preflight, fallback to direct) ---
    let preflight_result = if let Some(ref mut client) = tala_client {
        Some(
            tala_preflight(client, &workspace_str)
                .await
                .map_err(|e| e.to_string()),
        )
    } else {
        None
    };
    let direct_docker = if preflight_result.is_none() {
        Some(direct_docker_check().await)
    } else {
        None
    };
    checks.push(interpret_docker_preflight(preflight_result, direct_docker));

    // --- Check 4: Workspace / container status ---
    let status_result = if let Some(ref mut client) = tala_client {
        Some(
            tala_get_status(client, &workspace_str)
                .await
                .map_err(|e| e.to_string()),
        )
    } else {
        None
    };
    let (status_check, container_id) = interpret_workspace_status(status_result);
    checks.push(status_check);

    // --- Check 5: Capabilities ---
    let caps = if let Some(ref cid) = container_id {
        Some(check_capabilities_io(cid).await)
    } else {
        None
    };
    checks.push(interpret_capabilities(container_id.as_deref(), caps));

    // --- Verdict ---
    let verdict = compute_verdict(&checks);

    // --- Output ---
    if args.json {
        println!("{}", format_json_report(&workspace, &checks, verdict));
    } else {
        print!(
            "{}",
            format_text_report(&workspace, &checks, verdict, args.verbose, use_color)
        );
    }

    match verdict {
        Verdict::Broken => std::process::exit(1),
        _ => std::process::exit(0),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_tala_healthy() {
        let result = interpret_tala_health(Ok(("0.1.0".to_string(), true)), "/tmp/tala.sock");
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.message.contains("0.1.0"));
        assert!(result.suggestion.is_none());
    }

    #[test]
    fn test_tala_unhealthy() {
        let result = interpret_tala_health(Ok(("0.1.0".to_string(), false)), "/tmp/tala.sock");
        assert_eq!(result.status, CheckStatus::Error);
        assert!(result.suggestion.is_some());
    }

    #[test]
    fn test_tala_unreachable() {
        let result = interpret_tala_health(
            Err("connection refused".to_string()),
            "/tmp/tala.sock",
        );
        assert_eq!(result.status, CheckStatus::Error);
        assert!(result.message.contains("not reachable"));
        let suggestion = result.suggestion.as_ref().expect("should have suggestion");
        assert!(suggestion.contains("cargo run"));
    }

    #[test]
    fn test_config_found() {
        let ws = PathBuf::from("/workspace/project");
        let config = Some(PathBuf::from(
            "/workspace/project/.devcontainer/devcontainer.json",
        ));
        let result = interpret_devcontainer_config(&ws, config);
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.message.contains("devcontainer.json"));
    }

    #[test]
    fn test_config_not_found() {
        let ws = PathBuf::from("/workspace/project");
        let result = interpret_devcontainer_config(&ws, None);
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.suggestion.is_some());
    }

    #[test]
    fn test_docker_ok_via_tala() {
        let data = PrefightData {
            docker_available: true,
            docker_permissions_ok: true,
            error_message: String::new(),
        };
        let result = interpret_docker_preflight(Some(Ok(data)), None);
        assert_eq!(result.status, CheckStatus::Ok);
    }

    #[test]
    fn test_docker_no_permissions() {
        let data = PrefightData {
            docker_available: true,
            docker_permissions_ok: false,
            error_message: "permission denied".to_string(),
        };
        let result = interpret_docker_preflight(Some(Ok(data)), None);
        assert_eq!(result.status, CheckStatus::Error);
        assert!(result.suggestion.as_ref().expect("should have suggestion").contains("docker group"));
    }

    #[test]
    fn test_docker_unavailable() {
        let data = PrefightData {
            docker_available: false,
            docker_permissions_ok: false,
            error_message: "Cannot connect to Docker daemon".to_string(),
        };
        let result = interpret_docker_preflight(Some(Ok(data)), None);
        assert_eq!(result.status, CheckStatus::Error);
    }

    #[test]
    fn test_docker_direct_fallback_ok() {
        let result = interpret_docker_preflight(None, Some(true));
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.message.contains("direct check"));
    }

    #[test]
    fn test_docker_direct_fallback_fail() {
        let result = interpret_docker_preflight(None, Some(false));
        assert_eq!(result.status, CheckStatus::Error);
    }

    #[test]
    fn test_container_running() {
        let data = StatusData {
            container_id: "abc123def456789".to_string(),
            status: "running".to_string(),
            running: true,
        };
        let (result, cid) = interpret_workspace_status(Some(Ok(data)));
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.message.contains("abc123def456"));
        assert_eq!(cid.as_deref(), Some("abc123def456789"));
    }

    #[test]
    fn test_container_stopped() {
        let data = StatusData {
            container_id: "abc123".to_string(),
            status: "exited".to_string(),
            running: false,
        };
        let (result, cid) = interpret_workspace_status(Some(Ok(data)));
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.message.contains("not running"));
        assert!(cid.is_none());
    }

    #[test]
    fn test_no_container() {
        let data = StatusData {
            container_id: String::new(),
            status: String::new(),
            running: false,
        };
        let (result, cid) = interpret_workspace_status(Some(Ok(data)));
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.message.contains("No DevContainer"));
        assert!(cid.is_none());
    }

    #[test]
    fn test_status_skipped() {
        let (result, cid) = interpret_workspace_status(None);
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(cid.is_none());
    }

    #[test]
    fn test_all_capabilities_ok() {
        let result = interpret_capabilities(Some("abc123"), Some((true, true, true)));
        assert_eq!(result.status, CheckStatus::Ok);
        assert!(result.message.contains("terminal=OK"));
    }

    #[test]
    fn test_some_capabilities_fail() {
        let result = interpret_capabilities(Some("abc123"), Some((true, false, false)));
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.message.contains("rust_lsp=FAIL"));
        assert!(result.message.contains("py_lsp=FAIL"));
        let suggestion = result.suggestion.as_ref().expect("should have suggestion");
        assert!(suggestion.contains("rust-analyzer"));
        assert!(suggestion.contains("pyright"));
    }

    #[test]
    fn test_capabilities_skipped_no_container() {
        let result = interpret_capabilities(None, None);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    #[test]
    fn test_verdict_healthy() {
        let checks = vec![
            CheckResult {
                name: "A".to_string(),
                status: CheckStatus::Ok,
                message: String::new(),
                suggestion: None,
                details: None,
            },
            CheckResult {
                name: "B".to_string(),
                status: CheckStatus::Skip,
                message: String::new(),
                suggestion: None,
                details: None,
            },
        ];
        assert_eq!(compute_verdict(&checks), Verdict::Healthy);
    }

    #[test]
    fn test_verdict_degraded() {
        let checks = vec![
            CheckResult {
                name: "A".to_string(),
                status: CheckStatus::Ok,
                message: String::new(),
                suggestion: None,
                details: None,
            },
            CheckResult {
                name: "B".to_string(),
                status: CheckStatus::Warn,
                message: String::new(),
                suggestion: None,
                details: None,
            },
        ];
        assert_eq!(compute_verdict(&checks), Verdict::Degraded);
    }

    #[test]
    fn test_verdict_broken() {
        let checks = vec![
            CheckResult {
                name: "A".to_string(),
                status: CheckStatus::Error,
                message: String::new(),
                suggestion: None,
                details: None,
            },
            CheckResult {
                name: "B".to_string(),
                status: CheckStatus::Warn,
                message: String::new(),
                suggestion: None,
                details: None,
            },
        ];
        assert_eq!(compute_verdict(&checks), Verdict::Broken);
    }

    #[test]
    fn test_text_report_format() {
        let checks = vec![
            CheckResult {
                name: "Tala daemon".to_string(),
                status: CheckStatus::Ok,
                message: "Tala daemon reachable at /tmp/tala.sock (version 0.1.0)".to_string(),
                suggestion: None,
                details: None,
            },
            CheckResult {
                name: "Docker".to_string(),
                status: CheckStatus::Warn,
                message: "Docker running but no container".to_string(),
                suggestion: Some("Run Flow 1 build.".to_string()),
                details: None,
            },
        ];
        let report = format_text_report(
            Path::new("/workspace/test"),
            &checks,
            Verdict::Degraded,
            false,
            false,
        );
        assert!(report.contains("[OK]"));
        assert!(report.contains("[WARN]"));
        assert!(report.contains("Run Flow 1 build."));
        assert!(report.contains("DEGRADED"));
    }

    #[test]
    fn test_json_report_format() {
        let checks = vec![
            CheckResult {
                name: "Tala daemon".to_string(),
                status: CheckStatus::Ok,
                message: "ok".to_string(),
                suggestion: None,
                details: None,
            },
            CheckResult {
                name: "Docker".to_string(),
                status: CheckStatus::Error,
                message: "down".to_string(),
                suggestion: Some("start docker".to_string()),
                details: None,
            },
        ];
        let json_str = format_json_report(
            Path::new("/workspace/test"),
            &checks,
            Verdict::Broken,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("valid JSON");
        assert_eq!(parsed["overall_status"], "BROKEN");
        assert_eq!(parsed["workspace"], "/workspace/test");
        let arr = parsed["checks"].as_array().expect("checks array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["status"], "OK");
        assert_eq!(arr[1]["status"], "ERROR");
        assert_eq!(arr[1]["suggestion"], "start docker");
    }
}
