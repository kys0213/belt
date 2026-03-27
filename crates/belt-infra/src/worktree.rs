use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use belt_core::error::BeltError;

/// Git worktree lifecycle manager.
///
/// Abstracts worktree creation, cleanup, and existence checks
/// so that callers don't need to interact with git commands directly.
pub trait WorktreeManager: Send + Sync {
    /// Creates a worktree or reuses an existing one for the given `work_id`.
    fn create_or_reuse(&self, work_id: &str) -> Result<PathBuf, BeltError>;

    /// Creates a worktree or reuses an existing one, optionally reusing a
    /// preserved worktree from a previous (failed) item.
    ///
    /// When `previous_worktree_path` is provided and the directory exists,
    /// the implementation renames/moves it to the new work_id location so
    /// that the retry item inherits the preserved working tree state.
    /// Falls back to [`create_or_reuse`](Self::create_or_reuse) when the
    /// previous path is `None` or does not exist.
    fn create_or_reuse_with_previous(
        &self,
        work_id: &str,
        previous_worktree_path: Option<&str>,
    ) -> Result<PathBuf, BeltError> {
        // Default implementation: ignore previous path and delegate.
        let _ = previous_worktree_path;
        self.create_or_reuse(work_id)
    }

    /// Cleans up a worktree (`git worktree remove` + branch deletion).
    fn cleanup(&self, work_id: &str) -> Result<(), BeltError>;

    /// Returns `true` if a worktree for the given `work_id` exists.
    fn exists(&self, work_id: &str) -> bool;

    /// Returns the filesystem path for the worktree associated with `work_id`.
    fn path(&self, work_id: &str) -> PathBuf;

    /// Register a preserved worktree path for a given `source_id`.
    ///
    /// Called during graceful shutdown to record that a worktree was preserved
    /// so it can be reused when the daemon restarts and encounters the same
    /// `source_id`.
    fn register_preserved(&self, _source_id: &str, _worktree_path: PathBuf) {}

    /// Look up a previously preserved worktree path by `source_id`.
    ///
    /// Returns `Some(path)` if a preserved worktree exists on disk for the
    /// given source, `None` otherwise.
    fn lookup_preserved(&self, _source_id: &str) -> Option<PathBuf> {
        None
    }

    /// Remove a preserved worktree mapping for the given `source_id`.
    ///
    /// Called after the preserved worktree has been successfully handed off
    /// to a new work item or explicitly cleaned up.
    fn clear_preserved(&self, _source_id: &str) {}

    /// Validates that a preserved worktree path is still usable.
    ///
    /// Returns `true` if the path exists and is a valid working directory.
    /// For git worktree implementations, this also verifies the git state.
    /// The default implementation only checks that the path exists on disk.
    fn validate_preserved(&self, worktree_path: &std::path::Path) -> bool {
        worktree_path.exists()
    }
}

/// Thread-safe registry for tracking preserved worktrees by `source_id`.
///
/// During graceful shutdown, worktrees for running items are preserved
/// (not cleaned up). This registry remembers which `source_id` maps to which
/// worktree path so that on restart the daemon can hand the worktree off to
/// the re-queued item instead of creating a fresh one.
#[derive(Debug, Default)]
pub struct WorktreeRegistry {
    inner: Mutex<HashMap<String, PathBuf>>,
}

impl WorktreeRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Register a preserved worktree for a `source_id`.
    pub fn register(&self, source_id: &str, path: PathBuf) {
        let mut map = self.inner.lock().expect("worktree registry lock poisoned");
        tracing::debug!(source_id, ?path, "registered preserved worktree");
        map.insert(source_id.to_string(), path);
    }

    /// Look up a preserved worktree by `source_id`.
    ///
    /// Only returns `Some` if the recorded path still exists on disk.
    pub fn lookup(&self, source_id: &str) -> Option<PathBuf> {
        let map = self.inner.lock().expect("worktree registry lock poisoned");
        map.get(source_id)
            .and_then(|p| if p.exists() { Some(p.clone()) } else { None })
    }

    /// Remove a preserved worktree mapping.
    pub fn clear(&self, source_id: &str) {
        let mut map = self.inner.lock().expect("worktree registry lock poisoned");
        if map.remove(source_id).is_some() {
            tracing::debug!(source_id, "cleared preserved worktree mapping");
        }
    }

    /// Returns the number of registered preserved worktrees.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("worktree registry lock poisoned")
            .len()
    }

    /// Returns `true` if no preserved worktrees are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
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
    /// Registry for preserved worktrees (source_id -> path).
    registry: WorktreeRegistry,
}

impl GitWorktreeManager {
    /// Creates a new `GitWorktreeManager`.
    pub fn new(base_dir: PathBuf, repo_path: PathBuf) -> Self {
        Self {
            base_dir,
            repo_path,
            registry: WorktreeRegistry::new(),
        }
    }

    /// Returns the branch name used for a given sanitized work id.
    fn branch_name(sanitized: &str) -> String {
        format!("belt/{sanitized}")
    }

    /// Validates that a worktree path is a usable git worktree.
    ///
    /// Checks that the directory exists and is recognized as a valid git
    /// working directory by running `git -C <path> rev-parse --git-dir`.
    fn validate_worktree(&self, wt_path: &std::path::Path) -> bool {
        if !wt_path.exists() {
            tracing::debug!(?wt_path, "worktree path does not exist");
            return false;
        }

        // Verify that git recognizes this directory as a valid worktree.
        let output = Command::new("git")
            .args(["-C"])
            .arg(wt_path)
            .args(["rev-parse", "--git-dir"])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                tracing::debug!(?wt_path, "worktree validated as a valid git directory");
                true
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(?wt_path, "worktree failed git validation: {stderr}");
                false
            }
            Err(e) => {
                tracing::warn!(?wt_path, "failed to run git rev-parse for validation: {e}");
                false
            }
        }
    }
}

impl WorktreeManager for GitWorktreeManager {
    fn create_or_reuse_with_previous(
        &self,
        work_id: &str,
        previous_worktree_path: Option<&str>,
    ) -> Result<PathBuf, BeltError> {
        let wt_path = self.path(work_id);

        if wt_path.exists() {
            tracing::debug!(work_id, ?wt_path, "worktree already exists, reusing");
            return Ok(wt_path);
        }

        // If a previous worktree path is provided and exists on disk, validate
        // its git state and move it to the new location so the retry item
        // inherits the preserved working tree state.
        if let Some(prev) = previous_worktree_path {
            let prev_path = PathBuf::from(prev);
            if prev_path.exists() {
                // Validate the preserved worktree is still a valid git directory.
                if !self.validate_worktree(&prev_path) {
                    tracing::warn!(
                        work_id,
                        ?prev_path,
                        "preserved worktree failed validation, falling back to fresh create"
                    );
                } else {
                    tracing::info!(
                        work_id,
                        ?prev_path,
                        ?wt_path,
                        "reusing preserved worktree from previous item"
                    );

                    // git worktree move <old> <new>
                    let output = Command::new("git")
                        .args(["worktree", "move"])
                        .arg(&prev_path)
                        .arg(&wt_path)
                        .current_dir(&self.repo_path)
                        .output()
                        .map_err(|e| {
                            BeltError::Worktree(format!("failed to run git worktree move: {e}"))
                        })?;

                    if output.status.success() {
                        tracing::info!(work_id, ?wt_path, "worktree moved from preserved location");
                        return Ok(wt_path);
                    }

                    // Move failed -- fall through to normal creation.
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        work_id,
                        ?prev_path,
                        "git worktree move failed, falling back to fresh create: {stderr}"
                    );
                }
            }
        }

        // Fallback to normal creation.
        self.create_or_reuse(work_id)
    }

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

    fn register_preserved(&self, source_id: &str, worktree_path: PathBuf) {
        self.registry.register(source_id, worktree_path);
    }

    fn lookup_preserved(&self, source_id: &str) -> Option<PathBuf> {
        self.registry.lookup(source_id)
    }

    fn clear_preserved(&self, source_id: &str) {
        self.registry.clear(source_id);
    }

    fn validate_preserved(&self, worktree_path: &std::path::Path) -> bool {
        self.validate_worktree(worktree_path)
    }
}

/// Mock implementation of [`WorktreeManager`] for testing.
///
/// Creates and removes plain directories instead of real git worktrees.
pub struct MockWorktreeManager {
    /// Base directory for mock worktrees.
    base_dir: PathBuf,
    /// Registry for preserved worktrees (source_id -> path).
    registry: WorktreeRegistry,
}

impl MockWorktreeManager {
    /// Creates a new `MockWorktreeManager`.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            registry: WorktreeRegistry::new(),
        }
    }
}

impl WorktreeManager for MockWorktreeManager {
    fn create_or_reuse_with_previous(
        &self,
        work_id: &str,
        previous_worktree_path: Option<&str>,
    ) -> Result<PathBuf, BeltError> {
        let wt_path = self.path(work_id);

        if wt_path.exists() {
            return Ok(wt_path);
        }

        // If a previous worktree directory exists, rename it for the new work_id.
        if let Some(prev) = previous_worktree_path {
            let prev_path = PathBuf::from(prev);
            if prev_path.exists() {
                std::fs::rename(&prev_path, &wt_path).map_err(|e| {
                    BeltError::Worktree(format!(
                        "failed to rename previous worktree {prev_path:?} to {wt_path:?}: {e}"
                    ))
                })?;
                return Ok(wt_path);
            }
        }

        self.create_or_reuse(work_id)
    }

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

    fn register_preserved(&self, source_id: &str, worktree_path: PathBuf) {
        self.registry.register(source_id, worktree_path);
    }

    fn lookup_preserved(&self, source_id: &str) -> Option<PathBuf> {
        self.registry.lookup(source_id)
    }

    fn clear_preserved(&self, source_id: &str) {
        self.registry.clear(source_id);
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
    fn registry_register_and_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorktreeRegistry::new();
        let path = tmp.path().join("wt1");
        fs::create_dir_all(&path).unwrap();

        registry.register("source-1", path.clone());
        assert_eq!(registry.lookup("source-1"), Some(path));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_lookup_returns_none_for_missing_dir() {
        let registry = WorktreeRegistry::new();
        registry.register("source-1", PathBuf::from("/nonexistent/path"));
        // Path does not exist on disk, so lookup should return None.
        assert_eq!(registry.lookup("source-1"), None);
    }

    #[test]
    fn registry_clear_removes_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorktreeRegistry::new();
        let path = tmp.path().join("wt1");
        fs::create_dir_all(&path).unwrap();

        registry.register("source-1", path);
        registry.clear("source-1");
        assert!(registry.is_empty());
        assert_eq!(registry.lookup("source-1"), None);
    }

    #[test]
    fn registry_overwrite_same_source_id() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorktreeRegistry::new();

        let path1 = tmp.path().join("wt1");
        let path2 = tmp.path().join("wt2");
        fs::create_dir_all(&path1).unwrap();
        fs::create_dir_all(&path2).unwrap();

        registry.register("source-1", path1);
        registry.register("source-1", path2.clone());
        assert_eq!(registry.lookup("source-1"), Some(path2));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn mock_register_and_lookup_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        // Create a worktree first so the path exists.
        let path = mgr.create_or_reuse("work-1").unwrap();
        mgr.register_preserved("source-1", path.clone());

        assert_eq!(mgr.lookup_preserved("source-1"), Some(path));
    }

    #[test]
    fn mock_clear_preserved_removes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr.create_or_reuse("work-1").unwrap();
        mgr.register_preserved("source-1", path);
        mgr.clear_preserved("source-1");

        assert_eq!(mgr.lookup_preserved("source-1"), None);
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

    #[test]
    fn mock_create_or_reuse_with_previous_renames_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        // Create a "previous" worktree with some content.
        let prev_path = mgr.create_or_reuse("old-work").unwrap();
        fs::write(prev_path.join("state.txt"), "preserved").unwrap();

        // Create the retry worktree reusing the previous one.
        let new_path = mgr
            .create_or_reuse_with_previous("new-work", Some(prev_path.to_str().unwrap()))
            .unwrap();

        // New path should exist with the preserved content.
        assert!(new_path.exists());
        assert_eq!(
            fs::read_to_string(new_path.join("state.txt")).unwrap(),
            "preserved"
        );

        // Old path should no longer exist (it was renamed).
        assert!(!prev_path.exists());
    }

    #[test]
    fn mock_create_or_reuse_with_previous_none_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr.create_or_reuse_with_previous("work-1", None).unwrap();
        assert!(path.exists());
        assert!(path.is_dir());
    }

    #[test]
    fn mock_create_or_reuse_with_previous_nonexistent_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr
            .create_or_reuse_with_previous("work-1", Some("/nonexistent/path"))
            .unwrap();
        assert!(path.exists());
        assert!(path.is_dir());
    }

    #[test]
    fn mock_create_or_reuse_with_previous_existing_target_reuses() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        // Pre-create the target directory.
        let existing = mgr.create_or_reuse("work-1").unwrap();

        // Even with a previous path, the existing target takes precedence.
        let path = mgr
            .create_or_reuse_with_previous("work-1", Some("/some/old/path"))
            .unwrap();
        assert_eq!(path, existing);
    }

    #[test]
    fn mock_validate_preserved_returns_true_for_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        let path = mgr.create_or_reuse("work-1").unwrap();
        assert!(mgr.validate_preserved(&path));
    }

    #[test]
    fn mock_validate_preserved_returns_false_for_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path().to_path_buf());

        assert!(!mgr.validate_preserved(&PathBuf::from("/nonexistent/path")));
    }

    #[test]
    fn git_worktree_manager_validate_worktree_with_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        let wt_base = tmp.path().join("worktrees");

        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&wt_base).unwrap();

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
        fs::write(repo_path.join("README"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let mgr = GitWorktreeManager::new(wt_base.clone(), repo_path.clone());

        // Create a real worktree.
        let wt_path = mgr.create_or_reuse("task-valid").unwrap();
        assert!(mgr.validate_worktree(&wt_path));

        // A plain directory (not a git worktree) should fail validation.
        let plain_dir = tmp.path().join("not-a-worktree");
        fs::create_dir_all(&plain_dir).unwrap();
        assert!(!mgr.validate_worktree(&plain_dir));

        // Nonexistent path should fail validation.
        assert!(!mgr.validate_worktree(&PathBuf::from("/nonexistent")));

        // Cleanup.
        mgr.cleanup("task-valid").unwrap();
    }

    #[test]
    fn git_worktree_manager_reuse_with_previous_validates_and_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        let wt_base = tmp.path().join("worktrees");

        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&wt_base).unwrap();

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
        fs::write(repo_path.join("README"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let mgr = GitWorktreeManager::new(wt_base.clone(), repo_path.clone());

        // Create the "previous" worktree and add some content.
        let prev_path = mgr.create_or_reuse("old-task").unwrap();
        fs::write(prev_path.join("work.txt"), "preserved-state").unwrap();

        // Reuse the previous worktree for a new task.
        let new_path = mgr
            .create_or_reuse_with_previous("new-task", Some(prev_path.to_str().unwrap()))
            .unwrap();

        assert!(new_path.exists());
        assert_eq!(
            fs::read_to_string(new_path.join("work.txt")).unwrap(),
            "preserved-state"
        );
        // Old path should no longer exist (it was moved).
        assert!(!prev_path.exists());

        // Cleanup: worktree was moved but branch retains old name (belt/old-task).
        // Remove the worktree from the new location and delete the original branch.
        Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&new_path)
            .current_dir(&repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["branch", "-D", "belt/old-task"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
    }

    #[test]
    fn git_worktree_manager_reuse_with_invalid_previous_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("repo");
        let wt_base = tmp.path().join("worktrees");

        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&wt_base).unwrap();

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
        fs::write(repo_path.join("README"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);

        let mgr = GitWorktreeManager::new(wt_base.clone(), repo_path.clone());

        // Create a plain directory (not a valid git worktree) as the "previous".
        let fake_prev = tmp.path().join("fake-worktree");
        fs::create_dir_all(&fake_prev).unwrap();

        // Should fall back to fresh creation because validation fails.
        let new_path = mgr
            .create_or_reuse_with_previous("fallback-task", Some(fake_prev.to_str().unwrap()))
            .unwrap();

        assert!(new_path.exists());
        // The new worktree was created fresh (not moved from the fake).
        assert!(fake_prev.exists()); // fake dir still exists since it wasn't moved

        // Cleanup.
        mgr.cleanup("fallback-task").unwrap();
    }
}
