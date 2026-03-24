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
