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

    #[test]
    fn new_with_default_model_stores_model() {
        let runtime = GeminiRuntime::new(Some("gemini-1.5-pro".to_string()));
        assert_eq!(runtime.name(), "gemini");
        assert_eq!(runtime.default_model.as_deref(), Some("gemini-1.5-pro"));
    }

    #[test]
    fn new_with_no_default_model_is_none() {
        let runtime = GeminiRuntime::new(None);
        assert!(runtime.default_model.is_none());
    }

    #[test]
    fn parse_json_with_empty_text_field() {
        let json = r#"{
            "text": "",
            "usage_metadata": {
                "prompt_token_count": 5,
                "candidates_token_count": 0,
                "cached_content_token_count": 0
            }
        }"#;
        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_read_tokens, Some(0));
    }

    #[test]
    fn parse_json_missing_text_field_defaults_to_empty_string() {
        // text field is absent — unwrap_or_default() should return ""
        let json = r#"{
            "usage_metadata": {
                "prompt_token_count": 20,
                "candidates_token_count": 10,
                "cached_content_token_count": 3
            }
        }"#;
        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cache_read_tokens, Some(3));
    }

    #[test]
    fn parse_json_with_zero_usage_metadata() {
        let json = r#"{
            "text": "zero tokens",
            "usage_metadata": {
                "prompt_token_count": 0,
                "candidates_token_count": 0,
                "cached_content_token_count": 0
            }
        }"#;
        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "zero tokens");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert!(usage.cache_write_tokens.is_none());
    }

    #[test]
    fn parse_empty_string_returns_raw() {
        let raw = "";
        let (usage, result) = parse_gemini_json(raw);
        assert!(usage.is_none());
        assert_eq!(result, "");
    }

    #[test]
    fn parse_empty_json_object_returns_empty_result_no_usage() {
        // All fields are #[serde(default)] so {} parses successfully
        let json = r#"{}"#;
        let (usage, result) = parse_gemini_json(json);
        assert!(usage.is_none());
        assert_eq!(result, "");
    }

    #[test]
    fn parse_json_unknown_fields_are_ignored() {
        let json = r#"{
            "text": "good output",
            "usage_metadata": {
                "prompt_token_count": 30,
                "candidates_token_count": 15,
                "cached_content_token_count": 2,
                "totally_unknown": 99
            },
            "model_version": "gemini-1.5-flash-001"
        }"#;
        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "good output");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 15);
        assert_eq!(usage.cache_read_tokens, Some(2));
    }

    #[test]
    fn parse_json_with_large_token_counts() {
        let json = r#"{
            "text": "big result",
            "usage_metadata": {
                "prompt_token_count": 2000000,
                "candidates_token_count": 1500000,
                "cached_content_token_count": 500000
            }
        }"#;
        let (usage, result) = parse_gemini_json(json);
        assert_eq!(result, "big result");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, 2_000_000);
        assert_eq!(usage.output_tokens, 1_500_000);
        assert_eq!(usage.cache_read_tokens, Some(500_000));
        assert!(usage.cache_write_tokens.is_none());
    }

    #[test]
    fn parse_whitespace_only_string_returns_raw() {
        let raw = "   \n\t  ";
        let (usage, result) = parse_gemini_json(raw);
        assert!(usage.is_none());
        assert_eq!(result, raw);
    }

    #[test]
    fn capabilities_cache_write_tokens_are_always_none() {
        // Gemini does not report cache write tokens
        let json = r#"{
            "text": "x",
            "usage_metadata": {
                "prompt_token_count": 1,
                "candidates_token_count": 1,
                "cached_content_token_count": 1
            }
        }"#;
        let (usage, _) = parse_gemini_json(json);
        let usage = usage.unwrap();
        assert!(usage.cache_write_tokens.is_none());
    }
}
