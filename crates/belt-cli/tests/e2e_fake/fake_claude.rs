//! Fake `claude` CLI: belt's default runtime (`ClaudeRuntime`) spawns
//! `claude -p <prompt> --output-format json [...]` for prompt handlers, the
//! evaluator and the natural-language interpreter. The fake logs each call and
//! answers with the response configured by the test (default: success).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const RESPONSE_FILE: &str = "claude_response.json";
const CALL_LOG: &str = "claude_calls.jsonl";

/// What the next (and every following) `claude` call returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeResponse {
    pub exit_code: i32,
    /// Text placed in the JSON `result` field.
    pub result: String,
}

impl Default for ClaudeResponse {
    fn default() -> Self {
        Self {
            exit_code: 0,
            result: "ok".to_string(),
        }
    }
}

/// One logged `claude` invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCall {
    pub argv: Vec<String>,
    pub cwd: String,
}

/// Handle on the fake runtime's control and log files.
#[derive(Debug, Clone)]
pub struct FakeClaude {
    dir: PathBuf,
}

impl FakeClaude {
    pub fn open(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Answer every later call with `response`.
    #[allow(dead_code)] // harness API for the runtime-failure scenarios
    pub fn set_response(&self, response: &ClaudeResponse) {
        let tmp = self.dir.join(format!("{RESPONSE_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(response).unwrap()).unwrap();
        std::fs::rename(&tmp, self.dir.join(RESPONSE_FILE)).unwrap();
    }

    /// Every `claude` invocation so far, in order.
    pub fn calls(&self) -> Vec<ClaudeCall> {
        let Ok(text) = std::fs::read_to_string(self.dir.join(CALL_LOG)) else {
            return Vec::new();
        };
        text.lines()
            .map(|l| serde_json::from_str(l).expect("malformed claude call log line"))
            .collect()
    }

    fn response(&self) -> ClaudeResponse {
        match std::fs::read_to_string(self.dir.join(RESPONSE_FILE)) {
            Ok(text) => serde_json::from_str(&text).expect("invalid claude_response.json"),
            Err(_) => ClaudeResponse::default(),
        }
    }
}

/// Entry point when this binary runs as `claude`. Returns the exit code.
pub fn main(state_dir: &Path, args: Vec<String>) -> i32 {
    let fake = FakeClaude::open(state_dir);
    let call = ClaudeCall {
        argv: std::iter::once("claude".to_string()).chain(args).collect(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    };
    // One short line per call; O_APPEND keeps concurrent lines whole.
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(CALL_LOG))
        .unwrap();
    writeln!(log, "{}", serde_json::to_string(&call).unwrap()).unwrap();

    let response = fake.response();
    println!(
        "{}",
        serde_json::json!({
            "result": response.result,
            "model": "fake-claude",
            "session_id": "fake-session",
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        })
    );
    response.exit_code
}
