use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

/// AgentRuntime trait — LLM 실행 추상화.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    fn name(&self) -> &str;
    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse;
    fn capabilities(&self) -> RuntimeCapabilities;
}

#[derive(Debug, Clone)]
pub struct RuntimeRequest {
    pub working_dir: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub session_id: Option<String>,
    /// Optional structured output configuration for the runtime invocation.
    pub structured_output: Option<StructuredOutputConfig>,
}

/// Configuration for requesting structured (schema-validated) output from a runtime.
#[derive(Debug, Clone)]
pub struct StructuredOutputConfig {
    /// JSON Schema that the output must conform to.
    pub schema: serde_json::Value,
    /// Optional human-readable name for the output format.
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub token_usage: Option<TokenUsage>,
    pub session_id: Option<String>,
}

impl RuntimeResponse {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    pub fn error(message: &str) -> Self {
        Self {
            exit_code: -1,
            stdout: String::new(),
            stderr: message.to_string(),
            duration: Duration::ZERO,
            token_usage: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeCapabilities {
    pub supports_tool_use: bool,
    pub supports_structured_output: bool,
    pub supports_session: bool,
}

/// 런타임 레지스트리 — 이름으로 런타임을 resolve.
///
/// Holds a map of named runtimes and an optional workspace-level default model.
/// Model resolution priority:
///   1. `RuntimeRequest.model` (per-invocation override)
///   2. `workspace_default_model` (workspace yaml `runtime.<name>.model`)
///   3. Runtime-specific default (e.g. `ClaudeRuntime.default_model`)
pub struct RuntimeRegistry {
    runtimes: HashMap<String, Arc<dyn AgentRuntime>>,
    default_name: String,
    /// Workspace-level default model from workspace yaml `runtime.<default>.model`.
    workspace_default_model: Option<String>,
}

impl RuntimeRegistry {
    pub fn new(default_name: String) -> Self {
        Self {
            runtimes: HashMap::new(),
            default_name,
            workspace_default_model: None,
        }
    }

    /// Create a registry with a workspace-level default model.
    ///
    /// This model is used as a fallback when `RuntimeRequest.model` is `None`.
    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.workspace_default_model = model;
        self
    }

    pub fn register(&mut self, runtime: Arc<dyn AgentRuntime>) {
        let name = runtime.name().to_string();
        self.runtimes.insert(name, runtime);
    }

    pub fn resolve(&self, name: &str) -> Option<Arc<dyn AgentRuntime>> {
        self.runtimes
            .get(name)
            .or_else(|| self.runtimes.get(&self.default_name))
            .cloned()
    }

    pub fn default_runtime(&self) -> Option<Arc<dyn AgentRuntime>> {
        self.runtimes.get(&self.default_name).cloned()
    }

    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// Return the workspace-level default model, if configured.
    pub fn workspace_default_model(&self) -> Option<&str> {
        self.workspace_default_model.as_deref()
    }

    /// Resolve the model to use for a request.
    ///
    /// Priority: `request_model > workspace_default_model`.
    /// The runtime implementation may apply its own fallback after this.
    pub fn resolve_model(&self, request_model: Option<String>) -> Option<String> {
        request_model.or_else(|| self.workspace_default_model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyRuntime {
        rt_name: String,
    }

    #[async_trait]
    impl AgentRuntime for DummyRuntime {
        fn name(&self) -> &str {
            &self.rt_name
        }
        async fn invoke(&self, _request: RuntimeRequest) -> RuntimeResponse {
            RuntimeResponse {
                exit_code: 0,
                stdout: "ok".to_string(),
                stderr: String::new(),
                duration: Duration::from_secs(1),
                token_usage: None,
                session_id: None,
            }
        }
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities::default()
        }
    }

    #[test]
    fn registry_resolve() {
        let mut registry = RuntimeRegistry::new("claude".to_string());
        registry.register(Arc::new(DummyRuntime {
            rt_name: "claude".to_string(),
        }));
        registry.register(Arc::new(DummyRuntime {
            rt_name: "gemini".to_string(),
        }));
        assert_eq!(registry.resolve("claude").unwrap().name(), "claude");
        assert_eq!(registry.resolve("gemini").unwrap().name(), "gemini");
    }

    #[test]
    fn registry_fallback_to_default() {
        let mut registry = RuntimeRegistry::new("claude".to_string());
        registry.register(Arc::new(DummyRuntime {
            rt_name: "claude".to_string(),
        }));
        let resolved = registry.resolve("nonexistent").unwrap();
        assert_eq!(resolved.name(), "claude");
    }

    #[test]
    fn registry_empty_returns_none() {
        let registry = RuntimeRegistry::new("claude".to_string());
        assert!(registry.resolve("anything").is_none());
    }

    #[test]
    fn runtime_response_success() {
        let fail = RuntimeResponse::error("boom");
        assert!(!fail.success());
        assert_eq!(fail.exit_code, -1);
    }

    #[test]
    fn resolve_model_request_takes_priority() {
        let registry = RuntimeRegistry::new("claude".to_string())
            .with_default_model(Some("sonnet".to_string()));
        let resolved = registry.resolve_model(Some("opus".to_string()));
        assert_eq!(resolved.as_deref(), Some("opus"));
    }

    #[test]
    fn resolve_model_falls_back_to_workspace_default() {
        let registry = RuntimeRegistry::new("claude".to_string())
            .with_default_model(Some("sonnet".to_string()));
        let resolved = registry.resolve_model(None);
        assert_eq!(resolved.as_deref(), Some("sonnet"));
    }

    #[test]
    fn resolve_model_none_when_no_defaults() {
        let registry = RuntimeRegistry::new("claude".to_string());
        let resolved = registry.resolve_model(None);
        assert!(resolved.is_none());
    }

    #[test]
    fn workspace_default_model_accessor() {
        let registry = RuntimeRegistry::new("claude".to_string())
            .with_default_model(Some("haiku".to_string()));
        assert_eq!(registry.workspace_default_model(), Some("haiku"));

        let registry_no_model = RuntimeRegistry::new("claude".to_string());
        assert_eq!(registry_no_model.workspace_default_model(), None);
    }
}
