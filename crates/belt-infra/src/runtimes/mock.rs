use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use belt_core::platform::{NoopProcessSink, ProcessSink};
use belt_core::runtime::{
    AgentRuntime, RuntimeCapabilities, RuntimeRequest, RuntimeResponse, TokenUsage,
};

/// Default pid reported by [`MockRuntime`]; no live process owns it.
pub const DEFAULT_REPORTED_PID: u32 = 999_999_999;

/// 테스트용 MockRuntime.
pub struct MockRuntime {
    rt_name: String,
    exit_codes: Mutex<Vec<i32>>,
    calls: Mutex<Vec<String>>,
    /// Per-invocation token usage. If empty, no token usage is reported.
    token_usages: Mutex<Vec<TokenUsage>>,
    /// Pid reported to a [`ProcessSink`]. It names no live process, so a
    /// process killer pointed at it fails instead of hitting something real.
    reported_pid: u32,
}

impl MockRuntime {
    pub fn new(name: &str, exit_codes: Vec<i32>) -> Self {
        Self {
            rt_name: name.to_string(),
            exit_codes: Mutex::new(exit_codes),
            calls: Mutex::new(Vec::new()),
            token_usages: Mutex::new(Vec::new()),
            reported_pid: DEFAULT_REPORTED_PID,
        }
    }

    pub fn always_ok(name: &str) -> Self {
        Self::new(name, vec![])
    }

    /// Configure per-invocation token usage responses.
    pub fn with_token_usages(self, usages: Vec<TokenUsage>) -> Self {
        *self.token_usages.lock().unwrap() = usages;
        self
    }

    /// Report `pid` to the [`ProcessSink`] instead of [`DEFAULT_REPORTED_PID`].
    pub fn with_reported_pid(mut self, pid: u32) -> Self {
        self.reported_pid = pid;
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl AgentRuntime for MockRuntime {
    fn name(&self) -> &str {
        &self.rt_name
    }

    async fn invoke(&self, request: RuntimeRequest) -> RuntimeResponse {
        self.invoke_with_sink(request, Arc::new(NoopProcessSink))
            .await
    }

    async fn invoke_with_sink(
        &self,
        request: RuntimeRequest,
        sink: Arc<dyn ProcessSink>,
    ) -> RuntimeResponse {
        // The mock spawns nothing; the configured pid stands in for the handler.
        sink.spawned(self.reported_pid);
        self.calls.lock().unwrap().push(request.prompt.clone());

        let exit_code = {
            let mut codes = self.exit_codes.lock().unwrap();
            if codes.is_empty() { 0 } else { codes.remove(0) }
        };

        let token_usage = {
            let mut usages = self.token_usages.lock().unwrap();
            if usages.is_empty() {
                None
            } else {
                Some(usages.remove(0))
            }
        };

        RuntimeResponse {
            exit_code,
            stdout: format!("mock response for: {}", request.prompt),
            stderr: String::new(),
            duration: Duration::from_millis(100),
            token_usage,
            session_id: None,
        }
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            supports_tool_use: true,
            supports_structured_output: false,
            supports_session: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn mock_returns_exit_codes_in_order() {
        let mock = MockRuntime::new("test", vec![0, 1, 2]);
        let req = RuntimeRequest {
            working_dir: PathBuf::from("/tmp"),
            prompt: "hello".to_string(),
            model: None,
            system_prompt: None,
            session_id: None,
            structured_output: None,
        };
        assert_eq!(mock.invoke(req.clone()).await.exit_code, 0);
        assert_eq!(mock.invoke(req.clone()).await.exit_code, 1);
        assert_eq!(mock.invoke(req.clone()).await.exit_code, 2);
        assert_eq!(mock.invoke(req).await.exit_code, 0);
    }

    #[tokio::test]
    async fn mock_records_calls() {
        let mock = MockRuntime::always_ok("test");
        let req = RuntimeRequest {
            working_dir: PathBuf::from("/tmp"),
            prompt: "first".to_string(),
            model: None,
            system_prompt: None,
            session_id: None,
            structured_output: None,
        };
        mock.invoke(req).await;
        let req2 = RuntimeRequest {
            working_dir: PathBuf::from("/tmp"),
            prompt: "second".to_string(),
            model: None,
            system_prompt: None,
            session_id: None,
            structured_output: None,
        };
        mock.invoke(req2).await;
        assert_eq!(mock.calls(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn mock_invoke_with_sink_reports_a_pid_once() {
        use crate::platform::testing::RecordingSink;
        use std::sync::Arc;

        let mock = MockRuntime::always_ok("test");
        let sink = Arc::new(RecordingSink::default());
        let req = RuntimeRequest {
            working_dir: PathBuf::from("/tmp"),
            prompt: "hello".to_string(),
            model: None,
            system_prompt: None,
            session_id: None,
            structured_output: None,
        };

        let response = mock.invoke_with_sink(req, sink.clone()).await;

        assert_eq!(response.exit_code, 0);
        assert_eq!(sink.pids().len(), 1);
        assert_eq!(mock.calls(), vec!["hello"]);
    }
}
