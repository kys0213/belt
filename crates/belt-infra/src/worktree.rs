use std::path::PathBuf;
use std::process::Command;

use belt_core::error::BeltError;

/// Git worktree lifecycle manager.
///
/// Abstracts worktree creation, cleanup, and existence checks
/// so that callers don't need to interact with git commands directly.
pub trait WorktreeManager: Send + Sync {
    /// Creates a worktree or reuses an existing one for the given `work_id`.
    fn create_or_reuse(&self, work_id: &str) -> Result<PathBuf, BeltError>;

    /// Cleans up a worktree (`git worktree remove` + branch deletion).
    fn cleanup(&self, work_id: &str) -> Result<(), BeltError>;

    /// Returns `true` if a worktree for the given `work_id` exists.
    fn exists(&self, work_id: &str) -> bool;

    /// Returns the filesystem path for the worktree associated with `work_id`.
    fn path(&self, work_id: &str) -> PathBuf;
}

/// Sanitizes a `work_id` so it is safe for use as a directory name and git branch suffix.
///
/// Characters that are not alphanumeric, `-`, or `_` are replaced with `_`.
fn sanitize_work_id(work_id: &str) -> String {
    work_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Real git-worktree-based implementation of [`WorktreeManager`].
pub struct GitWorktreeManager {
    /// Base directory where worktrees are created (e.g. `~/.belt/worktrees/`).
    base_dir: PathBuf,
    /// Path to the git repository.
    repo_path: PathBuf,
}

impl GitWorktreeManager {
    /// Creates a new `GitWorktreeManager`.
    pub fn new(base_dir: PathBuf, repo_path: PathBuf) -> Self {
        Self {
            base_dir,
            repo_path,
        }
    }

    /// Returns the branch name used for a given sanitized work id.
    fn branch_name(sanitized: &str) -> String {
        format!("belt/{sanitized}")
    }
}

impl WorktreeManager for GitWorktreeManager {
    fn create_or_reuse(&self, work_id: &str) -> Result<PathBuf, BeltError> {
        let wt_path = self.path(work_id);

        if wt_path.exists() {
            tracing::debug!(work_id, ?wt_path, "worktree already exists, reusing");
            return Ok(wt_path);
        }

        let sanitized = sanitize_work_id(work_id);
        let branch = Self::branch_name(&sanitized);

        let output = Command::new("git")
            .arg("worktree")
            .arg("add")
            .arg(&wt_path)
            .arg("-b")
            .arg(&branch)
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| BeltError::Worktree(format!("failed to run git worktree add: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BeltError::Worktree(format!(
                "git worktree add failed: {stderr}"
            )));
        }

        tracing::info!(work_id, ?wt_path, "worktree created");
        Ok(wt_path)
    }

    fn cleanup(&self, work_id: &str) -> Result<(), BeltError> {
        let wt_path = self.path(work_id);
        let sanitized = sanitize_work_id(work_id);
        let branch = Self::branch_name(&sanitized);

        // git worktree remove --force <path>
        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&wt_path)
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| BeltError::Worktree(format!("failed to run git worktree remove: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BeltError::Worktree(format!(
                "git worktree remove failed: {stderr}"
            )));
        }

        // git branch -D belt/<sanitized>
        let output = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&self.repo_path)
            .output()
            .map_err(|e| BeltError::Worktree(format!("failed to run git branch -D: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BeltError::Worktree(format!(
                "git branch -D failed: {stderr}"
            )));
        }

        tracing::info!(work_id, ?wt_path, "worktree cleaned up");
        Ok(())
    }

    fn exists(&self, work_id: &str) -> bool {
        self.path(work_id).exists()
    }

    fn path(&self, work_id: &str) -> PathBuf {
        let sanitized = sanitize_work_id(work_id);
        self.base_dir.join(sanitized)
    }
}

/// Mock implementation of [`WorktreeManager`] for testing.
///
/// Creates and removes plain directories instead of real git worktrees.
pub struct MockWorktreeManager {
    /// Base directory for mock worktrees.
    base_dir: PathBuf,
}

impl MockWorktreeManager {
    /// Creates a new `MockWorktreeManager`.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
}

impl WorktreeManager for MockWorktreeManager {
    fn create_or_reuse(&self, work_id: &str) -> Result<PathBuf, BeltError> {
        let wt_path = self.path(work_id);

        if wt_path.exists() {
            return Ok(wt_path);
        }

        std::fs::create_dir_all(&wt_path).map_err(|e| {
            BeltError::Worktree(format!("failed to create mock worktree directory: {e}"))
        })?;

        Ok(wt_path)
    }

    fn cleanup(&self, work_id: &str) -> Result<(), BeltError> {
        let wt_path = self.path(work_id);

        if wt_path.exists() {
            std::fs::remove_dir_all(&wt_path).map_err(|e| {
                BeltError::Worktree(format!("failed to remove mock worktree directory: {e}"))
            })?;
        }

        Ok(())
    }

    fn exists(&self, work_id: &str) -> bool {
        self.path(work_id).exists()
    }

    fn path(&self, work_id: &str) -> PathBuf {
        let sanitized = sanitize_work_id(work_id);
        self.base_dir.join(sanitized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitize_simple_id() {
        assert_eq!(sanitize_work_id("hello-world"), "hello-world");
    }

    #[test]
    fn sanitize_complex_id() {
        assert_eq!(
            sanitize_work_id("github:org/repo#42:implement"),
            "github_org_repo_42_implement"
        );
    }

    #[test]
    fn sanitize_preserves_underscores_and_dashes() {
        assert_eq!(sanitize_work_id("my_work-id"), "my_work-id");
    }

    #[test]
    fn sanitize_all_special_chars() {
        assert_eq!(sanitize_work_id("a!@#$%b"), "a_____b");
    }

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize_work_id(""), "");
    }

    #[test]
    fn mock_create_or_reuse_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr.create_or_reuse("work-1").unwrap();
        assert!(path.exists());
        assert!(path.is_dir());
    }

    #[test]
    fn mock_create_or_reuse_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let p1 = mgr.create_or_reuse("work-1").unwrap();
        let p2 = mgr.create_or_reuse("work-1").unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn mock_exists_returns_false_initially() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        assert!(!mgr.exists("work-1"));
    }

    #[test]
    fn mock_exists_returns_true_after_create() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        mgr.create_or_reuse("work-1").unwrap();
        assert!(mgr.exists("work-1"));
    }

    #[test]
    fn mock_cleanup_removes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        mgr.create_or_reuse("work-1").unwrap();
        assert!(mgr.exists("work-1"));

        mgr.cleanup("work-1").unwrap();
        assert!(!mgr.exists("work-1"));
    }

    #[test]
    fn mock_cleanup_nonexistent_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        // Should not error when cleaning up something that doesn't exist.
        mgr.cleanup("nonexistent").unwrap();
    }

    #[test]
    fn mock_path_uses_sanitized_work_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr.path("github:org/repo#42");
        assert_eq!(path, tmp.path().join("github_org_repo_42"));
    }

    #[test]
    fn git_worktree_manager_with_temp_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        let wt_base = tmp.path().join("worktrees");

        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&wt_base).unwrap();

        // Initialize a bare-minimum git repo with an initial commit.
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&repo_path)
                .output()
                .expect("git command failed")
        };

        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Test"]);

        // Create an initial commit so HEAD exists.
        fs::write(repo_path.join("README"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let mgr = GitWorktreeManager::new(wt_base.clone(), repo_path.clone());

        // create
        let wt_path = mgr.create_or_reuse("task-1").unwrap();
        assert!(wt_path.exists());
        assert_eq!(wt_path, wt_base.join("task-1"));

        // exists
        assert!(mgr.exists("task-1"));

        // reuse
        let wt_path2 = mgr.create_or_reuse("task-1").unwrap();
        assert_eq!(wt_path, wt_path2);

        // cleanup
        mgr.cleanup("task-1").unwrap();
        assert!(!mgr.exists("task-1"));
    }
}
