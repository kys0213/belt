use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

/// Worktree 관리 trait.
#[async_trait]
pub trait WorktreeManager: Send + Sync {
    async fn create_or_reuse(&self, workspace_name: &str, source_id: &str) -> Result<PathBuf>;
    async fn cleanup(&self, worktree_path: &Path) -> Result<()>;
    fn exists(&self, worktree_path: &Path) -> bool;
}

/// 테스트용 MockWorktreeManager — 임시 디렉토리 기반.
pub struct MockWorktreeManager {
    base_dir: PathBuf,
}

impl MockWorktreeManager {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
        }
    }
}

#[async_trait]
impl WorktreeManager for MockWorktreeManager {
    async fn create_or_reuse(&self, workspace_name: &str, source_id: &str) -> Result<PathBuf> {
        let safe_name = source_id.replace([':', '/', '#'], "-");
        let path = self.base_dir.join(format!("{workspace_name}-{safe_name}"));
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }
        Ok(path)
    }

    async fn cleanup(&self, worktree_path: &Path) -> Result<()> {
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path)?;
        }
        Ok(())
    }

    fn exists(&self, worktree_path: &Path) -> bool {
        worktree_path.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn create_and_cleanup() {
        let tmp = TempDir::new().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path());
        let path = mgr.create_or_reuse("auth-project", "github:org/repo#42").await.unwrap();
        assert!(path.exists());
        mgr.cleanup(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn reuse_existing() {
        let tmp = TempDir::new().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path());
        let path1 = mgr.create_or_reuse("ws", "github:org/repo#42").await.unwrap();
        std::fs::write(path1.join("work.txt"), "previous work").unwrap();
        let path2 = mgr.create_or_reuse("ws", "github:org/repo#42").await.unwrap();
        assert_eq!(path1, path2);
        assert!(path2.join("work.txt").exists());
    }

    #[tokio::test]
    async fn different_source_ids_get_different_paths() {
        let tmp = TempDir::new().unwrap();
        let mgr = MockWorktreeManager::new(tmp.path());
        let p1 = mgr.create_or_reuse("ws", "github:org/repo#1").await.unwrap();
        let p2 = mgr.create_or_reuse("ws", "github:org/repo#2").await.unwrap();
        assert_ne!(p1, p2);
    }
}
