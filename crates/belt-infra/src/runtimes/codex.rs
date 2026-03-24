use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse, TokenUsage,
};

/// OpenAI Codex (코드 특화) AgentRuntime 구현.
///
/// `codex` CLI 도구를 호출하여 코드 생성/분석 프롬프트를 전달하고
/// JSON 출력에서 token usage 및 결과를 파싱한다.
///
/// Codex는 GPT-4 기반 코드 특화 모델로, OpenAI API를 통해 호출된다.
///
/// Model resolution priority:
///   1. RuntimeRequest.model (호출 시점 명시)
///   2. default_model (workspace yaml의 runtime.codex.model)
///   3. codex CLI 기본값
pub struct CodexRuntime {
    default_model: Option<String>,
}

impl CodexRuntime {
    pub fn new(default_model: Option<String>) -> Self {
        Self { default_model }
    }
}

/// Codex CLI JSON 출력 파싱 구조체.
#[derive(Debug, Deserialize)]
struct CodexJsonOutput {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<CodexUsage>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Codex CLI JSON 출력의 usage 블록.
#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// Codex CLI JSON stdout를 파싱한다.
/// 파싱 실패 시 `(None, None, raw_stdout)` 반환 (graceful degradation).
fn parse_codex_json(stdout: &str) -> (Option<TokenUsage>, Option<String>, String) {
    let parsed: Option<CodexJsonOutput> = serde_json::from_str(stdout).ok();
    match parsed {
        Some(output) => {
            let token_usage = output.usage.map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: None,
                cache_write_tokens: None,
            });
            let result_text = output.result.unwrap_or_default();
            (token_usage, output.session_id, result_text)
        }
        None => (None, None, stdout.to_string()),
    }
}

#[async_trait]
impl AgentRuntime for CodexRuntime {
    fn name(&self) -> &str {
        "codex"
    }

    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
        let start = Instant::now();
        let resolved_model = request.model.or_else(|| self.default_model.clone());

        let mut cmd = tokio::process::Command::new("codex");
        cmd.arg("--prompt").arg(&request.prompt);
        cmd.arg("--output-format").arg("json");
        cmd.current_dir(&request.working_dir);

        if let Some(ref model) = resolved_model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref system_prompt) = request.system_prompt {
            cmd.arg("--system-prompt").arg(system_prompt);
        }

        match cmd.output().await {
            Ok(output) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let (token_usage, session_id, result_text) = parse_codex_json(&raw_stdout);

                RuntimeResponse {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: result_text,
                    stderr,
                    duration: start.elapsed(),
                    token_usage,
                    session_id,
                }
            }
            Err(e) => RuntimeResponse::error(&format!("codex invocation failed: {e}")),
        }
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            supports_tool_use: true,
            supports_structured_output: false,
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
            "result": "fn main() { println!(\"hello\"); }",
            "usage": {
                "input_tokens": 200,
                "output_tokens": 150
            },
            "session_id": "codex-sess-456"
        }"#;

        let (usage, session_id, result) = parse_codex_json(json);
        assert_eq!(result, "fn main() { println!(\"hello\"); }");
        assert_eq!(session_id.unwrap(), "codex-sess-456");

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.output_tokens, 150);
        assert_eq!(usage.cache_read_tokens, None);
        assert_eq!(usage.cache_write_tokens, None);
    }

    #[test]
    fn parse_json_without_usage() {
        let json = r#"{"result": "code output"}"#;
        let (usage, session_id, result) = parse_codex_json(json);
        assert!(usage.is_none());
        assert!(session_id.is_none());
        assert_eq!(result, "code output");
    }

    #[test]
    fn parse_invalid_json_returns_raw() {
        let raw = "This is raw output";
        let (usage, session_id, result) = parse_codex_json(raw);
        assert!(usage.is_none());
        assert!(session_id.is_none());
        assert_eq!(result, raw);
    }

    #[test]
    fn capabilities_are_correct() {
        let runtime = CodexRuntime::new(None);
        let caps = runtime.capabilities();
        assert!(caps.supports_tool_use);
        assert!(!caps.supports_structured_output);
        assert!(caps.supports_session);
    }

    #[test]
    fn name_returns_codex() {
        let runtime = CodexRuntime::new(None);
        assert_eq!(runtime.name(), "codex");
    }
}
