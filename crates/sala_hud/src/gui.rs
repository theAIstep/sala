use crate::devcontainer_hud::DevContainerHud;

use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use futures::StreamExt;
use gpui::{
    actions, prelude::*, Action, App, AsyncApp, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Pixels, Render, Subscription, WeakEntity, Window,
};
use language::{
    LanguageName, LanguageServerName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain,
};
use lsp::LanguageServerBinary;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use task::{HideStrategy, RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{prelude::*, Button, ButtonStyle, IconName, Label, Tooltip};
use util::ResultExt;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub mod devcontainer {
    tonic::include_proto!("devcontainer");
}

actions!(sala_hud, [ToggleHud, StartDevContainer, OpenContainerTerminal]);

const SALA_HUD_KEY: &str = "SalaHud";
const DEFAULT_WIDTH: Pixels = px(300.);

enum ConnectionStatus {
    Connecting,
    Connected(String),
    Disconnected(String),
}

enum DevContainerState {
    NotDetected,
    Detected { config_path: PathBuf },
    Building { percent: i32, message: String },
    Connected { container_id: String },
    Failed { error: String },
}

pub struct SalaHud {
    width: Option<Pixels>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    daemon_client: Option<DevContainerDaemonClient>,
    connection_status: ConnectionStatus,
    devcontainer_state: DevContainerState,
    workspace_path: Option<PathBuf>,
    preflight_passed: bool,
    auto_reopen: bool,
    container_lsp_registered: bool,
    container_lsp_available: Option<bool>,
    hud: DevContainerHud,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
struct DevContainerDaemonClient {
    client: devcontainer::dev_container_service_client::DevContainerServiceClient<
        tonic::transport::Channel,
    >,
}

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

impl DevContainerDaemonClient {
    async fn connect() -> Result<Self> {
        #[cfg(unix)]
        let ipc_path = "/tmp/tais-devcontainerd.sock".to_string();
        #[cfg(windows)]
        let ipc_path = "\\\\.\\pipe\\tais-devcontainerd".to_string();

        log::info!("Attempting to connect to tais-devcontainerd daemon at {}", ipc_path);

        // tonic/hyper use tokio internally for the transport layer.
        // When running under a non-tokio executor (e.g. GPUI tests on
        // smol), bail gracefully instead of panicking in UnixStream.
        if tokio::runtime::Handle::try_current().is_err() {
            anyhow::bail!(
                "no tokio runtime available (connection to {} requires tokio)",
                ipc_path,
            );
        }

        let channel = ipc::create_channel(ipc_path).await?;
        let client =
            devcontainer::dev_container_service_client::DevContainerServiceClient::new(channel);

        Ok(Self { client })
    }

    async fn health_check(&mut self) -> Result<(String, bool)> {
        let request = tonic::Request::new(devcontainer::HealthCheckRequest {});
        let response = self.client.health_check(request).await?;
        let inner = response.into_inner();
        Ok((inner.version, inner.healthy))
    }

    async fn preflight_check(
        &mut self,
        workspace_path: &str,
    ) -> Result<devcontainer::PreflightResponse> {
        let request = tonic::Request::new(devcontainer::PreflightRequest {
            workspace_path: workspace_path.to_string(),
        });
        let response = self.client.preflight_check(request).await?;
        Ok(response.into_inner())
    }

    async fn build_container(
        &mut self,
        workspace_path: &str,
        config_path: &str,
        force_rebuild: bool,
    ) -> Result<tonic::Streaming<devcontainer::BuildProgress>> {
        let request = tonic::Request::new(devcontainer::BuildRequest {
            workspace_path: workspace_path.to_string(),
            config_path: config_path.to_string(),
            force_rebuild,
        });
        let response = self.client.build_container(request).await?;
        Ok(response.into_inner())
    }

    /// Check whether a given binary is available inside the container.
    async fn check_lsp_capability(container_id: &str, binary: &str) -> bool {
        log::info!(
            "checking {} availability in container {}",
            binary,
            container_id
        );
        let check_cmd = format!("command -v {binary}");
        let output = tokio::process::Command::new("docker")
            .args(["exec", container_id, "sh", "-c", &check_cmd])
            .output()
            .await;
        match output {
            Ok(output) => {
                let available = output.status.success();
                if available {
                    log::info!("{} found in container {}", binary, container_id);
                } else {
                    log::warn!("{} not found in container {}", binary, container_id);
                }
                available
            }
            Err(error) => {
                log::error!(
                    "failed to check {} in container {}: {}",
                    binary,
                    container_id,
                    error
                );
                false
            }
        }
    }
}

/// Check whether the container terminal is functional by running `echo SMOKE_OK`.
async fn check_terminal_smoke(container_id: &str) -> bool {
    log::info!(
        "checking terminal availability in container {}",
        container_id
    );
    let output = tokio::process::Command::new("docker")
        .args(["exec", container_id, "echo", "SMOKE_OK"])
        .output()
        .await;
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let ok = output.status.success() && stdout.contains("SMOKE_OK");
            if ok {
                log::info!(
                    "terminal smoke check passed for container {}",
                    container_id
                );
            } else {
                log::warn!(
                    "terminal smoke check failed for container {}",
                    container_id
                );
            }
            ok
        }
        Err(error) => {
            log::error!(
                "terminal smoke check error for container {}: {}",
                container_id,
                error
            );
            false
        }
    }
}

/// Generic LSP adapter that routes language server requests through
/// `docker exec` to a running DevContainer. Configured per-language via
/// constructor parameters. Registered dynamically when a container connects
/// and removed when it disconnects, so the editor falls back to the host
/// language server automatically.
struct ContainerLspAdapter {
    container_id: String,
    project_name: String,
    server_name: LanguageServerName,
    server_binary: String,
    server_args: Vec<String>,
    language_name: LanguageName,
    lsp_language_id: String,
    diagnostic_sources: Vec<String>,
    diagnostics_progress_token: Option<String>,
}

impl ContainerLspAdapter {
    fn rust(container_id: String, project_name: String) -> Self {
        Self {
            container_id,
            project_name,
            server_name: LanguageServerName::new_static("rust-analyzer"),
            server_binary: "rust-analyzer".to_string(),
            server_args: vec![],
            language_name: LanguageName::new("Rust"),
            lsp_language_id: "rust".to_string(),
            diagnostic_sources: vec!["rustc".to_owned()],
            diagnostics_progress_token: Some("rust-analyzer/flycheck".into()),
        }
    }

    fn python(container_id: String, project_name: String) -> Self {
        Self {
            container_id,
            project_name,
            server_name: LanguageServerName::new_static("pyright"),
            server_binary: "pyright-langserver".to_string(),
            server_args: vec!["--stdio".to_string()],
            language_name: LanguageName::new("Python"),
            lsp_language_id: "python".to_string(),
            diagnostic_sources: vec![],
            diagnostics_progress_token: None,
        }
    }

    fn build_binary(&self) -> LanguageServerBinary {
        let mut arguments = vec![
            OsString::from("exec"),
            OsString::from("-i"),
            OsString::from("-w"),
            OsString::from(format!("/workspaces/{}", self.project_name)),
            OsString::from(&self.container_id),
            OsString::from(&self.server_binary),
        ];
        for arg in &self.server_args {
            arguments.push(OsString::from(arg));
        }
        LanguageServerBinary {
            path: PathBuf::from("docker"),
            arguments,
            env: None,
        }
    }
}

#[async_trait(?Send)]
impl LspAdapter for ContainerLspAdapter {
    fn name(&self) -> LanguageServerName {
        self.server_name.clone()
    }

    fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        self.diagnostic_sources.clone()
    }

    fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        self.diagnostics_progress_token.clone()
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([(
            self.language_name.clone(),
            self.lsp_language_id.clone(),
        )])
    }

    fn prepare_initialize_params(
        &self,
        mut original: lsp::InitializeParams,
        _: &App,
    ) -> Result<lsp::InitializeParams> {
        let container_path = format!("/workspaces/{}", self.project_name);
        let container_uri = lsp::Uri::from_file_path(&container_path)
            .map_err(|()| anyhow::anyhow!("failed to construct container URI from {}", container_path))?;

        #[allow(deprecated)]
        {
            original.root_uri = Some(container_uri.clone());
            original.root_path = Some(container_path.clone());
        }

        if let Some(ref mut folders) = original.workspace_folders {
            for folder in folders.iter_mut() {
                let original_path: &str = &folder.uri.path();
                let tail = original_path
                    .rsplit('/')
                    .find(|segment| !segment.is_empty())
                    .unwrap_or("");
                let remapped_path = if tail.is_empty() {
                    container_path.clone()
                } else {
                    format!("{}/{}", container_path, tail)
                };
                folder.uri = lsp::Uri::from_file_path(&remapped_path).map_err(|()| {
                    anyhow::anyhow!(
                        "failed to construct workspace folder URI from {}",
                        remapped_path
                    )
                })?;
            }
        }

        log::info!(
            "remapped LSP root to {} for container {} ({})",
            container_path,
            self.container_id,
            self.server_binary
        );

        Ok(original)
    }
}

impl LspInstaller for ContainerLspAdapter {
    type BinaryVersion = ();

    async fn check_if_user_installed(
        &self,
        _delegate: &dyn LspAdapterDelegate,
        _toolchain: Option<Toolchain>,
        _cx: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        Some(self.build_binary())
    }

    async fn fetch_latest_server_version(
        &self,
        _delegate: &dyn LspAdapterDelegate,
        _pre_release: bool,
        _cx: &mut AsyncApp,
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "container-managed {} does not support version fetching",
            self.server_binary
        ))
    }

    async fn fetch_server_binary(
        &self,
        _version: (),
        _container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        Ok(self.build_binary())
    }

    async fn cached_server_binary(
        &self,
        _container_dir: PathBuf,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        Some(self.build_binary())
    }
}

/// Encapsulates logic for opening interactive terminals inside a running
/// DevContainer via `docker exec`.  Today this spawns a local process
/// (`docker exec -it <cid> <shell>`); the interface is designed so that a
/// future gRPC-backed implementation can be swapped in transparently.
struct ContainerTerminalService;

impl ContainerTerminalService {
    /// Build a `SpawnInTerminal` that opens an interactive shell inside the
    /// given container.  The caller is responsible for handing this to
    /// `TerminalPanel::spawn_task`.
    fn spawn_task_for_container(container_id: &str, container_shell: &str) -> SpawnInTerminal {
        SpawnInTerminal {
            id: TaskId(format!("devcontainer-terminal-{container_id}")),
            full_label: format!("DevContainer ({container_id})"),
            label: "DevContainer".to_string(),
            command: Some("docker".to_string()),
            args: vec![
                "exec".to_string(),
                "-it".to_string(),
                container_id.to_string(),
                container_shell.to_string(),
            ],
            command_label: format!(
                "docker exec -it {} {}",
                container_id, container_shell
            ),
            cwd: None,
            env: Default::default(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            hide: HideStrategy::Never,
            shell: Shell::WithArguments {
                program: "docker".to_string(),
                args: vec![
                    "exec".to_string(),
                    "-it".to_string(),
                    container_id.to_string(),
                    container_shell.to_string(),
                ],
                title_override: Some("DevContainer".to_string()),
            },
            show_summary: false,
            show_command: false,
            show_rerun: false,
        }
    }

    /// Open a terminal tab in the bottom panel for the given container.
    /// Must be called inside a `Workspace` update context (i.e. the caller
    /// has `&mut Workspace` or an action handler for `Workspace`).
    fn open_terminal_in_workspace(
        container_id: &str,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let task = Self::spawn_task_for_container(container_id, "/bin/bash");

        let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
            log::error!("ContainerTerminalService: terminal panel not registered");
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| panel.spawn_task(&task, window, cx))
            .detach_and_log_err(cx);
    }
}

fn detect_devcontainer_config(workspace_path: &std::path::Path) -> Option<PathBuf> {
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

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleHud, window, cx| {
            workspace.toggle_panel_focus::<SalaHud>(window, cx);
        });
        workspace.register_action(|workspace, _: &StartDevContainer, window, cx| {
            if let Some(panel) = workspace.panel::<SalaHud>(cx) {
                panel.update(cx, |hud, cx| {
                    hud.start_build(window, cx);
                });
            }
        });
        workspace.register_action(|workspace, _: &OpenContainerTerminal, window, cx| {
            let container_id = workspace
                .panel::<SalaHud>(cx)
                .and_then(|panel| panel.read(cx).connected_container_id());
            if let Some(container_id) = container_id {
                ContainerTerminalService::open_terminal_in_workspace(
                    &container_id,
                    workspace,
                    window,
                    cx,
                );
            }
        });
    })
    .detach();
}

impl SalaHud {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = cx.entity().downgrade();

        let workspace_path = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .and_then(|worktree| {
                let worktree = worktree.read(cx);
                if worktree.is_single_file() {
                    worktree.abs_path().parent().map(|p| p.to_path_buf())
                } else {
                    Some(worktree.abs_path().to_path_buf())
                }
            });

        let workspace_path_for_detection = workspace_path.clone();

        let entity = cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            Self {
                width: Some(DEFAULT_WIDTH),
                workspace: workspace_handle,
                focus_handle,
                daemon_client: None,
                connection_status: ConnectionStatus::Connecting,
                devcontainer_state: DevContainerState::NotDetected,
                workspace_path,
                preflight_passed: false,
                auto_reopen: false,
                container_lsp_registered: false,
                container_lsp_available: None,
                hud: DevContainerHud::new(),
                _subscriptions: Vec::new(),
            }
        });

        let config_detection_task = cx.background_spawn({
            let workspace_path = workspace_path_for_detection.clone();
            async move {
                workspace_path.and_then(|path| detect_devcontainer_config(&path))
            }
        });

        let connection_task =
            cx.background_spawn(async move { DevContainerDaemonClient::connect().await });

        let weak_entity = entity.downgrade();
        cx.spawn_in(window, {
            let workspace_path_for_preflight = workspace_path_for_detection;
            async move |_workspace, cx| {
                let detected_config = config_detection_task.await;

                match connection_task.await {
                    Ok(mut client) => {
                        // HUD: mark preflight phase
                        weak_entity
                            .update(cx, |this, _cx| {
                                if let Some(ws_path) = &this.workspace_path {
                                    let project = ws_path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "workspace".to_string());
                                    this.hud
                                        .on_preflight_started(ws_path.clone(), project);
                                }
                            })
                            .log_err();

                        match client.health_check().await {
                        Ok((version, healthy)) if healthy => {
                            let preflight_result =
                                if detected_config.is_some() {
                                    if let Some(ref ws_path) = workspace_path_for_preflight {
                                        client
                                            .preflight_check(&ws_path.to_string_lossy())
                                            .await
                                            .ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                            weak_entity
                                .update(cx, |this, cx| {
                                    this.connection_status =
                                        ConnectionStatus::Connected(version);

                                    let preflight_passed = preflight_result
                                        .as_ref()
                                        .map(|r| r.docker_available && r.docker_permissions_ok)
                                        .unwrap_or(false);
                                    this.preflight_passed = preflight_passed;

                                    if let Some(config_path) = detected_config {
                                        this.devcontainer_state =
                                            DevContainerState::Detected { config_path };
                                    }

                                    this.daemon_client = Some(client);
                                    cx.notify();
                                })
                                .log_err();
                        }
                        Ok((_, _)) => {
                            weak_entity
                                .update(cx, |this, cx| {
                                    this.connection_status = ConnectionStatus::Disconnected(
                                        "Daemon unhealthy".to_string(),
                                    );
                                    if let Some(config_path) = detected_config {
                                        this.devcontainer_state =
                                            DevContainerState::Detected { config_path };
                                    }
                                    if let Some(ws) = &this.workspace_path {
                                        this.hud.on_error(
                                            ws,
                                            "Daemon unhealthy".to_string(),
                                        );
                                    }
                                    cx.notify();
                                })
                                .log_err();
                        }
                        Err(error) => {
                            log::error!("Failed health check with tais-devcontainerd daemon: {}", error);
                            let error_msg =
                                format!("Health check failed: {}", error);
                            weak_entity
                                .update(cx, |this, cx| {
                                    this.connection_status =
                                        ConnectionStatus::Disconnected(error_msg.clone());
                                    if let Some(config_path) = detected_config {
                                        this.devcontainer_state =
                                            DevContainerState::Detected { config_path };
                                    }
                                    if let Some(ws) = &this.workspace_path {
                                        this.hud.on_error(ws, error_msg);
                                    }
                                    cx.notify();
                                })
                                .log_err();
                        }
                    }
                    },
                    Err(error) => {
                        log::error!("Failed to connect to tais-devcontainerd daemon: {}", error);
                        let error_msg = format!("Connection failed: {}", error);
                        weak_entity
                            .update(cx, |this, cx| {
                                this.connection_status =
                                    ConnectionStatus::Disconnected(error_msg.clone());
                                if let Some(config_path) = detected_config {
                                    this.devcontainer_state =
                                        DevContainerState::Detected { config_path };
                                }
                                if let Some(ws) = &this.workspace_path {
                                    this.hud.on_error(ws, error_msg);
                                }
                                cx.notify();
                            })
                            .log_err();
                    }
                }
            }
        })
        .detach();

        entity
    }

    /// Returns the container ID if the DevContainer is currently connected.
    fn connected_container_id(&self) -> Option<String> {
        match &self.devcontainer_state {
            DevContainerState::Connected { container_id } => Some(container_id.clone()),
            _ => None,
        }
    }

    fn start_build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace_path) = &self.workspace_path else {
            return;
        };
        let config_path = match &self.devcontainer_state {
            DevContainerState::Detected { config_path } => config_path.clone(),
            _ => return,
        };
        if !self.preflight_passed {
            return;
        }
        let Some(mut client) = self.daemon_client.clone() else {
            return;
        };

        let project_name = workspace_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        let workspace_path_string = workspace_path.to_string_lossy().to_string();
        let workspace_path_buf = workspace_path.clone();
        let config_path_string = config_path.to_string_lossy().to_string();
        let workspace_handle = self.workspace.clone();

        self.devcontainer_state = DevContainerState::Building {
            percent: 0,
            message: "Starting build...".to_string(),
        };
        self.hud
            .on_build_progress(&workspace_path_buf, 0, "Starting build...".to_string());
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let stream_result = client
                .build_container(&workspace_path_string, &config_path_string, false)
                .await;

            match stream_result {
                Ok(mut stream) => {
                    while let Some(progress) = stream.next().await {
                        match progress {
                            Ok(build_progress) => {
                                let stage = build_progress.stage();
                                let is_complete = matches!(
                                    stage,
                                    devcontainer::build_progress::Stage::Complete
                                );
                                let is_failed = matches!(
                                    stage,
                                    devcontainer::build_progress::Stage::Failed
                                );

                                // Extract values for HUD before they are moved
                                let bp_percent = build_progress.percent;
                                let bp_message = build_progress.message.clone();
                                let bp_error = build_progress.error.clone();

                                // The build_progress.message on Complete contains
                                // "Container ready: <container_id>" — extract the id.
                                let resolved_container_id = if is_complete {
                                    Some(
                                        build_progress
                                            .message
                                            .strip_prefix("Container ready: ")
                                            .unwrap_or(&build_progress.message)
                                            .to_string(),
                                    )
                                } else {
                                    None
                                };

                                let hud_container_id = resolved_container_id.clone();
                                let hud_workspace = workspace_path_buf.clone();

                                let new_state = if let Some(ref container_id) = resolved_container_id {
                                    DevContainerState::Connected {
                                        container_id: container_id.clone(),
                                    }
                                } else if is_failed {
                                    DevContainerState::Failed {
                                        error: if build_progress.error.is_empty() {
                                            build_progress.message
                                        } else {
                                            build_progress.error
                                        },
                                    }
                                } else {
                                    DevContainerState::Building {
                                        percent: build_progress.percent,
                                        message: build_progress.message,
                                    }
                                };

                                this.update(cx, |panel, cx| {
                                    panel.devcontainer_state = new_state;

                                    // HUD state tracking
                                    if let Some(container_id) = hud_container_id {
                                        panel.hud.on_container_started(
                                            &hud_workspace,
                                            container_id,
                                        );
                                    } else if is_failed {
                                        let err = if bp_error.is_empty() {
                                            bp_message.clone()
                                        } else {
                                            bp_error
                                        };
                                        panel.hud.on_error(&hud_workspace, err);
                                    } else {
                                        panel.hud.on_build_progress(
                                            &hud_workspace,
                                            bp_percent as u8,
                                            bp_message.clone(),
                                        );
                                    }

                                    cx.notify();
                                })?;

                                // Auto-open terminal and register container LSP when build completes
                                if let Some(container_id) = resolved_container_id {
                                    workspace_handle
                                        .update_in(cx, |workspace, window, cx| {
                                            ContainerTerminalService::open_terminal_in_workspace(
                                                &container_id,
                                                workspace,
                                                window,
                                                cx,
                                            );
                                        })
                                        .log_err();

                                    // Register container LSP adapter
                                    let project_name = project_name.clone();
                                    this.update(cx, |panel, cx| {
                                        panel.register_container_lsp(
                                            &container_id,
                                            &project_name,
                                            cx,
                                        );
                                    })
                                    .log_err();

                                    // Check rust-analyzer availability in background
                                    {
                                        let container_id_for_check = container_id.clone();
                                        let lsp_check = cx
                                            .background_executor()
                                            .spawn(async move {
                                                DevContainerDaemonClient::check_lsp_capability(
                                                    &container_id_for_check,
                                                    "rust-analyzer",
                                                )
                                                .await
                                            });
                                        let available = lsp_check.await;
                                        let hud_ws = workspace_path_buf.clone();
                                        this.update(cx, |panel, cx| {
                                            panel.container_lsp_available =
                                                Some(available);
                                            panel
                                                .hud
                                                .on_lsp_rust_result(&hud_ws, available);
                                            if !available {
                                                log::warn!(
                                                    "rust-analyzer not found in container; \
                                                     LSP features may be unavailable"
                                                );
                                            }
                                            cx.notify();
                                        })
                                        .log_err();
                                    }

                                    // Check pyright availability in background
                                    {
                                        let container_id_for_check = container_id.clone();
                                        let python_check = cx
                                            .background_executor()
                                            .spawn(async move {
                                                DevContainerDaemonClient::check_lsp_capability(
                                                    &container_id_for_check,
                                                    "pyright-langserver",
                                                )
                                                .await
                                            });
                                        let python_available = python_check.await;
                                        let hud_ws = workspace_path_buf.clone();
                                        this.update(cx, |panel, cx| {
                                            panel.hud.on_lsp_python_result(
                                                &hud_ws,
                                                python_available,
                                            );
                                            cx.notify();
                                        })
                                        .log_err();
                                    }

                                    // Terminal smoke check in background
                                    {
                                        let container_id_for_check = container_id.clone();
                                        let terminal_check = cx
                                            .background_executor()
                                            .spawn(async move {
                                                check_terminal_smoke(
                                                    &container_id_for_check,
                                                )
                                                .await
                                            });
                                        let terminal_ok = terminal_check.await;
                                        let hud_ws = workspace_path_buf.clone();
                                        this.update(cx, |panel, cx| {
                                            panel.hud.on_terminal_smoke_result(
                                                &hud_ws,
                                                terminal_ok,
                                            );
                                            cx.notify();
                                        })
                                        .log_err();
                                    }
                                }

                                if is_complete || is_failed {
                                    break;
                                }
                            }
                            Err(status) => {
                                let error_msg = status.message().to_string();
                                let hud_ws = workspace_path_buf.clone();
                                this.update(cx, |panel, cx| {
                                    panel.remove_container_lsp(cx);
                                    panel.devcontainer_state = DevContainerState::Failed {
                                        error: error_msg.clone(),
                                    };
                                    panel.hud.on_error(&hud_ws, error_msg);
                                    cx.notify();
                                })?;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    let error_msg = format!("{}", error);
                    let hud_ws = workspace_path_buf.clone();
                    this.update(cx, |panel, cx| {
                        panel.remove_container_lsp(cx);
                        panel.devcontainer_state = DevContainerState::Failed {
                            error: error_msg.clone(),
                        };
                        panel.hud.on_error(&hud_ws, error_msg);
                        cx.notify();
                    })
                    .log_err();
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn register_container_lsp(
        &mut self,
        container_id: &str,
        project_name: &str,
        cx: &mut Context<Self>,
    ) {
        if self.container_lsp_registered {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let adapters: Vec<(LanguageName, Arc<dyn LspAdapter>)> = vec![
            (
                LanguageName::new("Rust"),
                Arc::new(ContainerLspAdapter::rust(
                    container_id.to_string(),
                    project_name.to_string(),
                )),
            ),
            (
                LanguageName::new("Python"),
                Arc::new(ContainerLspAdapter::python(
                    container_id.to_string(),
                    project_name.to_string(),
                )),
            ),
        ];

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project();
            let languages = project.read(cx).languages();

            for (language_name, adapter) in &adapters {
                let server_name = adapter.name();
                languages.register_lsp_adapter(language_name.clone(), adapter.clone());
                log::info!(
                    "registered container {} LSP adapter for container {}",
                    server_name.0,
                    container_id
                );
            }

            // Restart language servers for any open buffers in registered languages
            for (language_name, adapter) in &adapters {
                let lang_ref = language_name.as_ref();
                let buffers: Vec<_> = project
                    .read(cx)
                    .opened_buffers(cx)
                    .into_iter()
                    .filter(|buffer| {
                        buffer
                            .read(cx)
                            .language()
                            .map(|lang| lang.name().as_ref() == lang_ref)
                            .unwrap_or(false)
                    })
                    .collect();

                if !buffers.is_empty() {
                    let server_name = adapter.name();
                    log::info!(
                        "restarting {} for {} open {} buffers",
                        server_name.0,
                        buffers.len(),
                        lang_ref
                    );
                    project.update(cx, |project, cx| {
                        project.restart_language_servers_for_buffers(
                            buffers,
                            collections::HashSet::from_iter([
                                lsp::LanguageServerSelector::Name(server_name),
                            ]),
                            cx,
                        );
                    });
                }
            }
        });
        self.container_lsp_registered = true;
    }

    fn remove_container_lsp(&mut self, cx: &mut Context<Self>) {
        if !self.container_lsp_registered {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let entries: &[(&str, LanguageServerName)] = &[
            ("Rust", LanguageServerName::new_static("rust-analyzer")),
            ("Python", LanguageServerName::new_static("pyright")),
        ];

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project();

            // Remove adapters first (borrows languages immutably)
            {
                let languages = project.read(cx).languages();
                for (lang_str, server_name) in entries {
                    let language_name = LanguageName::new(lang_str);
                    languages.remove_lsp_adapter(&language_name, server_name);
                    log::info!("removed container {} LSP adapter", server_name.0);
                }
            }

            // Restart affected buffers (borrows project mutably)
            for (lang_str, server_name) in entries {
                let buffers: Vec<_> = project
                    .read(cx)
                    .opened_buffers(cx)
                    .into_iter()
                    .filter(|buffer| {
                        buffer
                            .read(cx)
                            .language()
                            .map(|lang| lang.name().as_ref() == *lang_str)
                            .unwrap_or(false)
                    })
                    .collect();

                if !buffers.is_empty() {
                    log::info!(
                        "restarting {} for {} open {} buffers (falling back to host)",
                        server_name.0,
                        buffers.len(),
                        lang_str
                    );
                    project.update(cx, |project, cx| {
                        project.restart_language_servers_for_buffers(
                            buffers,
                            collections::HashSet::from_iter([
                                lsp::LanguageServerSelector::Name(server_name.clone()),
                            ]),
                            cx,
                        );
                    });
                }
            }
        });
        self.container_lsp_registered = false;
        self.container_lsp_available = None;
    }

    fn dismiss_devcontainer(&mut self, cx: &mut Context<Self>) {
        self.remove_container_lsp(cx);
        if let Some(ws) = &self.workspace_path {
            self.hud.remove_workspace(ws);
        }
        self.devcontainer_state = DevContainerState::NotDetected;
        cx.notify();
    }

    fn retry_build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_container_lsp(cx);
        if let Some(ws) = &self.workspace_path {
            self.hud.remove_workspace(ws);
        }
        if let Some(workspace_path) = &self.workspace_path {
            let workspace_path = workspace_path.clone();
            if let Some(config_path) = detect_devcontainer_config(&workspace_path) {
                self.devcontainer_state = DevContainerState::Detected { config_path };
                cx.notify();
                self.start_build(window, cx);
            }
        }
    }

    /// Dump HUD status for all tracked workspaces to the log.
    #[allow(dead_code)]
    fn dump_hud_status(&self) {
        let table = self.hud.format_status_table();
        log::info!("[HUD STATUS]\n{}", table);
    }

    fn render_devcontainer_section(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        match &self.devcontainer_state {
            DevContainerState::NotDetected => None,

            DevContainerState::Detected { config_path } => {
                let config_display = config_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "devcontainer.json".to_string());

                let docker_unavailable = !self.preflight_passed;
                let tooltip_text = if docker_unavailable {
                    "Docker is not available or lacks permissions"
                } else {
                    "Build and open workspace in DevContainer"
                };

                Some(
                    v_flex()
                        .gap_2()
                        .child(
                            Label::new(format!(
                                "DevContainer detected: {}",
                                config_display
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("reopen", "Reopen in Container")
                                        .style(ButtonStyle::Filled)
                                        .disabled(docker_unavailable)
                                        .tooltip(Tooltip::text(tooltip_text))
                                        .on_click(cx.listener(
                                            |this, _event, window, cx| {
                                                this.start_build(window, cx);
                                            },
                                        )),
                                )
                                .child(
                                    Button::new("ignore", "Ignore")
                                        .style(ButtonStyle::Subtle)
                                        .on_click(cx.listener(
                                            |this, _event, _window, cx| {
                                                this.dismiss_devcontainer(cx);
                                            },
                                        )),
                                )
                                .child(
                                    Button::new("always", "Always")
                                        .style(ButtonStyle::Subtle)
                                        .disabled(docker_unavailable)
                                        .tooltip(Tooltip::text(
                                            "Always reopen in container for this workspace",
                                        ))
                                        .on_click(cx.listener(
                                            |this, _event, window, cx| {
                                                this.auto_reopen = true;
                                                this.start_build(window, cx);
                                            },
                                        )),
                                ),
                        ),
                )
            }

            DevContainerState::Building { percent, message } => {
                let percent = *percent;
                let progress_text = format!("Building... {}%", percent);
                let message = message.clone();

                Some(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::ArrowCircle)
                                        .size(IconSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(progress_text)
                                        .size(LabelSize::Small)
                                        .color(Color::Accent),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(4.))
                                .rounded_sm()
                                .bg(cx.theme().colors().border)
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_sm()
                                        .bg(cx.theme().colors().icon_accent)
                                        .w(relative(percent as f32 / 100.0)),
                                ),
                        )
                        .child(
                            Label::new(message)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            }

            DevContainerState::Connected { container_id } => {
                let display_id = if container_id.len() > 12 {
                    &container_id[..12]
                } else {
                    container_id
                };

                let lsp_available = self.container_lsp_available;

                Some(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                                .child(
                                    Label::new(format!("Container started: {}", display_id))
                                        .size(LabelSize::Small)
                                        .color(Color::Success),
                                ),
                        )
                        .when(lsp_available == Some(false), |this| {
                            this.child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::Warning)
                                            .size(IconSize::Small)
                                            .color(Color::Warning),
                                    )
                                    .child(
                                        Label::new(
                                            "rust-analyzer not found in container. \
                                             Install it to enable code intelligence.",
                                        )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                    ),
                            )
                        })
                        .child(
                            Button::new("open-terminal", "Open Terminal")
                                .style(ButtonStyle::Filled)
                                .tooltip(Tooltip::text(
                                    "Open an interactive terminal inside the DevContainer",
                                ))
                                .on_click(cx.listener(
                                    |this, _event, window, cx| {
                                        let Some(container_id) = this.connected_container_id()
                                        else {
                                            return;
                                        };
                                        let workspace = this.workspace.clone();
                                        if let Some(workspace) = workspace.upgrade() {
                                            workspace.update(cx, |workspace, cx| {
                                                ContainerTerminalService::open_terminal_in_workspace(
                                                    &container_id,
                                                    workspace,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                    },
                                )),
                        ),
                )
            }

            DevContainerState::Failed { error } => {
                let error = error.clone();

                Some(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::XCircle)
                                        .size(IconSize::Small)
                                        .color(Color::Error),
                                )
                                .child(
                                    Label::new("Container build failed")
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                        )
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("retry", "Retry")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.retry_build(window, cx);
                                })),
                        ),
                )
            }
        }
    }
}

impl Panel for SalaHud {
    fn persistent_name() -> &'static str {
        "SalaHud"
    }

    fn panel_key() -> &'static str {
        SALA_HUD_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.width.unwrap_or(DEFAULT_WIDTH)
    }

    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        cx.notify();
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Holographic HUD")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleHud)
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}

impl Focusable for SalaHud {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SalaHud {}

impl ConnectionStatus {
    fn is_disconnected(&self) -> bool {
        matches!(self, ConnectionStatus::Disconnected(_))
    }

    fn disconnected_message(&self) -> Option<&str> {
        match self {
            ConnectionStatus::Disconnected(message) => Some(message),
            _ => None,
        }
    }
}

impl Render for SalaHud {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (connection_text, connection_color) = match &self.connection_status {
            ConnectionStatus::Connecting => {
                ("Connecting to tais-devcontainerd daemon...".to_string(), Color::Muted)
            }
            ConnectionStatus::Connected(version) => (
                format!("Connected to tais-devcontainerd daemon v{}", version),
                Color::Success,
            ),
            ConnectionStatus::Disconnected(error) => (
                format!("Not connected to tais-devcontainerd daemon: {}", error),
                Color::Warning,
            ),
        };

        let devcontainer_section = self.render_devcontainer_section(window, cx);

        v_flex()
            .id("sala-hud-panel")
            .size_full()
            .p_4()
            .gap_2()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Sparkle))
                    .child(Label::new("Holographic HUD").size(LabelSize::Large)),
            )
            .child(
                Label::new(connection_text)
                    .size(LabelSize::Small)
                    .color(connection_color),
            )
            .when_some(devcontainer_section, |this, section| this.child(section))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
    }

    #[gpui::test]
    async fn test_sala_hud_attempts_tala_connection(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let window = cx.add_window(|window, cx| {
            MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            SalaHud::new(workspace, window, cx)
        });

        panel.read_with(cx, |hud, _| {
            assert!(
                matches!(hud.connection_status, ConnectionStatus::Connecting),
                "panel should start in Connecting state"
            );
        });

        cx.run_until_parked();

        panel.read_with(cx, |hud, _| {
            assert!(
                hud.connection_status.is_disconnected(),
                "connection should have failed with no daemon running, got {:?}",
                match &hud.connection_status {
                    ConnectionStatus::Connecting => "Connecting",
                    ConnectionStatus::Connected(v) => v.as_str(),
                    ConnectionStatus::Disconnected(e) => e.as_str(),
                }
            );
            let message = hud
                .connection_status
                .disconnected_message()
                .expect("should have a disconnect message");
            assert!(
                message.contains("Connection failed"),
                "expected 'Connection failed' in message, got: {message}"
            );
        });
    }

    #[gpui::test]
    async fn test_sala_hud_detects_devcontainer_not_present(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;

        let window = cx.add_window(|window, cx| {
            MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            SalaHud::new(workspace, window, cx)
        });

        cx.run_until_parked();

        panel.read_with(cx, |hud, _| {
            assert!(
                matches!(hud.devcontainer_state, DevContainerState::NotDetected),
                "should not detect devcontainer when none exists"
            );
            assert!(
                !hud.preflight_passed,
                "preflight should not pass without daemon"
            );
        });
    }
}
