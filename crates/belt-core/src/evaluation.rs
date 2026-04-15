//! Progressive Evaluation Pipeline — trait definitions and pipeline composition.
//!
//! Evaluation proceeds through stages ordered by cost (cheapest first).
//! Each stage can return a definitive decision or `Inconclusive` to delegate
//! to the next stage. If all stages are inconclusive, the pipeline escalates
//! to HITL.
//!
//! ## Stages (v6)
//!
//! 1. **MechanicalStage** — deterministic checks (cargo test, clippy, etc.) in worktree. Cost $0.
//! 2. **SemanticStage** — single LLM call to judge whether the work is sufficient.
//!
//! Adding a new stage (e.g. `ConsensusStage` in v7) requires zero core changes (OCP).

use std::path::PathBuf;

use async_trait::async_trait;

/// Evaluation context passed to each stage.
///
/// Contains the information a stage needs to perform its judgment.
/// The semantic stage requires `issue_body`, `handler_stdout`, `handler_stderr`,
/// `execution_history`, and `classify_policy` to build a structured LLM prompt
/// (R-015).
#[derive(Debug, Clone)]
pub struct EvalContext {
    /// The work_id of the queue item being evaluated.
    pub work_id: String,
    /// The source_id of the queue item.
    pub source_id: String,
    /// The workspace name.
    pub workspace_name: String,
    /// Path to the worktree directory for this item.
    pub worktree_path: Option<PathBuf>,
    /// Original issue body from the data source.
    pub issue_body: Option<String>,
    /// Handler stdout captured after execution.
    pub handler_stdout: Option<String>,
    /// Handler stderr captured after execution.
    pub handler_stderr: Option<String>,
    /// Execution history summary (prior attempts, failures, lateral plans).
    pub execution_history: Option<String>,
    /// Contents of classify-policy.md for workspace-specific judgment guidelines.
    pub classify_policy: Option<String>,
}

/// Decision produced by an evaluation stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalDecision {
    /// Work is sufficient — trigger on_done hook and transit to Done.
    Done,
    /// Human review needed — create HITL event.
    Hitl { reason: String },
    /// Mechanical check failed — handler should re-execute (no LLM cost).
    Retry,
    /// This stage cannot make a definitive judgment — pass to the next stage.
    Inconclusive,
}

/// A single evaluation stage in the progressive pipeline.
///
/// Implementors perform one kind of judgment (mechanical, semantic, consensus, etc.)
/// and return an [`EvalDecision`]. Returning `Inconclusive` delegates to the next stage.
#[async_trait]
pub trait EvaluationStage: Send + Sync {
    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;

    /// Perform evaluation and return a decision.
    ///
    /// - `Done` / `Hitl` / `Retry` are definitive — the pipeline stops.
    /// - `Inconclusive` means this stage cannot judge — the next stage runs.
    async fn evaluate(&self, ctx: &EvalContext) -> anyhow::Result<EvalDecision>;
}

/// Composite pipeline that runs stages in order (cheapest first).
///
/// When a stage returns a definitive decision, the pipeline short-circuits.
/// If all stages return `Inconclusive`, the pipeline escalates to HITL.
pub struct EvaluationPipeline {
    stages: Vec<Box<dyn EvaluationStage>>,
}

impl EvaluationPipeline {
    /// Create a new pipeline with the given stages (ordered cheapest-first).
    pub fn new(stages: Vec<Box<dyn EvaluationStage>>) -> Self {
        Self { stages }
    }

    /// Create an empty pipeline (all items will escalate to HITL).
    pub fn empty() -> Self {
        Self { stages: vec![] }
    }

    /// Return the number of registered stages.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Return the names of registered stages in order.
    pub fn stage_names(&self) -> Vec<&str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run stages sequentially. Stop on the first definitive decision.
    ///
    /// If all stages return `Inconclusive`, escalates to HITL as a safe default.
    pub async fn evaluate(&self, ctx: &EvalContext) -> anyhow::Result<EvalDecision> {
        for stage in &self.stages {
            let decision = stage.evaluate(ctx).await?;
            match decision {
                EvalDecision::Inconclusive => {
                    tracing::debug!(
                        stage = stage.name(),
                        work_id = %ctx.work_id,
                        "stage returned Inconclusive, proceeding to next"
                    );
                    continue;
                }
                definitive => {
                    tracing::info!(
                        stage = stage.name(),
                        work_id = %ctx.work_id,
                        decision = ?definitive,
                        "stage returned definitive decision"
                    );
                    return Ok(definitive);
                }
            }
        }

        // All stages inconclusive — safe default is HITL.
        tracing::warn!(
            work_id = %ctx.work_id,
            "all {} stages returned Inconclusive, escalating to HITL",
            self.stages.len()
        );
        Ok(EvalDecision::Hitl {
            reason: "all stages inconclusive".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test stage that always returns a fixed decision.
    struct FixedStage {
        stage_name: &'static str,
        decision: EvalDecision,
    }

    #[async_trait]
    impl EvaluationStage for FixedStage {
        fn name(&self) -> &str {
            self.stage_name
        }
        async fn evaluate(&self, _ctx: &EvalContext) -> anyhow::Result<EvalDecision> {
            Ok(self.decision.clone())
        }
    }

    fn test_ctx() -> EvalContext {
        EvalContext {
            work_id: "test:implement".into(),
            source_id: "test".into(),
            workspace_name: "test-ws".into(),
            worktree_path: None,
            issue_body: None,
            handler_stdout: None,
            handler_stderr: None,
            execution_history: None,
            classify_policy: None,
        }
    }

    #[tokio::test]
    async fn pipeline_short_circuits_on_done() {
        let pipeline = EvaluationPipeline::new(vec![
            Box::new(FixedStage {
                stage_name: "mechanical",
                decision: EvalDecision::Inconclusive,
            }),
            Box::new(FixedStage {
                stage_name: "semantic",
                decision: EvalDecision::Done,
            }),
        ]);

        let decision = pipeline.evaluate(&test_ctx()).await.unwrap();
        assert_eq!(decision, EvalDecision::Done);
    }

    #[tokio::test]
    async fn pipeline_short_circuits_on_retry() {
        let pipeline = EvaluationPipeline::new(vec![
            Box::new(FixedStage {
                stage_name: "mechanical",
                decision: EvalDecision::Retry,
            }),
            Box::new(FixedStage {
                stage_name: "semantic",
                decision: EvalDecision::Done,
            }),
        ]);

        let decision = pipeline.evaluate(&test_ctx()).await.unwrap();
        assert_eq!(decision, EvalDecision::Retry);
    }

    #[tokio::test]
    async fn pipeline_escalates_hitl_when_all_inconclusive() {
        let pipeline = EvaluationPipeline::new(vec![
            Box::new(FixedStage {
                stage_name: "mechanical",
                decision: EvalDecision::Inconclusive,
            }),
            Box::new(FixedStage {
                stage_name: "semantic",
                decision: EvalDecision::Inconclusive,
            }),
        ]);

        let decision = pipeline.evaluate(&test_ctx()).await.unwrap();
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
    }

    #[tokio::test]
    async fn empty_pipeline_escalates_to_hitl() {
        let pipeline = EvaluationPipeline::empty();
        let decision = pipeline.evaluate(&test_ctx()).await.unwrap();
        assert!(matches!(decision, EvalDecision::Hitl { .. }));
    }

    #[test]
    fn stage_names_returns_ordered_names() {
        let pipeline = EvaluationPipeline::new(vec![
            Box::new(FixedStage {
                stage_name: "mechanical",
                decision: EvalDecision::Inconclusive,
            }),
            Box::new(FixedStage {
                stage_name: "semantic",
                decision: EvalDecision::Done,
            }),
        ]);

        assert_eq!(pipeline.stage_names(), vec!["mechanical", "semantic"]);
        assert_eq!(pipeline.stage_count(), 2);
    }

    #[tokio::test]
    async fn pipeline_hitl_decision_stops_pipeline() {
        let pipeline = EvaluationPipeline::new(vec![
            Box::new(FixedStage {
                stage_name: "mechanical",
                decision: EvalDecision::Hitl {
                    reason: "test failure".into(),
                },
            }),
            Box::new(FixedStage {
                stage_name: "semantic",
                decision: EvalDecision::Done,
            }),
        ]);

        let decision = pipeline.evaluate(&test_ctx()).await.unwrap();
        assert_eq!(
            decision,
            EvalDecision::Hitl {
                reason: "test failure".into()
            }
        );
    }
}
