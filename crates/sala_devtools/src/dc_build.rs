// dc-build — Build or rebuild a DevContainer via tais-devcontainerd.
//
// Thin wrapper around the TAIS DevContainer daemon's `BuildContainer` gRPC call with streaming
// progress output. Text mode shows live progress lines; JSON mode emits
// a single summary object at the end.
//
// # Usage
//
// ```sh
// dc-build --workspace /workspace/_JAGORA/playground-rust
// dc-build --workspace /workspace/_JAGORA/playground-rust --force
// dc-build --workspace /workspace/_JAGORA/playground-rust --json
// dc-build --max-wait-seconds 120
// ```
//
// # Exit codes
//
// | Code | Meaning                          |
// |------|----------------------------------|
// | 0    | Build succeeded                  |
// | 1    | Build failed or error            |

use anyhow::{Context as _, bail};
use futures::StreamExt;
use sala_devtools::cli;
use sala_devtools::ipc::{self, devcontainer};
use sala_devtools::schemas::DcBuildJson;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

const DEFAULT_MAX_WAIT_SECONDS: u64 = 600;

struct BuildArgs {
    common: cli::CommonArgs,
    force_rebuild: bool,
    max_wait_seconds: u64,
}

fn parse_args() -> anyhow::Result<BuildArgs> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: dc-build [OPTIONS]");
        println!();
        println!("Build or rebuild a DevContainer for the given workspace via the TAIS DevContainer daemon.");
        println!();
        println!("Options:");
        println!("  -w, --workspace <PATH>     Workspace to build (default: cwd)");
        println!("  -j, --json                 Machine-readable JSON output");
        println!("  -f, --force                Force rebuild (remove existing container first)");
        println!("  --max-wait-seconds <N>     Build timeout in seconds (default: {DEFAULT_MAX_WAIT_SECONDS})");
        println!("  -h, --help                 Show this help");
        std::process::exit(0);
    }

    let common = cli::parse_common_args(&mut args);
    let force_rebuild = cli::extract_bool_flag(&mut args, "--force", "-f");
    let max_wait_seconds: u64 = cli::extract_flag(&mut args, "--max-wait-seconds")
        .map(|v| {
            v.parse()
                .context("--max-wait-seconds must be a positive integer")
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_WAIT_SECONDS);

    Ok(BuildArgs {
        common,
        force_rebuild,
        max_wait_seconds,
    })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!("run with --help for usage");
            std::process::exit(1);
        }
    };

    let workspace = args
        .common
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.common.workspace.clone());
    let workspace_str = workspace.to_string_lossy().to_string();
    let json_mode = args.common.json;
    let use_color = !json_mode && std::io::stdout().is_terminal();
    let timeout = Duration::from_secs(args.max_wait_seconds);
    let action = if args.force_rebuild { "rebuild" } else { "build" };

    // Detect devcontainer config
    let config_path = match ipc::detect_devcontainer_config(&workspace) {
        Some(p) => p,
        None => {
            let msg = format!(
                "No .devcontainer/devcontainer.json found in {}",
                workspace.display()
            );
            if json_mode {
                print_json(&DcBuildJson {
                    workspace: workspace_str,
                    action: action.to_string(),
                    status: "failed".to_string(),
                    duration_ms: 0,
                    container_id: None,
                    error_message: Some(msg),
                });
            } else {
                eprintln!("error: {msg}");
            }
            std::process::exit(1);
        }
    };
    let config_str = config_path.to_string_lossy().to_string();

    // Connect to daemon
    if !json_mode {
        println!("dc-build — connecting to tais-devcontainerd daemon ...");
    }
    let mut client = match ipc::connect_daemon().await {
        Ok(c) => c,
        Err(err) => {
            let msg = format!("Failed to connect to TAIS DevContainer daemon: {err}");
            if json_mode {
                print_json(&DcBuildJson {
                    workspace: workspace_str,
                    action: action.to_string(),
                    status: "failed".to_string(),
                    duration_ms: 0,
                    container_id: None,
                    error_message: Some(msg),
                });
            } else {
                eprintln!("error: {msg}");
            }
            std::process::exit(1);
        }
    };

    if !json_mode {
        if use_color {
            println!(
                "\x1b[1mdc-build\x1b[0m — {} for {}",
                action,
                workspace.display()
            );
        } else {
            println!("dc-build — {} for {}", action, workspace.display());
        }
        println!("  config:  {config_str}");
        println!("  timeout: {}s", timeout.as_secs());
        println!();
    }

    let started = Instant::now();

    // Call BuildContainer RPC (streaming)
    let build_result = async {
        let mut stream = client
            .build_container(devcontainer::BuildRequest {
                workspace_path: workspace_str.clone(),
                config_path: config_str.clone(),
                force_rebuild: args.force_rebuild,
            })
            .await
            .context("BuildContainer RPC failed")?
            .into_inner();

        let mut container_id: Option<String> = None;

        while let Some(progress) = stream.next().await {
            let progress = progress.context("stream error during build")?;
            let stage = progress.stage();

            match stage {
                devcontainer::build_progress::Stage::Complete => {
                    let cid = progress
                        .message
                        .strip_prefix("Container ready: ")
                        .unwrap_or(&progress.message)
                        .to_string();
                    if !json_mode {
                        if use_color {
                            println!("\x1b[32m[COMPLETE]\x1b[0m container {cid}");
                        } else {
                            println!("[COMPLETE] container {cid}");
                        }
                    }
                    container_id = Some(cid);
                    break;
                }
                devcontainer::build_progress::Stage::Failed => {
                    let error = if progress.error.is_empty() {
                        progress.message
                    } else {
                        progress.error
                    };
                    bail!("{error}");
                }
                _ => {
                    if !json_mode {
                        println!(
                            "[{:?}] {}% — {}",
                            stage, progress.percent, progress.message
                        );
                    }
                }
            }
        }

        container_id.context("build stream ended without a container ID")
    };

    let result = tokio::time::timeout(timeout, build_result).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(container_id)) => {
            if json_mode {
                print_json(&DcBuildJson {
                    workspace: workspace_str,
                    action: action.to_string(),
                    status: "success".to_string(),
                    duration_ms,
                    container_id: Some(container_id),
                    error_message: None,
                });
            } else {
                println!();
                if use_color {
                    println!("\x1b[1;32mBuild succeeded\x1b[0m in {}ms.", duration_ms);
                } else {
                    println!("Build succeeded in {}ms.", duration_ms);
                }
            }
            std::process::exit(0);
        }
        Ok(Err(err)) => {
            let msg = format!("{err}");
            if json_mode {
                print_json(&DcBuildJson {
                    workspace: workspace_str,
                    action: action.to_string(),
                    status: "failed".to_string(),
                    duration_ms,
                    container_id: None,
                    error_message: Some(msg),
                });
            } else {
                eprintln!();
                if use_color {
                    eprintln!("\x1b[1;31mBuild failed\x1b[0m after {}ms: {err}", duration_ms);
                } else {
                    eprintln!("Build failed after {}ms: {err}", duration_ms);
                }
            }
            std::process::exit(1);
        }
        Err(_) => {
            let msg = format!(
                "Build timed out after {}s (use --max-wait-seconds to increase)",
                timeout.as_secs()
            );
            if json_mode {
                print_json(&DcBuildJson {
                    workspace: workspace_str,
                    action: action.to_string(),
                    status: "failed".to_string(),
                    duration_ms,
                    container_id: None,
                    error_message: Some(msg),
                });
            } else {
                eprintln!();
                if use_color {
                    eprintln!("\x1b[1;31mBuild timed out\x1b[0m after {}s.", timeout.as_secs());
                } else {
                    eprintln!("Build timed out after {}s.", timeout.as_secs());
                }
            }
            std::process::exit(1);
        }
    }
}

fn print_json(build: &DcBuildJson) {
    match serde_json::to_string_pretty(build) {
        Ok(s) => println!("{s}"),
        Err(err) => eprintln!("JSON serialization failed: {err}"),
    }
}
