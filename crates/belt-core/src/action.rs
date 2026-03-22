/// Action — handler/on_done/on_fail/on_enter에서 사용하는 실행 단위.
///
/// Prompt는 AgentRuntime을 통해 LLM을 호출하고,
/// Script는 bash를 통해 deterministic하게 실행한다.
#[derive(Debug, Clone)]
pub enum Action {
    Prompt {
        text: String,
        runtime: Option<String>,
        model: Option<String>,
    },
    Script {
        command: String,
    },
}

impl Action {
    pub fn prompt(text: &str) -> Self {
        Action::Prompt {
            text: text.to_string(),
            runtime: None,
            model: None,
        }
    }

    pub fn prompt_with_runtime(text: &str, runtime: &str, model: Option<&str>) -> Self {
        Action::Prompt {
            text: text.to_string(),
            runtime: Some(runtime.to_string()),
            model: model.map(|s| s.to_string()),
        }
    }

    pub fn script(command: &str) -> Self {
        Action::Script {
            command: command.to_string(),
        }
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self, Action::Prompt { .. })
    }

    pub fn is_script(&self) -> bool {
        matches!(self, Action::Script { .. })
    }
}

/// workspace yaml의 HandlerConfig → Action 변환.
impl From<&crate::workspace::HandlerConfig> for Action {
    fn from(config: &crate::workspace::HandlerConfig) -> Self {
        match config {
            crate::workspace::HandlerConfig::Prompt {
                prompt,
                runtime,
                model,
            } => Action::Prompt {
                text: prompt.clone(),
                runtime: runtime.clone(),
                model: model.clone(),
            },
            crate::workspace::HandlerConfig::Script { script } => Action::Script {
                command: script.clone(),
            },
        }
    }
}

/// workspace yaml의 ScriptAction → Action 변환.
impl From<&crate::workspace::ScriptAction> for Action {
    fn from(config: &crate::workspace::ScriptAction) -> Self {
        Action::Script {
            command: config.script.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_constructors() {
        let p = Action::prompt("analyze this");
        assert!(p.is_prompt());
        assert!(!p.is_script());

        let s = Action::script("cargo test");
        assert!(s.is_script());
        assert!(!s.is_prompt());
    }

    #[test]
    fn prompt_with_runtime() {
        let a = Action::prompt_with_runtime("review", "gemini", Some("pro"));
        match a {
            Action::Prompt {
                text,
                runtime,
                model,
            } => {
                assert_eq!(text, "review");
                assert_eq!(runtime.as_deref(), Some("gemini"));
                assert_eq!(model.as_deref(), Some("pro"));
            }
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn from_handler_config() {
        use crate::workspace::{HandlerConfig, ScriptAction};

        let handler = HandlerConfig::Prompt {
            prompt: "do it".to_string(),
            runtime: Some("claude".to_string()),
            model: None,
        };
        let action: Action = (&handler).into();
        assert!(action.is_prompt());

        let script = ScriptAction {
            script: "echo hello".to_string(),
        };
        let action: Action = (&script).into();
        assert!(action.is_script());
    }
}
