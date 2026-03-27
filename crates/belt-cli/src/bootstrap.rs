//! Bootstrap command for generating `.claude/rules` files in a workspace.
//!
//! Creates a set of opinionated rule files (project, coding, commit, testing)
//! under `.claude/rules/` in the target directory. Existing files are preserved
//! unless `--force` is specified.
//!
//! When `--llm` is specified, the bootstrap process uses an [`AgentRuntime`] to
//! analyze the project and generate tailored convention files instead of using
//! static templates.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use belt_core::runtime::{AgentRuntime, RuntimeRequest};

/// Project information collected for LLM-based convention generation.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// Human-readable project name.
    pub name: String,
    /// Primary programming language (e.g., "Rust", "TypeScript").
    pub language: String,
    /// Framework or runtime (e.g., "tokio", "Next.js").
    pub framework: String,
    /// Brief description of the project purpose.
    pub description: String,
}

/// Result of a bootstrap operation.
pub struct BootstrapResult {
    /// The `.claude/rules` directory that was created or updated.
    pub rules_dir: PathBuf,
    /// Files that were written.
    pub written: Vec<PathBuf>,
    /// Files that were skipped (already existed).
    pub skipped: Vec<PathBuf>,
    /// Whether LLM-based generation was used.
    pub llm_generated: bool,
    /// URL of the created pull request, if any.
    pub pr_url: Option<String>,
}

/// Outcome of the interactive review step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    /// User approved the generated content.
    Approved,
    /// User rejected the generated content.
    Rejected,
}

/// Callback type for interactive review confirmation.
///
/// Receives the list of generated `(filename, content)` pairs and returns
/// whether the user approved or rejected the generated conventions.
pub type ConfirmFn = Box<dyn Fn(&[(String, String)]) -> ReviewDecision>;

/// Run the bootstrap process for the given workspace root directory.
///
/// Creates `.claude/rules/` and generates default rule files. Existing files
/// are preserved unless `force` is `true`.
pub fn run(workspace_root: &Path, force: bool) -> anyhow::Result<BootstrapResult> {
    let rules_dir = workspace_root.join(".claude/rules");
    run_in_dir(&rules_dir, force)
}

/// Run the bootstrap process writing rule files directly into `rules_dir`.
///
/// The directory is created if it does not exist. Existing files are preserved
/// unless `force` is `true`.
pub fn run_in_dir(rules_dir: &Path, force: bool) -> anyhow::Result<BootstrapResult> {
    fs::create_dir_all(rules_dir)?;

    let mut written = Vec::new();
    let mut skipped = Vec::new();

    let files: &[(&str, &str)] = &[
        ("project.md", default_project_md()),
        ("coding.md", default_coding_md()),
        ("commit.md", default_commit_md()),
        ("testing.md", default_testing_md()),
    ];

    for (filename, contents) in files {
        let path = rules_dir.join(filename);
        if force || !path.exists() {
            fs::write(&path, contents)?;
            written.push(path);
        } else {
            tracing::info!(path = %path.display(), "file already exists, skipping");
            skipped.push(path);
        }
    }

    Ok(BootstrapResult {
        rules_dir: rules_dir.to_path_buf(),
        written,
        skipped,
        llm_generated: false,
        pr_url: None,
    })
}

/// Run the bootstrap process using an LLM to generate tailored convention files.
///
/// The LLM is prompted with the provided [`ProjectInfo`] and asked to produce
/// customized project, coding, commit, and testing rule files. The generated
/// content is written to `.claude/rules/` in the workspace root. Existing files
/// are preserved unless `force` is `true`.
///
/// If the LLM invocation fails (non-zero exit code or empty output), the
/// function falls back to the static template approach.
///
/// This is a non-interactive variant that auto-approves all generated content.
/// For interactive review, use [`run_with_llm_interactive`].
#[cfg(test)]
pub async fn run_with_llm(
    workspace_root: &Path,
    force: bool,
    runtime: Arc<dyn AgentRuntime>,
    project_info: &ProjectInfo,
) -> anyhow::Result<BootstrapResult> {
    let auto_approve: ConfirmFn = Box::new(|_files: &[(String, String)]| ReviewDecision::Approved);
    run_with_llm_interactive(
        workspace_root,
        force,
        runtime,
        project_info,
        false,
        Some(auto_approve),
    )
    .await
}

/// Build the user prompt for convention generation.
fn build_llm_prompt(info: &ProjectInfo) -> String {
    format!(
        r#"Generate convention rule files for the following project:

- Project name: {name}
- Language: {language}
- Framework: {framework}
- Description: {description}

Generate four Markdown files with project-specific conventions. Output each file using the following delimiter format:

--- FILE: project.md ---
(project rules content here)

--- FILE: coding.md ---
(coding style guide content here)

--- FILE: commit.md ---
(commit convention content here)

--- FILE: testing.md ---
(testing rules content here)

Requirements:
- Each file must start with a level-1 heading.
- Tailor the content to the specific language, framework, and project type.
- Include concrete examples and tool commands relevant to the stack.
- For project.md: describe the language, stack, architecture patterns, and dependency policy.
- For coding.md: describe formatting tools, naming conventions, error handling, and SOLID principles.
- For commit.md: describe conventional commit format, types, and rules.
- For testing.md: describe unit/integration test structure, quality guidelines, and how to run tests."#,
        name = info.name,
        language = info.language,
        framework = info.framework,
        description = info.description,
    )
}

/// Build the system prompt for convention generation.
fn build_system_prompt() -> String {
    "You are a software engineering conventions expert. \
     Generate clear, actionable convention documents in Markdown format. \
     Use the exact delimiter format requested. \
     Do not include any text outside the file delimiters."
        .to_string()
}

/// Parse the LLM response into a list of (filename, content) pairs.
///
/// Expects the format:
/// ```text
/// --- FILE: <filename> ---
/// <content>
/// ```
///
/// Falls back to assigning the entire response to `project.md` if no
/// delimiters are found, then fills missing files with static templates.
pub fn parse_llm_response(response: &str) -> Vec<(String, String)> {
    let expected_files = ["project.md", "coding.md", "commit.md", "testing.md"];
    let mut files: Vec<(String, String)> = Vec::new();

    // Split by the delimiter pattern.
    let delimiter_prefix = "--- FILE: ";
    let delimiter_suffix = " ---";

    let mut current_file: Option<String> = None;
    let mut current_content = String::new();

    for line in response.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(delimiter_prefix) && trimmed.ends_with(delimiter_suffix) {
            // Save previous file if any.
            if let Some(ref name) = current_file {
                files.push((name.clone(), current_content.trim().to_string()));
            }
            // Extract filename from delimiter.
            let name = &trimmed[delimiter_prefix.len()..trimmed.len() - delimiter_suffix.len()];
            current_file = Some(name.trim().to_string());
            current_content = String::new();
        } else if current_file.is_some() {
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }

    // Save the last file.
    if let Some(ref name) = current_file {
        let content = current_content.trim().to_string();
        if !content.is_empty() {
            files.push((name.clone(), content));
        }
    }

    // If no delimiters were found, assign the whole response to project.md.
    if files.is_empty() && !response.trim().is_empty() {
        files.push(("project.md".to_string(), response.trim().to_string()));
    }

    // Fill missing expected files with static templates.
    let defaults: Vec<(&str, String)> = vec![
        ("project.md", default_project_md().to_string()),
        ("coding.md", default_coding_md().to_string()),
        ("commit.md", default_commit_md().to_string()),
        ("testing.md", default_testing_md().to_string()),
    ];

    for (name, content) in defaults {
        let already_exists = files.iter().any(|(n, _)| n == name);
        if !already_exists {
            files.push((name.to_string(), content));
        }
    }

    // Keep only expected files in the correct order.
    let mut ordered = Vec::new();
    for name in &expected_files {
        if let Some(pos) = files.iter().position(|(n, _)| n == name) {
            ordered.push(files.remove(pos));
        }
    }
    // Append any extra files the LLM may have generated.
    ordered.extend(files);

    ordered
}

/// Returns the default project rules template.
pub fn default_project_md() -> &'static str {
    r#"# Project Rules

## Language & Stack
<!-- Describe the primary language, framework, and key libraries -->
- Language: (e.g., Rust, TypeScript, Go)
- Framework: (e.g., tokio, Next.js, Gin)
- Database: (e.g., SQLite, PostgreSQL)

## Architecture
<!-- Describe the high-level architecture and module boundaries -->
- Module layout and dependency direction
- Key abstractions and interfaces

## Dependencies
- Prefer existing dependencies over adding new ones
- Justify new dependency additions
- Enable only necessary feature flags
"#
}

/// Returns the default coding style guide template.
pub fn default_coding_md() -> &'static str {
    r#"# Coding Style Guide

## Formatting
- Use the project formatter (e.g., `cargo fmt`, `prettier`, `gofmt`)
- Linter must pass with zero warnings

## Naming
- Use descriptive, intention-revealing names
- Follow language idioms (snake_case for Rust, camelCase for JS/TS)

## Error Handling
- Handle errors explicitly; do not silently ignore them
- Use typed errors in library code, anyhow/eyre in application code
- Provide context when propagating errors

## Documentation
- Public APIs must have doc comments
- Complex logic should have inline comments explaining "why"
- Keep comments up to date with code changes

## SOLID Principles
- Single Responsibility: one module, one reason to change
- Open/Closed: extend via traits/interfaces, not modification
- Dependency Inversion: depend on abstractions, not concretions
"#
}

/// Returns the default commit convention template.
pub fn default_commit_md() -> &'static str {
    r#"# Commit Convention

## Format
Use Conventional Commits:

```
<type>(scope): <short summary>

<optional body>

<optional footer>
```

## Types
- `feat`: A new feature
- `fix`: A bug fix
- `refactor`: Code restructuring without behavior change
- `docs`: Documentation only
- `test`: Adding or updating tests
- `chore`: Build, CI, or tooling changes
- `ci`: CI/CD pipeline changes

## Rules
- Summary line: imperative mood, lowercase, no period, max 72 chars
- Body: explain "what" and "why", not "how"
- Reference issues with `Closes #N` or `Refs #N`
"#
}

/// Returns the default testing rules template.
pub fn default_testing_md() -> &'static str {
    r#"# Testing Rules

## Unit Tests
- Place unit tests in the same file under `#[cfg(test)] mod tests` (Rust)
  or co-located `*.test.ts` / `*_test.go` files
- Test one behavior per test function
- Use descriptive test names that explain the scenario

## Integration Tests
- Place in a dedicated `tests/` directory
- Test cross-module interactions and external boundaries
- Use fixtures or factories for test data setup

## Test Quality
- Tests must be deterministic (no flaky tests)
- Mock external dependencies (DB, API, filesystem)
- Prefer black-box testing over white-box testing
- Aim for high coverage on business logic; skip trivial getters/setters

## Running Tests
- All tests must pass before committing
- CI runs the full test suite on every PR
"#
}

/// Run the LLM-based bootstrap with interactive review and optional PR creation.
///
/// This function generates convention files via LLM, displays a preview to the
/// user, and asks for confirmation. If approved, the files are written and
/// optionally a pull request is created via `gh` CLI.
///
/// The `confirm_fn` parameter allows injecting a custom confirmation function
/// for testing. Pass `None` to use the default stdin-based confirmation.
pub async fn run_with_llm_interactive(
    workspace_root: &Path,
    force: bool,
    runtime: Arc<dyn AgentRuntime>,
    project_info: &ProjectInfo,
    create_pr: bool,
    confirm_fn: Option<ConfirmFn>,
) -> anyhow::Result<BootstrapResult> {
    let rules_dir = workspace_root.join(".claude/rules");
    fs::create_dir_all(&rules_dir)?;

    let prompt = build_llm_prompt(project_info);
    let system_prompt = build_system_prompt();

    let request = RuntimeRequest {
        working_dir: workspace_root.to_path_buf(),
        prompt,
        model: None,
        system_prompt: Some(system_prompt),
        session_id: None,
        structured_output: None,
    };

    let response = runtime.invoke(request).await;

    if !response.success() || response.stdout.trim().is_empty() {
        tracing::warn!(
            exit_code = response.exit_code,
            "LLM invocation failed, falling back to static templates"
        );
        return run_in_dir(&rules_dir, force);
    }

    let generated = parse_llm_response(&response.stdout);

    // Interactive review: show preview and ask for confirmation.
    let decision = match confirm_fn {
        Some(f) => f(&generated),
        None => prompt_user_review(&generated),
    };

    if decision == ReviewDecision::Rejected {
        tracing::info!("user rejected LLM-generated conventions");
        return Ok(BootstrapResult {
            rules_dir,
            written: Vec::new(),
            skipped: Vec::new(),
            llm_generated: true,
            pr_url: None,
        });
    }

    // Write approved files.
    let mut written = Vec::new();
    let mut skipped = Vec::new();

    for (filename, contents) in &generated {
        let path = rules_dir.join(filename);
        if force || !path.exists() {
            fs::write(&path, contents)?;
            written.push(path);
        } else {
            tracing::info!(path = %path.display(), "file already exists, skipping");
            skipped.push(path);
        }
    }

    // Optionally create a PR.
    let pr_url = if create_pr && !written.is_empty() {
        match create_bootstrap_pr(workspace_root) {
            Ok(url) => {
                tracing::info!(pr_url = %url, "pull request created");
                Some(url)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create pull request");
                None
            }
        }
    } else {
        None
    };

    Ok(BootstrapResult {
        rules_dir,
        written,
        skipped,
        llm_generated: true,
        pr_url,
    })
}

/// Display generated convention files and prompt the user for approval via stdin.
fn prompt_user_review(files: &[(String, String)]) -> ReviewDecision {
    println!("\n--- Generated Convention Files ---\n");
    for (filename, content) in files {
        println!("=== {} ===", filename);
        // Show a truncated preview (first 20 lines) to avoid flooding the terminal.
        let preview_lines: Vec<&str> = content.lines().take(20).collect();
        for line in &preview_lines {
            println!("  {}", line);
        }
        let total_lines = content.lines().count();
        if total_lines > 20 {
            println!("  ... ({} more lines)", total_lines - 20);
        }
        println!();
    }

    print!("Accept these conventions? [Y/n] ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return ReviewDecision::Rejected;
    }

    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() || trimmed == "y" || trimmed == "yes" {
        ReviewDecision::Approved
    } else {
        ReviewDecision::Rejected
    }
}

/// Create a git branch, commit convention files, and open a PR via `gh` CLI.
///
/// Returns the PR URL on success.
fn create_bootstrap_pr(workspace_root: &Path) -> anyhow::Result<String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let branch_name = format!("bootstrap/{}", timestamp);

    // Create and checkout a new branch.
    run_git(workspace_root, &["checkout", "-b", &branch_name])?;

    // Stage the convention files.
    run_git(workspace_root, &["add", ".claude/rules/"])?;

    // Commit.
    run_git(
        workspace_root,
        &[
            "commit",
            "-m",
            "chore(bootstrap): generate conventions\n\nGenerated by `belt bootstrap --llm`.",
        ],
    )?;

    // Push the branch.
    run_git(workspace_root, &["push", "-u", "origin", &branch_name])?;

    // Create PR via gh CLI.
    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            "chore: add convention.yaml",
            "--body",
            "Generated convention files via `belt bootstrap --llm`.\n\n\
             This PR adds project-specific convention rules under `.claude/rules/`.",
        ])
        .current_dir(workspace_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh pr create failed: {}", stderr.trim());
    }

    let pr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(pr_url)
}

/// Run a git command in the given directory.
fn run_git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::runtime::{RuntimeCapabilities, RuntimeResponse, TokenUsage};
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;

    /// Test-local mock runtime that returns configurable stdout.
    struct StubRuntime {
        response_stdout: String,
        exit_code: i32,
        calls: Mutex<Vec<String>>,
    }

    impl StubRuntime {
        fn with_stdout(stdout: &str) -> Self {
            Self {
                response_stdout: stdout.to_string(),
                exit_code: 0,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                response_stdout: String::new(),
                exit_code: 1,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentRuntime for StubRuntime {
        fn name(&self) -> &str {
            "stub"
        }

        async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
            self.calls.lock().unwrap().push(request.prompt.clone());
            RuntimeResponse {
                exit_code: self.exit_code,
                stdout: self.response_stdout.clone(),
                stderr: String::new(),
                duration: Duration::from_millis(10),
                token_usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 200,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }),
                session_id: None,
            }
        }

        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities::default()
        }
    }

    #[test]
    fn bootstrap_creates_rules_directory_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let result = run(tmp.path(), false).unwrap();

        assert!(result.rules_dir.exists());
        assert!(result.rules_dir.join("project.md").is_file());
        assert!(result.rules_dir.join("coding.md").is_file());
        assert!(result.rules_dir.join("commit.md").is_file());
        assert!(result.rules_dir.join("testing.md").is_file());
        assert_eq!(result.written.len(), 4);
        assert!(result.skipped.is_empty());
        assert!(!result.llm_generated);
    }

    #[test]
    fn bootstrap_preserves_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), false).unwrap();

        // Modify one file.
        let project_path = tmp.path().join(".claude/rules/project.md");
        let custom = "# Custom project rules";
        fs::write(&project_path, custom).unwrap();

        // Re-run without force.
        let result = run(tmp.path(), false).unwrap();
        assert_eq!(result.skipped.len(), 4);
        assert!(result.written.is_empty());

        let content = fs::read_to_string(&project_path).unwrap();
        assert_eq!(content, custom);
    }

    #[test]
    fn bootstrap_force_overwrites_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), false).unwrap();

        // Modify one file.
        let project_path = tmp.path().join(".claude/rules/project.md");
        fs::write(&project_path, "# Custom").unwrap();

        // Re-run with force.
        let result = run(tmp.path(), true).unwrap();
        assert_eq!(result.written.len(), 4);
        assert!(result.skipped.is_empty());

        let content = fs::read_to_string(&project_path).unwrap();
        assert_eq!(content, default_project_md());
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let r1 = run(tmp.path(), false).unwrap();
        let r2 = run(tmp.path(), false).unwrap();
        assert_eq!(r1.rules_dir, r2.rules_dir);
        // Second run should skip all files.
        assert_eq!(r2.skipped.len(), 4);
    }

    #[test]
    fn default_templates_are_not_empty() {
        assert!(!default_project_md().is_empty());
        assert!(!default_coding_md().is_empty());
        assert!(!default_commit_md().is_empty());
        assert!(!default_testing_md().is_empty());
    }

    // ── parse_llm_response tests ───────────────────────────────────────

    #[test]
    fn parse_response_with_all_delimiters() {
        let response = "\
--- FILE: project.md ---
# My Project Rules
- Rust project

--- FILE: coding.md ---
# Coding Guide
- Use cargo fmt

--- FILE: commit.md ---
# Commit Rules
- Conventional commits

--- FILE: testing.md ---
# Test Rules
- cargo test";

        let files = parse_llm_response(response);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].0, "project.md");
        assert!(files[0].1.contains("My Project Rules"));
        assert_eq!(files[1].0, "coding.md");
        assert!(files[1].1.contains("cargo fmt"));
        assert_eq!(files[2].0, "commit.md");
        assert!(files[2].1.contains("Conventional commits"));
        assert_eq!(files[3].0, "testing.md");
        assert!(files[3].1.contains("cargo test"));
    }

    #[test]
    fn parse_response_fills_missing_files_with_defaults() {
        let response = "\
--- FILE: project.md ---
# Custom Project
- Custom content";

        let files = parse_llm_response(response);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].0, "project.md");
        assert!(files[0].1.contains("Custom Project"));
        // Missing files should use defaults.
        assert_eq!(files[1].0, "coding.md");
        assert_eq!(files[1].1, default_coding_md());
        assert_eq!(files[2].0, "commit.md");
        assert_eq!(files[2].1, default_commit_md());
        assert_eq!(files[3].0, "testing.md");
        assert_eq!(files[3].1, default_testing_md());
    }

    #[test]
    fn parse_response_no_delimiters_assigns_to_project() {
        let response = "# Some freeform content\n- bullet point";
        let files = parse_llm_response(response);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].0, "project.md");
        assert!(files[0].1.contains("Some freeform content"));
    }

    #[test]
    fn parse_response_empty_input() {
        let files = parse_llm_response("");
        // All four defaults should be present.
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].0, "project.md");
        assert_eq!(files[0].1, default_project_md());
    }

    #[test]
    fn parse_response_preserves_order() {
        // Even if the LLM produces files in a different order, output is normalized.
        let response = "\
--- FILE: testing.md ---
# Tests first

--- FILE: project.md ---
# Project second";

        let files = parse_llm_response(response);
        assert_eq!(files[0].0, "project.md");
        assert_eq!(files[1].0, "coding.md"); // default
        assert_eq!(files[2].0, "commit.md"); // default
        assert_eq!(files[3].0, "testing.md");
    }

    // ── build_llm_prompt tests ─────────────────────────────────────────

    #[test]
    fn build_prompt_includes_project_info() {
        let info = ProjectInfo {
            name: "MyApp".to_string(),
            language: "Rust".to_string(),
            framework: "axum".to_string(),
            description: "A web service".to_string(),
        };
        let prompt = build_llm_prompt(&info);
        assert!(prompt.contains("MyApp"));
        assert!(prompt.contains("Rust"));
        assert!(prompt.contains("axum"));
        assert!(prompt.contains("A web service"));
        assert!(prompt.contains("project.md"));
        assert!(prompt.contains("coding.md"));
        assert!(prompt.contains("commit.md"));
        assert!(prompt.contains("testing.md"));
    }

    // ── LLM-based bootstrap integration tests ──────────────────────────

    #[tokio::test]
    async fn llm_bootstrap_writes_generated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_response = "\
--- FILE: project.md ---
# LLM Project Rules
- Generated by LLM

--- FILE: coding.md ---
# LLM Coding Guide
- LLM suggestions

--- FILE: commit.md ---
# LLM Commit Convention
- LLM format

--- FILE: testing.md ---
# LLM Testing Rules
- LLM test approach";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "test-project".to_string(),
            language: "Rust".to_string(),
            framework: "tokio".to_string(),
            description: "test".to_string(),
        };

        let result = run_with_llm(tmp.path(), false, runtime.clone(), &info)
            .await
            .unwrap();

        assert!(result.llm_generated);
        assert_eq!(result.written.len(), 4);
        assert!(result.skipped.is_empty());

        let project_content =
            fs::read_to_string(tmp.path().join(".claude/rules/project.md")).unwrap();
        assert!(project_content.contains("LLM Project Rules"));

        // Verify the runtime was called.
        assert_eq!(runtime.calls().len(), 1);
        assert!(runtime.calls()[0].contains("test-project"));
    }

    #[tokio::test]
    async fn llm_bootstrap_falls_back_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(StubRuntime::failing());
        let info = ProjectInfo {
            name: "fail-project".to_string(),
            language: "Go".to_string(),
            framework: "gin".to_string(),
            description: "test".to_string(),
        };

        let result = run_with_llm(tmp.path(), false, runtime, &info)
            .await
            .unwrap();

        // Should fall back to static templates.
        assert!(!result.llm_generated);
        assert_eq!(result.written.len(), 4);

        let project_content =
            fs::read_to_string(tmp.path().join(".claude/rules/project.md")).unwrap();
        assert_eq!(project_content, default_project_md());
    }

    #[tokio::test]
    async fn llm_bootstrap_respects_force_flag() {
        let tmp = tempfile::tempdir().unwrap();

        // First run with static templates.
        run(tmp.path(), false).unwrap();

        // Second run with LLM and force.
        let llm_response = "\
--- FILE: project.md ---
# Overwritten by LLM

--- FILE: coding.md ---
# LLM Coding

--- FILE: commit.md ---
# LLM Commit

--- FILE: testing.md ---
# LLM Testing";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "force-test".to_string(),
            language: "Python".to_string(),
            framework: "FastAPI".to_string(),
            description: "test".to_string(),
        };

        let result = run_with_llm(tmp.path(), true, runtime, &info)
            .await
            .unwrap();

        assert!(result.llm_generated);
        assert_eq!(result.written.len(), 4);
        assert!(result.skipped.is_empty());

        let content = fs::read_to_string(tmp.path().join(".claude/rules/project.md")).unwrap();
        assert!(content.contains("Overwritten by LLM"));
    }

    #[tokio::test]
    async fn llm_bootstrap_preserves_existing_without_force() {
        let tmp = tempfile::tempdir().unwrap();

        // First run with static templates.
        run(tmp.path(), false).unwrap();

        let llm_response = "\
--- FILE: project.md ---
# Should not overwrite

--- FILE: coding.md ---
# Should not overwrite

--- FILE: commit.md ---
# Should not overwrite

--- FILE: testing.md ---
# Should not overwrite";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "no-force".to_string(),
            language: "Rust".to_string(),
            framework: "tokio".to_string(),
            description: "test".to_string(),
        };

        let result = run_with_llm(tmp.path(), false, runtime, &info)
            .await
            .unwrap();

        assert!(result.llm_generated);
        assert_eq!(result.skipped.len(), 4);
        assert!(result.written.is_empty());

        // Content should still be the original static template.
        let content = fs::read_to_string(tmp.path().join(".claude/rules/project.md")).unwrap();
        assert_eq!(content, default_project_md());
    }

    #[tokio::test]
    async fn llm_bootstrap_prompt_contains_project_info() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(StubRuntime::with_stdout(""));
        let info = ProjectInfo {
            name: "belt".to_string(),
            language: "Rust".to_string(),
            framework: "tokio".to_string(),
            description: "CI automation".to_string(),
        };

        // Even if LLM returns empty, we fall back gracefully.
        let _result = run_with_llm(tmp.path(), false, runtime.clone(), &info)
            .await
            .unwrap();

        let calls = runtime.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("belt"));
        assert!(calls[0].contains("Rust"));
        assert!(calls[0].contains("tokio"));
        assert!(calls[0].contains("CI automation"));
    }

    // ── Interactive bootstrap tests ──────────────────────────────────────

    #[tokio::test]
    async fn interactive_bootstrap_approved_writes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_response = "\
--- FILE: project.md ---
# Interactive Project Rules
- Approved by user

--- FILE: coding.md ---
# Interactive Coding Guide

--- FILE: commit.md ---
# Interactive Commit Rules

--- FILE: testing.md ---
# Interactive Testing Rules";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "interactive-test".to_string(),
            language: "Rust".to_string(),
            framework: "tokio".to_string(),
            description: "test".to_string(),
        };

        let confirm = Box::new(|_files: &[(String, String)]| ReviewDecision::Approved);

        let result = run_with_llm_interactive(
            tmp.path(),
            false,
            runtime,
            &info,
            false, // no PR creation in test
            Some(confirm),
        )
        .await
        .unwrap();

        assert!(result.llm_generated);
        assert_eq!(result.written.len(), 4);
        assert!(result.pr_url.is_none());

        let content = fs::read_to_string(tmp.path().join(".claude/rules/project.md")).unwrap();
        assert!(content.contains("Interactive Project Rules"));
    }

    #[tokio::test]
    async fn interactive_bootstrap_rejected_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_response = "\
--- FILE: project.md ---
# Should not be written

--- FILE: coding.md ---
# Should not be written

--- FILE: commit.md ---
# Should not be written

--- FILE: testing.md ---
# Should not be written";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "reject-test".to_string(),
            language: "Rust".to_string(),
            framework: "tokio".to_string(),
            description: "test".to_string(),
        };

        let confirm = Box::new(|_files: &[(String, String)]| ReviewDecision::Rejected);

        let result =
            run_with_llm_interactive(tmp.path(), false, runtime, &info, false, Some(confirm))
                .await
                .unwrap();

        assert!(result.llm_generated);
        assert!(result.written.is_empty());
        assert!(result.skipped.is_empty());
        assert!(result.pr_url.is_none());

        // No files should have been created.
        let rules_dir = tmp.path().join(".claude/rules");
        assert!(
            !rules_dir.join("project.md").exists(),
            "project.md should not exist after rejection"
        );
    }

    #[tokio::test]
    async fn interactive_bootstrap_falls_back_on_llm_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(StubRuntime::failing());
        let info = ProjectInfo {
            name: "fallback-test".to_string(),
            language: "Go".to_string(),
            framework: "gin".to_string(),
            description: "test".to_string(),
        };

        // confirm_fn should NOT be called when LLM fails (falls back directly).
        let confirm = Box::new(|_files: &[(String, String)]| {
            panic!("confirm_fn should not be called on LLM failure");
        });

        let result =
            run_with_llm_interactive(tmp.path(), false, runtime, &info, false, Some(confirm))
                .await
                .unwrap();

        // Should fall back to static templates without interactive review.
        assert!(!result.llm_generated);
        assert_eq!(result.written.len(), 4);
    }

    #[tokio::test]
    async fn interactive_bootstrap_confirm_receives_generated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let llm_response = "\
--- FILE: project.md ---
# Verify Content

--- FILE: coding.md ---
# Coding Content

--- FILE: commit.md ---
# Commit Content

--- FILE: testing.md ---
# Testing Content";

        let runtime = Arc::new(StubRuntime::with_stdout(llm_response));
        let info = ProjectInfo {
            name: "verify-content".to_string(),
            language: "Rust".to_string(),
            framework: "axum".to_string(),
            description: "test".to_string(),
        };

        let confirm = Box::new(|files: &[(String, String)]| {
            assert_eq!(files.len(), 4);
            assert_eq!(files[0].0, "project.md");
            assert!(files[0].1.contains("Verify Content"));
            ReviewDecision::Approved
        });

        let result =
            run_with_llm_interactive(tmp.path(), false, runtime, &info, false, Some(confirm))
                .await
                .unwrap();

        assert!(result.llm_generated);
        assert_eq!(result.written.len(), 4);
    }

    #[test]
    fn review_decision_equality() {
        assert_eq!(ReviewDecision::Approved, ReviewDecision::Approved);
        assert_eq!(ReviewDecision::Rejected, ReviewDecision::Rejected);
        assert_ne!(ReviewDecision::Approved, ReviewDecision::Rejected);
    }

    #[test]
    fn bootstrap_result_has_pr_url_field() {
        let result = BootstrapResult {
            rules_dir: PathBuf::from("/tmp/test"),
            written: Vec::new(),
            skipped: Vec::new(),
            llm_generated: false,
            pr_url: Some("https://github.com/test/pr/1".to_string()),
        };
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/1")
        );
    }
}
