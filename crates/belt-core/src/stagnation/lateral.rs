//! Lateral thinking analysis for breaking out of stagnation.
//!
//! When a stagnation pattern is detected, [`LateralAnalyzer`] selects
//! a [`Persona`] that suggests a fundamentally different approach,
//! producing a [`LateralPlan`] that can guide the next agent attempt.

use serde::{Deserialize, Serialize};

use super::pattern::{StagnationDetection, StagnationPattern};

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
}

/// Selects a [`Persona`] based on the detected stagnation pattern.
///
/// The mapping is deterministic:
/// - `Spinning` → `Contrarian` (same output = wrong assumptions)
/// - `Oscillation` → `Architect` (flip-flopping = structural problem)
/// - `NoDrift` → `Researcher` (no progress = need external knowledge)
/// - `DiminishingReturns` → `Simplifier` (incremental gains plateau = over-engineering)
///
/// A `Hacker` persona is available as a fallback or can be selected manually.
#[derive(Debug, Default)]
pub struct LateralAnalyzer;

impl LateralAnalyzer {
    /// Create a new analyzer.
    pub fn new() -> Self {
        Self
    }

    /// Given a stagnation detection, produce a lateral plan.
    pub fn analyze(&self, detection: &StagnationDetection) -> LateralPlan {
        let persona = self.select_persona(detection.pattern);
        LateralPlan {
            persona,
            directive: persona.directive().to_string(),
            triggered_by: detection.pattern,
            detection_confidence: detection.confidence,
        }
    }

    /// Select a persona for the given pattern.
    pub fn select_persona(&self, pattern: StagnationPattern) -> Persona {
        match pattern {
            StagnationPattern::Spinning => Persona::Contrarian,
            StagnationPattern::Oscillation => Persona::Architect,
            StagnationPattern::NoDrift => Persona::Researcher,
            StagnationPattern::DiminishingReturns => Persona::Simplifier,
        }
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
    fn lateral_analyzer_spinning_selects_contrarian() {
        let analyzer = LateralAnalyzer::new();
        assert_eq!(
            analyzer.select_persona(StagnationPattern::Spinning),
            Persona::Contrarian
        );
    }

    #[test]
    fn lateral_analyzer_oscillation_selects_architect() {
        let analyzer = LateralAnalyzer::new();
        assert_eq!(
            analyzer.select_persona(StagnationPattern::Oscillation),
            Persona::Architect
        );
    }

    #[test]
    fn lateral_analyzer_no_drift_selects_researcher() {
        let analyzer = LateralAnalyzer::new();
        assert_eq!(
            analyzer.select_persona(StagnationPattern::NoDrift),
            Persona::Researcher
        );
    }

    #[test]
    fn lateral_analyzer_diminishing_returns_selects_simplifier() {
        let analyzer = LateralAnalyzer::new();
        assert_eq!(
            analyzer.select_persona(StagnationPattern::DiminishingReturns),
            Persona::Simplifier
        );
    }

    #[test]
    fn lateral_plan_from_detection() {
        let analyzer = LateralAnalyzer::new();
        let detection = StagnationDetection {
            pattern: StagnationPattern::Spinning,
            confidence: 0.95,
            reason: "3 consecutive identical outputs".to_string(),
        };
        let plan = analyzer.analyze(&detection);
        assert_eq!(plan.persona, Persona::Contrarian);
        assert_eq!(plan.triggered_by, StagnationPattern::Spinning);
        assert!((plan.detection_confidence - 0.95).abs() < f64::EPSILON);
        assert!(!plan.directive.is_empty());
    }

    #[test]
    fn lateral_plan_serde_roundtrip() {
        let plan = LateralPlan {
            persona: Persona::Architect,
            directive: "redesign".to_string(),
            triggered_by: StagnationPattern::Oscillation,
            detection_confidence: 0.8,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: LateralPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.persona, plan.persona);
        assert_eq!(parsed.triggered_by, plan.triggered_by);
    }
}
