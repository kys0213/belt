use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;

use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse, TokenUsage,
};

/// Google Generative AI (Gemini) API를 호출하는 AgentRuntime 구현.
///
/// `gemini` CLI 도구를 호출하여 프롬프트를 전달하고 JSON 출력에서
/// token usage 및 결과를 파싱한다.
///
/// Model resolution priority:
///   1. RuntimeRequest.model (호출 시점 명시)
///   2. default_model (workspace yaml의 runtime.gemini.model)
///   3. gemini CLI 기본값 (gemini-pro)
pub struct GeminiRuntime {
    default_model: Option<String>,
}

impl GeminiRuntime {
    pub fn new(default_model: Option<String>) -> Self {
        Self { default_model }
    }
}

/// Gemini CLI JSON 출력 파싱 구조체.
#[derive(Debug, Deserialize)]
struct GeminiJsonOutput {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

/// Gemini API의 usage metadata 블록.
#[derive(Debug, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
    #[serde(default)]
    cached_content_token_count: u64,
}

/// Gemini CLI JSON stdout를 파싱한다.
/// 파싱 실패 시 `(None, raw_stdout)` 반환 (graceful degradation).
fn parse_gemini_json(stdout: &str) -> (Option<TokenUsage>, String) {
    let parsed: Option<GeminiJsonOutput> = serde_json::from_str(stdout).ok();
    match parsed {
        Some(output) => {
            let token_usage = output.usage_metadata.map(|u| TokenUsage {
                input_tokens: u.prompt_token_count,
                output_tokens: u.candidates_token_count,
                cache_read_tokens: Some(u.cached_content_token_count),
                cache_write_tokens: None,
            });
            let result_text = output.text.unwrap_or_default();
            (token_usage, result_text)
        }
        None => (None, stdout.to_string()),
    }
}

#[async_trait]
impl AgentRuntime for GeminiRuntime {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
        let start = Instant::now();
        let resolved_model = request.model.or_else(|| self.default_model.clone());

        let mut cmd = tokio::process::Command::new("gemini");
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
                let (token_usage, result_text) = parse_gemini_json(&raw_stdout);

                RuntimeResponse {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: result_text,
                    stderr,
                    duration: start.elapsed(),
                    token_usage,
                    session_id: None,
                }
            }
            Err(e) => RuntimeResponse::error(&format!("gemini invocation failed: {e}")),
        }
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            supports_tool_use: true,
            supports_structured_output: true,
            supports_session: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_json_with_usage() {
        let json = r#"{
            "text": "Hello from Gemini",
            "usage_metadata": {
                "prompt_token_count": 120,
                "candidates_token_count": 80,
                "cached_content_token_count": 15
            }
        }"#;

        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "Hello from Gemini");

        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 80);
        assert_eq!(usage.cache_read_tokens, Some(15));
        assert_eq!(usage.cache_write_tokens, None);
    }

    #[test]
    fn parse_json_without_usage() {
        let json = r#"{"text": "Just text"}"#;
        let (usage, result) = parse_gemini_json(json);
        assert!(usage.is_none());
        assert_eq!(result, "Just text");
    }

    #[test]
    fn parse_invalid_json_returns_raw() {
        let raw = "Not JSON at all";
        let (usage, result) = parse_gemini_json(raw);
        assert!(usage.is_none());
        assert_eq!(result, raw);
    }

    #[test]
    fn parse_json_with_partial_usage() {
        let json = r#"{
            "text": "ok",
            "usage_metadata": {
                "prompt_token_count": 50,
                "candidates_token_count": 25
            }
        }"#;

        let (usage, _result) = parse_gemini_json(json);
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.cache_read_tokens, Some(0));
    }

    #[test]
    fn capabilities_are_correct() {
        let runtime = GeminiRuntime::new(None);
        let caps = runtime.capabilities();
        assert!(caps.supports_tool_use);
        assert!(caps.supports_structured_output);
        assert!(!caps.supports_session);
    }

    #[test]
    fn name_returns_gemini() {
        let runtime = GeminiRuntime::new(None);
        assert_eq!(runtime.name(), "gemini");
    }
}
