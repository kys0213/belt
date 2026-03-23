use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::escalation::EscalationPolicy;

/// 워크스페이스 설정.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    #[serde(default)]
    pub sources: HashMap<String, SourceConfig>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

/// DataSource별 설정.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub url: String,
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
    #[serde(default)]
    pub states: HashMap<String, StateConfig>,
    #[serde(default)]
    pub escalation: EscalationPolicy,
}

fn default_scan_interval() -> u64 {
    300
}

fn default_concurrency() -> u32 {
    1
}

/// 워크플로우 상태 설정.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    #[serde(default)]
    pub trigger: TriggerConfig,
    #[serde(default)]
    pub handlers: Vec<HandlerConfig>,
    #[serde(default)]
    pub on_enter: Vec<ScriptAction>,
    #[serde(default)]
    pub on_done: Vec<ScriptAction>,
    #[serde(default)]
    pub on_fail: Vec<ScriptAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TriggerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HandlerConfig {
    Prompt {
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        runtime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    Script {
        script: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptAction {
    pub script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_runtime_name")]
    pub default: String,
    #[serde(flatten)]
    pub runtimes: HashMap<String, RuntimeInstanceConfig>,
}

fn default_runtime_name() -> String {
    "claude".to_string()
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            default: default_runtime_name(),
            runtimes: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInstanceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// 워크스페이스 참조 (경량 식별 정보).
#[derive(Debug, Clone)]
pub struct WorkspaceRef {
    pub id: String,
    pub name: String,
    pub url: String,
    pub concurrency: u32,
}

impl WorkspaceRef {
    pub fn from_config(id: &str, config: &WorkspaceConfig, source_name: &str) -> Option<Self> {
        let source = config.sources.get(source_name)?;
        Some(Self {
            id: id.to_string(),
            name: config.name.clone(),
            url: source.url.clone(),
            concurrency: config.concurrency,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE_YAML: &str = r#"
name: auth-project
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300
    states:
      analyze:
        trigger:
          label: "belt:analyze"
        handlers:
          - prompt: "이슈를 분석하고 구현 가능 여부를 판단해줘"
        on_done:
          - script: |
              gh issue edit $ISSUE --remove-label "belt:analyze"
      implement:
        trigger:
          label: "belt:implement"
        handlers:
          - prompt: "이슈를 구현해줘"
            runtime: claude
            model: sonnet
          - script: "cargo test"
        on_done:
          - script: "gh pr create --title $TITLE"
        on_fail:
          - script: "gh issue comment $ISSUE --body 'failed'"
    escalation:
      1: retry
      2: retry_with_comment
      3: hitl
      4: skip
      5: replan
runtime:
  default: claude
"#;

    #[test]
    fn parse_full_workspace_yaml() {
        let config: WorkspaceConfig = serde_yaml::from_str(WORKSPACE_YAML).unwrap();
        assert_eq!(config.name, "auth-project");
        assert_eq!(config.concurrency, 2);
        let github = config.sources.get("github").unwrap();
        assert_eq!(github.url, "https://github.com/org/repo");
    }

    #[test]
    fn parse_states() {
        let config: WorkspaceConfig = serde_yaml::from_str(WORKSPACE_YAML).unwrap();
        let github = config.sources.get("github").unwrap();
        let analyze = github.states.get("analyze").unwrap();
        assert_eq!(analyze.trigger.label.as_deref(), Some("belt:analyze"));
        assert_eq!(analyze.handlers.len(), 1);
        let implement = github.states.get("implement").unwrap();
        assert_eq!(implement.handlers.len(), 2);
    }

    #[test]
    fn parse_handler_types() {
        let config: WorkspaceConfig = serde_yaml::from_str(WORKSPACE_YAML).unwrap();
        let github = config.sources.get("github").unwrap();
        let implement = github.states.get("implement").unwrap();
        match &implement.handlers[0] {
            HandlerConfig::Prompt {
                prompt,
                runtime,
                model,
            } => {
                assert!(prompt.contains("구현"));
                assert_eq!(runtime.as_deref(), Some("claude"));
                assert_eq!(model.as_deref(), Some("sonnet"));
            }
            _ => panic!("expected Prompt handler"),
        }
    }

    #[test]
    fn defaults() {
        let yaml = "name: minimal\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.concurrency, 1);
        let github = config.sources.get("github").unwrap();
        assert_eq!(github.scan_interval_secs, 300);
        assert_eq!(config.runtime.default, "claude");
    }

    #[test]
    fn workspace_ref_from_config() {
        let config: WorkspaceConfig = serde_yaml::from_str(WORKSPACE_YAML).unwrap();
        let ws_ref = WorkspaceRef::from_config("ws-1", &config, "github").unwrap();
        assert_eq!(ws_ref.name, "auth-project");
        assert_eq!(ws_ref.concurrency, 2);
    }
}
