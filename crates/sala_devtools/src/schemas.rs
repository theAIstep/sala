use serde::Serialize;

// --- dc-status ---

#[derive(Debug, Serialize)]
pub struct DcStatusJson {
    pub workspace: String,
    pub phase: String,
    pub container_id: Option<String>,
    pub terminal_ok: bool,
    pub lsp_rust_ok: bool,
    pub lsp_python_ok: bool,
}

// --- dc-doctor ---

#[derive(Debug, Clone, Serialize)]
pub struct DcDoctorJson {
    pub workspace: String,
    pub overall_status: String,
    pub checks: Vec<DcDoctorCheckJson>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DcDoctorCheckJson {
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

// --- dc-build ---

#[derive(Debug, Serialize)]
pub struct DcBuildJson {
    pub workspace: String,
    pub action: String,
    pub status: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

// --- dc-connect ---

#[derive(Debug, Serialize)]
pub struct DcConnectJson {
    pub workspace: String,
    pub container_id: String,
    pub workspace_folder: String,
    pub config_changed: bool,
}

// --- dc-smoke ---

#[derive(Debug, Serialize)]
pub struct DcSmokeJson {
    pub workspace: String,
    pub overall: String,
    pub steps: Vec<DcSmokeStepJson>,
}

#[derive(Debug, Serialize)]
pub struct DcSmokeStepJson {
    pub name: String,
    pub status: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_status_json_round_trip() {
        let status = DcStatusJson {
            workspace: "/workspace/test".to_string(),
            phase: "Running".to_string(),
            container_id: Some("abc123".to_string()),
            terminal_ok: true,
            lsp_rust_ok: true,
            lsp_python_ok: false,
        };
        let json = serde_json::to_string_pretty(&status).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["workspace"], "/workspace/test");
        assert_eq!(parsed["phase"], "Running");
        assert_eq!(parsed["terminal_ok"], true);
        assert_eq!(parsed["lsp_python_ok"], false);
    }

    #[test]
    fn dc_build_json_skips_none_fields() {
        let build = DcBuildJson {
            workspace: "/workspace/test".to_string(),
            action: "build".to_string(),
            status: "success".to_string(),
            duration_ms: 12345,
            container_id: Some("abc".to_string()),
            error_message: None,
        };
        let json = serde_json::to_string(&build).expect("serialize");
        assert!(!json.contains("error_message"));
        assert!(json.contains("container_id"));
    }

    #[test]
    fn dc_build_json_includes_error() {
        let build = DcBuildJson {
            workspace: "/workspace/test".to_string(),
            action: "rebuild".to_string(),
            status: "failed".to_string(),
            duration_ms: 500,
            container_id: None,
            error_message: Some("Docker not running".to_string()),
        };
        let json = serde_json::to_string_pretty(&build).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["status"], "failed");
        assert_eq!(parsed["error_message"], "Docker not running");
        assert!(parsed.get("container_id").is_none());
    }

    #[test]
    fn dc_doctor_json_round_trip() {
        let doctor = DcDoctorJson {
            workspace: "/workspace/test".to_string(),
            overall_status: "HEALTHY".to_string(),
            checks: vec![
                DcDoctorCheckJson {
                    name: "Tala daemon".to_string(),
                    status: "OK".to_string(),
                    message: "reachable".to_string(),
                    suggestion: None,
                    details: None,
                },
                DcDoctorCheckJson {
                    name: "Docker".to_string(),
                    status: "ERROR".to_string(),
                    message: "not running".to_string(),
                    suggestion: Some("start docker".to_string()),
                    details: Some(serde_json::json!({"reason": "socket missing"})),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&doctor).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["overall_status"], "HEALTHY");
        let checks = parsed["checks"].as_array().expect("array");
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0]["status"], "OK");
        assert!(checks[0].get("suggestion").is_none());
        assert_eq!(checks[1]["suggestion"], "start docker");
    }

    #[test]
    fn dc_connect_json_round_trip() {
        let connect = DcConnectJson {
            workspace: "/workspace/test".to_string(),
            container_id: "abc123def456".to_string(),
            workspace_folder: "/workspace".to_string(),
            config_changed: true,
        };
        let json = serde_json::to_string_pretty(&connect).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["container_id"], "abc123def456");
        assert_eq!(parsed["config_changed"], true);
    }

    #[test]
    fn dc_smoke_json_round_trip() {
        let smoke = DcSmokeJson {
            workspace: "/workspace/test".to_string(),
            overall: "pass".to_string(),
            steps: vec![
                DcSmokeStepJson {
                    name: "devcontainer".to_string(),
                    status: "ok".to_string(),
                    message: "container abc123".to_string(),
                },
                DcSmokeStepJson {
                    name: "terminal".to_string(),
                    status: "ok".to_string(),
                    message: "SMOKE_OK received".to_string(),
                },
                DcSmokeStepJson {
                    name: "lsp".to_string(),
                    status: "skip".to_string(),
                    message: "rust-analyzer not found".to_string(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&smoke).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["overall"], "pass");
        let steps = parsed["steps"].as_array().expect("array");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2]["status"], "skip");
    }
}
