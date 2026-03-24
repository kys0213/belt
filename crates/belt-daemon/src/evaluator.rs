use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use belt_core::phase::QueuePhase;
use belt_core::queue::QueueItem;

/// Default maximum number of evaluate failures before HITL escalation.
pub const DEFAULT_MAX_EVAL_FAILURES: u32 = 3;

/// Completed 아이템의 평가 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalDecision {
    Done,
    Hitl {
        reason: String,
    },
    /// Evaluate 실행 실패 — Completed 유지, 다음 tick에서 재시도.
    Retry,
}

/// Completed 아이템을 스캔하여 Done 또는 HITL로 분류.
///
/// Per-item evaluate failure count를 추적하며, `max_eval_failures`회
/// 연속 실패 시 자동으로 HITL로 에스컬레이션한다.
pub struct Evaluator {
    workspace_name: String,
    /// Per-item evaluate failure counts (keyed by work_id).
    eval_failure_counts: HashMap<String, u32>,
    /// Maximum evaluate failures before HITL escalation.
    max_eval_failures: u32,
}

impl Evaluator {
    pub fn new(workspace_name: &str) -> Self {
        Self {
            workspace_name: workspace_name.to_string(),
            eval_failure_counts: HashMap::new(),
            max_eval_failures: DEFAULT_MAX_EVAL_FAILURES,
        }
    }

    /// Set the maximum evaluate failure threshold for HITL escalation.
    pub fn with_max_eval_failures(mut self, max: u32) -> Self {
        self.max_eval_failures = max;
        self
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
            EvalDecision::Retry => QueuePhase::Completed,
        }
    }

    /// Record an evaluate failure for the given work_id and return the
    /// appropriate decision based on accumulated failure count.
    ///
    /// If failures >= `max_eval_failures`, escalates to HITL.
    /// Otherwise, returns `Retry` to keep the item in Completed phase.
    pub fn record_eval_failure(&mut self, work_id: &str, error: &str) -> EvalDecision {
        let count = self
            .eval_failure_counts
            .entry(work_id.to_string())
            .or_insert(0);
        *count += 1;

        tracing::warn!(
            work_id,
            eval_failure_count = *count,
            max = self.max_eval_failures,
            "evaluate failure recorded"
        );

        if *count >= self.max_eval_failures {
            tracing::error!(
                work_id,
                eval_failure_count = *count,
                "escalating to HITL after {} evaluate failures",
                *count
            );
            EvalDecision::Hitl {
                reason: format!(
                    "evaluate failed {} times (threshold={}): {}",
                    *count, self.max_eval_failures, error
                ),
            }
        } else {
            EvalDecision::Retry
        }
    }

    /// Clear the evaluate failure count for a work_id (e.g. on successful evaluation).
    pub fn clear_eval_failures(&mut self, work_id: &str) {
        self.eval_failure_counts.remove(work_id);
    }

    /// Return the current evaluate failure count for a work_id.
    pub fn eval_failure_count(&self, work_id: &str) -> u32 {
        self.eval_failure_counts.get(work_id).copied().unwrap_or(0)
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
    fn target_phase_retry() {
        assert_eq!(
            Evaluator::target_phase(&EvalDecision::Retry),
            QueuePhase::Completed
        );
    }

    #[test]
    fn build_evaluate_script_contains_workspace() {
        let evaluator = Evaluator::new("auth-project");
        let script = evaluator.build_evaluate_script();
        assert!(script.contains("auth-project"));
        assert!(script.contains("belt agent"));
        assert!(script.contains("belt queue done"));
    }

    #[test]
    fn record_eval_failure_escalates_after_threshold() {
        let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(3);

        // First two failures -> Retry
        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert_eq!(decision, EvalDecision::Retry);
        assert_eq!(evaluator.eval_failure_count("item-1"), 1);

        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert_eq!(decision, EvalDecision::Retry);
        assert_eq!(evaluator.eval_failure_count("item-1"), 2);

        // Third failure -> HITL escalation
        let decision = evaluator.record_eval_failure("item-1", "script error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
        assert_eq!(evaluator.eval_failure_count("item-1"), 3);
    }

    #[test]
    fn clear_eval_failures_resets_count() {
        let mut evaluator = Evaluator::new("test-ws");
        evaluator.record_eval_failure("item-1", "error");
        evaluator.record_eval_failure("item-1", "error");
        assert_eq!(evaluator.eval_failure_count("item-1"), 2);

        evaluator.clear_eval_failures("item-1");
        assert_eq!(evaluator.eval_failure_count("item-1"), 0);
    }

    #[test]
    fn independent_failure_tracking_per_item() {
        let mut evaluator = Evaluator::new("test-ws").with_max_eval_failures(2);

        evaluator.record_eval_failure("item-1", "error");
        evaluator.record_eval_failure("item-2", "error");

        assert_eq!(evaluator.eval_failure_count("item-1"), 1);
        assert_eq!(evaluator.eval_failure_count("item-2"), 1);

        // item-1 hits threshold
        let decision = evaluator.record_eval_failure("item-1", "error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));

        // item-2 still retrying
        let decision = evaluator.record_eval_failure("item-2", "error");
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
    }

    #[test]
    fn default_max_eval_failures_is_three() {
        let evaluator = Evaluator::new("test-ws");
        assert_eq!(evaluator.max_eval_failures, DEFAULT_MAX_EVAL_FAILURES);
        assert_eq!(evaluator.max_eval_failures, 3);
    }
}
