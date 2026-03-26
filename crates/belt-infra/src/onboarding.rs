//! Workspace onboarding flow.
//!
//! Orchestrates the full workspace registration process:
//! 1. Parse workspace.yml
//! 2. Save workspace to DB
//! 3. Seed per-workspace cron jobs (CR-13)
//! 4. Create per-workspace Claw directory (R-052)

use std::path::{Path, PathBuf};

use belt_core::workspace::WorkspaceConfig;

use crate::db::Database;
use crate::workspace_loader;

/// Result of a successful onboarding operation.
#[derive(Debug)]
pub struct OnboardingResult {
    /// The parsed workspace name.
    pub workspace_name: String,
    /// Absolute path to the config file that was registered.
    pub config_path: String,
    /// Number of data sources configured.
    pub source_count: usize,
    /// Number of cron jobs seeded.
    pub cron_jobs_seeded: usize,
    /// Whether the workspace was newly created (vs. updated).
    pub created: bool,
    /// Path to the per-workspace Claw directory.
    pub claw_dir: PathBuf,
}

/// Per-workspace cron seed definitions (CR-13).
///
/// Each tuple is `(job_name_suffix, schedule_expression)`.
const WORKSPACE_CRON_SEEDS: &[(&str, &str)] = &[
    ("hitl_timeout", "*/5 * * * *"),
    ("daily_report", "0 6 * * *"),
    ("log_cleanup", "0 0 * * *"),
    ("evaluate", "*/1 * * * *"),
    ("gap_detection", "*/30 * * * *"),
    ("knowledge_extraction", "0 2 * * *"),
];

/// Execute the full onboarding flow for a workspace.
///
/// Steps:
/// 1. Load and validate the workspace config from `config_path`.
/// 2. Register (or update) the workspace in the database.
/// 3. Seed per-workspace cron jobs.
/// 4. Create per-workspace Claw directory structure.
///
/// `belt_home` is the root Belt data directory (typically `~/.belt`).
///
/// # Errors
/// Returns an error if the config file cannot be read/parsed, or if DB
/// operations fail.
pub fn onboard_workspace(
    db: &Database,
    config_path: &Path,
    belt_home: &Path,
) -> anyhow::Result<OnboardingResult> {
    // Step 1: Parse workspace.yml
    let config: WorkspaceConfig = workspace_loader::load_workspace_config(config_path)?;

    let abs_path = std::fs::canonicalize(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf())
        .to_string_lossy()
        .to_string();

    // Step 2: Save workspace to DB (insert or update)
    let created = match db.get_workspace(&config.name) {
        Ok(_) => {
            db.update_workspace(&config.name, &abs_path)?;
            tracing::info!(name = %config.name, "workspace updated");
            false
        }
        Err(belt_core::error::BeltError::WorkspaceNotFound(_)) => {
            db.add_workspace(&config.name, &abs_path)?;
            tracing::info!(name = %config.name, "workspace registered");
            true
        }
        Err(e) => return Err(e.into()),
    };

    // Step 3: Seed per-workspace cron jobs (CR-13)
    let cron_jobs_seeded = seed_workspace_cron_jobs(db, &config.name)?;

    // Step 4: Create per-workspace Claw directory (R-052)
    let claw_dir = init_workspace_claw_dir(belt_home, &config.name)?;

    Ok(OnboardingResult {
        workspace_name: config.name,
        config_path: abs_path,
        source_count: config.sources.len(),
        cron_jobs_seeded,
        created,
        claw_dir,
    })
}

/// Default content for the per-workspace Claw `config.yaml`.
fn default_claw_config_yaml(workspace_name: &str) -> String {
    format!(
        r#"# Claw configuration for workspace: {workspace_name}
#
# This file is auto-generated during workspace onboarding.
# Customize per-workspace Claw behavior here.

workspace: {workspace_name}
auto_approve: false
"#
    )
}

/// Create the per-workspace Claw directory structure (R-052).
///
/// Produces the following layout under `belt_home`:
/// ```text
/// {belt_home}/workspaces/<workspace-name>/
///   └── claw/
///       ├── system/
///       ├── session/
///       └── config.yaml
/// ```
///
/// The function is idempotent: existing directories and files are preserved.
///
/// Returns the path to the workspace's `claw/` directory.
pub fn init_workspace_claw_dir(belt_home: &Path, workspace_name: &str) -> anyhow::Result<PathBuf> {
    let ws_dir = belt_home.join("workspaces").join(workspace_name);
    let claw_dir = ws_dir.join("claw");
    let system_dir = claw_dir.join("system");
    let session_dir = claw_dir.join("session");

    std::fs::create_dir_all(&system_dir)?;
    std::fs::create_dir_all(&session_dir)?;

    let config_path = claw_dir.join("config.yaml");
    if !config_path.exists() {
        std::fs::write(&config_path, default_claw_config_yaml(workspace_name))?;
    }

    tracing::info!(
        workspace = %workspace_name,
        path = %claw_dir.display(),
        "per-workspace claw directory initialized"
    );

    Ok(claw_dir)
}

/// Seed built-in cron jobs scoped to a workspace.
///
/// Job names are prefixed with the workspace name to avoid collisions
/// (e.g. `my-project:hitl_timeout`). Existing jobs are skipped.
fn seed_workspace_cron_jobs(db: &Database, workspace_name: &str) -> anyhow::Result<usize> {
    let mut seeded = 0;

    for (suffix, schedule) in WORKSPACE_CRON_SEEDS {
        let job_name = format!("{workspace_name}:{suffix}");

        // Check if job already exists by listing and filtering.
        // This is acceptable because the number of cron jobs is small.
        let existing = db.list_cron_jobs()?;
        let already_exists = existing.iter().any(|j| j.name == job_name);

        if already_exists {
            tracing::debug!(job = %job_name, "cron job already exists, skipping");
            continue;
        }

        db.add_cron_job(&job_name, schedule, "", Some(workspace_name))?;
        tracing::info!(job = %job_name, schedule, "cron job seeded");
        seeded += 1;
    }

    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory DB should open")
    }

    fn write_workspace_yaml(content: &str) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp
    }

    /// Create a temporary directory to serve as `belt_home` for tests.
    fn test_belt_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("should create temp dir")
    }

    const VALID_YAML: &str = r#"
name: test-project
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
    scan_interval_secs: 300
  slack:
    url: https://slack.com/workspace
"#;

    #[test]
    fn onboard_creates_workspace_and_cron_jobs() {
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);
        let belt_home = test_belt_home();

        let result = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert_eq!(result.workspace_name, "test-project");
        assert_eq!(result.source_count, 2);
        assert_eq!(result.cron_jobs_seeded, 6);
        assert!(result.created);

        // Verify workspace is in DB
        let (name, _config_path, _created_at) = db.get_workspace("test-project").unwrap();
        assert_eq!(name, "test-project");

        // Verify cron jobs are in DB
        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 6);
        let job_names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(job_names.contains(&"test-project:hitl_timeout"));
        assert!(job_names.contains(&"test-project:daily_report"));
        assert!(job_names.contains(&"test-project:log_cleanup"));
        assert!(job_names.contains(&"test-project:evaluate"));
        assert!(job_names.contains(&"test-project:gap_detection"));
        assert!(job_names.contains(&"test-project:knowledge_extraction"));

        // All jobs should be scoped to the workspace
        for job in &jobs {
            assert_eq!(job.workspace.as_deref(), Some("test-project"));
            assert!(job.enabled);
        }
    }

    #[test]
    fn onboard_updates_existing_workspace() {
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);
        let belt_home = test_belt_home();

        // First onboard
        let result1 = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert!(result1.created);
        assert_eq!(result1.cron_jobs_seeded, 6);

        // Second onboard should update, not create
        let result2 = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert!(!result2.created);
        assert_eq!(result2.cron_jobs_seeded, 0); // Already exist

        // Still only 6 cron jobs total
        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 6);
    }

    #[test]
    fn onboard_invalid_config_errors() {
        let db = test_db();
        let belt_home = test_belt_home();
        let result = onboard_workspace(
            &db,
            Path::new("/nonexistent/workspace.yml"),
            belt_home.path(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn cron_seed_names_are_workspace_scoped() {
        let db = test_db();
        let belt_home = test_belt_home();
        let yaml_a = r#"
name: project-a
sources:
  github:
    url: https://github.com/org/repo-a
"#;
        let yaml_b = r#"
name: project-b
sources:
  github:
    url: https://github.com/org/repo-b
"#;
        let tmp_a = write_workspace_yaml(yaml_a);
        let tmp_b = write_workspace_yaml(yaml_b);

        onboard_workspace(&db, tmp_a.path(), belt_home.path()).unwrap();
        onboard_workspace(&db, tmp_b.path(), belt_home.path()).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 12); // 6 per workspace

        let job_names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(job_names.contains(&"project-a:hitl_timeout"));
        assert!(job_names.contains(&"project-b:hitl_timeout"));
    }

    #[test]
    fn onboard_invalid_yaml_content_errors() {
        // File exists but contains invalid YAML — distinct from missing file.
        let db = test_db();
        let belt_home = test_belt_home();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"not: [valid: yaml: {{").unwrap();

        let result = onboard_workspace(&db, tmp.path(), belt_home.path());
        assert!(result.is_err());
        // No workspace or cron jobs should have been created.
        assert_eq!(db.list_cron_jobs().unwrap().len(), 0);
    }

    #[test]
    fn onboard_yaml_missing_name_field_errors() {
        let db = test_db();
        let belt_home = test_belt_home();
        let yaml = r#"
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
"#;
        let tmp = write_workspace_yaml(yaml);
        let result = onboard_workspace(&db, tmp.path(), belt_home.path());
        assert!(result.is_err());
    }

    #[test]
    fn onboard_source_count_reflects_config() {
        let db = test_db();
        let belt_home = test_belt_home();
        let yaml = r#"
name: three-source-project
sources:
  github:
    url: https://github.com/org/repo
  slack:
    url: https://slack.com/workspace
  jira:
    url: https://jira.example.com
"#;
        let tmp = write_workspace_yaml(yaml);
        let result = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert_eq!(result.source_count, 3);
    }

    #[test]
    fn onboard_zero_sources_succeeds() {
        let db = test_db();
        let belt_home = test_belt_home();
        let yaml = "name: no-sources\nsources: {}\n";
        let tmp = write_workspace_yaml(yaml);
        let result = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert_eq!(result.workspace_name, "no-sources");
        assert_eq!(result.source_count, 0);
        assert!(result.created);
        // Cron jobs are still seeded regardless of source count.
        assert_eq!(result.cron_jobs_seeded, 6);
    }

    #[test]
    fn onboard_result_config_path_is_nonempty() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);
        let result = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert!(!result.config_path.is_empty());
    }

    #[test]
    fn onboard_cron_jobs_have_correct_schedules() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        let job_map: std::collections::HashMap<&str, &str> = jobs
            .iter()
            .map(|j| (j.name.as_str(), j.schedule.as_str()))
            .collect();

        assert_eq!(
            job_map.get("test-project:hitl_timeout"),
            Some(&"*/5 * * * *")
        );
        assert_eq!(job_map.get("test-project:daily_report"), Some(&"0 6 * * *"));
        assert_eq!(job_map.get("test-project:log_cleanup"), Some(&"0 0 * * *"));
        assert_eq!(job_map.get("test-project:evaluate"), Some(&"*/1 * * * *"));
        assert_eq!(
            job_map.get("test-project:gap_detection"),
            Some(&"*/30 * * * *")
        );
        assert_eq!(
            job_map.get("test-project:knowledge_extraction"),
            Some(&"0 2 * * *")
        );
    }

    #[test]
    fn onboard_triple_call_is_idempotent() {
        // Running onboard three times should not accumulate cron jobs.
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);

        onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        let result3 = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();

        assert!(!result3.created);
        assert_eq!(result3.cron_jobs_seeded, 0);
        assert_eq!(db.list_cron_jobs().unwrap().len(), 6);
    }

    #[test]
    fn onboard_all_seeded_cron_jobs_are_enabled() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        for job in &jobs {
            assert!(
                job.enabled,
                "cron job '{}' should be enabled after seeding",
                job.name
            );
        }
    }

    #[test]
    fn onboard_cron_jobs_have_correct_workspace_scope() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        for job in &jobs {
            assert_eq!(
                job.workspace.as_deref(),
                Some("test-project"),
                "cron job '{}' should be scoped to 'test-project'",
                job.name
            );
        }
    }

    #[test]
    fn onboard_creates_per_workspace_claw_dir() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);

        let result = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();

        // Verify claw directory structure
        let claw_dir = belt_home.path().join("workspaces/test-project/claw");
        assert_eq!(result.claw_dir, claw_dir);
        assert!(claw_dir.join("system").is_dir());
        assert!(claw_dir.join("session").is_dir());
        assert!(claw_dir.join("config.yaml").is_file());

        // Verify config.yaml content
        let config_content = std::fs::read_to_string(claw_dir.join("config.yaml")).unwrap();
        assert!(config_content.contains("workspace: test-project"));
    }

    #[test]
    fn onboard_claw_dir_is_idempotent() {
        let db = test_db();
        let belt_home = test_belt_home();
        let tmp = write_workspace_yaml(VALID_YAML);

        let result1 = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        let claw_dir = result1.claw_dir.clone();

        // Write custom content to config.yaml
        let custom = "# custom config\n";
        std::fs::write(claw_dir.join("config.yaml"), custom).unwrap();

        // Re-onboard should preserve existing config.yaml
        let result2 = onboard_workspace(&db, tmp.path(), belt_home.path()).unwrap();
        assert_eq!(result2.claw_dir, claw_dir);

        let content = std::fs::read_to_string(claw_dir.join("config.yaml")).unwrap();
        assert_eq!(content, custom, "existing config.yaml should be preserved");
    }

    #[test]
    fn init_workspace_claw_dir_creates_structure() {
        let belt_home = test_belt_home();
        let claw_dir = init_workspace_claw_dir(belt_home.path(), "my-project").unwrap();

        assert!(claw_dir.join("system").is_dir());
        assert!(claw_dir.join("session").is_dir());
        assert!(claw_dir.join("config.yaml").is_file());
        assert_eq!(
            claw_dir,
            belt_home.path().join("workspaces/my-project/claw")
        );
    }

    // ── init_workspace_claw_dir() unit tests ──────────────────────────

    #[test]
    fn init_claw_dir_already_exists_preserves_content() {
        let belt_home = test_belt_home();

        // First call creates the structure.
        let claw_dir = init_workspace_claw_dir(belt_home.path(), "existing-ws").unwrap();
        assert!(claw_dir.join("config.yaml").is_file());

        // Write extra files into the created directories.
        std::fs::write(claw_dir.join("system/custom.txt"), "keep me").unwrap();
        std::fs::write(claw_dir.join("session/state.json"), "{}").unwrap();

        // Second call should succeed without removing existing content.
        let claw_dir2 = init_workspace_claw_dir(belt_home.path(), "existing-ws").unwrap();
        assert_eq!(claw_dir, claw_dir2);

        // Custom files should still be present.
        assert_eq!(
            std::fs::read_to_string(claw_dir.join("system/custom.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            std::fs::read_to_string(claw_dir.join("session/state.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn init_claw_dir_preserves_existing_config_yaml() {
        let belt_home = test_belt_home();

        // Create the directory once.
        let claw_dir = init_workspace_claw_dir(belt_home.path(), "preserve-ws").unwrap();

        // Overwrite config.yaml with custom content.
        let custom_config = "# user-customized\nauto_approve: true\n";
        std::fs::write(claw_dir.join("config.yaml"), custom_config).unwrap();

        // Re-initialize should NOT overwrite the existing config.yaml.
        init_workspace_claw_dir(belt_home.path(), "preserve-ws").unwrap();

        let content = std::fs::read_to_string(claw_dir.join("config.yaml")).unwrap();
        assert_eq!(content, custom_config);
    }

    #[test]
    fn init_claw_dir_config_yaml_contains_workspace_name() {
        let belt_home = test_belt_home();
        let claw_dir = init_workspace_claw_dir(belt_home.path(), "named-ws").unwrap();

        let content = std::fs::read_to_string(claw_dir.join("config.yaml")).unwrap();
        assert!(content.contains("workspace: named-ws"));
        assert!(content.contains("auto_approve: false"));
    }

    #[test]
    fn init_claw_dir_invalid_path_returns_error() {
        // Use a path that cannot be created (file used as directory component).
        let belt_home = test_belt_home();
        let blocker = belt_home.path().join("workspaces");
        // Create a *file* where a directory is expected.
        std::fs::write(&blocker, "I am a file, not a directory").unwrap();

        let result = init_workspace_claw_dir(belt_home.path(), "blocked-ws");
        assert!(result.is_err(), "should fail when path component is a file");
    }

    // ── seed_workspace_cron_jobs() unit tests ───────────────────────────

    #[test]
    fn seed_cron_jobs_creates_all_expected_jobs() {
        let db = test_db();
        let seeded = seed_workspace_cron_jobs(&db, "seed-test").unwrap();

        assert_eq!(seeded, WORKSPACE_CRON_SEEDS.len());

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), WORKSPACE_CRON_SEEDS.len());

        for (suffix, schedule) in WORKSPACE_CRON_SEEDS {
            let expected_name = format!("seed-test:{suffix}");
            let job = jobs.iter().find(|j| j.name == expected_name);
            assert!(job.is_some(), "missing cron job: {expected_name}");
            assert_eq!(job.unwrap().schedule, *schedule);
            assert_eq!(job.unwrap().workspace.as_deref(), Some("seed-test"));
        }
    }

    #[test]
    fn seed_cron_jobs_skips_duplicates() {
        let db = test_db();

        let first = seed_workspace_cron_jobs(&db, "dup-ws").unwrap();
        assert_eq!(first, WORKSPACE_CRON_SEEDS.len());

        // Second call should seed zero new jobs.
        let second = seed_workspace_cron_jobs(&db, "dup-ws").unwrap();
        assert_eq!(second, 0);

        // Total jobs unchanged.
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            WORKSPACE_CRON_SEEDS.len()
        );
    }

    #[test]
    fn seed_cron_jobs_different_workspaces_are_independent() {
        let db = test_db();

        let seeded_a = seed_workspace_cron_jobs(&db, "ws-alpha").unwrap();
        let seeded_b = seed_workspace_cron_jobs(&db, "ws-beta").unwrap();

        assert_eq!(seeded_a, WORKSPACE_CRON_SEEDS.len());
        assert_eq!(seeded_b, WORKSPACE_CRON_SEEDS.len());
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            WORKSPACE_CRON_SEEDS.len() * 2
        );

        // Re-seeding one workspace should not affect the other.
        let re_a = seed_workspace_cron_jobs(&db, "ws-alpha").unwrap();
        assert_eq!(re_a, 0);
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            WORKSPACE_CRON_SEEDS.len() * 2
        );
    }

    #[test]
    fn seed_cron_jobs_job_names_use_colon_separator() {
        let db = test_db();
        seed_workspace_cron_jobs(&db, "sep-check").unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        for job in &jobs {
            assert!(
                job.name.starts_with("sep-check:"),
                "job name '{}' should start with 'sep-check:'",
                job.name
            );
        }
    }

    #[test]
    fn multiple_workspaces_get_separate_claw_dirs() {
        let db = test_db();
        let belt_home = test_belt_home();
        let yaml_a = "name: project-a\nsources:\n  github:\n    url: https://github.com/org/a\n";
        let yaml_b = "name: project-b\nsources:\n  github:\n    url: https://github.com/org/b\n";
        let tmp_a = write_workspace_yaml(yaml_a);
        let tmp_b = write_workspace_yaml(yaml_b);

        let result_a = onboard_workspace(&db, tmp_a.path(), belt_home.path()).unwrap();
        let result_b = onboard_workspace(&db, tmp_b.path(), belt_home.path()).unwrap();

        assert_ne!(result_a.claw_dir, result_b.claw_dir);
        assert!(result_a.claw_dir.join("config.yaml").is_file());
        assert!(result_b.claw_dir.join("config.yaml").is_file());

        let content_a = std::fs::read_to_string(result_a.claw_dir.join("config.yaml")).unwrap();
        let content_b = std::fs::read_to_string(result_b.claw_dir.join("config.yaml")).unwrap();
        assert!(content_a.contains("workspace: project-a"));
        assert!(content_b.contains("workspace: project-b"));
    }
}
