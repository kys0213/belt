//! Lateral thinking analysis for breaking out of stagnation.
//!
//! When a stagnation pattern is detected, [`LateralAnalyzer`] selects
//! a [`Persona`] that suggests a fundamentally different approach,
//! producing a [`LateralPlan`] that can guide the next agent attempt.
//!
//! The analyzer invokes an LLM subprocess (`belt agent -p`) with the
//! selected persona's embedded prompt template and the failure context,
//! then parses the response into a structured [`LateralPlan`].

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::pattern::{StagnationDetection, StagnationPattern};
use crate::error::BeltError;
use crate::platform::ShellExecutor;

// --- Embedded persona prompts (compiled into the binary) ---

const PERSONA_HACKER: &str = include_str!("personas/hacker.md");
const PERSONA_ARCHITECT: &str = include_str!("personas/architect.md");
const PERSONA_RESEARCHER: &str = include_str!("personas/researcher.md");
const PERSONA_SIMPLIFIER: &str = include_str!("personas/simplifier.md");
const PERSONA_CONTRARIAN: &str = include_str!("personas/contrarian.md");

/// A lateral-thinking persona that frames the problem from a different angle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Persona {
    /// "Hack it until it works" — quick, pragmatic, possibly dirty.
    Hacker,
    /// Step back and redesign the architecture.
    Architect,
    /// Research prior art, docs, and similar solutions.
    Researcher,
    /// Radically simplify — remove code, reduce scope.
    Simplifier,
    /// Challenge every assumption that led here.
    Contrarian,
}

impl Persona {
    /// All available personas.
    pub const ALL: [Persona; 5] = [
        Persona::Hacker,
        Persona::Architect,
        Persona::Researcher,
        Persona::Simplifier,
        Persona::Contrarian,
    ];

    /// A short directive that characterizes this persona's approach.
    pub fn directive(&self) -> &'static str {
        match self {
            Persona::Hacker => {
                "Take the most pragmatic shortcut. Hardcode, monkey-patch, \
                 or use an escape hatch — make it work first, clean up later."
            }
            Persona::Architect => {
                "Step back and reconsider the design. Look for a structural change \
                 (new abstraction, different data flow) that avoids the current obstacle."
            }
            Persona::Researcher => {
                "Search documentation, prior issues, and known patterns for this kind of problem. \
                 Someone has likely solved this before."
            }
            Persona::Simplifier => {
                "Aggressively reduce scope. Remove unnecessary code, simplify the approach, \
                 and strip the solution to its minimal form."
            }
            Persona::Contrarian => {
                "Challenge the assumptions. What if the requirement is wrong? \
                 What if the constraint we're working around doesn't actually apply?"
            }
        }
    }

    /// Returns the embedded prompt template for this persona.
    pub fn prompt_template(&self) -> &'static str {
        match self {
            Persona::Hacker => PERSONA_HACKER,
            Persona::Architect => PERSONA_ARCHITECT,
            Persona::Researcher => PERSONA_RESEARCHER,
            Persona::Simplifier => PERSONA_SIMPLIFIER,
            Persona::Contrarian => PERSONA_CONTRARIAN,
        }
    }

    /// Affinity-ordered list of personas for a given stagnation pattern.
    ///
    /// The first persona is the best match for the pattern, and subsequent
    /// personas serve as fallbacks in decreasing affinity order.
    pub fn affinity_order(pattern: StagnationPattern) -> [Persona; 5] {
        match pattern {
            StagnationPattern::Spinning => [
                Persona::Hacker,
                Persona::Contrarian,
                Persona::Simplifier,
                Persona::Architect,
                Persona::Researcher,
            ],
            StagnationPattern::Oscillation => [
                Persona::Architect,
                Persona::Contrarian,
                Persona::Simplifier,
                Persona::Hacker,
                Persona::Researcher,
            ],
            StagnationPattern::NoDrift => [
                Persona::Researcher,
                Persona::Contrarian,
                Persona::Architect,
                Persona::Hacker,
                Persona::Simplifier,
            ],
            StagnationPattern::DiminishingReturns => [
                Persona::Simplifier,
                Persona::Contrarian,
                Persona::Researcher,
                Persona::Architect,
                Persona::Hacker,
            ],
        }
    }
}

impl std::fmt::Display for Persona {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Persona::Hacker => f.write_str("hacker"),
            Persona::Architect => f.write_str("architect"),
            Persona::Researcher => f.write_str("researcher"),
            Persona::Simplifier => f.write_str("simplifier"),
            Persona::Contrarian => f.write_str("contrarian"),
        }
    }
}

/// A plan produced by lateral analysis to break out of stagnation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LateralPlan {
    /// The selected persona.
    pub persona: Persona,
    /// The persona's directive text.
    pub directive: String,
    /// The stagnation pattern that triggered this plan.
    pub triggered_by: StagnationPattern,
    /// Confidence of the original detection.
    pub detection_confidence: f64,
    /// LLM-generated analysis of why previous attempts failed.
    pub failure_analysis: String,
    /// LLM-generated alternative approach suggestion.
    pub alternative_approach: String,
    /// LLM-generated step-by-step execution plan.
    pub execution_plan: String,
    /// LLM-generated warnings and caveats.
    pub warnings: String,
}

/// Parameters for [`LateralAnalyzer::analyze`].
///
/// Grouped into a struct to avoid excessive function parameters.
#[derive(Debug)]
pub struct AnalyzeParams<'a> {
    /// The stagnation detection result.
    pub detection: &'a StagnationDetection,
    /// Context describing recent failures (error messages, summaries).
    pub failure_context: &'a str,
    /// Personas that have already been tried and should be skipped.
    pub attempted_personas: &'a [Persona],
    /// Working directory for subprocess execution.
    pub workspace: &'a Path,
}

/// Selects a [`Persona`] based on the detected stagnation pattern and
/// invokes an LLM subprocess to generate a [`LateralPlan`].
///
/// Persona selection follows affinity ordering per pattern type:
/// - `Spinning` -> `Hacker` > `Contrarian` > `Simplifier` > `Architect` > `Researcher`
/// - `Oscillation` -> `Architect` > `Contrarian` > `Simplifier` > `Hacker` > `Researcher`
/// - `NoDrift` -> `Researcher` > `Contrarian` > `Architect` > `Hacker` > `Simplifier`
/// - `DiminishingReturns` -> `Simplifier` > `Contrarian` > `Researcher` > `Architect` > `Hacker`
///
/// Already-tried personas are filtered out. If all personas are exhausted,
/// analysis returns an error.
#[derive(Debug, Default)]
pub struct LateralAnalyzer;

impl LateralAnalyzer {
    /// Create a new analyzer.
    pub fn new() -> Self {
        Self
    }

    /// Select the best available persona for the given pattern, excluding
    /// any personas that have already been attempted.
    ///
    /// Returns `None` if all personas have been exhausted.
    pub fn select_persona(
        &self,
        pattern: StagnationPattern,
        attempted: &[Persona],
    ) -> Option<Persona> {
        Persona::affinity_order(pattern)
            .iter()
            .find(|p| !attempted.contains(p))
            .copied()
    }

    /// Build the full prompt by combining the persona template with failure context.
    pub fn build_prompt(&self, persona: Persona, failure_context: &str) -> String {
        format!(
            "{}\n\n---\n\n## Failure Context\n\n{failure_context}",
            persona.prompt_template(),
        )
    }

    /// Given a stagnation detection and failure context, invoke the LLM
    /// subprocess to produce a lateral plan.
    ///
    /// Uses `belt agent -p` via the provided [`ShellExecutor`].
    pub async fn analyze(
        &self,
        executor: &dyn ShellExecutor,
        params: &AnalyzeParams<'_>,
    ) -> Result<LateralPlan, BeltError> {
        let persona = self
            .select_persona(params.detection.pattern, params.attempted_personas)
            .ok_or_else(|| {
                BeltError::Stagnation(format!(
                    "all personas exhausted for pattern {}",
                    params.detection.pattern,
                ))
            })?;

        let prompt = self.build_prompt(persona, params.failure_context);

        // Escape single quotes in the prompt for safe shell embedding.
        let escaped_prompt = prompt.replace('\'', "'\\''");
        let command = format!("belt agent -p '{escaped_prompt}'");

        let output = executor
            .execute(&command, params.workspace, &HashMap::new())
            .await
            .map_err(|e| {
                BeltError::Stagnation(format!(
                    "subprocess invocation failed for persona {persona}: {e}"
                ))
            })?;

        if !output.success() {
            return Err(BeltError::Stagnation(format!(
                "subprocess failed for persona {persona} (exit code {:?}): {}",
                output.exit_code,
                output.stderr.trim(),
            )));
        }

        let response = output.stdout.trim();
        if response.is_empty() {
            return Err(BeltError::Stagnation(format!(
                "empty response from subprocess for persona {persona}"
            )));
        }

        Ok(self.parse_response(persona, params.detection, response))
    }

    /// Parse the LLM response into a [`LateralPlan`].
    ///
    /// Extracts sections delimited by `**Failure Analysis**:`,
    /// `**Alternative Approach**:`, `**Execution Plan**:`, and
    /// `**Warnings**:` headers. If a section is not found, falls back to
    /// the persona directive or the full response text.
    pub fn parse_response(
        &self,
        persona: Persona,
        detection: &StagnationDetection,
        response: &str,
    ) -> LateralPlan {
        let failure_analysis =
            extract_section(response, "Failure Analysis").unwrap_or_else(|| response.to_string());
        let alternative_approach = extract_section(response, "Alternative Approach")
            .unwrap_or_else(|| persona.directive().to_string());
        let execution_plan = extract_section(response, "Execution Plan").unwrap_or_default();
        let warnings = extract_section(response, "Warnings").unwrap_or_default();

        LateralPlan {
            persona,
            directive: persona.directive().to_string(),
            triggered_by: detection.pattern,
            detection_confidence: detection.confidence,
            failure_analysis,
            alternative_approach,
            execution_plan,
            warnings,
        }
    }
}

/// Extract a named section from a markdown-style response.
///
/// Looks for patterns like `**Section Name**:` or `- **Section Name**:` and
/// captures text until the next section header or end of input.
fn extract_section(text: &str, section_name: &str) -> Option<String> {
    let pattern = format!("**{section_name}**:");
    let start = text.find(&pattern)?;
    let content_start = start + pattern.len();
    let rest = &text[content_start..];

    // Find the next section header or end of text.
    let end = rest
        .find("\n- **")
        .or_else(|| rest.find("\n**"))
        .unwrap_or(rest.len());

    let section = rest[..end].trim();
    if section.is_empty() {
        None
    } else {
        Some(section.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_display() {
        assert_eq!(Persona::Hacker.to_string(), "hacker");
        assert_eq!(Persona::Architect.to_string(), "architect");
        assert_eq!(Persona::Researcher.to_string(), "researcher");
        assert_eq!(Persona::Simplifier.to_string(), "simplifier");
        assert_eq!(Persona::Contrarian.to_string(), "contrarian");
    }

    #[test]
    fn persona_all_has_five() {
        assert_eq!(Persona::ALL.len(), 5);
    }

    #[test]
    fn persona_serde_roundtrip() {
        for persona in Persona::ALL {
            let json = serde_json::to_string(&persona).unwrap();
            let parsed: Persona = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, persona);
        }
    }

    #[test]
    fn persona_directive_non_empty() {
        for persona in Persona::ALL {
            assert!(!persona.directive().is_empty());
        }
    }

    #[test]
    fn persona_prompt_template_non_empty() {
        for persona in Persona::ALL {
            let template = persona.prompt_template();
            assert!(!template.is_empty(), "{persona} has empty template");
            assert!(
                template.contains("## Your Approach"),
                "{persona} template missing expected section"
            );
        }
    }

    #[test]
    fn affinity_spinning_starts_with_hacker() {
        let order = Persona::affinity_order(StagnationPattern::Spinning);
        assert_eq!(order[0], Persona::Hacker);
    }

    #[test]
    fn affinity_oscillation_starts_with_architect() {
        let order = Persona::affinity_order(StagnationPattern::Oscillation);
        assert_eq!(order[0], Persona::Architect);
    }

    #[test]
    fn affinity_no_drift_starts_with_researcher() {
        let order = Persona::affinity_order(StagnationPattern::NoDrift);
        assert_eq!(order[0], Persona::Researcher);
    }

    #[test]
    fn affinity_diminishing_starts_with_simplifier() {
        let order = Persona::affinity_order(StagnationPattern::DiminishingReturns);
        assert_eq!(order[0], Persona::Simplifier);
    }

    #[test]
    fn affinity_orders_contain_all_personas() {
        for pattern in [
            StagnationPattern::Spinning,
            StagnationPattern::Oscillation,
            StagnationPattern::NoDrift,
            StagnationPattern::DiminishingReturns,
        ] {
            let order = Persona::affinity_order(pattern);
            for persona in Persona::ALL {
                assert!(
                    order.contains(&persona),
                    "pattern {pattern} missing persona {persona}"
                );
            }
        }
    }

    #[test]
    fn select_persona_filters_attempted() {
        let analyzer = LateralAnalyzer::new();
        // Spinning affinity: Hacker > Contrarian > Simplifier > Architect > Researcher
        let attempted = &[Persona::Hacker, Persona::Contrarian];
        let selected = analyzer.select_persona(StagnationPattern::Spinning, attempted);
        assert_eq!(selected, Some(Persona::Simplifier));
    }

    #[test]
    fn select_persona_returns_none_when_exhausted() {
        let analyzer = LateralAnalyzer::new();
        let all = Persona::ALL.to_vec();
        let selected = analyzer.select_persona(StagnationPattern::Spinning, &all);
        assert!(selected.is_none());
    }

    #[test]
    fn select_persona_no_filter_returns_first_affinity() {
        let analyzer = LateralAnalyzer::new();
        let selected = analyzer.select_persona(StagnationPattern::Oscillation, &[]);
        assert_eq!(selected, Some(Persona::Architect));
    }

    #[test]
    fn build_prompt_contains_persona_template_and_context() {
        let analyzer = LateralAnalyzer::new();
        let prompt = analyzer.build_prompt(Persona::Hacker, "compile error: Session not found");
        assert!(prompt.contains("# Hacker Persona"));
        assert!(prompt.contains("compile error: Session not found"));
        assert!(prompt.contains("## Failure Context"));
    }

    #[test]
    fn parse_response_extracts_sections() {
        let analyzer = LateralAnalyzer::new();
        let response = "\
- **Failure Analysis**: The code is looping on the same compile error.
- **Alternative Approach**: Use tower-sessions crate instead.
- **Execution Plan**: 1. Add dependency. 2. Replace type.
- **Warnings**: This introduces a new dependency.";

        let detection = StagnationDetection {
            pattern: StagnationPattern::Spinning,
            confidence: 0.9,
            reason: "test".to_string(),
        };

        let plan = analyzer.parse_response(Persona::Hacker, &detection, response);
        assert_eq!(plan.persona, Persona::Hacker);
        assert_eq!(plan.triggered_by, StagnationPattern::Spinning);
        assert!(plan.failure_analysis.contains("looping"));
        assert!(plan.alternative_approach.contains("tower-sessions"));
        assert!(plan.execution_plan.contains("Add dependency"));
        assert!(plan.warnings.contains("new dependency"));
    }

    #[test]
    fn parse_response_falls_back_on_missing_sections() {
        let analyzer = LateralAnalyzer::new();
        let response = "Just a plain text response with no sections.";
        let detection = StagnationDetection {
            pattern: StagnationPattern::NoDrift,
            confidence: 0.7,
            reason: "test".to_string(),
        };

        let plan = analyzer.parse_response(Persona::Researcher, &detection, response);
        // failure_analysis falls back to full response
        assert_eq!(plan.failure_analysis, response);
        // alternative_approach falls back to directive
        assert_eq!(plan.alternative_approach, Persona::Researcher.directive());
        // execution_plan and warnings fall back to empty
        assert!(plan.execution_plan.is_empty());
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn extract_section_with_bold_markers() {
        let text = "**Failure Analysis**: Something failed.\n**Alternative Approach**: Try X.";
        let result = extract_section(text, "Failure Analysis");
        assert_eq!(result, Some("Something failed.".to_string()));
    }

    #[test]
    fn extract_section_returns_none_for_missing() {
        let text = "No sections here.";
        assert!(extract_section(text, "Failure Analysis").is_none());
    }

    #[test]
    fn lateral_plan_serde_roundtrip() {
        let plan = LateralPlan {
            persona: Persona::Architect,
            directive: "redesign".to_string(),
            triggered_by: StagnationPattern::Oscillation,
            detection_confidence: 0.8,
            failure_analysis: "structural issue".to_string(),
            alternative_approach: "new abstraction".to_string(),
            execution_plan: "1. refactor 2. test".to_string(),
            warnings: "may break API".to_string(),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: LateralPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.persona, plan.persona);
        assert_eq!(parsed.triggered_by, plan.triggered_by);
        assert_eq!(parsed.failure_analysis, plan.failure_analysis);
        assert_eq!(parsed.alternative_approach, plan.alternative_approach);
    }

    // --- Subprocess mock tests for analyze() ---

    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::platform::ShellOutput;

    /// A configurable mock shell executor for testing `analyze()`.
    struct MockShell {
        /// The result to return from `execute`.
        result: Mutex<Result<ShellOutput, BeltError>>,
    }

    impl MockShell {
        /// Create a mock that returns a successful response with the given stdout.
        fn success(stdout: &str) -> Self {
            Self {
                result: Mutex::new(Ok(ShellOutput {
                    exit_code: Some(0),
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                })),
            }
        }

        /// Create a mock that returns a non-zero exit code with the given stderr.
        fn failure(exit_code: i32, stderr: &str) -> Self {
            Self {
                result: Mutex::new(Ok(ShellOutput {
                    exit_code: Some(exit_code),
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                })),
            }
        }

        /// Create a mock that returns an execution error (simulating e.g. timeout).
        fn exec_error(message: &str) -> Self {
            Self {
                result: Mutex::new(Err(BeltError::Runtime(message.to_string()))),
            }
        }
    }

    #[async_trait]
    impl ShellExecutor for MockShell {
        async fn execute(
            &self,
            _command: &str,
            _working_dir: &Path,
            _env_vars: &HashMap<String, String>,
        ) -> Result<ShellOutput, BeltError> {
            let mut guard = self.result.lock().unwrap();
            std::mem::replace(
                &mut *guard,
                Err(BeltError::Runtime("mock already consumed".to_string())),
            )
        }
    }

    fn test_detection() -> StagnationDetection {
        StagnationDetection {
            pattern: StagnationPattern::Spinning,
            confidence: 0.9,
            reason: "repeated identical errors".to_string(),
        }
    }

    #[tokio::test]
    async fn analyze_success_returns_lateral_plan() {
        let response = "\
- **Failure Analysis**: The session type is not imported correctly.
- **Alternative Approach**: Use a type alias to simplify the import chain.
- **Execution Plan**: 1. Add type alias. 2. Update imports.
- **Warnings**: Alias may confuse new contributors.";

        let shell = MockShell::success(response);
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error: Session not found",
            attempted_personas: &[],
            workspace: &workspace,
        };

        let plan = analyzer.analyze(&shell, &params).await.unwrap();

        assert_eq!(plan.persona, Persona::Hacker); // first affinity for Spinning
        assert_eq!(plan.triggered_by, StagnationPattern::Spinning);
        assert!((plan.detection_confidence - 0.9).abs() < f64::EPSILON);
        assert!(plan.failure_analysis.contains("session type"));
        assert!(plan.alternative_approach.contains("type alias"));
        assert!(plan.execution_plan.contains("Add type alias"));
        assert!(plan.warnings.contains("confuse"));
    }

    #[tokio::test]
    async fn analyze_success_skips_attempted_personas() {
        let response = "**Failure Analysis**: root cause found.";
        let shell = MockShell::success(response);
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let attempted = [Persona::Hacker, Persona::Contrarian];
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error: Session not found",
            attempted_personas: &attempted,
            workspace: &workspace,
        };

        let plan = analyzer.analyze(&shell, &params).await.unwrap();

        // Third in Spinning affinity order is Simplifier.
        assert_eq!(plan.persona, Persona::Simplifier);
    }

    #[tokio::test]
    async fn analyze_subprocess_nonzero_exit_returns_stagnation_error() {
        let shell = MockShell::failure(1, "agent crashed");
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error",
            attempted_personas: &[],
            workspace: &workspace,
        };

        let err = analyzer.analyze(&shell, &params).await.unwrap_err();

        match &err {
            BeltError::Stagnation(msg) => {
                assert!(msg.contains("subprocess failed"), "got: {msg}");
                assert!(msg.contains("agent crashed"), "got: {msg}");
                assert!(msg.contains("hacker"), "got: {msg}");
            }
            other => panic!("expected BeltError::Stagnation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_subprocess_exec_error_returns_stagnation_error() {
        let shell = MockShell::exec_error("connection timed out");
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error",
            attempted_personas: &[],
            workspace: &workspace,
        };

        let err = analyzer.analyze(&shell, &params).await.unwrap_err();

        match &err {
            BeltError::Stagnation(msg) => {
                assert!(msg.contains("subprocess invocation failed"), "got: {msg}");
                assert!(msg.contains("connection timed out"), "got: {msg}");
            }
            other => panic!("expected BeltError::Stagnation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_empty_response_returns_stagnation_error() {
        let shell = MockShell::success("   \n  "); // whitespace-only
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error",
            attempted_personas: &[],
            workspace: &workspace,
        };

        let err = analyzer.analyze(&shell, &params).await.unwrap_err();

        match &err {
            BeltError::Stagnation(msg) => {
                assert!(msg.contains("empty response"), "got: {msg}");
                assert!(msg.contains("hacker"), "got: {msg}");
            }
            other => panic!("expected BeltError::Stagnation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_all_personas_exhausted_returns_stagnation_error() {
        let shell = MockShell::exec_error("should not be called");
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let all_personas = Persona::ALL.to_vec();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error",
            attempted_personas: &all_personas,
            workspace: &workspace,
        };

        let err = analyzer.analyze(&shell, &params).await.unwrap_err();

        match &err {
            BeltError::Stagnation(msg) => {
                assert!(msg.contains("all personas exhausted"), "got: {msg}");
            }
            other => panic!("expected BeltError::Stagnation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_nonzero_exit_with_signal_terminated() {
        let shell = MockShell {
            result: Mutex::new(Ok(ShellOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: "killed by signal".to_string(),
            })),
        };
        let analyzer = LateralAnalyzer::new();
        let detection = test_detection();
        let workspace = PathBuf::from("/tmp/test-workspace");
        let params = AnalyzeParams {
            detection: &detection,
            failure_context: "compile error",
            attempted_personas: &[],
            workspace: &workspace,
        };

        let err = analyzer.analyze(&shell, &params).await.unwrap_err();

        match &err {
            BeltError::Stagnation(msg) => {
                assert!(msg.contains("subprocess failed"), "got: {msg}");
                assert!(msg.contains("killed by signal"), "got: {msg}");
            }
            other => panic!("expected BeltError::Stagnation, got: {other:?}"),
        }
    }
}
