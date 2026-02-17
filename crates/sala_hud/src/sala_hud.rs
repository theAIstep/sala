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
    daemon_client: Option<TalaDaemonClient>,
    connection_status: ConnectionStatus,
    devcontainer_state: DevContainerState,
    workspace_path: Option<PathBuf>,
    preflight_passed: bool,
    auto_reopen: bool,
    container_lsp_registered: bool,
    container_lsp_available: Option<bool>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
struct TalaDaemonClient {
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

impl TalaDaemonClient {
    async fn connect() -> Result<Self> {
        #[cfg(unix)]
        let ipc_path = "/tmp/tala.sock".to_string();
        #[cfg(windows)]
        let ipc_path = "\\\\.\\pipe\\tala".to_string();

        log::info!("Attempting to connect to tala daemon at {}", ipc_path);

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

    /// Check whether rust-analyzer is available inside the given container.
    async fn check_lsp_capability(container_id: &str) -> bool {
        log::info!(
            "checking rust-analyzer availability in container {}",
            container_id
        );
        let output = tokio::process::Command::new("docker")
            .args(["exec", container_id, "sh", "-c", "command -v rust-analyzer"])
            .output()
            .await;
        match output {
            Ok(output) => {
                let available = output.status.success();
                if available {
                    log::info!(
                        "rust-analyzer found in container {}",
                        container_id
                    );
                } else {
                    log::warn!(
                        "rust-analyzer not found in container {}",
                        container_id
                    );
                }
                available
            }
            Err(error) => {
                log::error!(
                    "failed to check rust-analyzer in container {}: {}",
                    container_id,
                    error
                );
                false
            }
        }
    }
}

/// LSP adapter that routes rust-analyzer requests through `docker exec` to a
/// running DevContainer. Registered dynamically when a container connects and
/// removed when it disconnects, so the editor falls back to the host
/// rust-analyzer automatically.
struct ContainerRustLspAdapter {
    container_id: String,
    project_name: String,
}

impl ContainerRustLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("rust-analyzer");

    fn new(container_id: String, project_name: String) -> Self {
        Self {
            container_id,
            project_name,
        }
    }

    fn build_binary(&self) -> LanguageServerBinary {
        LanguageServerBinary {
            path: PathBuf::from("docker"),
            arguments: vec![
                OsString::from("exec"),
                OsString::from("-i"),
                OsString::from("-w"),
                OsString::from(format!("/workspaces/{}", self.project_name)),
                OsString::from(&self.container_id),
                OsString::from("rust-analyzer"),
            ],
            env: None,
        }
    }
}

#[async_trait(?Send)]
impl LspAdapter for ContainerRustLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    fn disk_based_diagnostic_sources(&self) -> Vec<String> {
        vec!["rustc".to_owned()]
    }

    fn disk_based_diagnostics_progress_token(&self) -> Option<String> {
        Some("rust-analyzer/flycheck".into())
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([(LanguageName::new("Rust"), "rust".to_string())])
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
            "remapped LSP root to {} for container {}",
            container_path,
            self.container_id
        );

        Ok(original)
    }
}

impl LspInstaller for ContainerRustLspAdapter {
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
            "container-managed rust-analyzer does not support version fetching"
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
            cx.background_spawn(async move { TalaDaemonClient::connect().await });

        let weak_entity = entity.downgrade();
        cx.spawn_in(window, {
            let workspace_path_for_preflight = workspace_path_for_detection;
            async move |_workspace, cx| {
                let detected_config = config_detection_task.await;

                match connection_task.await {
                    Ok(mut client) => match client.health_check().await {
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
                                    cx.notify();
                                })
                                .log_err();
                        }
                        Err(error) => {
                            log::error!("Failed health check with tala daemon: {}", error);
                            weak_entity
                                .update(cx, |this, cx| {
                                    this.connection_status = ConnectionStatus::Disconnected(
                                        format!("Health check failed: {}", error),
                                    );
                                    if let Some(config_path) = detected_config {
                                        this.devcontainer_state =
                                            DevContainerState::Detected { config_path };
                                    }
                                    cx.notify();
                                })
                                .log_err();
                        }
                    },
                    Err(error) => {
                        log::error!("Failed to connect to tala daemon: {}", error);
                        weak_entity
                            .update(cx, |this, cx| {
                                this.connection_status = ConnectionStatus::Disconnected(
                                    format!("Connection failed: {}", error),
                                );
                                if let Some(config_path) = detected_config {
                                    this.devcontainer_state =
                                        DevContainerState::Detected { config_path };
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
        let config_path_string = config_path.to_string_lossy().to_string();
        let workspace_handle = self.workspace.clone();

        self.devcontainer_state = DevContainerState::Building {
            percent: 0,
            message: "Starting build...".to_string(),
        };
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

                                this.update(cx, |hud, cx| {
                                    hud.devcontainer_state = new_state;
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
                                    this.update(cx, |hud, cx| {
                                        hud.register_container_lsp(
                                            &container_id,
                                            &project_name,
                                            cx,
                                        );
                                    })
                                    .log_err();

                                    // Check rust-analyzer availability in background
                                    let container_id_for_check = container_id.clone();
                                    let lsp_check = cx
                                        .background_executor()
                                        .spawn(async move {
                                            TalaDaemonClient::check_lsp_capability(
                                                &container_id_for_check,
                                            )
                                            .await
                                        });
                                    let available = lsp_check.await;
                                    this.update(cx, |hud, cx| {
                                        hud.container_lsp_available = Some(available);
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

                                if is_complete || is_failed {
                                    break;
                                }
                            }
                            Err(status) => {
                                this.update(cx, |hud, cx| {
                                    hud.remove_container_lsp(cx);
                                    hud.devcontainer_state = DevContainerState::Failed {
                                        error: status.message().to_string(),
                                    };
                                    cx.notify();
                                })?;
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    this.update(cx, |hud, cx| {
                        hud.remove_container_lsp(cx);
                        hud.devcontainer_state = DevContainerState::Failed {
                            error: format!("{}", error),
                        };
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
        let adapter = Arc::new(ContainerRustLspAdapter::new(
            container_id.to_string(),
            project_name.to_string(),
        ));
        let language_name: LanguageName = "Rust".into();

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project();
            let languages = project.read(cx).languages();
            languages.register_lsp_adapter(language_name, adapter);
            log::info!(
                "registered container rust-analyzer LSP adapter for container {}",
                container_id
            );

            let rust_buffers: Vec<_> = project
                .read(cx)
                .opened_buffers(cx)
                .into_iter()
                .filter(|buffer| {
                    buffer
                        .read(cx)
                        .language()
                        .map(|lang| lang.name().as_ref() == "Rust")
                        .unwrap_or(false)
                })
                .collect();

            if !rust_buffers.is_empty() {
                log::info!(
                    "restarting rust-analyzer for {} open Rust buffers",
                    rust_buffers.len()
                );
                project.update(cx, |project, cx| {
                    project.restart_language_servers_for_buffers(
                        rust_buffers,
                        collections::HashSet::from_iter([lsp::LanguageServerSelector::Name(
                            ContainerRustLspAdapter::SERVER_NAME,
                        )]),
                        cx,
                    );
                });
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
        let language_name: LanguageName = "Rust".into();
        let server_name = ContainerRustLspAdapter::SERVER_NAME;

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project();
            let languages = project.read(cx).languages();
            languages.remove_lsp_adapter(&language_name, &server_name);
            log::info!("removed container rust-analyzer LSP adapter");

            let rust_buffers: Vec<_> = project
                .read(cx)
                .opened_buffers(cx)
                .into_iter()
                .filter(|buffer| {
                    buffer
                        .read(cx)
                        .language()
                        .map(|lang| lang.name().as_ref() == "Rust")
                        .unwrap_or(false)
                })
                .collect();

            if !rust_buffers.is_empty() {
                log::info!(
                    "restarting rust-analyzer for {} open Rust buffers (falling back to host)",
                    rust_buffers.len()
                );
                project.update(cx, |project, cx| {
                    project.restart_language_servers_for_buffers(
                        rust_buffers,
                        collections::HashSet::from_iter([lsp::LanguageServerSelector::Name(
                            server_name,
                        )]),
                        cx,
                    );
                });
            }
        });
        self.container_lsp_registered = false;
        self.container_lsp_available = None;
    }

    fn dismiss_devcontainer(&mut self, cx: &mut Context<Self>) {
        self.remove_container_lsp(cx);
        self.devcontainer_state = DevContainerState::NotDetected;
        cx.notify();
    }

    fn retry_build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_container_lsp(cx);
        if let Some(workspace_path) = &self.workspace_path {
            let workspace_path = workspace_path.clone();
            if let Some(config_path) = detect_devcontainer_config(&workspace_path) {
                self.devcontainer_state = DevContainerState::Detected { config_path };
                cx.notify();
                self.start_build(window, cx);
            }
        }
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
                ("Connecting to tala daemon...".to_string(), Color::Muted)
            }
            ConnectionStatus::Connected(version) => (
                format!("Connected to tala daemon v{}", version),
                Color::Success,
            ),
            ConnectionStatus::Disconnected(error) => (
                format!("Not connected to tala daemon: {}", error),
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
