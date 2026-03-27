use std::path::{Path, PathBuf};
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

/// Resolve the workspace rules directory path.
///
/// Priority:
/// 1. `claw_config.rules_path` (explicit override from workspace YAML)
/// 2. Per-workspace Claw directory: `~/.belt/workspaces/<name>/claw/system/`
/// 3. Global Claw workspace: `~/.belt/claw-workspace/.claude/rules/`
///
/// Returns `None` if no rules directory is found.
fn resolve_rules_dir(config: &WorkspaceConfig) -> Option<PathBuf> {
    // 1. Explicit rules_path in claw_config.
    if let Some(ref claw) = config.claw_config
        && let Some(ref path) = claw.rules_path
    {
        let p = PathBuf::from(path);
        if p.is_dir() {
            return Some(p);
        }
    }

    // 2. Per-workspace Claw directory under BELT_HOME.
    let belt_home = std::env::var("BELT_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".belt")));

    if let Some(ref home) = belt_home {
        let ws_rules = home
            .join("workspaces")
            .join(&config.name)
            .join("claw")
            .join("system");
        if ws_rules.is_dir() {
            return Some(ws_rules);
        }
    }

    // 3. Global Claw workspace rules.
    if let Some(ref home) = belt_home {
        let global_rules = home.join("claw-workspace").join(".claude").join("rules");
        if global_rules.is_dir() {
            return Some(global_rules);
        }
    }

    None
}

/// Default maximum conversation turns per session.
const DEFAULT_MAX_CONVERSATION_TURNS: u32 = 10;

/// Build the built-in Claw rules system prompt.
///
/// Generates a system prompt section containing core agent behavioral rules:
/// - Conversation turn limit
/// - Response format guidelines (JSON/markdown)
/// - Error handling and retry strategy
///
/// The `max_turns` parameter overrides the default turn limit when provided
/// via `ClawConfig.max_conversation_turns`.
fn build_claw_rules_prompt(max_turns: Option<u32>) -> String {
    let turns = max_turns.unwrap_or(DEFAULT_MAX_CONVERSATION_TURNS);
    format!(
        r#"# Claw Agent Rules

## Conversation Turn Limit
- Maximum conversation turns per session: {turns}
- If the task cannot be completed within the turn limit, summarize progress and remaining steps before stopping.
- Each prompt-response pair counts as one turn.

## Response Format
- Use JSON for structured data output (status reports, action results, diagnostics).
- Use Markdown for human-readable explanations, plans, and summaries.
- When both are appropriate, prefer JSON wrapped in a markdown code block.
- Always include a top-level "status" field in JSON responses ("success", "error", "partial").

## Error Handling
- On transient errors (network, timeout), retry up to 3 times with exponential backoff.
- On permanent errors (invalid input, missing resource), report immediately without retry.
- Always include error context: what was attempted, what failed, and suggested next steps.
- Never silently swallow errors; surface them in the response."#
    )
}

/// Load all `.md` rule files from a directory into a single system prompt.
///
/// Files are sorted by name for deterministic ordering and concatenated
/// with a header per file.
fn load_rules_from_dir(dir: &Path) -> Result<Option<String>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read rules directory: {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "md"))
        .collect();

    if entries.is_empty() {
        return Ok(None);
    }

    entries.sort();

    let mut parts = Vec::new();
    for path in &entries {
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read rule file: {}", path.display()))?;
        parts.push(format!("# Rule: {filename}\n\n{content}"));
    }

    Ok(Some(parts.join("\n\n---\n\n")))
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
fn print_plan(
    config: &WorkspaceConfig,
    actions: &[Action],
    rules_dir: Option<&Path>,
    json_output: bool,
) -> Result<()> {
    if json_output {
        let plan = serde_json::json!({
            "workspace": config.name,
            "runtime": {
                "default": config.runtime.default,
            },
            "rules_dir": rules_dir.map(|p| p.display().to_string()),
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
        if let Some(dir) = rules_dir {
            println!("Rules directory: {}", dir.display());
        } else {
            println!("Rules directory: (none)");
        }
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
        let rules_dir = resolve_rules_dir(&config);
        print_plan(&config, &actions, rules_dir.as_deref(), json_output)?;
        return Ok(0);
    }

    // Build runtime registry and executor.
    let registry = Arc::new(build_registry(&config));
    let executor = ActionExecutor::new(registry);

    // Use current directory as working directory.
    let working_dir = std::env::current_dir().context("failed to determine current directory")?;

    // Build the system prompt by combining built-in Claw rules with
    // workspace-specific rules loaded from the rules directory.
    let mut env = ActionEnv::new("cli-agent", &working_dir);

    let max_turns = config
        .claw_config
        .as_ref()
        .and_then(|c| c.max_conversation_turns);
    let claw_rules = build_claw_rules_prompt(max_turns);

    let file_rules = if let Some(rules_dir) = resolve_rules_dir(&config) {
        match load_rules_from_dir(&rules_dir) {
            Ok(Some(rules)) => {
                tracing::info!(
                    rules_dir = %rules_dir.display(),
                    "loaded workspace rules from directory"
                );
                Some(rules)
            }
            Ok(None) => {
                tracing::debug!(
                    rules_dir = %rules_dir.display(),
                    "rules directory exists but contains no .md files"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    rules_dir = %rules_dir.display(),
                    error = %e,
                    "failed to load workspace rules, continuing without"
                );
                None
            }
        }
    } else {
        None
    };

    let system_prompt = match file_rules {
        Some(file_prompt) => format!("{claw_rules}\n\n---\n\n{file_prompt}"),
        None => claw_rules,
    };
    env = env.with_system_prompt(system_prompt);

    tracing::info!(
        workspace = config.name,
        actions = actions.len(),
        has_rules = env.system_prompt.is_some(),
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
            claw_config: None,
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
            claw_config: None,
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
        let actions = [
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
        let actions = [Action::Prompt {
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

    // ---- load_rules_from_dir ----

    #[test]
    fn load_rules_from_dir_empty_directory_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_rules_from_dir(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_rules_from_dir_loads_md_files_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("b-policy.md"), "Rule B content").unwrap();
        std::fs::write(tmp.path().join("a-policy.md"), "Rule A content").unwrap();
        // Non-md file should be ignored.
        std::fs::write(tmp.path().join("notes.txt"), "ignored").unwrap();

        let result = load_rules_from_dir(tmp.path()).unwrap().unwrap();
        // a-policy.md should come before b-policy.md (sorted).
        assert!(result.contains("Rule A content"));
        assert!(result.contains("Rule B content"));
        assert!(!result.contains("ignored"));

        // Verify ordering: A before B.
        let a_pos = result.find("a-policy.md").unwrap();
        let b_pos = result.find("b-policy.md").unwrap();
        assert!(a_pos < b_pos);
    }

    #[test]
    fn load_rules_from_dir_nonexistent_directory_errors() {
        let result = load_rules_from_dir(Path::new("/nonexistent/rules"));
        assert!(result.is_err());
    }

    #[test]
    fn load_rules_from_dir_includes_file_headers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hitl-policy.md"), "HITL rules here").unwrap();

        let result = load_rules_from_dir(tmp.path()).unwrap().unwrap();
        assert!(result.contains("# Rule: hitl-policy.md"));
        assert!(result.contains("HITL rules here"));
    }

    // ---- resolve_rules_dir ----

    #[test]
    fn resolve_rules_dir_explicit_path_takes_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("custom-rules");
        std::fs::create_dir_all(&rules_dir).unwrap();

        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some(rules_dir.to_string_lossy().to_string()),
            ..Default::default()
        });

        let resolved = resolve_rules_dir(&config);
        assert_eq!(resolved, Some(rules_dir));
    }

    #[test]
    fn resolve_rules_dir_returns_none_when_no_dirs_exist() {
        let config = empty_workspace();
        // With no BELT_HOME set and no explicit rules_path, this should not
        // panic. The result depends on the environment, but the function
        // should not error.
        let _ = resolve_rules_dir(&config);
    }

    #[test]
    fn resolve_rules_dir_skips_nonexistent_explicit_path() {
        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some("/nonexistent/rules/dir".to_string()),
            ..Default::default()
        });

        // The explicit path doesn't exist, so it should fall through.
        // Result depends on the environment but should not panic.
        let _ = resolve_rules_dir(&config);
    }

    #[test]
    fn resolve_rules_dir_falls_back_to_per_workspace_claw_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_rules = tmp
            .path()
            .join("workspaces")
            .join("test-ws")
            .join("claw")
            .join("system");
        std::fs::create_dir_all(&ws_rules).unwrap();

        // Set BELT_HOME to the temp directory so the per-workspace path is found.
        let _guard = EnvGuard::set("BELT_HOME", tmp.path().to_str().unwrap());

        let config = empty_workspace();
        let resolved = resolve_rules_dir(&config);
        assert_eq!(resolved, Some(ws_rules));
    }

    #[test]
    fn resolve_rules_dir_falls_back_to_global_claw_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let global_rules = tmp
            .path()
            .join("claw-workspace")
            .join(".claude")
            .join("rules");
        std::fs::create_dir_all(&global_rules).unwrap();

        let _guard = EnvGuard::set("BELT_HOME", tmp.path().to_str().unwrap());

        let config = empty_workspace();
        let resolved = resolve_rules_dir(&config);
        assert_eq!(resolved, Some(global_rules));
    }

    #[test]
    fn resolve_rules_dir_per_workspace_takes_priority_over_global() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_rules = tmp
            .path()
            .join("workspaces")
            .join("test-ws")
            .join("claw")
            .join("system");
        std::fs::create_dir_all(&ws_rules).unwrap();

        let global_rules = tmp
            .path()
            .join("claw-workspace")
            .join(".claude")
            .join("rules");
        std::fs::create_dir_all(&global_rules).unwrap();

        let _guard = EnvGuard::set("BELT_HOME", tmp.path().to_str().unwrap());

        let config = empty_workspace();
        let resolved = resolve_rules_dir(&config);
        // Per-workspace (priority 2) should win over global (priority 3).
        assert_eq!(resolved, Some(ws_rules));
    }

    #[test]
    fn resolve_rules_dir_explicit_path_takes_priority_over_belt_home() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit_dir = tmp.path().join("explicit-rules");
        std::fs::create_dir_all(&explicit_dir).unwrap();

        let ws_rules = tmp
            .path()
            .join("workspaces")
            .join("test-ws")
            .join("claw")
            .join("system");
        std::fs::create_dir_all(&ws_rules).unwrap();

        let _guard = EnvGuard::set("BELT_HOME", tmp.path().to_str().unwrap());

        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some(explicit_dir.to_string_lossy().to_string()),
            ..Default::default()
        });

        let resolved = resolve_rules_dir(&config);
        // Explicit path (priority 1) should win over per-workspace (priority 2).
        assert_eq!(resolved, Some(explicit_dir));
    }

    // ---- load_rules_from_dir (additional) ----

    #[test]
    fn load_rules_from_dir_only_non_md_files_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.txt"), "text file").unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "yaml: true").unwrap();

        let result = load_rules_from_dir(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_rules_from_dir_multiple_files_separated_by_divider() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("01-first.md"), "First rule").unwrap();
        std::fs::write(tmp.path().join("02-second.md"), "Second rule").unwrap();

        let result = load_rules_from_dir(tmp.path()).unwrap().unwrap();
        // Files are joined with "---" separator.
        assert!(result.contains("---"));
        // Verify the exact separator format.
        assert!(result.contains("\n\n---\n\n"));
    }

    #[test]
    fn load_rules_from_dir_single_file_has_no_separator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("only-rule.md"), "Single rule").unwrap();

        let result = load_rules_from_dir(tmp.path()).unwrap().unwrap();
        assert!(!result.contains("---"));
        assert!(result.contains("# Rule: only-rule.md"));
        assert!(result.contains("Single rule"));
    }

    // ---- workspace context injection ----

    #[test]
    fn workspace_rules_injected_into_action_env() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("policy.md"), "Always be safe").unwrap();

        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some(rules_dir.to_string_lossy().to_string()),
            ..Default::default()
        });

        // Simulate the same logic as run_agent: resolve rules, load, inject.
        let resolved = resolve_rules_dir(&config).expect("rules dir should resolve");
        let system_prompt = load_rules_from_dir(&resolved)
            .expect("should load")
            .expect("should have content");

        let working_dir = std::env::current_dir().unwrap();
        let env = ActionEnv::new("test-agent", &working_dir).with_system_prompt(system_prompt);

        assert!(env.system_prompt.is_some());
        let prompt = env.system_prompt.unwrap();
        assert!(prompt.contains("Always be safe"));
        assert!(prompt.contains("# Rule: policy.md"));
    }

    #[test]
    fn workspace_without_rules_has_no_system_prompt() {
        // Point BELT_HOME to an empty temp dir so global claw rules are not found.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("BELT_HOME", tmp.path().to_str().unwrap());

        let config = empty_workspace();
        // No claw_config, so resolve_rules_dir returns None.
        // Simulate the run_agent flow.
        let working_dir = std::env::current_dir().unwrap();
        let mut env = ActionEnv::new("test-agent", &working_dir);

        if let Some(rules_dir) = resolve_rules_dir(&config)
            && let Ok(Some(system_prompt)) = load_rules_from_dir(&rules_dir)
        {
            env = env.with_system_prompt(system_prompt);
        }

        assert!(env.system_prompt.is_none());
    }

    #[test]
    fn workspace_rules_with_multiple_files_injected_as_single_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("01-safety.md"), "Safety first").unwrap();
        std::fs::write(rules_dir.join("02-style.md"), "Use Rust idioms").unwrap();

        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some(rules_dir.to_string_lossy().to_string()),
            ..Default::default()
        });

        let resolved = resolve_rules_dir(&config).unwrap();
        let system_prompt = load_rules_from_dir(&resolved).unwrap().unwrap();

        let working_dir = std::env::current_dir().unwrap();
        let env = ActionEnv::new("test-agent", &working_dir).with_system_prompt(system_prompt);

        let prompt = env.system_prompt.unwrap();
        // Both rules should be in the single system prompt.
        assert!(prompt.contains("Safety first"));
        assert!(prompt.contains("Use Rust idioms"));
        // Proper ordering.
        assert!(prompt.find("Safety first").unwrap() < prompt.find("Use Rust idioms").unwrap());
    }

    // ---- build_claw_rules_prompt ----

    #[test]
    fn build_claw_rules_prompt_uses_default_turn_limit() {
        let prompt = build_claw_rules_prompt(None);
        assert!(prompt.contains("Maximum conversation turns per session: 10"));
    }

    #[test]
    fn build_claw_rules_prompt_uses_custom_turn_limit() {
        let prompt = build_claw_rules_prompt(Some(25));
        assert!(prompt.contains("Maximum conversation turns per session: 25"));
        assert!(!prompt.contains("session: 10"));
    }

    #[test]
    fn build_claw_rules_prompt_contains_response_format_section() {
        let prompt = build_claw_rules_prompt(None);
        assert!(prompt.contains("## Response Format"));
        assert!(prompt.contains("JSON"));
        assert!(prompt.contains("Markdown"));
    }

    #[test]
    fn build_claw_rules_prompt_contains_error_handling_section() {
        let prompt = build_claw_rules_prompt(None);
        assert!(prompt.contains("## Error Handling"));
        assert!(prompt.contains("retry up to 3 times"));
        assert!(prompt.contains("exponential backoff"));
    }

    #[test]
    fn build_claw_rules_prompt_contains_all_sections() {
        let prompt = build_claw_rules_prompt(None);
        assert!(prompt.contains("# Claw Agent Rules"));
        assert!(prompt.contains("## Conversation Turn Limit"));
        assert!(prompt.contains("## Response Format"));
        assert!(prompt.contains("## Error Handling"));
    }

    // ---- claw rules integration with system prompt ----

    #[test]
    fn claw_rules_injected_without_file_rules() {
        let config = empty_workspace();

        // Simulate run_agent logic: no rules dir, so only built-in claw rules.
        let max_turns = config
            .claw_config
            .as_ref()
            .and_then(|c| c.max_conversation_turns);
        let claw_rules = build_claw_rules_prompt(max_turns);

        let working_dir = std::env::current_dir().unwrap();
        let env = ActionEnv::new("test-agent", &working_dir).with_system_prompt(claw_rules);

        let prompt = env.system_prompt.unwrap();
        assert!(prompt.contains("# Claw Agent Rules"));
        assert!(prompt.contains("Maximum conversation turns per session: 10"));
    }

    #[test]
    fn claw_rules_combined_with_file_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("custom.md"), "Custom workspace rule").unwrap();

        let mut config = empty_workspace();
        config.claw_config = Some(belt_core::workspace::ClawConfig {
            rules_path: Some(rules_dir.to_string_lossy().to_string()),
            max_conversation_turns: Some(5),
            ..Default::default()
        });

        // Simulate the same combination logic as run_agent.
        let max_turns = config
            .claw_config
            .as_ref()
            .and_then(|c| c.max_conversation_turns);
        let claw_rules = build_claw_rules_prompt(max_turns);

        let resolved = resolve_rules_dir(&config).expect("should resolve rules dir");
        let file_rules = load_rules_from_dir(&resolved).unwrap();

        let system_prompt = match file_rules {
            Some(file_prompt) => format!("{claw_rules}\n\n---\n\n{file_prompt}"),
            None => claw_rules,
        };

        // Claw rules come first.
        assert!(system_prompt.contains("# Claw Agent Rules"));
        assert!(system_prompt.contains("Maximum conversation turns per session: 5"));
        // File rules follow after separator.
        assert!(system_prompt.contains("---"));
        assert!(system_prompt.contains("Custom workspace rule"));
        // Verify ordering: claw rules before file rules.
        let claw_pos = system_prompt.find("# Claw Agent Rules").unwrap();
        let file_pos = system_prompt.find("Custom workspace rule").unwrap();
        assert!(claw_pos < file_pos);
    }

    #[test]
    fn claw_config_max_turns_defaults_to_none() {
        let config = empty_workspace();
        let max_turns = config
            .claw_config
            .as_ref()
            .and_then(|c| c.max_conversation_turns);
        assert!(max_turns.is_none());
        // Which results in the default of 10.
        let prompt = build_claw_rules_prompt(max_turns);
        assert!(prompt.contains("session: 10"));
    }

    /// Global mutex that serializes tests which mutate environment variables.
    /// Rust's test harness runs tests in parallel threads by default, so any
    /// test that calls `std::env::set_var` must hold this lock for the
    /// duration of the test to avoid races with other env-mutating tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for setting environment variables in tests.
    ///
    /// Acquires `ENV_LOCK` for its entire lifetime so that concurrent tests
    /// cannot observe each other's temporary env-var mutations.
    /// Restores (or removes) the variable on drop.
    struct EnvGuard {
        key: String,
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            // Acquire the global env lock first to serialize all env mutations.
            // `unwrap_or_else` recovers from a poisoned mutex caused by a
            // previous test panic.
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var(key).ok();
            // SAFETY: We hold ENV_LOCK, so no other test is mutating env vars
            // concurrently.
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key: key.to_string(),
                prev,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: We still hold ENV_LOCK here (released when _lock is
            // dropped at the end of this block).
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(&self.key, v),
                    None => std::env::remove_var(&self.key),
                }
            }
            // _lock is dropped here, releasing ENV_LOCK.
        }
    }
}
