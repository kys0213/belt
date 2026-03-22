use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::BeltError;

/// Request to invoke an LLM agent.
#[derive(Debug, Clone)]
pub struct RuntimeRequest {
    pub working_dir: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub session_id: Option<String>,
}

/// Response from an LLM agent invocation.
#[derive(Debug, Clone)]
pub struct RuntimeResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub token_usage: Option<TokenUsage>,
    pub session_id: Option<String>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// LLM execution abstraction.
///
/// New LLM = new AgentRuntime impl, zero core changes (OCP).
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Runtime name (e.g. "claude", "gemini", "codex").
    fn name(&self) -> &str;

    /// Invoke the LLM with the given request.
    async fn invoke(&self, request: RuntimeRequest) -> Result<RuntimeResponse, BeltError>;
}

impl RuntimeResponse {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}
