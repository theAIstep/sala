use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Phase of a DevContainer lifecycle.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase")]
pub enum DevContainerPhase {
    Idle,
    Preflight {
        #[serde(skip)]
        started_at: Instant,
    },
    Building {
        #[serde(skip)]
        started_at: Instant,
        progress: u8,
        message: String,
    },
    Running {
        #[serde(skip)]
        started_at: Instant,
    },
    Error {
        #[serde(skip)]
        at: Instant,
        message: String,
    },
}

impl DevContainerPhase {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub fn is_preflight(&self) -> bool {
        matches!(self, Self::Preflight { .. })
    }

    pub fn is_building(&self) -> bool {
        matches!(self, Self::Building { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Preflight { .. } => "Preflight",
            Self::Building { .. } => "Building",
            Self::Running { .. } => "Running",
            Self::Error { .. } => "Error",
        }
    }
}

/// Capabilities detected inside a running DevContainer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DevContainerCapabilities {
    pub terminal_ok: bool,
    pub lsp_rust_ok: bool,
    pub lsp_python_ok: bool,
}

/// Full status of a DevContainer for a given workspace.
#[derive(Debug, Clone, Serialize)]
pub struct DevContainerStatus {
    pub workspace_root: PathBuf,
    pub project_name: String,
    pub container_id: Option<String>,
    pub phase: DevContainerPhase,
    pub capabilities: DevContainerCapabilities,
}

/// Events emitted by the HUD on state transitions. Future UI layers can
/// subscribe to these via `drain_events()`.
#[derive(Debug, Clone)]
pub enum DevContainerHudEvent {
    PhaseChanged {
        workspace: PathBuf,
        phase_label: String,
    },
    CapabilityUpdated {
        workspace: PathBuf,
        capability: String,
        value: bool,
    },
}

/// Central store that tracks DevContainer status per workspace.
/// Emits structured logs on every transition and buffers events for
/// UI subscribers.
pub struct DevContainerHud {
    workspaces: HashMap<PathBuf, DevContainerStatus>,
    pending_events: Vec<DevContainerHudEvent>,
}

impl DevContainerHud {
    pub fn new() -> Self {
        Self {
            workspaces: HashMap::new(),
            pending_events: Vec::new(),
        }
    }

    /// Get status for a workspace.
    pub fn status(&self, workspace: &PathBuf) -> Option<&DevContainerStatus> {
        self.workspaces.get(workspace)
    }

    /// Get all tracked workspace statuses.
    pub fn all_statuses(&self) -> &HashMap<PathBuf, DevContainerStatus> {
        &self.workspaces
    }

    /// Drain pending events for consumption by a UI subscriber.
    pub fn drain_events(&mut self) -> Vec<DevContainerHudEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Remove a workspace from tracking (e.g. on dismiss).
    pub fn remove_workspace(&mut self, workspace: &PathBuf) {
        if self.workspaces.remove(workspace).is_some() {
            log::info!(
                "[HUD] workspace={} removed from tracking",
                workspace.display(),
            );
        }
    }

    /// Transition workspace to Preflight phase.
    pub fn on_preflight_started(&mut self, workspace: PathBuf, project_name: String) {
        log::info!(
            "[HUD] workspace={} phase=Preflight project={}",
            workspace.display(),
            project_name,
        );
        let status = self.get_or_create(&workspace);
        status.project_name = project_name;
        status.phase = DevContainerPhase::Preflight {
            started_at: Instant::now(),
        };
        self.pending_events.push(DevContainerHudEvent::PhaseChanged {
            workspace,
            phase_label: "Preflight".to_string(),
        });
    }

    /// Update build progress.
    pub fn on_build_progress(&mut self, workspace: &PathBuf, progress: u8, message: String) {
        log::info!(
            "[HUD] workspace={} phase=Building progress={} message=\"{}\"",
            workspace.display(),
            progress,
            message,
        );
        let status = self.get_or_create(workspace);
        let started_at = match &status.phase {
            DevContainerPhase::Building { started_at, .. } => *started_at,
            _ => Instant::now(),
        };
        status.phase = DevContainerPhase::Building {
            started_at,
            progress,
            message,
        };
        self.pending_events.push(DevContainerHudEvent::PhaseChanged {
            workspace: workspace.clone(),
            phase_label: "Building".to_string(),
        });
    }

    /// Transition to Running when container starts.
    pub fn on_container_started(&mut self, workspace: &PathBuf, container_id: String) {
        log::info!(
            "[HUD] workspace={} phase=Running container_id={}",
            workspace.display(),
            container_id,
        );
        let status = self.get_or_create(workspace);
        status.phase = DevContainerPhase::Running {
            started_at: Instant::now(),
        };
        status.container_id = Some(container_id);
        status.capabilities = DevContainerCapabilities::default();
        self.pending_events.push(DevContainerHudEvent::PhaseChanged {
            workspace: workspace.clone(),
            phase_label: "Running".to_string(),
        });
    }

    /// Record terminal smoke test result.
    pub fn on_terminal_smoke_result(&mut self, workspace: &PathBuf, ok: bool) {
        log::info!(
            "[HUD] workspace={} capability=terminal ok={}",
            workspace.display(),
            ok,
        );
        let status = self.get_or_create(workspace);
        status.capabilities.terminal_ok = ok;
        self.pending_events
            .push(DevContainerHudEvent::CapabilityUpdated {
                workspace: workspace.clone(),
                capability: "terminal".to_string(),
                value: ok,
            });
    }

    /// Record Rust LSP (rust-analyzer) check result.
    pub fn on_lsp_rust_result(&mut self, workspace: &PathBuf, ok: bool) {
        log::info!(
            "[HUD] workspace={} capability=lsp_rust ok={}",
            workspace.display(),
            ok,
        );
        let status = self.get_or_create(workspace);
        status.capabilities.lsp_rust_ok = ok;
        self.pending_events
            .push(DevContainerHudEvent::CapabilityUpdated {
                workspace: workspace.clone(),
                capability: "lsp_rust".to_string(),
                value: ok,
            });
    }

    /// Record Python LSP (pyright) check result.
    pub fn on_lsp_python_result(&mut self, workspace: &PathBuf, ok: bool) {
        log::info!(
            "[HUD] workspace={} capability=lsp_python ok={}",
            workspace.display(),
            ok,
        );
        let status = self.get_or_create(workspace);
        status.capabilities.lsp_python_ok = ok;
        self.pending_events
            .push(DevContainerHudEvent::CapabilityUpdated {
                workspace: workspace.clone(),
                capability: "lsp_python".to_string(),
                value: ok,
            });
    }

    /// Transition to Error phase.
    pub fn on_error(&mut self, workspace: &PathBuf, message: String) {
        log::error!(
            "[HUD] workspace={} phase=Error message=\"{}\"",
            workspace.display(),
            message,
        );
        let status = self.get_or_create(workspace);
        status.phase = DevContainerPhase::Error {
            at: Instant::now(),
            message,
        };
        self.pending_events.push(DevContainerHudEvent::PhaseChanged {
            workspace: workspace.clone(),
            phase_label: "Error".to_string(),
        });
    }

    /// Format a human-readable status table for all tracked workspaces.
    pub fn format_status_table(&self) -> String {
        if self.workspaces.is_empty() {
            return "No tracked workspaces.".to_string();
        }

        let mut lines = vec![format!(
            "{:<40} {:<12} {:<14} {:<5} {:<5} {:<5}",
            "Workspace", "Phase", "Container", "Term", "Rust", "Py"
        )];
        lines.push("-".repeat(85));

        for status in self.workspaces.values() {
            let container_short = status
                .container_id
                .as_ref()
                .map(|id| {
                    if id.len() > 12 {
                        &id[..12]
                    } else {
                        id.as_str()
                    }
                })
                .unwrap_or("\u{2014}");

            let cap = &status.capabilities;
            lines.push(format!(
                "{:<40} {:<12} {:<14} {:<5} {:<5} {:<5}",
                status.workspace_root.display(),
                status.phase.label(),
                container_short,
                if cap.terminal_ok { "OK" } else { "\u{2014}" },
                if cap.lsp_rust_ok { "OK" } else { "\u{2014}" },
                if cap.lsp_python_ok { "OK" } else { "\u{2014}" },
            ));
        }

        lines.join("\n")
    }

    /// Serialize all workspace statuses as a pretty-printed JSON string.
    pub fn format_status_json(&self) -> Result<String, serde_json::Error> {
        let statuses: Vec<&DevContainerStatus> = self.workspaces.values().collect();
        serde_json::to_string_pretty(&statuses)
    }

    fn get_or_create(&mut self, workspace: &PathBuf) -> &mut DevContainerStatus {
        self.workspaces
            .entry(workspace.clone())
            .or_insert_with(|| {
                let project_name = workspace
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string());
                DevContainerStatus {
                    workspace_root: workspace.clone(),
                    project_name,
                    container_id: None,
                    phase: DevContainerPhase::Idle,
                    capabilities: DevContainerCapabilities::default(),
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_workspace() -> PathBuf {
        PathBuf::from("/workspace/test-project")
    }

    #[test]
    fn test_happy_path_full_lifecycle() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        // Initially no entry
        assert!(hud.status(&ws).is_none());

        // Preflight
        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_preflight());
        assert_eq!(status.project_name, "test-project");

        // Build progress
        hud.on_build_progress(&ws, 20, "Pulling image".to_string());
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_building());
        if let DevContainerPhase::Building {
            progress, message, ..
        } = &status.phase
        {
            assert_eq!(*progress, 20);
            assert_eq!(message, "Pulling image");
        }

        hud.on_build_progress(&ws, 80, "Starting container".to_string());
        if let DevContainerPhase::Building { progress, .. } =
            &hud.status(&ws).expect("workspace should be tracked").phase
        {
            assert_eq!(*progress, 80);
        }

        // Container started
        hud.on_container_started(&ws, "abc123def456".to_string());
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_running());
        assert_eq!(status.container_id.as_deref(), Some("abc123def456"));
        assert_eq!(status.capabilities, DevContainerCapabilities::default());

        // Terminal OK
        hud.on_terminal_smoke_result(&ws, true);
        assert!(
            hud.status(&ws)
                .expect("workspace should be tracked")
                .capabilities
                .terminal_ok
        );

        // LSP Rust OK
        hud.on_lsp_rust_result(&ws, true);
        assert!(
            hud.status(&ws)
                .expect("workspace should be tracked")
                .capabilities
                .lsp_rust_ok
        );

        // LSP Python OK
        hud.on_lsp_python_result(&ws, true);
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.capabilities.lsp_python_ok);
        assert!(status.phase.is_running());
    }

    #[test]
    fn test_error_during_build() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_build_progress(&ws, 10, "Parsing config".to_string());

        hud.on_error(&ws, "Docker build failed: OOM".to_string());

        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_error());
        if let DevContainerPhase::Error { message, .. } = &status.phase {
            assert_eq!(message, "Docker build failed: OOM");
        } else {
            panic!("expected Error phase");
        }
    }

    #[test]
    fn test_capability_failures_while_running() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_build_progress(&ws, 100, "Done".to_string());
        hud.on_container_started(&ws, "abc123".to_string());

        // Terminal fails
        hud.on_terminal_smoke_result(&ws, false);
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_running(), "phase stays Running");
        assert!(!status.capabilities.terminal_ok);

        // LSP Rust fails
        hud.on_lsp_rust_result(&ws, false);
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_running());
        assert!(!status.capabilities.lsp_rust_ok);

        // LSP Python OK
        hud.on_lsp_python_result(&ws, true);
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.phase.is_running());
        assert!(status.capabilities.lsp_python_ok);
    }

    #[test]
    fn test_events_are_emitted_and_drained() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_build_progress(&ws, 50, "building".to_string());
        hud.on_container_started(&ws, "abc".to_string());
        hud.on_terminal_smoke_result(&ws, true);

        let events = hud.drain_events();
        assert_eq!(events.len(), 4);

        // Verify event types
        assert!(matches!(
            &events[0],
            DevContainerHudEvent::PhaseChanged { phase_label, .. } if phase_label == "Preflight"
        ));
        assert!(matches!(
            &events[1],
            DevContainerHudEvent::PhaseChanged { phase_label, .. } if phase_label == "Building"
        ));
        assert!(matches!(
            &events[2],
            DevContainerHudEvent::PhaseChanged { phase_label, .. } if phase_label == "Running"
        ));
        assert!(matches!(
            &events[3],
            DevContainerHudEvent::CapabilityUpdated { capability, value, .. }
                if capability == "terminal" && *value
        ));

        // Drain again should be empty
        assert!(hud.drain_events().is_empty());
    }

    #[test]
    fn test_multiple_workspaces() {
        let mut hud = DevContainerHud::new();
        let ws1 = PathBuf::from("/workspace/project-a");
        let ws2 = PathBuf::from("/workspace/project-b");

        hud.on_preflight_started(ws1.clone(), "project-a".to_string());
        hud.on_preflight_started(ws2.clone(), "project-b".to_string());

        hud.on_build_progress(&ws1, 50, "building A".to_string());
        hud.on_container_started(&ws1, "aaa".to_string());

        // ws1 is Running, ws2 still in Preflight
        assert!(
            hud.status(&ws1)
                .expect("ws1 should be tracked")
                .phase
                .is_running()
        );
        assert!(
            hud.status(&ws2)
                .expect("ws2 should be tracked")
                .phase
                .is_preflight()
        );
        assert_eq!(hud.all_statuses().len(), 2);
    }

    #[test]
    fn test_build_preserves_started_at() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_build_progress(&ws, 10, "step 1".to_string());

        let started_at_1 = match &hud
            .status(&ws)
            .expect("workspace should be tracked")
            .phase
        {
            DevContainerPhase::Building { started_at, .. } => *started_at,
            other => panic!("expected Building, got {:?}", other.label()),
        };

        // Second progress update should preserve started_at
        hud.on_build_progress(&ws, 50, "step 2".to_string());
        let started_at_2 = match &hud
            .status(&ws)
            .expect("workspace should be tracked")
            .phase
        {
            DevContainerPhase::Building { started_at, .. } => *started_at,
            other => panic!("expected Building, got {:?}", other.label()),
        };

        assert_eq!(started_at_1, started_at_2);
    }

    #[test]
    fn test_remove_workspace() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        assert!(hud.status(&ws).is_some());

        hud.remove_workspace(&ws);
        assert!(hud.status(&ws).is_none());
    }

    #[test]
    fn test_format_status_table_empty() {
        let hud = DevContainerHud::new();
        assert_eq!(hud.format_status_table(), "No tracked workspaces.");
    }

    #[test]
    fn test_format_status_table_with_entries() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_container_started(&ws, "abc123def456789".to_string());
        hud.on_terminal_smoke_result(&ws, true);
        hud.on_lsp_rust_result(&ws, true);

        let table = hud.format_status_table();
        assert!(table.contains("Running"));
        assert!(table.contains("abc123def456")); // truncated to 12
        assert!(table.contains("OK"));
    }

    #[test]
    fn test_error_without_prior_preflight() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        // on_error auto-creates the workspace entry
        hud.on_error(&ws, "Connection failed".to_string());

        let status = hud.status(&ws).expect("workspace should be auto-created");
        assert!(status.phase.is_error());
        if let DevContainerPhase::Error { message, .. } = &status.phase {
            assert_eq!(message, "Connection failed");
        } else {
            panic!("expected Error phase");
        }
    }

    #[test]
    fn test_container_started_resets_capabilities() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_container_started(&ws, "first-container".to_string());
        hud.on_terminal_smoke_result(&ws, true);
        hud.on_lsp_rust_result(&ws, true);

        // Verify capabilities are set
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert!(status.capabilities.terminal_ok);
        assert!(status.capabilities.lsp_rust_ok);

        // New container replaces the old one and resets capabilities
        hud.on_container_started(&ws, "second-container".to_string());
        let status = hud.status(&ws).expect("workspace should be tracked");
        assert_eq!(status.container_id.as_deref(), Some("second-container"));
        assert_eq!(status.capabilities, DevContainerCapabilities::default());
    }

    #[test]
    fn test_format_status_json() {
        let mut hud = DevContainerHud::new();
        let ws = test_workspace();

        hud.on_preflight_started(ws.clone(), "test-project".to_string());
        hud.on_container_started(&ws, "abc123def456".to_string());
        hud.on_terminal_smoke_result(&ws, true);
        hud.on_lsp_rust_result(&ws, true);
        hud.on_lsp_python_result(&ws, false);

        let json_str = hud.format_status_json().expect("json serialization should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("should be valid JSON");

        let arr = parsed.as_array().expect("root should be an array");
        assert_eq!(arr.len(), 1);

        let entry = &arr[0];
        assert_eq!(entry["project_name"], "test-project");
        assert_eq!(entry["container_id"], "abc123def456");
        assert_eq!(entry["phase"]["phase"], "Running");
        assert_eq!(entry["capabilities"]["terminal_ok"], true);
        assert_eq!(entry["capabilities"]["lsp_rust_ok"], true);
        assert_eq!(entry["capabilities"]["lsp_python_ok"], false);
    }

    #[test]
    fn test_format_status_json_empty() {
        let hud = DevContainerHud::new();
        let json_str = hud.format_status_json().expect("json serialization should succeed");
        assert_eq!(json_str, "[]");
    }
}
