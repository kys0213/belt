use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse, TokenUsage,
};

/// Claude CLI를 직접 호출하는 AgentRuntime 구현.
///
/// `--output-format json` 옵션으로 실행하여 stdout에서 token usage를 파싱한다.
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

/// Claude CLI JSON 출력에서 usage 필드를 파싱하기 위한 구조체.
#[derive(Debug, Deserialize)]
struct ClaudeJsonOutput {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Claude CLI JSON 출력의 usage 블록.
#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// stdout JSON에서 token usage를 파싱한다.
/// 파싱 실패 시 `(None, None, None, raw_stdout)` 반환 (graceful degradation).
fn parse_claude_json(
    stdout: &str,
) -> (Option<TokenUsage>, Option<String>, Option<String>, String) {
    let parsed: Option<ClaudeJsonOutput> = serde_json::from_str(stdout).ok();
    match parsed {
        Some(output) => {
            let token_usage = output.usage.map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: u.cache_read_input_tokens,
                cache_write_tokens: u.cache_creation_input_tokens,
            });
            let result_text = output.result.unwrap_or_default();
            (token_usage, output.model, output.session_id, result_text)
        }
        None => (None, None, None, stdout.to_string()),
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
        cmd.arg("--output-format").arg("json");
        cmd.current_dir(&request.working_dir);

        if let Some(ref model) = resolved_model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref system_prompt) = request.system_prompt {
            cmd.arg("--append-system-prompt").arg(system_prompt);
        }

        match cmd.output().await {
            Ok(output) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let (token_usage, _model, session_id, result_text) =
                    parse_claude_json(&raw_stdout);

                RuntimeResponse {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: result_text,
                    stderr,
                    duration: start.elapsed(),
                    token_usage,
                    session_id,
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_json_with_usage() {
        let json = r#"{
            "result": "Hello world",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 10,
                "cache_creation_input_tokens": 5
            },
            "model": "claude-sonnet-4-20250514",
            "session_id": "sess-123"
        }"#;

        let (usage, model, session_id, result) = parse_claude_json(json);
        assert_eq!(result, "Hello world");
        assert_eq!(model.unwrap(), "claude-sonnet-4-20250514");
        assert_eq!(session_id.unwrap(), "sess-123");

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_tokens, 10);
        assert_eq!(usage.cache_write_tokens, 5);
    }

    #[test]
    fn parse_json_without_usage() {
        let json = r#"{"result": "Hello"}"#;
        let (usage, _model, _session_id, result) = parse_claude_json(json);
        assert!(usage.is_none());
        assert_eq!(result, "Hello");
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        let raw = "This is not JSON output";
        let (usage, _model, _session_id, result) = parse_claude_json(raw);
        assert!(usage.is_none());
        assert_eq!(result, raw);
    }

    #[test]
    fn parse_json_with_partial_usage() {
        let json = r#"{
            "result": "ok",
            "usage": {
                "input_tokens": 200,
                "output_tokens": 100
            }
        }"#;

        let (usage, _model, _session_id, _result) = parse_claude_json(json);
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }
}
