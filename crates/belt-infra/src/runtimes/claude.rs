use std::time::Instant;

use async_trait::async_trait;

use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse,
};

/// Claude CLI를 직접 호출하는 AgentRuntime 구현.
///
/// Model resolution priority:
///   1. RuntimeRequest.model (호출 시점 명시)
///   2. default_model (workspace yaml의 runtime.claude.model)
///   3. Claude CLI 기본값
pub struct ClaudeRuntime {
    default_model: Option<String>,
}

impl ClaudeRuntime {
    pub fn new(default_model: Option<String>) -> Self {
        Self { default_model }
    }
}

#[async_trait]
impl AgentRuntime for ClaudeRuntime {
    fn name(&self) -> &str {
        "claude"
    }

    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
        let start = Instant::now();
        let resolved_model = request.model.or_else(|| self.default_model.clone());

        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p").arg(&request.prompt);
        cmd.current_dir(&request.working_dir);

        if let Some(ref model) = resolved_model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref system_prompt) = request.system_prompt {
            cmd.arg("--append-system-prompt").arg(system_prompt);
        }

        match cmd.output().await {
            Ok(output) => RuntimeResponse {
                exit_code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                duration: start.elapsed(),
                token_usage: None,
                session_id: None,
            },
            Err(e) => RuntimeResponse::error(&format!("claude invocation failed: {e}")),
        }
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            supports_tool_use: true,
            supports_structured_output: true,
            supports_session: true,
        }
    }
}
