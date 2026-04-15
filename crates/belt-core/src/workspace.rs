use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::escalation::EscalationPolicy;
use crate::stagnation::StagnationConfig;

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
    /// Progressive evaluation pipeline configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluate: Option<EvaluateConfig>,
    /// Per-workspace Claw configuration overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claw_config: Option<ClawConfig>,
    /// Stagnation detection configuration.
    #[serde(default)]
    pub stagnation: StagnationConfig,
}

/// Evaluation pipeline configuration.
///
/// Defines the mechanical (deterministic) checks that run before the
/// semantic (LLM) evaluation stage. If `mechanical` is empty or absent,
/// the pipeline skips directly to semantic evaluation.
///
/// ```yaml
/// evaluate:
///   mechanical:
///     - "cargo test"
///     - "cargo clippy -- -D warnings"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EvaluateConfig {
    /// Shell commands to run in the worktree for deterministic verification.
    ///
    /// Each command is executed sequentially. If any command fails (non-zero
    /// exit code), the item is marked for retry without invoking the LLM.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mechanical: Vec<String>,
}

/// Per-workspace Claw configuration.
///
/// When present on a `WorkspaceConfig`, these values override the global
/// Claw defaults for this workspace only.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ClawConfig {
    /// Whether auto-approve is enabled for this workspace.
    #[serde(default)]
    pub auto_approve: bool,
    /// Custom HITL policy file path (relative to workspace root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitl_policy: Option<String>,
    /// Custom classify policy file path (relative to workspace root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classify_policy: Option<String>,
    /// Custom slash commands enabled for this workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_commands: Vec<String>,
    /// Path to workspace-specific rules directory.
    ///
    /// When set, all `.md` files in this directory are loaded and injected
    /// as context into the agent runtime's system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_path: Option<String>,
    /// Maximum number of conversation turns allowed per session.
    ///
    /// Defaults to 10 when not set. Injected into the system prompt
    /// to guide the LLM's behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_conversation_turns: Option<u32>,
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
    /// When `true`, this state is triggered by PR `CHANGES_REQUESTED` reviews.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changes_requested: bool,
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

    #[test]
    fn trigger_changes_requested_defaults_to_false() {
        let config: WorkspaceConfig = serde_yaml::from_str(WORKSPACE_YAML).unwrap();
        let github = config.sources.get("github").unwrap();
        let analyze = github.states.get("analyze").unwrap();
        assert!(!analyze.trigger.changes_requested);
    }

    #[test]
    fn trigger_changes_requested_parses_true() {
        let yaml = r#"
name: review-ws
sources:
  github:
    url: https://github.com/org/repo
    states:
      fix_review:
        trigger:
          changes_requested: true
        handlers:
          - prompt: "Fix the review comments"
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let github = config.sources.get("github").unwrap();
        let fix_review = github.states.get("fix_review").unwrap();
        assert!(fix_review.trigger.changes_requested);
        assert!(fix_review.trigger.label.is_none());
    }

    #[test]
    fn trigger_changes_requested_skipped_in_json_when_false() {
        let trigger = TriggerConfig::default();
        let json = serde_json::to_string(&trigger).unwrap();
        assert!(!json.contains("changes_requested"));
    }

    #[test]
    fn claw_config_defaults_to_none() {
        let yaml = "name: minimal\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.claw_config.is_none());
    }

    #[test]
    fn claw_config_parses_override() {
        let yaml = r#"
name: with-claw
sources:
  github:
    url: https://github.com/org/repo
claw_config:
  auto_approve: true
  hitl_policy: custom-hitl.md
  enabled_commands:
    - auto
    - spec
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let claw = config.claw_config.unwrap();
        assert!(claw.auto_approve);
        assert_eq!(claw.hitl_policy.as_deref(), Some("custom-hitl.md"));
        assert_eq!(claw.enabled_commands, vec!["auto", "spec"]);
    }

    #[test]
    fn claw_config_parses_rules_path() {
        let yaml = r#"
name: with-rules
sources:
  github:
    url: https://github.com/org/repo
claw_config:
  rules_path: /custom/rules
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let claw = config.claw_config.unwrap();
        assert_eq!(claw.rules_path.as_deref(), Some("/custom/rules"));
    }

    #[test]
    fn claw_config_rules_path_defaults_to_none() {
        let yaml = r#"
name: no-rules
sources:
  github:
    url: https://github.com/org/repo
claw_config:
  auto_approve: false
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let claw = config.claw_config.unwrap();
        assert!(claw.rules_path.is_none());
    }

    #[test]
    fn claw_config_parses_max_conversation_turns() {
        let yaml = r#"
name: with-turns
sources:
  github:
    url: https://github.com/org/repo
claw_config:
  max_conversation_turns: 25
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let claw = config.claw_config.unwrap();
        assert_eq!(claw.max_conversation_turns, Some(25));
    }

    #[test]
    fn claw_config_max_conversation_turns_defaults_to_none() {
        let yaml = r#"
name: no-turns
sources:
  github:
    url: https://github.com/org/repo
claw_config:
  auto_approve: false
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let claw = config.claw_config.unwrap();
        assert!(claw.max_conversation_turns.is_none());
    }

    #[test]
    fn evaluate_defaults_to_none() {
        let yaml = "name: minimal\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.evaluate.is_none());
    }

    #[test]
    fn evaluate_parses_mechanical_commands() {
        let yaml = r#"
name: with-eval
sources:
  github:
    url: https://github.com/org/repo
evaluate:
  mechanical:
    - "cargo test"
    - "cargo clippy -- -D warnings"
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let eval = config.evaluate.unwrap();
        assert_eq!(eval.mechanical.len(), 2);
        assert_eq!(eval.mechanical[0], "cargo test");
        assert_eq!(eval.mechanical[1], "cargo clippy -- -D warnings");
    }

    #[test]
    fn evaluate_empty_mechanical_defaults_to_empty_vec() {
        let yaml = r#"
name: empty-eval
sources:
  github:
    url: https://github.com/org/repo
evaluate: {}
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let eval = config.evaluate.unwrap();
        assert!(eval.mechanical.is_empty());
    }

    #[test]
    fn stagnation_defaults_to_enabled() {
        let yaml = "name: minimal\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.stagnation.enabled);
    }

    #[test]
    fn stagnation_parses_disabled() {
        let yaml = r#"
name: stag-off
sources:
  github:
    url: https://github.com/org/repo
stagnation:
  enabled: false
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.stagnation.enabled);
    }

    #[test]
    fn stagnation_parses_enabled_explicitly() {
        let yaml = r#"
name: stag-on
sources:
  github:
    url: https://github.com/org/repo
stagnation:
  enabled: true
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.stagnation.enabled);
    }
}
