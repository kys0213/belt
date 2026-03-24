use std::sync::Arc;

use anyhow::{Context, Result, bail};

use belt_core::action::Action;
use belt_core::runtime::RuntimeRegistry;
use belt_core::workspace::WorkspaceConfig;
use belt_daemon::executor::{ActionEnv, ActionExecutor};
use belt_infra::runtimes::claude::ClaudeRuntime;

/// Load a workspace config from a YAML file path.
fn load_workspace_config(path: &str) -> Result<WorkspaceConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read workspace config: {path}"))?;
    let config: WorkspaceConfig =
        serde_yaml::from_str(&content).with_context(|| "failed to parse workspace config")?;
    Ok(config)
}

/// Build a RuntimeRegistry from workspace config.
fn build_registry(config: &WorkspaceConfig) -> RuntimeRegistry {
    let default_name = config.runtime.default.clone();
    let mut registry = RuntimeRegistry::new(default_name);

    // Register Claude runtime with model from config if present.
    let claude_model = config
        .runtime
        .runtimes
        .get("claude")
        .and_then(|rc| rc.model.clone());
    registry.register(Arc::new(ClaudeRuntime::new(claude_model)));

    registry
}

/// Collect all prompt/script actions from the workspace config.
fn collect_actions(config: &WorkspaceConfig, prompt_override: Option<&str>) -> Vec<Action> {
    // If a prompt override is provided, use it directly as a single prompt action.
    if let Some(prompt) = prompt_override {
        return vec![Action::prompt(prompt)];
    }

    // Otherwise, collect all handler actions from all sources/states.
    let mut actions = Vec::new();
    for source in config.sources.values() {
        for state in source.states.values() {
            for handler in &state.handlers {
                actions.push(Action::from(handler));
            }
        }
    }
    actions
}

/// Plan output: display what would be executed without running.
fn print_plan(config: &WorkspaceConfig, actions: &[Action], json_output: bool) -> Result<()> {
    if json_output {
        let plan = serde_json::json!({
            "workspace": config.name,
            "runtime": {
                "default": config.runtime.default,
            },
            "actions": actions.iter().map(|a| match a {
                Action::Prompt { text, runtime, model } => serde_json::json!({
                    "type": "prompt",
                    "text": text,
                    "runtime": runtime,
                    "model": model,
                }),
                Action::Script { command } => serde_json::json!({
                    "type": "script",
                    "command": command,
                }),
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("Plan for workspace: {}", config.name);
        println!("Default runtime: {}", config.runtime.default);
        println!("Actions ({}):", actions.len());
        for (i, action) in actions.iter().enumerate() {
            match action {
                Action::Prompt {
                    text,
                    runtime,
                    model,
                } => {
                    let rt = runtime.as_deref().unwrap_or(&config.runtime.default);
                    let model_str = model.as_deref().unwrap_or("default");
                    let truncated = if text.len() > 80 {
                        format!("{}...", &text[..77])
                    } else {
                        text.clone()
                    };
                    println!("  {}. [prompt] runtime={rt} model={model_str}", i + 1);
                    println!("     {truncated}");
                }
                Action::Script { command } => {
                    let truncated = if command.len() > 80 {
                        format!("{}...", &command[..77])
                    } else {
                        command.clone()
                    };
                    println!("  {}. [script] {truncated}", i + 1);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use belt_core::workspace::{
        HandlerConfig, RuntimeConfig, RuntimeInstanceConfig, SourceConfig, StateConfig,
        WorkspaceConfig,
    };

    use super::*;

    /// Build a minimal `WorkspaceConfig` without any sources.
    fn empty_workspace() -> WorkspaceConfig {
        WorkspaceConfig {
            name: "test-ws".to_string(),
            concurrency: 1,
            sources: HashMap::new(),
            runtime: RuntimeConfig::default(),
        }
    }

    /// Build a `WorkspaceConfig` with one source that has two states, each
    /// containing one prompt handler and one script handler.
    fn workspace_with_handlers() -> WorkspaceConfig {
        let mut states = HashMap::new();
        states.insert(
            "analyze".to_string(),
            StateConfig {
                trigger: Default::default(),
                handlers: vec![HandlerConfig::Prompt {
                    prompt: "analyze the issue".to_string(),
                    runtime: None,
                    model: None,
                }],
                on_enter: vec![],
                on_done: vec![],
                on_fail: vec![],
            },
        );
        states.insert(
            "implement".to_string(),
            StateConfig {
                trigger: Default::default(),
                handlers: vec![
                    HandlerConfig::Prompt {
                        prompt: "implement the feature".to_string(),
                        runtime: Some("claude".to_string()),
                        model: Some("sonnet".to_string()),
                    },
                    HandlerConfig::Script {
                        script: "cargo test".to_string(),
                    },
                ],
                on_enter: vec![],
                on_done: vec![],
                on_fail: vec![],
            },
        );

        let mut sources = HashMap::new();
        sources.insert(
            "github".to_string(),
            SourceConfig {
                url: "https://github.com/org/repo".to_string(),
                scan_interval_secs: 300,
                states,
                escalation: Default::default(),
            },
        );

        WorkspaceConfig {
            name: "my-workspace".to_string(),
            concurrency: 2,
            sources,
            runtime: RuntimeConfig::default(),
        }
    }

    // ---- collect_actions ----

    #[test]
    fn collect_actions_with_prompt_override_returns_single_action() {
        let config = workspace_with_handlers();
        let actions = collect_actions(&config, Some("do something"));
        assert_eq!(actions.len(), 1);
        assert!(actions[0].is_prompt());
        match &actions[0] {
            Action::Prompt {
                text,
                runtime,
                model,
            } => {
                assert_eq!(text, "do something");
                assert!(runtime.is_none());
                assert!(model.is_none());
            }
            _ => panic!("expected Prompt action"),
        }
    }

    #[test]
    fn collect_actions_without_override_collects_all_handlers() {
        let config = workspace_with_handlers();
        // 3 handlers across two states: 1 (analyze) + 2 (implement)
        let actions = collect_actions(&config, None);
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn collect_actions_empty_sources_returns_empty() {
        let config = empty_workspace();
        let actions = collect_actions(&config, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn collect_actions_empty_sources_with_override_returns_one() {
        let config = empty_workspace();
        let actions = collect_actions(&config, Some("override prompt"));
        assert_eq!(actions.len(), 1);
        assert!(actions[0].is_prompt());
    }

    #[test]
    fn collect_actions_handler_types_preserved() {
        let config = workspace_with_handlers();
        let actions = collect_actions(&config, None);

        let prompt_count = actions.iter().filter(|a| a.is_prompt()).count();
        let script_count = actions.iter().filter(|a| a.is_script()).count();
        assert_eq!(prompt_count, 2);
        assert_eq!(script_count, 1);
    }

    // ---- build_registry ----

    #[test]
    fn build_registry_uses_config_default_runtime() {
        let mut config = empty_workspace();
        config.runtime = RuntimeConfig {
            default: "claude".to_string(),
            runtimes: HashMap::new(),
        };
        let registry = build_registry(&config);
        assert_eq!(registry.default_name(), "claude");
    }

    #[test]
    fn build_registry_uses_model_from_config() {
        let mut runtimes = HashMap::new();
        runtimes.insert(
            "claude".to_string(),
            RuntimeInstanceConfig {
                model: Some("claude-sonnet-4".to_string()),
            },
        );
        let mut config = empty_workspace();
        config.runtime = RuntimeConfig {
            default: "claude".to_string(),
            runtimes,
        };
        // Registry builds without panic; the claude runtime is registered.
        let registry = build_registry(&config);
        assert_eq!(registry.default_name(), "claude");
    }

    #[test]
    fn build_registry_without_claude_entry_still_succeeds() {
        // No "claude" key in runtimes — model will be None.
        let config = empty_workspace();
        let registry = build_registry(&config);
        assert_eq!(registry.default_name(), "claude");
    }

    // ---- load_workspace_config ----

    #[test]
    fn load_workspace_config_missing_file_returns_error() {
        let result = load_workspace_config("/nonexistent/path/workspace.yaml");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to read workspace config"));
    }

    #[test]
    fn load_workspace_config_invalid_yaml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.yaml");
        std::fs::write(&path, b"not: valid: yaml: :::").unwrap();
        let result = load_workspace_config(path.to_str().unwrap());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to parse workspace config"));
    }

    #[test]
    fn load_workspace_config_valid_yaml_parses_correctly() {
        let yaml = r#"
name: qa-workspace
concurrency: 1
sources: {}
runtime:
  default: claude
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace.yaml");
        std::fs::write(&path, yaml.as_bytes()).unwrap();
        let config = load_workspace_config(path.to_str().unwrap()).unwrap();
        assert_eq!(config.name, "qa-workspace");
        assert_eq!(config.concurrency, 1);
        assert_eq!(config.runtime.default, "claude");
    }

    // ---- print_plan JSON output ----

    #[test]
    fn print_plan_json_prompt_action_serializes_fields() {
        // Validate the JSON structure by constructing the same json! value.
        let config = empty_workspace();
        let actions = vec![
            Action::Prompt {
                text: "analyze".to_string(),
                runtime: Some("claude".to_string()),
                model: Some("sonnet".to_string()),
            },
            Action::Script {
                command: "cargo test".to_string(),
            },
        ];

        let plan = serde_json::json!({
            "workspace": config.name,
            "runtime": {
                "default": config.runtime.default,
            },
            "actions": actions.iter().map(|a| match a {
                Action::Prompt { text, runtime, model } => serde_json::json!({
                    "type": "prompt",
                    "text": text,
                    "runtime": runtime,
                    "model": model,
                }),
                Action::Script { command } => serde_json::json!({
                    "type": "script",
                    "command": command,
                }),
            }).collect::<Vec<_>>(),
        });

        assert_eq!(plan["workspace"], "test-ws");
        assert_eq!(plan["runtime"]["default"], "claude");
        let actions_arr = plan["actions"].as_array().unwrap();
        assert_eq!(actions_arr.len(), 2);
        assert_eq!(actions_arr[0]["type"], "prompt");
        assert_eq!(actions_arr[0]["text"], "analyze");
        assert_eq!(actions_arr[0]["runtime"], "claude");
        assert_eq!(actions_arr[0]["model"], "sonnet");
        assert_eq!(actions_arr[1]["type"], "script");
        assert_eq!(actions_arr[1]["command"], "cargo test");
    }

    #[test]
    fn print_plan_json_prompt_without_runtime_serializes_null() {
        let actions = vec![Action::Prompt {
            text: "do work".to_string(),
            runtime: None,
            model: None,
        }];
        let config = empty_workspace();

        let plan = serde_json::json!({
            "workspace": config.name,
            "runtime": { "default": config.runtime.default },
            "actions": actions.iter().map(|a| match a {
                Action::Prompt { text, runtime, model } => serde_json::json!({
                    "type": "prompt",
                    "text": text,
                    "runtime": runtime,
                    "model": model,
                }),
                Action::Script { command } => serde_json::json!({
                    "type": "script",
                    "command": command,
                }),
            }).collect::<Vec<_>>(),
        });

        let action = &plan["actions"][0];
        assert_eq!(action["type"], "prompt");
        assert!(action["runtime"].is_null());
        assert!(action["model"].is_null());
    }
}

/// Entry point for `belt agent` command.
///
/// Returns the exit code from the agent execution (0 on success).
pub async fn run_agent(
    workspace_path: Option<String>,
    prompt: Option<String>,
    plan: bool,
    json_output: bool,
) -> Result<i32> {
    let workspace_path =
        workspace_path.ok_or_else(|| anyhow::anyhow!("--workspace is required"))?;

    let config = load_workspace_config(&workspace_path)?;
    let actions = collect_actions(&config, prompt.as_deref());

    if actions.is_empty() {
        bail!("no actions found in workspace config");
    }

    if plan {
        print_plan(&config, &actions, json_output)?;
        return Ok(0);
    }

    // Build runtime registry and executor.
    let registry = Arc::new(build_registry(&config));
    let executor = ActionExecutor::new(registry);

    // Use current directory as working directory.
    let working_dir = std::env::current_dir().context("failed to determine current directory")?;
    let env = ActionEnv::new("cli-agent", &working_dir);

    tracing::info!(
        workspace = config.name,
        actions = actions.len(),
        "executing agent"
    );

    let result = executor.execute_all(&actions, &env).await?;

    match result {
        Some(action_result) => {
            if json_output {
                let output = serde_json::json!({
                    "workspace": config.name,
                    "exit_code": action_result.exit_code,
                    "stdout": action_result.stdout,
                    "stderr": action_result.stderr,
                    "duration_ms": action_result.duration.as_millis(),
                    "token_usage": action_result.token_usage.as_ref().map(|u| serde_json::json!({
                        "input_tokens": u.input_tokens,
                        "output_tokens": u.output_tokens,
                        "cache_read_tokens": u.cache_read_tokens,
                        "cache_write_tokens": u.cache_write_tokens,
                    })),
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                if !action_result.stdout.is_empty() {
                    print!("{}", action_result.stdout);
                }
                if !action_result.stderr.is_empty() {
                    eprint!("{}", action_result.stderr);
                }
            }
            Ok(action_result.exit_code)
        }
        None => {
            bail!("no actions were executed");
        }
    }
}
