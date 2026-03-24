//! Load and parse workspace configuration from a YAML file.

use std::path::Path;

use belt_core::workspace::WorkspaceConfig;

/// Load a [`WorkspaceConfig`] from the given YAML file path.
///
/// # Errors
/// Returns an error if the file cannot be read or parsed.
pub fn load_workspace_config(path: &Path) -> anyhow::Result<WorkspaceConfig> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        anyhow::anyhow!("failed to read workspace config '{}': {e}", path.display())
    })?;
    let config: WorkspaceConfig = serde_yaml::from_str(&content).map_err(|e| {
        anyhow::anyhow!("failed to parse workspace config '{}': {e}", path.display())
    })?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_valid_workspace_config() {
        let yaml = r#"
name: test-project
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let config = load_workspace_config(tmp.path()).unwrap();
        assert_eq!(config.name, "test-project");
        assert_eq!(config.concurrency, 2);
        assert!(config.sources.contains_key("github"));
    }

    #[test]
    fn load_missing_file_errors() {
        let result = load_workspace_config(Path::new("/nonexistent/workspace.yml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_invalid_yaml_errors() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"not: [valid: yaml: {{").unwrap();

        let result = load_workspace_config(tmp.path());
        assert!(result.is_err());
    }
}
