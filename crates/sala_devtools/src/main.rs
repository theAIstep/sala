// Headless DevContainer smoke test for Sala.
//
// Exercises Flow 1 (StartDevContainer), Phase 2B (Terminal), and Phase 2C
// (Local Exec LSP) without requiring a GPUI window. Designed to run inside
// a DevContainer terminal or any environment where `tala` is listening on
// `/tmp/tala.sock` (Unix) or `\\.\pipe\tala` (Windows).
//
// # Usage
//
// ```sh
// # Rust project (default):
// cargo run -p sala_devtools --bin devcontainer_smoke_test -- /workspace/_JAGORA/playground-rust
//
// # Python project:
// cargo run -p sala_devtools --bin devcontainer_smoke_test -- --language python /workspace/_JAGORA/playground-python
//
// # Override the default build timeout (600s):
// cargo run -p sala_devtools --bin devcontainer_smoke_test -- --max-wait-seconds 120 /workspace/_JAGORA/playground-rust
//
// # Or use the Makefile shortcut:
// make -C crates/sala_devtools smoke
// make -C crates/sala_devtools smoke-python
// ```
//
// # Preconditions
//
// - **tala daemon** must be running and listening on `/tmp/tala.sock`.
//   Start it with: `cargo run -p tala-server --bin tala` (from the tala repo).
// - **Docker** must be installed and the current user must have permissions
//   to run `docker` commands without sudo.
// - The target workspace must contain `.devcontainer/devcontainer.json` or
//   `.devcontainer.json`.
// - For `--language rust`: rust-analyzer must be installed in the container.
// - For `--language python`: pyright-langserver must be installed in the
//   container (typically via `npm install -g pyright`).
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

mod devcontainer {
    tonic::include_proto!("devcontainer");
}

use anyhow::{Context as _, Result, bail};
use devcontainer::dev_container_service_client::DevContainerServiceClient;
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_MAX_WAIT_SECONDS: u64 = 600;

// ---------------------------------------------------------------------------
// Language configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LspConfig {
    /// Display name shown in logs and summary, e.g. "rust-analyzer", "pyright"
    display_name: &'static str,
    /// Binary to check for in the container
    check_binary: &'static str,
    /// Binary to run as LSP server
    server_binary: &'static str,
    /// Extra arguments after the binary (e.g. ["--stdio"] for pyright)
    server_args: Vec<&'static str>,
    /// Log tag prefix, e.g. "[LSP-RUST]  ", "[LSP-PYTHON]"
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
// CLI argument parsing (no external deps)
// ---------------------------------------------------------------------------

struct Args {
    workspace_path: PathBuf,
    max_wait_seconds: u64,
    lsp_config: LspConfig,
}

fn extract_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    // --flag value
    if let Some(pos) = args.iter().position(|a| a == flag) {
        if pos + 1 < args.len() {
            let val = args[pos + 1].clone();
            args.drain(pos..=pos + 1);
            return Some(val);
        }
    }
    // --flag=value
    let prefix = format!("{flag}=");
    if let Some(pos) = args.iter().position(|a| a.starts_with(&prefix)) {
        let val = args[pos][prefix.len()..].to_string();
        args.remove(pos);
        return Some(val);
    }
    None
}

fn parse_args() -> Result<Args> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: devcontainer_smoke_test [OPTIONS] [WORKSPACE_PATH]");
        println!();
        println!("Options:");
        println!("  --max-wait-seconds <N>     Build timeout in seconds (default: {DEFAULT_MAX_WAIT_SECONDS})");
        println!("  --language <rust|python>    Language LSP to test (default: rust)");
        println!("  -h, --help                 Show this help");
        println!();
        println!("WORKSPACE_PATH defaults to the current directory.");
        std::process::exit(0);
    }

    let max_wait_seconds: u64 = extract_flag(&mut args, "--max-wait-seconds")
        .map(|v| v.parse().context("--max-wait-seconds must be a positive integer"))
        .transpose()?
        .unwrap_or(DEFAULT_MAX_WAIT_SECONDS);

    let language = extract_flag(&mut args, "--language")
        .unwrap_or_else(|| "rust".to_string());

    let lsp_config = match language.as_str() {
        "rust" => LspConfig::rust(),
        "python" => LspConfig::python(),
        other => bail!("unsupported --language value: {other} (expected: rust, python)"),
    };

    let workspace_path = match args.into_iter().find(|a| !a.starts_with('-')) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };

    Ok(Args {
        workspace_path,
        max_wait_seconds,
        lsp_config,
    })
}

// ---------------------------------------------------------------------------
// IPC transport (mirrors sala_hud's platform-specific channel creation)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod ipc {
    use anyhow::Result;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::UnixStream;

    pub struct IpcStream(UnixStream);

    impl AsyncRead for IpcStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for IpcStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl hyper::client::connect::Connection for IpcStream {
        fn connected(&self) -> hyper::client::connect::Connected {
            hyper::client::connect::Connected::new()
        }
    }

    pub async fn create_channel(path: String) -> Result<tonic::transport::Channel> {
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: hyper::Uri| {
                let path = path.clone();
                async move {
                    let stream = UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(IpcStream(stream))
                }
            }))
            .await?;
        Ok(channel)
    }
}

#[cfg(windows)]
mod ipc {
    use anyhow::Result;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::NamedPipeClient;

    pub struct IpcStream(NamedPipeClient);

    impl AsyncRead for IpcStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for IpcStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl hyper::client::connect::Connection for IpcStream {
        fn connected(&self) -> hyper::client::connect::Connected {
            hyper::client::connect::Connected::new()
        }
    }

    pub async fn create_channel(path: String) -> Result<tonic::transport::Channel> {
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: hyper::Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::windows::named_pipe::ClientOptions::new()
                        .open(&path)?;
                    Ok::<_, std::io::Error>(IpcStream(stream))
                }
            }))
            .await?;
        Ok(channel)
    }
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
        // Pad LSP label to align with other rows
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
            Some(StepResult::Ok(msg)) => {
                println!("  {label}  OK    {msg}");
            }
            Some(StepResult::Fail(msg)) => {
                println!("  {label}  FAIL  {msg}");
            }
            Some(StepResult::Skip(msg)) => {
                println!("  {label}  SKIP  {msg}");
            }
            None => {
                println!("  {label}  ---   (not run)");
            }
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
}

// ---------------------------------------------------------------------------
// gRPC client wrapper (headless, no GPUI)
// ---------------------------------------------------------------------------

async fn connect_daemon() -> Result<DevContainerServiceClient<tonic::transport::Channel>> {
    #[cfg(unix)]
    let ipc_path = "/tmp/tala.sock".to_string();
    #[cfg(windows)]
    let ipc_path = "\\\\.\\pipe\\tala".to_string();

    println!("[DAEMON]    Connecting to tala daemon at {ipc_path} ...");
    let channel = ipc::create_channel(ipc_path)
        .await
        .context("failed to connect to tala daemon -- is it running?")?;
    let client = DevContainerServiceClient::new(channel);
    Ok(client)
}

// ---------------------------------------------------------------------------
// Step: Health + preflight
// ---------------------------------------------------------------------------

async fn step_health_and_preflight(
    client: &mut DevContainerServiceClient<tonic::transport::Channel>,
    workspace_path: &str,
) -> Result<()> {
    println!("[DAEMON]    Health check ...");
    let health = client
        .health_check(devcontainer::HealthCheckRequest {})
        .await
        .context("health check RPC failed")?
        .into_inner();

    if !health.healthy {
        bail!("daemon reported unhealthy (version={})", health.version);
    }
    println!("[DAEMON]    Healthy (version={})", health.version);

    println!("[PREFLIGHT] Checking Docker availability ...");
    let preflight = client
        .preflight_check(devcontainer::PreflightRequest {
            workspace_path: workspace_path.to_string(),
        })
        .await
        .context("preflight RPC failed")?
        .into_inner();

    if !preflight.docker_available {
        bail!(
            "docker not available: {}",
            preflight.error_message
        );
    }
    if !preflight.docker_permissions_ok {
        bail!(
            "docker permissions issue: {}",
            preflight.error_message
        );
    }
    println!("[PREFLIGHT] Passed (docker available, permissions ok)");
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
) -> Result<String> {
    println!("[DEVCONTAINER] Building (timeout={}s) ...", timeout.as_secs());
    println!("[DEVCONTAINER] workspace: {workspace_path}");
    println!("[DEVCONTAINER] config:    {config_path}");

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
                    println!("[DEVCONTAINER] Build complete -- container {cid}");
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
                    println!(
                        "[DEVCONTAINER] [{:?}] {}% -- {}",
                        stage, progress.percent, progress.message
                    );
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
// Step: Terminal smoke (Phase 2B) -- headless docker exec
// ---------------------------------------------------------------------------

async fn step_terminal(container_id: &str) -> Result<()> {
    println!("[TERMINAL]  Running: docker exec {container_id} echo SMOKE_OK");

    let output = tokio::process::Command::new("docker")
        .args(["exec", container_id, "echo", "SMOKE_OK"])
        .output()
        .await
        .context("failed to run docker exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        bail!(
            "docker exec exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    if !stdout.contains("SMOKE_OK") {
        bail!(
            "expected SMOKE_OK in stdout, got: {}",
            stdout.trim()
        );
    }

    println!("[TERMINAL]  Received SMOKE_OK");
    Ok(())
}

// ---------------------------------------------------------------------------
// Step: LSP smoke (Phase 2C) -- generic, language-configurable
// ---------------------------------------------------------------------------

async fn step_lsp(container_id: &str, project_name: &str, config: &LspConfig) -> Result<()> {
    let tag = config.log_tag;

    println!("{tag} Checking {} availability ...", config.display_name);

    let check_cmd = format!("command -v {}", config.check_binary);
    let capability_check = tokio::process::Command::new("docker")
        .args(["exec", container_id, "sh", "-c", &check_cmd])
        .output()
        .await
        .context(format!(
            "failed to check {} availability",
            config.display_name
        ))?;

    if !capability_check.status.success() {
        bail!(
            "{} not found in container (install it for LSP tests)",
            config.display_name
        );
    }
    println!(
        "{tag} Found: {}",
        String::from_utf8_lossy(&capability_check.stdout).trim()
    );

    println!("{tag} Running initialize/shutdown cycle ...");

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
        .context(format!(
            "failed to spawn {} via docker exec",
            config.server_binary
        ))?;

    let stdin = child.stdin.as_mut().context("no stdin")?;
    let stdout = child.stdout.as_mut().context("no stdout")?;

    // Send LSP initialize request
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

    // Read response with timeout. Some language servers (e.g. pyright) send
    // notification messages (window/logMessage, etc.) before the initialize
    // response. We loop reading messages until we find the one with "id": 1.
    let lsp_timeout = Duration::from_secs(60);
    let read_init_response = async {
        loop {
            let raw = read_lsp_message(stdout)
                .await
                .context("failed to read LSP message")?;

            let msg: serde_json::Value =
                serde_json::from_str(&raw).context("invalid JSON in LSP message")?;

            // Notifications have no "id" field — skip them
            if msg.get("id").is_none() {
                let method = msg
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("<unknown>");
                println!("{tag} (skipping notification: {method})");
                continue;
            }

            // Check if this is the response to our request (id: 1)
            if msg.get("id") == Some(&serde_json::json!(1)) {
                return Ok::<serde_json::Value, anyhow::Error>(msg);
            }

            // Some other response id — skip it too
            println!(
                "{tag} (skipping message with id={:?})",
                msg.get("id")
            );
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
    println!("{tag} Server: {server_name} v{server_version}");

    // Send initialized notification
    send_lsp_message(stdin, &serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }))
    .await?;

    // Send shutdown request
    send_lsp_message(stdin, &serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "shutdown",
        "params": null
    }))
    .await?;

    // Send exit notification
    send_lsp_message(stdin, &serde_json::json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    }))
    .await?;

    // Wait for child to exit (with timeout)
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    println!("{tag} Initialize/shutdown cycle completed");
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

/// Read one LSP message from a reader. Parses the `Content-Length` header and
/// reads exactly that many bytes.
async fn read_lsp_message(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let mut header_buf = Vec::new();

    // Read headers byte-by-byte until we see \r\n\r\n
    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);

        if header_buf.len() >= 4
            && header_buf[header_buf.len() - 4..] == *b"\r\n\r\n"
        {
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
// Detect devcontainer config (same logic as sala_hud)
// ---------------------------------------------------------------------------

fn detect_config(workspace_path: &std::path::Path) -> Option<PathBuf> {
    let primary = workspace_path.join(".devcontainer/devcontainer.json");
    if primary.exists() {
        return Some(primary);
    }
    let fallback = workspace_path.join(".devcontainer.json");
    if fallback.exists() {
        return Some(fallback);
    }
    None
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

    let project_name = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    println!("=== Sala DevContainer Smoke Test ===");
    println!("workspace:  {workspace_str}");
    println!("project:    {project_name}");
    println!("language:   {}", lsp_config.display_name);
    println!("timeout:    {}s", build_timeout.as_secs());
    println!();

    let mut results = SmokeResults::new(lsp_config.display_name);

    // -- Connect ----------------------------------------------------------
    let mut client = match connect_daemon().await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[DAEMON]    FAIL: {err}");
            results.devcontainer = Some(StepResult::Fail(format!("connect: {err}")));
            results.terminal = Some(StepResult::Skip("no daemon".into()));
            results.lsp = Some(StepResult::Skip("no daemon".into()));
            results.print_summary();
            std::process::exit(1);
        }
    };
    println!("[DAEMON]    Connected");

    // -- Health + Preflight -----------------------------------------------
    if let Err(err) = step_health_and_preflight(&mut client, &workspace_str).await {
        eprintln!("[PREFLIGHT] FAIL: {err}");
        results.devcontainer = Some(StepResult::Fail(format!("{err}")));
        results.terminal = Some(StepResult::Skip("preflight failed".into()));
        results.lsp = Some(StepResult::Skip("preflight failed".into()));
        results.print_summary();
        std::process::exit(2);
    }

    // -- Detect config ----------------------------------------------------
    let config_path = match detect_config(&workspace_path) {
        Some(p) => p,
        None => {
            eprintln!("[DEVCONTAINER] FAIL: no .devcontainer/devcontainer.json found");
            results.devcontainer = Some(StepResult::Fail(
                "no .devcontainer/devcontainer.json found".into(),
            ));
            results.terminal = Some(StepResult::Skip("no config".into()));
            results.lsp = Some(StepResult::Skip("no config".into()));
            results.print_summary();
            std::process::exit(3);
        }
    };
    let config_str = config_path.to_string_lossy().to_string();
    println!("[DEVCONTAINER] Config: {config_str}");

    // -- Build / Connect --------------------------------------------------
    let container_id =
        match step_devcontainer(&mut client, &workspace_str, &config_str, build_timeout).await {
            Ok(cid) => {
                results.devcontainer = Some(StepResult::Ok(format!(
                    "container {}",
                    &cid[..cid.len().min(12)]
                )));
                cid
            }
            Err(err) => {
                eprintln!("[DEVCONTAINER] FAIL: {err}");
                results.devcontainer = Some(StepResult::Fail(format!("{err}")));
                results.terminal = Some(StepResult::Skip("no container".into()));
                results.lsp = Some(StepResult::Skip("no container".into()));
                results.print_summary();
                std::process::exit(4);
            }
        };

    // -- Terminal ---------------------------------------------------------
    match step_terminal(&container_id).await {
        Ok(()) => {
            results.terminal = Some(StepResult::Ok("echo SMOKE_OK received".into()));
        }
        Err(err) => {
            eprintln!("[TERMINAL]  FAIL: {err}");
            results.terminal = Some(StepResult::Fail(format!("{err}")));
        }
    }

    // -- LSP --------------------------------------------------------------
    let tag = lsp_config.log_tag;
    match step_lsp(&container_id, &project_name, &lsp_config).await {
        Ok(()) => {
            results.lsp = Some(StepResult::Ok("initialize/shutdown ok".into()));
        }
        Err(err) => {
            let msg = format!("{err}");
            if msg.contains("not found in container") {
                println!("{tag} SKIP: {msg}");
                results.lsp = Some(StepResult::Skip(msg));
            } else {
                eprintln!("{tag} FAIL: {msg}");
                results.lsp = Some(StepResult::Fail(msg));
            }
        }
    }

    // -- Summary ----------------------------------------------------------
    results.print_summary();
    std::process::exit(results.exit_code());
}
