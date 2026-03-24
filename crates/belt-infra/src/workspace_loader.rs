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

    #[test]
    fn load_empty_file_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Empty file produces null YAML, which cannot deserialize into WorkspaceConfig.
        let result = load_workspace_config(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn load_yaml_missing_name_field_errors() {
        let yaml = r#"
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let result = load_workspace_config(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn load_error_message_contains_file_path() {
        let result = load_workspace_config(std::path::Path::new("/nonexistent/workspace.yml"));
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("/nonexistent/workspace.yml"),
            "error message should contain the file path for debuggability, got: {err_msg}"
        );
    }

    #[test]
    fn load_parse_error_message_contains_file_path() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"not: [valid: yaml: {{").unwrap();

        let result = load_workspace_config(tmp.path());
        let err_msg = result.unwrap_err().to_string();
        let path_str = tmp.path().to_string_lossy();
        assert!(
            err_msg.contains(path_str.as_ref()),
            "parse error message should contain the file path, got: {err_msg}"
        );
    }

    #[test]
    fn load_minimal_config_applies_defaults() {
        // Only `name` and one source `url` are required; everything else should default.
        let yaml = "name: minimal-project\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let config = load_workspace_config(tmp.path()).unwrap();
        assert_eq!(config.name, "minimal-project");
        // Default concurrency is 1.
        assert_eq!(config.concurrency, 1);
        let github = config.sources.get("github").unwrap();
        // Default scan interval is 300s.
        assert_eq!(github.scan_interval_secs, 300);
        // Default runtime name is "claude".
        assert_eq!(config.runtime.default, "claude");
    }

    #[test]
    fn load_multiple_sources_all_parsed() {
        let yaml = r#"
name: multi-source
sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 60
  slack:
    url: https://slack.com/workspace
    scan_interval_secs: 120
  jira:
    url: https://jira.example.com
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let config = load_workspace_config(tmp.path()).unwrap();
        assert_eq!(config.sources.len(), 3);
        assert!(config.sources.contains_key("github"));
        assert!(config.sources.contains_key("slack"));
        assert!(config.sources.contains_key("jira"));
        assert_eq!(config.sources["github"].scan_interval_secs, 60);
        assert_eq!(config.sources["slack"].scan_interval_secs, 120);
        // jira uses the default.
        assert_eq!(config.sources["jira"].scan_interval_secs, 300);
    }

    #[test]
    fn load_config_with_runtime_section() {
        let yaml = r#"
name: runtime-project
sources:
  github:
    url: https://github.com/org/repo
runtime:
  default: gemini
"#;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let config = load_workspace_config(tmp.path()).unwrap();
        assert_eq!(config.runtime.default, "gemini");
    }

    #[test]
    fn load_config_with_zero_sources() {
        // A workspace with an empty sources map is structurally valid YAML.
        let yaml = "name: no-sources\nsources: {}\n";
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();

        let config = load_workspace_config(tmp.path()).unwrap();
        assert_eq!(config.name, "no-sources");
        assert!(config.sources.is_empty());
    }
}
