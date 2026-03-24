//! Bootstrap command for generating `.claude/rules` files in a workspace.
//!
//! Creates a set of opinionated rule files (project, coding, commit, testing)
//! under `.claude/rules/` in the target directory. Existing files are preserved
//! unless `--force` is specified.

use std::fs;
use std::path::{Path, PathBuf};

/// Result of a bootstrap operation.
pub struct BootstrapResult {
    /// The `.claude/rules` directory that was created or updated.
    pub rules_dir: PathBuf,
    /// Files that were written.
    pub written: Vec<PathBuf>,
    /// Files that were skipped (already existed).
    pub skipped: Vec<PathBuf>,
}

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
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
