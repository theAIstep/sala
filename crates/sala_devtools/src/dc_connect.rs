// dc-connect — Fast-path connection to an existing DevContainer via Tala.
//
// Thin wrapper around Tala's `ConnectContainer` gRPC call. Asks Tala to
// check if a container exists/runs for the workspace and "connect" to it
// (ensure running, update internal state).
//
// # Usage
//
// ```sh
// dc-connect --workspace /workspace/_JAGORA/playground-rust
// dc-connect --workspace /workspace/_JAGORA/playground-rust --json
// ```
//
// # Exit codes
//
// | Code | Meaning                       |
// |------|-------------------------------|
// | 0    | Connected successfully        |
// | 1    | Connection failed or error    |

use anyhow::Context as _;
use sala_devtools::cli;
use sala_devtools::ipc::{self, devcontainer};
use sala_devtools::schemas::DcConnectJson;
use std::io::IsTerminal;
use std::time::Duration;

fn parse_args() -> cli::CommonArgs {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: dc-connect [OPTIONS]");
        println!();
        println!("Connect to an existing DevContainer for the given workspace via Tala.");
        println!("If the container is stopped, Tala will restart it.");
        println!("If config has changed, a rebuild hint is shown.");
        println!();
        println!("Options:");
        println!("  -w, --workspace <PATH>  Workspace to connect (default: cwd)");
        println!("  -j, --json              Machine-readable JSON output");
        println!("  -v, --verbose           Show extra detail");
        println!("  -h, --help              Show this help");
        std::process::exit(0);
    }

    cli::parse_common_args(&mut args)
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    let args = parse_args();
    let workspace = args
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.workspace.clone());
    let workspace_str = workspace.to_string_lossy().to_string();
    let json_mode = args.json;
    let use_color = !json_mode && std::io::stdout().is_terminal();

    if !json_mode {
        if use_color {
            println!(
                "\x1b[1mdc-connect\x1b[0m — {}",
                workspace.display()
            );
        } else {
            println!("dc-connect — {}", workspace.display());
        }
    }

    // Connect to Tala
    let mut client = match ipc::connect_tala().await {
        Ok(c) => c,
        Err(err) => {
            if json_mode {
                print_error_json(&workspace_str, &format!("Failed to connect to tala: {err}"));
            } else {
                eprintln!("error: failed to connect to tala daemon: {err}");
            }
            std::process::exit(1);
        }
    };

    if !json_mode {
        println!("  Connecting ...");
    }

    // Call ConnectContainer RPC
    let connect_result = tokio::time::timeout(CONNECT_TIMEOUT, async {
        client
            .connect_container(devcontainer::ConnectRequest {
                workspace_path: workspace_str.clone(),
            })
            .await
            .context("ConnectContainer RPC failed")
    })
    .await;

    match connect_result {
        Ok(Ok(response)) => {
            let inner = response.into_inner();
            let short_id = if inner.container_id.len() > 12 {
                &inner.container_id[..12]
            } else {
                &inner.container_id
            };

            if json_mode {
                let json = DcConnectJson {
                    workspace: workspace_str,
                    container_id: inner.container_id.clone(),
                    workspace_folder: inner.workspace_folder.clone(),
                    config_changed: inner.config_changed,
                };
                match serde_json::to_string_pretty(&json) {
                    Ok(s) => println!("{s}"),
                    Err(err) => eprintln!("JSON serialization failed: {err}"),
                }
            } else {
                if use_color {
                    println!("  \x1b[32mConnected\x1b[0m to container {short_id}");
                } else {
                    println!("  Connected to container {short_id}");
                }
                println!("  workspace_folder: {}", inner.workspace_folder);

                if inner.config_changed {
                    if use_color {
                        println!();
                        println!("  \x1b[33mNote:\x1b[0m devcontainer config has changed since last build.");
                        println!("  Consider running: dc-build --force --workspace {}", workspace.display());
                    } else {
                        println!();
                        println!("  Note: devcontainer config has changed since last build.");
                        println!("  Consider running: dc-build --force --workspace {}", workspace.display());
                    }
                }
            }
            std::process::exit(0);
        }
        Ok(Err(err)) => {
            if json_mode {
                print_error_json(&workspace_str, &format!("{err}"));
            } else {
                eprintln!("error: {err}");
            }
            std::process::exit(1);
        }
        Err(_) => {
            let msg = format!("Connection timed out after {}s", CONNECT_TIMEOUT.as_secs());
            if json_mode {
                print_error_json(&workspace_str, &msg);
            } else {
                eprintln!("error: {msg}");
            }
            std::process::exit(1);
        }
    }
}

fn print_error_json(workspace: &str, error: &str) {
    let json = serde_json::json!({
        "workspace": workspace,
        "error": error,
    });
    match serde_json::to_string_pretty(&json) {
        Ok(s) => println!("{s}"),
        Err(err) => eprintln!("JSON serialization failed: {err}"),
    }
}
