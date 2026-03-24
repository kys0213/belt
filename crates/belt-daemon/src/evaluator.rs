use std::path::Path;

use anyhow::Result;

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;

/// Completed 아이템의 평가 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalDecision {
    Done,
    Hitl { reason: String },
}

/// Completed 아이템을 스캔하여 Done 또는 HITL로 분류.
pub struct Evaluator {
    workspace_name: String,
}

impl Evaluator {
    pub fn new(workspace_name: &str) -> Self {
        Self {
            workspace_name: workspace_name.to_string(),
        }
    }

    pub fn filter_completed(items: &[QueueItem]) -> Vec<&QueueItem> {
        items
            .iter()
            .filter(|item| item.phase == QueuePhase::Completed)
            .collect()
    }

    pub fn target_phase(decision: &EvalDecision) -> QueuePhase {
        match decision {
            EvalDecision::Done => QueuePhase::Done,
            EvalDecision::Hitl { .. } => QueuePhase::Hitl,
        }
    }

    pub fn build_evaluate_script(&self) -> String {
        format!(
            r#"#!/bin/bash
COMPLETED=$(belt queue list --phase completed --json 2>/dev/null | jq 'length' 2>/dev/null)
if [ "$COMPLETED" = "0" ] || [ -z "$COMPLETED" ]; then exit 0; fi

belt agent --workspace "{ws}" -p \
  "Completed 아이템의 완료 여부를 판단하고, belt queue done 또는 belt queue hitl 을 실행해줘"
"#,
            ws = self.workspace_name
        )
    }

    pub async fn run_evaluate(&self, belt_home: &Path) -> Result<EvaluateResult> {
        let script = self.build_evaluate_script();
        let belt_db = belt_home.join("belt.db");
        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("WORKSPACE", &self.workspace_name)
            .env("BELT_HOME", belt_home.to_string_lossy().as_ref())
            .env("BELT_DB", belt_db.to_string_lossy().as_ref())
            .output()
            .await?;

        Ok(EvaluateResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

#[derive(Debug)]
pub struct EvaluateResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl EvaluateResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belt_core::queue::testing::test_item;

    #[test]
    fn filter_completed_items() {
        let mut items = vec![
            test_item("s1", "analyze"),
            test_item("s2", "implement"),
            test_item("s3", "review"),
        ];
        items[1].phase = QueuePhase::Completed;
        let completed = Evaluator::filter_completed(&items);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].work_id, "s2:implement");
    }

    #[test]
    fn target_phase_done() {
        assert_eq!(
            Evaluator::target_phase(&EvalDecision::Done),
            QueuePhase::Done
        );
    }

    #[test]
    fn target_phase_hitl() {
        let decision = EvalDecision::Hitl {
            reason: "needs review".to_string(),
        };
        assert_eq!(Evaluator::target_phase(&decision), QueuePhase::Hitl);
    }

    #[test]
    fn build_evaluate_script_contains_workspace() {
        let evaluator = Evaluator::new("auth-project");
        let script = evaluator.build_evaluate_script();
        assert!(script.contains("auth-project"));
        assert!(script.contains("belt agent"));
        assert!(script.contains("belt queue done"));
    }
}
