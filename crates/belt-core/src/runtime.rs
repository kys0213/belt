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
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeCapabilities {
    pub supports_tool_use: bool,
    pub supports_structured_output: bool,
    pub supports_session: bool,
}

/// 런타임 레지스트리 — 이름으로 런타임을 resolve.
pub struct RuntimeRegistry {
    runtimes: HashMap<String, Arc<dyn AgentRuntime>>,
    default_name: String,
}

impl RuntimeRegistry {
    pub fn new(default_name: String) -> Self {
        Self {
            runtimes: HashMap::new(),
            default_name,
        }
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
        registry.register(Arc::new(DummyRuntime { rt_name: "claude".to_string() }));
        registry.register(Arc::new(DummyRuntime { rt_name: "gemini".to_string() }));
        assert_eq!(registry.resolve("claude").unwrap().name(), "claude");
        assert_eq!(registry.resolve("gemini").unwrap().name(), "gemini");
    }

    #[test]
    fn registry_fallback_to_default() {
        let mut registry = RuntimeRegistry::new("claude".to_string());
        registry.register(Arc::new(DummyRuntime { rt_name: "claude".to_string() }));
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
}
