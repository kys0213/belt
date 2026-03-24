//! Workspace onboarding flow.
//!
//! Orchestrates the full workspace registration process:
//! 1. Parse workspace.yml
//! 2. Save workspace to DB
//! 3. Seed per-workspace cron jobs (CR-13)

use std::path::Path;

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
}

/// Per-workspace cron seed definitions (CR-13).
///
/// Each tuple is `(job_name_suffix, schedule_expression)`.
const WORKSPACE_CRON_SEEDS: &[(&str, &str)] = &[
    ("hitl_timeout", "*/5 * * * *"),
    ("daily_report", "0 6 * * *"),
    ("log_cleanup", "0 0 * * *"),
    ("evaluate", "*/1 * * * *"),
];

/// Execute the full onboarding flow for a workspace.
///
/// Steps:
/// 1. Load and validate the workspace config from `config_path`.
/// 2. Register (or update) the workspace in the database.
/// 3. Seed per-workspace cron jobs.
///
/// # Errors
/// Returns an error if the config file cannot be read/parsed, or if DB
/// operations fail.
pub fn onboard_workspace(db: &Database, config_path: &Path) -> anyhow::Result<OnboardingResult> {
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

    Ok(OnboardingResult {
        workspace_name: config.name,
        config_path: abs_path,
        source_count: config.sources.len(),
        cron_jobs_seeded,
        created,
    })
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

        db.add_cron_job(&job_name, schedule, Some(workspace_name))?;
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

        let result = onboard_workspace(&db, tmp.path()).unwrap();
        assert_eq!(result.workspace_name, "test-project");
        assert_eq!(result.source_count, 2);
        assert_eq!(result.cron_jobs_seeded, 4);
        assert!(result.created);

        // Verify workspace is in DB
        let (name, _config_path, _created_at) = db.get_workspace("test-project").unwrap();
        assert_eq!(name, "test-project");

        // Verify cron jobs are in DB
        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 4);
        let job_names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(job_names.contains(&"test-project:hitl_timeout"));
        assert!(job_names.contains(&"test-project:daily_report"));
        assert!(job_names.contains(&"test-project:log_cleanup"));
        assert!(job_names.contains(&"test-project:evaluate"));

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

        // First onboard
        let result1 = onboard_workspace(&db, tmp.path()).unwrap();
        assert!(result1.created);
        assert_eq!(result1.cron_jobs_seeded, 4);

        // Second onboard should update, not create
        let result2 = onboard_workspace(&db, tmp.path()).unwrap();
        assert!(!result2.created);
        assert_eq!(result2.cron_jobs_seeded, 0); // Already exist

        // Still only 4 cron jobs total
        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 4);
    }

    #[test]
    fn onboard_invalid_config_errors() {
        let db = test_db();
        let result = onboard_workspace(&db, Path::new("/nonexistent/workspace.yml"));
        assert!(result.is_err());
    }

    #[test]
    fn cron_seed_names_are_workspace_scoped() {
        let db = test_db();
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

        onboard_workspace(&db, tmp_a.path()).unwrap();
        onboard_workspace(&db, tmp_b.path()).unwrap();

        let jobs = db.list_cron_jobs().unwrap();
        assert_eq!(jobs.len(), 8); // 4 per workspace

        let job_names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert!(job_names.contains(&"project-a:hitl_timeout"));
        assert!(job_names.contains(&"project-b:hitl_timeout"));
    }

    #[test]
    fn onboard_invalid_yaml_content_errors() {
        // File exists but contains invalid YAML — distinct from missing file.
        let db = test_db();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"not: [valid: yaml: {{").unwrap();

        let result = onboard_workspace(&db, tmp.path());
        assert!(result.is_err());
        // No workspace or cron jobs should have been created.
        assert_eq!(db.list_cron_jobs().unwrap().len(), 0);
    }

    #[test]
    fn onboard_yaml_missing_name_field_errors() {
        let db = test_db();
        let yaml = r#"
concurrency: 2
sources:
  github:
    url: https://github.com/org/repo
"#;
        let tmp = write_workspace_yaml(yaml);
        let result = onboard_workspace(&db, tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn onboard_source_count_reflects_config() {
        let db = test_db();
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
        let result = onboard_workspace(&db, tmp.path()).unwrap();
        assert_eq!(result.source_count, 3);
    }

    #[test]
    fn onboard_zero_sources_succeeds() {
        let db = test_db();
        let yaml = "name: no-sources\nsources: {}\n";
        let tmp = write_workspace_yaml(yaml);
        let result = onboard_workspace(&db, tmp.path()).unwrap();
        assert_eq!(result.workspace_name, "no-sources");
        assert_eq!(result.source_count, 0);
        assert!(result.created);
        // Cron jobs are still seeded regardless of source count.
        assert_eq!(result.cron_jobs_seeded, 4);
    }

    #[test]
    fn onboard_result_config_path_is_nonempty() {
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);
        let result = onboard_workspace(&db, tmp.path()).unwrap();
        assert!(!result.config_path.is_empty());
    }

    #[test]
    fn onboard_cron_jobs_have_correct_schedules() {
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path()).unwrap();

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
    }

    #[test]
    fn onboard_triple_call_is_idempotent() {
        // Running onboard three times should not accumulate cron jobs.
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);

        onboard_workspace(&db, tmp.path()).unwrap();
        onboard_workspace(&db, tmp.path()).unwrap();
        let result3 = onboard_workspace(&db, tmp.path()).unwrap();

        assert!(!result3.created);
        assert_eq!(result3.cron_jobs_seeded, 0);
        assert_eq!(db.list_cron_jobs().unwrap().len(), 4);
    }

    #[test]
    fn onboard_all_seeded_cron_jobs_are_enabled() {
        let db = test_db();
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path()).unwrap();

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
        let tmp = write_workspace_yaml(VALID_YAML);
        onboard_workspace(&db, tmp.path()).unwrap();

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
}
