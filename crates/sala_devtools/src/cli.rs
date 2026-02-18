use std::path::PathBuf;

pub struct CommonArgs {
    pub workspace: PathBuf,
    pub json: bool,
    pub verbose: bool,
}

/// Extract `--flag value` or `--flag=value` from args, removing the consumed entries.
pub fn extract_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    if let Some(pos) = args.iter().position(|a| a == flag) {
        if pos + 1 < args.len() {
            let val = args[pos + 1].clone();
            args.drain(pos..=pos + 1);
            return Some(val);
        }
    }
    let prefix = format!("{flag}=");
    if let Some(pos) = args.iter().position(|a| a.starts_with(&prefix)) {
        let val = args[pos][prefix.len()..].to_string();
        args.remove(pos);
        return Some(val);
    }
    None
}

/// Extract a boolean flag like `--json` or `-j`, removing it from args.
pub fn extract_bool_flag(args: &mut Vec<String>, long: &str, short: &str) -> bool {
    if let Some(pos) = args.iter().position(|a| a == long || a == short) {
        args.remove(pos);
        true
    } else {
        false
    }
}

/// Parse the normalized `--workspace`, `--json`, `--verbose` flags from args.
/// Workspace defaults to the current working directory.
pub fn parse_common_args(args: &mut Vec<String>) -> CommonArgs {
    let json = extract_bool_flag(args, "--json", "-j");
    let verbose = extract_bool_flag(args, "--verbose", "-v");
    let workspace = extract_flag(args, "--workspace")
        .or_else(|| extract_flag(args, "-w"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    CommonArgs {
        workspace,
        json,
        verbose,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_flag_with_space() {
        let mut args = vec![
            "--workspace".into(),
            "/foo".into(),
            "--json".into(),
        ];
        assert_eq!(extract_flag(&mut args, "--workspace"), Some("/foo".into()));
        assert_eq!(args, vec!["--json"]);
    }

    #[test]
    fn extract_flag_with_equals() {
        let mut args = vec!["--workspace=/foo".into(), "--json".into()];
        assert_eq!(extract_flag(&mut args, "--workspace"), Some("/foo".into()));
        assert_eq!(args, vec!["--json"]);
    }

    #[test]
    fn extract_flag_missing() {
        let mut args = vec!["--json".into()];
        assert_eq!(extract_flag(&mut args, "--workspace"), None);
        assert_eq!(args, vec!["--json"]);
    }

    #[test]
    fn extract_bool_flag_long() {
        let mut args = vec!["--json".into(), "-v".into()];
        assert!(extract_bool_flag(&mut args, "--json", "-j"));
        assert_eq!(args, vec!["-v"]);
    }

    #[test]
    fn extract_bool_flag_short() {
        let mut args = vec!["-j".into(), "-v".into()];
        assert!(extract_bool_flag(&mut args, "--json", "-j"));
        assert_eq!(args, vec!["-v"]);
    }

    #[test]
    fn extract_bool_flag_missing() {
        let mut args = vec!["-v".into()];
        assert!(!extract_bool_flag(&mut args, "--json", "-j"));
        assert_eq!(args, vec!["-v"]);
    }
}
