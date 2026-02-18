// dc-smoke — Headless DevContainer smoke test.
//
// Exercises Flow 1 (BuildContainer), Phase 2B (Terminal), and Phase 2C
// (Local Exec LSP) without requiring a GPUI window. Designed to run inside
// a DevContainer terminal or any environment where `tala` is listening on
// its IPC socket.
//
// # Usage
//
// ```sh
// dc-smoke --workspace /workspace/_JAGORA/playground-rust
// dc-smoke --workspace /workspace/_JAGORA/playground-python --language python
// dc-smoke --workspace /workspace/_JAGORA/playground-rust --json
// dc-smoke --max-wait-seconds 120
// ```
//
// # Exit codes
//
// | Code | Meaning                            |
// |------|------------------------------------|
// | 0    | All stages passed                  |
// | 1    | Daemon connection / health failed  |
// | 2    | Preflight failed (Docker issue)    |
// | 3    | DevContainer config not found      |
// | 4    | DevContainer build/connect failed  |
// | 5    | Terminal stage failed              |
// | 6    | LSP stage failed                   |

use anyhow::{Context as _, Result, bail};
use futures::StreamExt;
use sala_devtools::cli;
use sala_devtools::ipc::{self, devcontainer, DevContainerServiceClient};
use sala_devtools::schemas::{DcSmokeJson, DcSmokeStepJson};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_MAX_WAIT_SECONDS: u64 = 600;

// ---------------------------------------------------------------------------
// Language configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LspConfig {
    display_name: &'static str,
    check_binary: &'static str,
    server_binary: &'static str,
    server_args: Vec<&'static str>,
    log_tag: &'static str,
}

impl LspConfig {
    fn rust() -> Self {
        Self {
            display_name: "rust-analyzer",
            check_binary: "rust-analyzer",
            server_binary: "rust-analyzer",
            server_args: vec![],
            log_tag: "[LSP-RUST]  ",
        }
    }

    fn python() -> Self {
        Self {
            display_name: "pyright",
            check_binary: "pyright-langserver",
            server_binary: "pyright-langserver",
            server_args: vec!["--stdio"],
            log_tag: "[LSP-PYTHON]",
        }
    }
}

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

struct Args {
    workspace_path: PathBuf,
    max_wait_seconds: u64,
    json: bool,
    lsp_config: LspConfig,
}

fn parse_args() -> Result<Args> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: dc-smoke [OPTIONS] [WORKSPACE_PATH]");
        println!();
        println!("End-to-end smoke test for a DevContainer workspace.");
        println!();
        println!("Options:");
        println!("  -w, --workspace <PATH>     Workspace to test (default: cwd)");
        println!("  -j, --json                 Machine-readable JSON output");
        println!("  --max-wait-seconds <N>     Build timeout in seconds (default: {DEFAULT_MAX_WAIT_SECONDS})");
        println!("  --language <rust|python>   Language LSP to test (default: rust)");
        println!("  -h, --help                 Show this help");
        std::process::exit(0);
    }

    let common = cli::parse_common_args(&mut args);

    let max_wait_seconds: u64 = cli::extract_flag(&mut args, "--max-wait-seconds")
        .map(|v| v.parse().context("--max-wait-seconds must be a positive integer"))
        .transpose()?
        .unwrap_or(DEFAULT_MAX_WAIT_SECONDS);

    let language = cli::extract_flag(&mut args, "--language")
        .unwrap_or_else(|| "rust".to_string());

    let lsp_config = match language.as_str() {
        "rust" => LspConfig::rust(),
        "python" => LspConfig::python(),
        other => bail!("unsupported --language value: {other} (expected: rust, python)"),
    };

    // Accept positional arg as workspace override (convenience)
    let workspace_path = match args.into_iter().find(|a| !a.starts_with('-')) {
        Some(p) => PathBuf::from(p),
        None => common.workspace,
    };

    Ok(Args {
        workspace_path,
        max_wait_seconds,
        json: common.json,
        lsp_config,
    })
}

// ---------------------------------------------------------------------------
// Result tracking
// ---------------------------------------------------------------------------

struct SmokeResults {
    devcontainer: Option<StepResult>,
    terminal: Option<StepResult>,
    lsp: Option<StepResult>,
    lsp_label: String,
}

enum StepResult {
    Ok(String),
    Fail(String),
    Skip(String),
}

impl StepResult {
    fn status_str(&self) -> &str {
        match self {
            StepResult::Ok(_) => "ok",
            StepResult::Fail(_) => "fail",
            StepResult::Skip(_) => "skip",
        }
    }

    fn message(&self) -> &str {
        match self {
            StepResult::Ok(m) | StepResult::Fail(m) | StepResult::Skip(m) => m,
        }
    }
}

impl SmokeResults {
    fn new(lsp_display_name: &str) -> Self {
        Self {
            devcontainer: None,
            terminal: None,
            lsp: None,
            lsp_label: format!("LSP ({lsp_display_name})"),
        }
    }

    fn print_summary(&self) {
        let lsp_padded = format!("{:<12}", self.lsp_label);

        println!();
        println!("========================================");
        println!("  DevContainer Smoke Test  --  Summary  ");
        println!("========================================");
        Self::print_step("DevContainer", &self.devcontainer);
        Self::print_step("Terminal    ", &self.terminal);
        Self::print_step(&lsp_padded, &self.lsp);
        println!("========================================");
        println!();
    }

    fn print_step(label: &str, result: &Option<StepResult>) {
        match result {
            Some(StepResult::Ok(msg)) => println!("  {label}  OK    {msg}"),
            Some(StepResult::Fail(msg)) => println!("  {label}  FAIL  {msg}"),
            Some(StepResult::Skip(msg)) => println!("  {label}  SKIP  {msg}"),
            None => println!("  {label}  ---   (not run)"),
        }
    }

    fn exit_code(&self) -> i32 {
        if matches!(self.devcontainer, Some(StepResult::Fail(_))) {
            return 4;
        }
        if matches!(self.terminal, Some(StepResult::Fail(_))) {
            return 5;
        }
        if matches!(self.lsp, Some(StepResult::Fail(_))) {
            return 6;
        }
        0
    }

    fn has_any_fail(&self) -> bool {
        matches!(self.devcontainer, Some(StepResult::Fail(_)))
            || matches!(self.terminal, Some(StepResult::Fail(_)))
            || matches!(self.lsp, Some(StepResult::Fail(_)))
    }

    fn to_json(&self, workspace: &str) -> DcSmokeJson {
        let mut steps = Vec::new();
        if let Some(ref step) = self.devcontainer {
            steps.push(DcSmokeStepJson {
                name: "devcontainer".to_string(),
                status: step.status_str().to_string(),
                message: step.message().to_string(),
            });
        }
        if let Some(ref step) = self.terminal {
            steps.push(DcSmokeStepJson {
                name: "terminal".to_string(),
                status: step.status_str().to_string(),
                message: step.message().to_string(),
            });
        }
        if let Some(ref step) = self.lsp {
            steps.push(DcSmokeStepJson {
                name: "lsp".to_string(),
                status: step.status_str().to_string(),
                message: step.message().to_string(),
            });
        }
        DcSmokeJson {
            workspace: workspace.to_string(),
            overall: if self.has_any_fail() {
                "fail".to_string()
            } else {
                "pass".to_string()
            },
            steps,
        }
    }
}

// ---------------------------------------------------------------------------
// Step: Health + preflight
// ---------------------------------------------------------------------------

async fn step_health_and_preflight(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
    workspace_path: &str,
    json_mode: bool,
) -> Result<()> {
    if !json_mode {
        println!("[DAEMON]    Health check ...");
    }
    let health = client
        .health_check(devcontainer::HealthCheckRequest {})
        .await
        .context("health check RPC failed")?
        .into_inner();

    if !health.healthy {
        bail!("daemon reported unhealthy (version={})", health.version);
    }
    if !json_mode {
        println!("[DAEMON]    Healthy (version={})", health.version);
        println!("[PREFLIGHT] Checking Docker availability ...");
    }

    let preflight = client
        .preflight_check(devcontainer::PreflightRequest {
            workspace_path: workspace_path.to_string(),
        })
        .await
        .context("preflight RPC failed")?
        .into_inner();

    if !preflight.docker_available {
        bail!("docker not available: {}", preflight.error_message);
    }
    if !preflight.docker_permissions_ok {
        bail!("docker permissions issue: {}", preflight.error_message);
    }
    if !json_mode {
        println!("[PREFLIGHT] Passed (docker available, permissions ok)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Step: Build / connect DevContainer (Flow 1)
// ---------------------------------------------------------------------------

async fn step_devcontainer(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
    workspace_path: &str,
    config_path: &str,
    timeout: Duration,
    json_mode: bool,
) -> Result<String> {
    if !json_mode {
        println!("[DEVCONTAINER] Building (timeout={}s) ...", timeout.as_secs());
        println!("[DEVCONTAINER] workspace: {workspace_path}");
        println!("[DEVCONTAINER] config:    {config_path}");
    }

    let stream_future = async {
        let mut stream = client
            .build_container(devcontainer::BuildRequest {
                workspace_path: workspace_path.to_string(),
                config_path: config_path.to_string(),
                force_rebuild: false,
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
                        println!("[DEVCONTAINER] Build complete -- container {cid}");
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
                    bail!("build failed: {error}");
                }
                _ => {
                    if !json_mode {
                        println!(
                            "[DEVCONTAINER] [{:?}] {}% -- {}",
                            stage, progress.percent, progress.message
                        );
                    }
                }
            }
        }

        container_id.context("build stream ended without a container ID")
    };

    tokio::time::timeout(timeout, stream_future)
        .await
        .context(format!(
            "DevContainer build timed out after {}s (use --max-wait-seconds to increase)",
            timeout.as_secs()
        ))?
}

// ---------------------------------------------------------------------------
// Step: Terminal smoke (Phase 2B)
// ---------------------------------------------------------------------------

async fn step_terminal(container_id: &str, json_mode: bool) -> Result<()> {
    if !json_mode {
        println!("[TERMINAL]  Running: docker exec {container_id} echo SMOKE_OK");
    }

    let output = tokio::process::Command::new("docker")
        .args(["exec", container_id, "echo", "SMOKE_OK"])
        .output()
        .await
        .context("failed to run docker exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        bail!("docker exec exited with {}: {}", output.status, stderr.trim());
    }

    if !stdout.contains("SMOKE_OK") {
        bail!("expected SMOKE_OK in stdout, got: {}", stdout.trim());
    }

    if !json_mode {
        println!("[TERMINAL]  Received SMOKE_OK");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Step: LSP smoke (Phase 2C)
// ---------------------------------------------------------------------------

async fn step_lsp(
    container_id: &str,
    project_name: &str,
    config: &LspConfig,
    json_mode: bool,
) -> Result<()> {
    let tag = config.log_tag;

    if !json_mode {
        println!("{tag} Checking {} availability ...", config.display_name);
    }

    let check_cmd = format!("command -v {}", config.check_binary);
    let capability_check = tokio::process::Command::new("docker")
        .args(["exec", container_id, "sh", "-c", &check_cmd])
        .output()
        .await
        .context(format!("failed to check {} availability", config.display_name))?;

    if !capability_check.status.success() {
        bail!(
            "{} not found in container (install it for LSP tests)",
            config.display_name
        );
    }
    if !json_mode {
        println!(
            "{tag} Found: {}",
            String::from_utf8_lossy(&capability_check.stdout).trim()
        );
        println!("{tag} Running initialize/shutdown cycle ...");
    }

    let workdir = format!("/workspaces/{project_name}");

    let mut docker_args = vec![
        "exec".to_string(),
        "-i".to_string(),
        "-w".to_string(),
        workdir.clone(),
        container_id.to_string(),
        config.server_binary.to_string(),
    ];
    for arg in &config.server_args {
        docker_args.push(arg.to_string());
    }

    let mut child = tokio::process::Command::new("docker")
        .args(&docker_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(format!("failed to spawn {} via docker exec", config.server_binary))?;

    let stdin = child.stdin.as_mut().context("no stdin")?;
    let stdout = child.stdout.as_mut().context("no stdout")?;

    let root_uri = format!("file://{workdir}");
    let initialize_params = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [{
                "uri": root_uri,
                "name": project_name
            }]
        }
    });
    send_lsp_message(stdin, &initialize_params).await?;

    let lsp_timeout = Duration::from_secs(60);
    let read_init_response = async {
        loop {
            let raw = read_lsp_message(stdout)
                .await
                .context("failed to read LSP message")?;

            let msg: serde_json::Value =
                serde_json::from_str(&raw).context("invalid JSON in LSP message")?;

            if msg.get("id").is_none() {
                if !json_mode {
                    let method = msg
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("<unknown>");
                    println!("{tag} (skipping notification: {method})");
                }
                continue;
            }

            if msg.get("id") == Some(&serde_json::json!(1)) {
                return Ok::<serde_json::Value, anyhow::Error>(msg);
            }

            if !json_mode {
                println!("{tag} (skipping message with id={:?})", msg.get("id"));
            }
        }
    };

    let parsed = tokio::time::timeout(lsp_timeout, read_init_response)
        .await
        .context(format!(
            "timeout ({}s) waiting for {} initialize response",
            lsp_timeout.as_secs(),
            config.display_name
        ))??;

    if parsed.get("error").is_some() {
        bail!(
            "{} initialize returned error: {}",
            config.display_name,
            serde_json::to_string_pretty(&parsed["error"])?
        );
    }

    let server_name = parsed
        .pointer("/result/serverInfo/name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let server_version = parsed
        .pointer("/result/serverInfo/version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    if !json_mode {
        println!("{tag} Server: {server_name} v{server_version}");
    }

    send_lsp_message(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await?;

    send_lsp_message(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": null
        }),
    )
    .await?;

    send_lsp_message(
        stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }),
    )
    .await?;

    // Wait briefly for child to exit; not critical if it times out
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => eprintln!("{tag} warning: child wait error: {err}"),
        Err(_) => eprintln!("{tag} warning: child did not exit within 5s"),
    }

    if !json_mode {
        println!("{tag} Initialize/shutdown cycle completed");
    }
    Ok(())
}

async fn send_lsp_message(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    message: &serde_json::Value,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = serde_json::to_string(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_lsp_message(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let mut header_buf = Vec::new();

    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);

        if header_buf.len() >= 4 && header_buf[header_buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }

        if header_buf.len() > 4096 {
            bail!("LSP header too large (>4096 bytes)");
        }
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let content_length = header_str
        .lines()
        .find_map(|line| {
            let line = line.trim();
            if line.to_ascii_lowercase().starts_with("content-length:") {
                line.split(':')
                    .nth(1)
                    .and_then(|v| v.trim().parse::<usize>().ok())
            } else {
                None
            }
        })
        .context("missing Content-Length header in LSP response")?;

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    String::from_utf8(body).context("LSP response body is not valid UTF-8")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

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

    let workspace_path = args
        .workspace_path
        .canonicalize()
        .unwrap_or_else(|_| args.workspace_path.clone());
    let workspace_str = workspace_path.to_string_lossy().to_string();
    let build_timeout = Duration::from_secs(args.max_wait_seconds);
    let lsp_config = args.lsp_config;
    let json_mode = args.json;

    let project_name = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    if !json_mode {
        println!("=== dc-smoke — DevContainer Smoke Test ===");
        println!("workspace:  {workspace_str}");
        println!("project:    {project_name}");
        println!("language:   {}", lsp_config.display_name);
        println!("timeout:    {}s", build_timeout.as_secs());
        println!();
    }

    let mut results = SmokeResults::new(lsp_config.display_name);

    // -- Connect ----------------------------------------------------------
    if !json_mode {
        println!(
            "[DAEMON]    Connecting to tala daemon at {} ...",
            ipc::TALA_SOCKET
        );
    }
    let mut client = match ipc::connect_tala().await {
        Ok(c) => c,
        Err(err) => {
            if !json_mode {
                eprintln!("[DAEMON]    FAIL: {err}");
            }
            results.devcontainer = Some(StepResult::Fail(format!("connect: {err}")));
            results.terminal = Some(StepResult::Skip("no daemon".into()));
            results.lsp = Some(StepResult::Skip("no daemon".into()));
            if json_mode {
                print_json(&results, &workspace_str);
            } else {
                results.print_summary();
            }
            std::process::exit(1);
        }
    };
    if !json_mode {
        println!("[DAEMON]    Connected");
    }

    // -- Health + Preflight -----------------------------------------------
    if let Err(err) = step_health_and_preflight(&mut client, &workspace_str, json_mode).await {
        if !json_mode {
            eprintln!("[PREFLIGHT] FAIL: {err}");
        }
        results.devcontainer = Some(StepResult::Fail(format!("{err}")));
        results.terminal = Some(StepResult::Skip("preflight failed".into()));
        results.lsp = Some(StepResult::Skip("preflight failed".into()));
        if json_mode {
            print_json(&results, &workspace_str);
        } else {
            results.print_summary();
        }
        std::process::exit(2);
    }

    // -- Detect config ----------------------------------------------------
    let config_path = match ipc::detect_devcontainer_config(&workspace_path) {
        Some(p) => p,
        None => {
            if !json_mode {
                eprintln!("[DEVCONTAINER] FAIL: no .devcontainer/devcontainer.json found");
            }
            results.devcontainer = Some(StepResult::Fail(
                "no .devcontainer/devcontainer.json found".into(),
            ));
            results.terminal = Some(StepResult::Skip("no config".into()));
            results.lsp = Some(StepResult::Skip("no config".into()));
            if json_mode {
                print_json(&results, &workspace_str);
            } else {
                results.print_summary();
            }
            std::process::exit(3);
        }
    };
    let config_str = config_path.to_string_lossy().to_string();
    if !json_mode {
        println!("[DEVCONTAINER] Config: {config_str}");
    }

    // -- Build / Connect --------------------------------------------------
    let container_id = match step_devcontainer(
        &mut client,
        &workspace_str,
        &config_str,
        build_timeout,
        json_mode,
    )
    .await
    {
        Ok(cid) => {
            results.devcontainer = Some(StepResult::Ok(format!(
                "container {}",
                &cid[..cid.len().min(12)]
            )));
            cid
        }
        Err(err) => {
            if !json_mode {
                eprintln!("[DEVCONTAINER] FAIL: {err}");
            }
            results.devcontainer = Some(StepResult::Fail(format!("{err}")));
            results.terminal = Some(StepResult::Skip("no container".into()));
            results.lsp = Some(StepResult::Skip("no container".into()));
            if json_mode {
                print_json(&results, &workspace_str);
            } else {
                results.print_summary();
            }
            std::process::exit(4);
        }
    };

    // -- Terminal ---------------------------------------------------------
    match step_terminal(&container_id, json_mode).await {
        Ok(()) => {
            results.terminal = Some(StepResult::Ok("echo SMOKE_OK received".into()));
        }
        Err(err) => {
            if !json_mode {
                eprintln!("[TERMINAL]  FAIL: {err}");
            }
            results.terminal = Some(StepResult::Fail(format!("{err}")));
        }
    }

    // -- LSP --------------------------------------------------------------
    let tag = lsp_config.log_tag;
    match step_lsp(&container_id, &project_name, &lsp_config, json_mode).await {
        Ok(()) => {
            results.lsp = Some(StepResult::Ok("initialize/shutdown ok".into()));
        }
        Err(err) => {
            let msg = format!("{err}");
            if msg.contains("not found in container") {
                if !json_mode {
                    println!("{tag} SKIP: {msg}");
                }
                results.lsp = Some(StepResult::Skip(msg));
            } else {
                if !json_mode {
                    eprintln!("{tag} FAIL: {msg}");
                }
                results.lsp = Some(StepResult::Fail(msg));
            }
        }
    }

    // -- Summary ----------------------------------------------------------
    if json_mode {
        print_json(&results, &workspace_str);
    } else {
        results.print_summary();
    }
    std::process::exit(results.exit_code());
}

fn print_json(results: &SmokeResults, workspace: &str) {
    let json = results.to_json(workspace);
    match serde_json::to_string_pretty(&json) {
        Ok(s) => println!("{s}"),
        Err(err) => eprintln!("JSON serialization failed: {err}"),
    }
}
