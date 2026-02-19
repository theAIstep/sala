// dc-status — DevContainer status inspector (direct Docker view).
//
// Discovers running DevContainers via `docker ps`, checks capabilities
// (terminal, rust-analyzer, pyright), and prints the results using the
// shared HUD model from `sala_hud::devcontainer_hud`.
//
// This tool queries Docker directly (not via tais-devcontainerd), providing a raw
// "what Docker sees" perspective.
//
// # Usage
//
// ```sh
// dc-status
// dc-status --workspace /workspace/_JAGORA/playground-rust
// dc-status --json
// dc-status --verbose
// ```

use sala_hud::{DevContainerHud, DevContainerPhase};
use std::io::IsTerminal;
use std::path::PathBuf;

struct CliArgs {
    workspace: Option<PathBuf>,
    json: bool,
    verbose: bool,
}

fn parse_args() -> CliArgs {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: dc-status [OPTIONS]");
        println!();
        println!("Show running DevContainers discovered via Docker.");
        println!();
        println!("Options:");
        println!("  -w, --workspace <PATH>  Filter to a specific workspace");
        println!("  -j, --json              Output as JSON");
        println!("  -v, --verbose           Show extra detail (container IDs, etc.)");
        println!("  -h, --help              Show this help");
        std::process::exit(0);
    }

    let common = sala_devtools::cli::parse_common_args(&mut args);

    // workspace from common is always set (defaults to cwd); but dc-status
    // traditionally shows ALL containers unless --workspace is given.
    // We detect whether --workspace was explicitly provided by checking
    // if parse_common_args consumed a -w/--workspace flag.
    // Since parse_common_args already consumed it, we need a different approach:
    // check the original args for --workspace/-w presence.
    let original_args: Vec<String> = std::env::args().collect();
    let workspace_explicit = original_args
        .iter()
        .any(|a| a == "--workspace" || a == "-w" || a.starts_with("--workspace=") || a.starts_with("-w="));

    let workspace = if workspace_explicit {
        Some(common.workspace)
    } else {
        None
    };

    CliArgs {
        workspace,
        json: common.json,
        verbose: common.verbose,
    }
}

struct DiscoveredContainer {
    id: String,
    workspace_folder: PathBuf,
}

async fn discover_containers() -> anyhow::Result<Vec<DiscoveredContainer>> {
    let output = tokio::process::Command::new("docker")
        .args([
            "ps",
            "-q",
            "--filter",
            "label=devcontainer.local_folder",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker ps failed: {stderr}");
    }

    let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mut containers = Vec::new();
    for id in ids {
        let inspect = tokio::process::Command::new("docker")
            .args([
                "inspect",
                "-f",
                "{{index .Config.Labels \"devcontainer.local_folder\"}}",
                &id,
            ])
            .output()
            .await?;

        if inspect.status.success() {
            let folder = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
            if !folder.is_empty() {
                containers.push(DiscoveredContainer {
                    id,
                    workspace_folder: PathBuf::from(folder),
                });
            }
        }
    }

    Ok(containers)
}

async fn check_terminal(container_id: &str) -> bool {
    let output = tokio::process::Command::new("docker")
        .args(["exec", container_id, "echo", "SMOKE_OK"])
        .output()
        .await;
    match output {
        Ok(out) => out.status.success() && String::from_utf8_lossy(&out.stdout).contains("SMOKE_OK"),
        Err(_) => false,
    }
}

async fn check_binary(container_id: &str, binary: &str) -> bool {
    let check_cmd = format!("command -v {binary}");
    let output = tokio::process::Command::new("docker")
        .args(["exec", container_id, "sh", "-c", &check_cmd])
        .output()
        .await;
    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

async fn check_capabilities(container_id: &str) -> (bool, bool, bool) {
    let (terminal, rust, python) = tokio::join!(
        check_terminal(container_id),
        check_binary(container_id, "rust-analyzer"),
        check_binary(container_id, "pyright-langserver"),
    );
    (terminal, rust, python)
}

// ANSI color helpers
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn phase_colored(phase: &DevContainerPhase, use_color: bool) -> String {
    let label = phase.label();
    if !use_color {
        return label.to_string();
    }
    match phase {
        DevContainerPhase::Idle => label.to_string(),
        DevContainerPhase::Preflight { .. } => format!("{YELLOW}{label}{RESET}"),
        DevContainerPhase::Building { .. } => format!("{YELLOW}{label}{RESET}"),
        DevContainerPhase::Running { .. } => format!("{GREEN}{label}{RESET}"),
        DevContainerPhase::Error { .. } => format!("{RED}{label}{RESET}"),
    }
}

fn capability_colored(ok: bool, use_color: bool) -> &'static str {
    match (ok, use_color) {
        (true, true) => "\x1b[32mOK\x1b[0m",
        (true, false) => "OK",
        (false, true) => "\x1b[31m\u{2014}\x1b[0m",
        (false, false) => "\u{2014}",
    }
}

pub fn format_colored_table(hud: &DevContainerHud, use_color: bool, verbose: bool) -> String {
    let statuses = hud.all_statuses();
    if statuses.is_empty() {
        return "No running DevContainers found.".to_string();
    }

    let header = if use_color {
        format!(
            "{BOLD}{:<40} {:<12} {:<14} {:<5} {:<5} {:<5}{RESET}",
            "Workspace", "Phase", "Container", "Term", "Rust", "Py"
        )
    } else {
        format!(
            "{:<40} {:<12} {:<14} {:<5} {:<5} {:<5}",
            "Workspace", "Phase", "Container", "Term", "Rust", "Py"
        )
    };

    let separator = if use_color {
        format!("{CYAN}{}{RESET}", "\u{2500}".repeat(85))
    } else {
        "-".repeat(85)
    };

    let mut lines = vec![header, separator];

    for status in statuses.values() {
        let container_display = status
            .container_id
            .as_ref()
            .map(|id| {
                if verbose {
                    id.as_str()
                } else if id.len() > 12 {
                    &id[..12]
                } else {
                    id.as_str()
                }
            })
            .unwrap_or("\u{2014}");

        let cap = &status.capabilities;
        let phase_str = phase_colored(&status.phase, use_color);
        let phase_pad = 12_usize.saturating_sub(status.phase.label().len());

        lines.push(format!(
            "{:<40} {}{:>pad$} {:<14} {:<5} {:<5} {:<5}",
            status.workspace_root.display(),
            phase_str,
            "",
            container_display,
            capability_colored(cap.terminal_ok, use_color),
            capability_colored(cap.lsp_rust_ok, use_color),
            capability_colored(cap.lsp_python_ok, use_color),
            pad = phase_pad,
        ));

        if verbose {
            if use_color {
                lines.push(format!(
                    "  {DIM}project: {}{RESET}",
                    status.project_name
                ));
            } else {
                lines.push(format!("  project: {}", status.project_name));
            }
        }
    }

    lines.join("\n")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let use_color = !args.json && std::io::stdout().is_terminal();

    let containers = discover_containers().await?;

    let mut hud = DevContainerHud::new();

    for container in &containers {
        if let Some(ref filter) = args.workspace {
            if &container.workspace_folder != filter {
                continue;
            }
        }

        let project_name = container
            .workspace_folder
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());

        hud.on_preflight_started(container.workspace_folder.clone(), project_name);
        hud.on_container_started(&container.workspace_folder, container.id.clone());

        let (terminal_ok, rust_ok, python_ok) = check_capabilities(&container.id).await;
        hud.on_terminal_smoke_result(&container.workspace_folder, terminal_ok);
        hud.on_lsp_rust_result(&container.workspace_folder, rust_ok);
        hud.on_lsp_python_result(&container.workspace_folder, python_ok);
    }

    if args.json {
        let json = hud
            .format_status_json()
            .map_err(|err| anyhow::anyhow!("JSON serialization failed: {err}"))?;
        println!("{json}");
    } else {
        println!("{}", format_colored_table(&hud, use_color, args.verbose));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_colored_table_empty() {
        let hud = DevContainerHud::new();
        let output = format_colored_table(&hud, false, false);
        assert_eq!(output, "No running DevContainers found.");
    }

    #[test]
    fn test_format_colored_table_with_entries() {
        let mut hud = DevContainerHud::new();
        let ws = PathBuf::from("/workspace/my-project");

        hud.on_preflight_started(ws.clone(), "my-project".to_string());
        hud.on_container_started(&ws, "abc123def456789".to_string());
        hud.on_terminal_smoke_result(&ws, true);
        hud.on_lsp_rust_result(&ws, true);
        hud.on_lsp_python_result(&ws, false);

        let output = format_colored_table(&hud, false, false);
        assert!(output.contains("Workspace"));
        assert!(output.contains("Phase"));
        assert!(output.contains("Running"));
        assert!(output.contains("abc123def456"));
        assert!(output.contains("OK"));
    }

    #[test]
    fn test_format_colored_table_with_color() {
        let mut hud = DevContainerHud::new();
        let ws = PathBuf::from("/workspace/colored-test");

        hud.on_preflight_started(ws.clone(), "colored-test".to_string());
        hud.on_container_started(&ws, "abc123".to_string());
        hud.on_terminal_smoke_result(&ws, true);

        let output = format_colored_table(&hud, true, false);
        assert!(output.contains("\x1b["));
        assert!(output.contains("Running"));
    }

    #[test]
    fn test_format_colored_table_verbose() {
        let mut hud = DevContainerHud::new();
        let ws = PathBuf::from("/workspace/verbose-test");

        hud.on_preflight_started(ws.clone(), "verbose-test".to_string());
        hud.on_container_started(&ws, "abc123def456789full".to_string());
        hud.on_terminal_smoke_result(&ws, true);

        let output = format_colored_table(&hud, false, true);
        // Verbose shows full container ID and project name
        assert!(output.contains("abc123def456789full"));
        assert!(output.contains("project: verbose-test"));
    }

    #[test]
    fn test_phase_colored_no_color() {
        let phase = DevContainerPhase::Running {
            started_at: std::time::Instant::now(),
        };
        assert_eq!(phase_colored(&phase, false), "Running");
    }

    #[test]
    fn test_phase_colored_with_color() {
        let phase = DevContainerPhase::Error {
            at: std::time::Instant::now(),
            message: "fail".to_string(),
        };
        let colored = phase_colored(&phase, true);
        assert!(colored.contains(RED));
        assert!(colored.contains("Error"));
    }

    #[test]
    fn test_capability_colored() {
        assert_eq!(capability_colored(true, false), "OK");
        assert_eq!(capability_colored(false, false), "\u{2014}");
        assert!(capability_colored(true, true).contains("\x1b[32m"));
        assert!(capability_colored(false, true).contains("\x1b[31m"));
    }
}
