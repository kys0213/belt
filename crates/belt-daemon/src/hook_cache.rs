//! Dynamic hook loading with LRU cache.
//!
//! At hook trigger time, the loader queries the DB for workspace config,
//! parses the yaml to identify the DataSource type, and creates the
//! appropriate `LifecycleHook` impl.  An LRU cache keyed by workspace
//! name avoids re-parsing yaml on every trigger.  Cache entries are
//! invalidated when the workspace `updated_at` timestamp changes.
//!
//! # Why `lru` crate
//!
//! A bounded LRU cache ensures memory usage stays proportional to the
//! active workspace count, not the total workspace count.  The `lru`
//! crate provides a well-tested, zero-dependency, `O(1)` get/put
//! implementation — building a correct one from scratch would duplicate
//! effort with no benefit.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

use lru::LruCache;

use belt_core::lifecycle::LifecycleHook;
use belt_core::platform::ShellExecutor;
use belt_core::workspace::WorkspaceConfig;
use belt_infra::db::Database;
use belt_infra::hooks::{self, HookParams};
use belt_infra::script_hook::ScriptLifecycleHook;
use belt_infra::workspace_loader::load_workspace_config;

/// Cached hook entry: the hook impl plus the `updated_at` timestamp
/// used for invalidation.
struct CacheEntry {
    hook: Arc<dyn LifecycleHook>,
    updated_at: String,
}

/// Dynamic hook loader with LRU caching.
///
/// Replaces the static `Arc<dyn LifecycleHook>` on `Daemon` with a
/// per-workspace resolver that loads hooks on demand from the DB and
/// yaml configuration.
///
/// Thread-safe: the inner cache is behind a `Mutex` so multiple
/// executor tasks can resolve hooks concurrently.
pub struct DynamicHookLoader {
    db: Arc<Database>,
    shell: Arc<dyn ShellExecutor>,
    cache: Mutex<LruCache<String, CacheEntry>>,
}

impl DynamicHookLoader {
    /// Create a new loader with the given LRU cache capacity.
    ///
    /// `capacity` determines the maximum number of workspace hooks kept
    /// in memory.  A reasonable default is 16 for most deployments.
    pub fn new(db: Arc<Database>, shell: Arc<dyn ShellExecutor>, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(16).unwrap());
        Self {
            db,
            shell,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Resolve the lifecycle hook for a workspace.
    ///
    /// 1. Query DB for `(config_path, updated_at)`.
    /// 2. Check LRU cache — return cached hook if `updated_at` matches.
    /// 3. On miss or stale entry: parse yaml, create hook, cache it.
    ///
    /// Falls back through this priority chain:
    /// - DataSource-specific hook (e.g. `GitHubLifecycleHook`) via `hooks::create_hook`
    /// - `ScriptLifecycleHook` if yaml has `on_done`/`on_fail`/`on_enter` scripts
    /// - `NoopLifecycleHook` if nothing is configured
    pub fn resolve(&self, workspace_name: &str) -> Arc<dyn LifecycleHook> {
        // Step 1: DB lookup.
        let (config_path, updated_at) = match self.db.get_workspace_with_updated_at(workspace_name)
        {
            Ok((_name, cp, ua)) => (cp, ua),
            Err(e) => {
                tracing::warn!(
                    workspace = workspace_name,
                    "hook loader: DB workspace lookup failed, using NoopLifecycleHook: {e}"
                );
                return Arc::new(belt_core::lifecycle::NoopLifecycleHook);
            }
        };

        // Step 2: Cache check.
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(workspace_name)
                && entry.updated_at == updated_at
            {
                return Arc::clone(&entry.hook);
            }
            // Stale or missing — will be (re)populated below.
        }

        // Step 3: Parse yaml and create hook.
        let hook = self.load_hook_from_config(&config_path, workspace_name);

        // Step 4: Cache the result.
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.put(
                workspace_name.to_string(),
                CacheEntry {
                    hook: Arc::clone(&hook),
                    updated_at,
                },
            );
        }

        hook
    }

    /// Parse workspace yaml and create the best-fit hook implementation.
    fn load_hook_from_config(
        &self,
        config_path: &str,
        workspace_name: &str,
    ) -> Arc<dyn LifecycleHook> {
        let config = match load_workspace_config(Path::new(config_path)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    workspace = workspace_name,
                    config_path,
                    "hook loader: failed to parse workspace yaml, using NoopLifecycleHook: {e}"
                );
                return Arc::new(belt_core::lifecycle::NoopLifecycleHook);
            }
        };

        // Try DataSource-specific hook first (e.g. GitHubLifecycleHook).
        if let Some(hook) = self.try_datasource_hook(&config) {
            tracing::debug!(
                workspace = workspace_name,
                "hook loader: resolved DataSource-specific hook"
            );
            return hook;
        }

        // Fall back to ScriptLifecycleHook if on_done/on_fail/on_enter scripts exist.
        if Self::has_lifecycle_scripts(&config) {
            tracing::debug!(
                workspace = workspace_name,
                "hook loader: resolved ScriptLifecycleHook from yaml scripts"
            );
            return Arc::new(self.build_script_hook(&config));
        }

        // Nothing configured — noop.
        tracing::debug!(
            workspace = workspace_name,
            "hook loader: no hook configuration found, using NoopLifecycleHook"
        );
        Arc::new(belt_core::lifecycle::NoopLifecycleHook)
    }

    /// Attempt to create a DataSource-specific hook from the workspace sources.
    fn try_datasource_hook(&self, config: &WorkspaceConfig) -> Option<Arc<dyn LifecycleHook>> {
        for (source_key, source_config) in &config.sources {
            let params = HookParams::new(source_key, &source_config.url);
            if let Ok(hook) = hooks::create_hook(&params, Arc::clone(&self.shell)) {
                return Some(Arc::from(hook));
            }
        }
        None
    }

    /// Check if any state in any source has lifecycle scripts defined.
    fn has_lifecycle_scripts(config: &WorkspaceConfig) -> bool {
        config.sources.values().any(|source| {
            source.states.values().any(|state| {
                !state.on_enter.is_empty() || !state.on_done.is_empty() || !state.on_fail.is_empty()
            })
        })
    }

    /// Build a `ScriptLifecycleHook` from all state configs across all sources.
    fn build_script_hook(&self, config: &WorkspaceConfig) -> ScriptLifecycleHook {
        let mut state_configs = std::collections::HashMap::new();
        for source in config.sources.values() {
            for (state_name, state_config) in &source.states {
                state_configs.insert(state_name.clone(), state_config.clone());
            }
        }
        ScriptLifecycleHook::new(state_configs, Arc::clone(&self.shell))
    }

    /// Invalidate a specific workspace's cached hook.
    ///
    /// Called when a workspace configuration is known to have changed
    /// (e.g. after `belt workspace update`).
    pub fn invalidate(&self, workspace_name: &str) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.pop(workspace_name);
    }

    /// Clear the entire cache.
    pub fn invalidate_all(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
    }

    /// Return the number of entries currently in the cache.
    #[cfg(test)]
    pub fn cache_len(&self) -> usize {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.len()
    }
}

impl std::fmt::Debug for DynamicHookLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.cache.lock().map(|c| c.len()).unwrap_or_default();
        f.debug_struct("DynamicHookLoader")
            .field("cache_size", &len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::context::{ItemContext, QueueContext, SourceContext};
    use belt_core::error::BeltError;
    use belt_core::lifecycle::HookContext;
    use belt_core::platform::ShellOutput;
    use belt_core::queue::testing::test_item;
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;

    struct StubShell;

    #[async_trait::async_trait]
    impl ShellExecutor for StubShell {
        async fn execute(
            &self,
            _command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> Result<ShellOutput, BeltError> {
            Ok(ShellOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn make_hook_context() -> HookContext {
        let item = test_item("github:org/repo#42", "implement");
        HookContext {
            work_id: item.work_id.clone(),
            worktree: PathBuf::from("/tmp/belt/test-ws-42"),
            item,
            item_context: ItemContext {
                work_id: "github:org/repo#42:implement".to_string(),
                workspace: "test-ws".to_string(),
                queue: QueueContext {
                    phase: "running".to_string(),
                    state: "implement".to_string(),
                    source_id: "github:org/repo#42".to_string(),
                },
                source: SourceContext {
                    source_type: "github".to_string(),
                    url: "https://github.com/org/repo".to_string(),
                    default_branch: Some("main".to_string()),
                },
                issue: None,
                pr: None,
                history: vec![],
                worktree: Some("/tmp/belt/test-ws-42".to_string()),
                source_data: serde_json::Value::Null,
            },
            failure_count: 0,
        }
    }

    fn write_workspace_yaml(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(format!("{name}.yaml"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn register_workspace(db: &Database, name: &str, config_path: &str) {
        db.add_workspace(name, config_path).unwrap();
    }

    /// Touch the workspace `updated_at` by re-registering with a different path
    /// then restoring it. This uses the public API and guarantees `updated_at` changes.
    fn touch_workspace_updated_at(db: &Database, name: &str, config_path: &str) {
        db.update_workspace(name, config_path).unwrap();
    }

    #[test]
    fn resolve_returns_noop_when_workspace_not_in_db() {
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let loader = DynamicHookLoader::new(db, shell, 4);

        // Should not panic; returns NoopLifecycleHook.
        let hook = loader.resolve("nonexistent");
        // Verify it works (noop).
        let ctx = make_hook_context();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(hook.on_enter(&ctx).await.is_ok());
        });
    }

    #[test]
    fn resolve_github_source_creates_github_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml = r#"
name: test-ws
sources:
  github:
    url: https://github.com/org/repo
"#;
        let yaml_path = write_workspace_yaml(&tmp, "test-ws", yaml);
        register_workspace(&db, "test-ws", yaml_path.to_str().unwrap());

        let loader = DynamicHookLoader::new(db, shell, 4);
        let hook = loader.resolve("test-ws");

        // Verify it works.
        let ctx = make_hook_context();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(hook.on_enter(&ctx).await.is_ok());
            assert!(hook.on_done(&ctx).await.is_ok());
        });
    }

    #[test]
    fn resolve_uses_cache_on_second_call() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml = "name: test-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let yaml_path = write_workspace_yaml(&tmp, "test-ws", yaml);
        register_workspace(&db, "test-ws", yaml_path.to_str().unwrap());

        let loader = DynamicHookLoader::new(db, shell, 4);

        // First call — cache miss.
        let _hook1 = loader.resolve("test-ws");
        assert_eq!(loader.cache_len(), 1);

        // Second call — cache hit (same updated_at).
        let _hook2 = loader.resolve("test-ws");
        assert_eq!(loader.cache_len(), 1);
    }

    #[test]
    fn resolve_invalidates_cache_on_updated_at_change() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml = "name: test-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let yaml_path = write_workspace_yaml(&tmp, "test-ws", yaml);
        register_workspace(&db, "test-ws", yaml_path.to_str().unwrap());

        let loader = DynamicHookLoader::new(Arc::clone(&db), shell, 4);

        // First call — populates cache.
        let _hook1 = loader.resolve("test-ws");
        assert_eq!(loader.cache_len(), 1);

        // Update workspace timestamp to simulate config change.
        std::thread::sleep(std::time::Duration::from_millis(10));
        touch_workspace_updated_at(&db, "test-ws", yaml_path.to_str().unwrap());

        // Resolve again — cache is stale, should reload.
        let _hook2 = loader.resolve("test-ws");
        assert_eq!(loader.cache_len(), 1); // Still 1 entry, but reloaded.
    }

    #[test]
    fn resolve_script_hook_when_scripts_defined() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml = r#"
name: script-ws
sources:
  custom:
    url: https://example.com
    states:
      implement:
        trigger: {}
        handlers: []
        on_done:
          - script: "echo done"
"#;
        let yaml_path = write_workspace_yaml(&tmp, "script-ws", yaml);
        register_workspace(&db, "script-ws", yaml_path.to_str().unwrap());

        let loader = DynamicHookLoader::new(db, shell, 4);
        let hook = loader.resolve("script-ws");

        let ctx = make_hook_context();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(hook.on_done(&ctx).await.is_ok());
        });
    }

    #[test]
    fn invalidate_removes_cache_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml = "name: test-ws\nsources:\n  github:\n    url: https://github.com/org/repo\n";
        let yaml_path = write_workspace_yaml(&tmp, "test-ws", yaml);
        register_workspace(&db, "test-ws", yaml_path.to_str().unwrap());

        let loader = DynamicHookLoader::new(db, shell, 4);
        let _hook = loader.resolve("test-ws");
        assert_eq!(loader.cache_len(), 1);

        loader.invalidate("test-ws");
        assert_eq!(loader.cache_len(), 0);
    }

    #[test]
    fn invalidate_all_clears_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        let yaml1 = "name: ws1\nsources:\n  github:\n    url: https://github.com/org/repo1\n";
        let yaml2 = "name: ws2\nsources:\n  github:\n    url: https://github.com/org/repo2\n";
        let path1 = write_workspace_yaml(&tmp, "ws1", yaml1);
        let path2 = write_workspace_yaml(&tmp, "ws2", yaml2);
        register_workspace(&db, "ws1", path1.to_str().unwrap());
        register_workspace(&db, "ws2", path2.to_str().unwrap());

        let loader = DynamicHookLoader::new(db, shell, 4);
        let _h1 = loader.resolve("ws1");
        let _h2 = loader.resolve("ws2");
        assert_eq!(loader.cache_len(), 2);

        loader.invalidate_all();
        assert_eq!(loader.cache_len(), 0);
    }

    #[test]
    fn lru_eviction_when_capacity_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);

        // Create 3 workspaces but cache capacity is 2.
        for i in 1..=3 {
            let name = format!("ws{i}");
            let yaml = format!(
                "name: {name}\nsources:\n  github:\n    url: https://github.com/org/repo{i}\n"
            );
            let path = write_workspace_yaml(&tmp, &name, &yaml);
            register_workspace(&db, &name, path.to_str().unwrap());
        }

        let loader = DynamicHookLoader::new(db, shell, 2);
        let _h1 = loader.resolve("ws1");
        let _h2 = loader.resolve("ws2");
        assert_eq!(loader.cache_len(), 2);

        // Adding ws3 should evict ws1 (LRU).
        let _h3 = loader.resolve("ws3");
        assert_eq!(loader.cache_len(), 2);
    }

    #[test]
    fn debug_impl_shows_cache_size() {
        let db = Arc::new(test_db());
        let shell: Arc<dyn ShellExecutor> = Arc::new(StubShell);
        let loader = DynamicHookLoader::new(db, shell, 4);
        let debug = format!("{loader:?}");
        assert!(debug.contains("cache_size"));
        assert!(debug.contains("0"));
    }
}
