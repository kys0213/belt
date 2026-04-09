//! Stagnation pattern detection.
//!
//! Defines [`StagnationPattern`] enum representing detected stagnation types,
//! [`StagnationDetection`] as the detection result, and [`PatternDetector`] trait
//! with [`SpinningDetector`] and [`OscillationDetector`] implementations.

use serde::{Deserialize, Serialize};

use super::similarity::{SimilarityJudge, SimilarityScore};

/// A recognized stagnation pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagnationPattern {
    /// Agent produces nearly identical outputs across consecutive attempts.
    Spinning,
    /// Agent alternates between two (or few) distinct outputs.
    Oscillation,
    /// Agent output changes but the evaluation score does not improve.
    NoDrift,
    /// Each successive attempt yields smaller improvements, approaching zero.
    DiminishingReturns,
}

impl std::fmt::Display for StagnationPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StagnationPattern::Spinning => f.write_str("spinning"),
            StagnationPattern::Oscillation => f.write_str("oscillation"),
            StagnationPattern::NoDrift => f.write_str("no_drift"),
            StagnationPattern::DiminishingReturns => f.write_str("diminishing_returns"),
        }
    }
}

/// Result of a pattern detection pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagnationDetection {
    /// The detected pattern.
    pub pattern: StagnationPattern,
    /// Confidence level in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Human-readable explanation of why the pattern was detected.
    pub reason: String,
}

/// Detect a specific stagnation pattern from a history of outputs.
pub trait PatternDetector: Send + Sync {
    /// Analyze `outputs` (oldest first) and return a detection if the pattern is found.
    fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection>;

    /// Which pattern this detector targets.
    fn target_pattern(&self) -> StagnationPattern;
}

/// Detects [`StagnationPattern::Spinning`] — consecutive near-identical outputs.
pub struct SpinningDetector {
    judge: Box<dyn SimilarityJudge>,
    /// Minimum similarity to consider two outputs as "the same".
    threshold: SimilarityScore,
    /// Number of consecutive similar pairs required to declare spinning.
    min_consecutive: usize,
}

impl SpinningDetector {
    /// Create a new spinning detector.
    ///
    /// - `judge`: similarity judge to compare consecutive outputs.
    /// - `threshold`: similarity score above which two outputs are "the same" (default 0.9).
    /// - `min_consecutive`: how many consecutive similar pairs needed (default 2).
    pub fn new(
        judge: Box<dyn SimilarityJudge>,
        threshold: SimilarityScore,
        min_consecutive: usize,
    ) -> Self {
        Self {
            judge,
            threshold,
            min_consecutive,
        }
    }
}

impl PatternDetector for SpinningDetector {
    fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection> {
        if outputs.len() < 2 {
            return None;
        }

        let mut consecutive = 0_usize;
        let mut total_score = 0.0_f64;

        for pair in outputs.windows(2) {
            let score = self.judge.score(pair[0], pair[1]);
            if score >= self.threshold {
                consecutive += 1;
                total_score += score;
            } else {
                consecutive = 0;
                total_score = 0.0;
            }

            if consecutive >= self.min_consecutive {
                let avg_score = total_score / consecutive as f64;
                return Some(StagnationDetection {
                    pattern: StagnationPattern::Spinning,
                    confidence: avg_score,
                    reason: format!(
                        "{} consecutive pairs above threshold {:.2} (avg similarity {:.3})",
                        consecutive, self.threshold, avg_score
                    ),
                });
            }
        }

        None
    }

    fn target_pattern(&self) -> StagnationPattern {
        StagnationPattern::Spinning
    }
}

/// Detects [`StagnationPattern::Oscillation`] — alternating between a small set of outputs.
///
/// Checks if output[i] is similar to output[i-2] (A-B-A-B pattern).
pub struct OscillationDetector {
    judge: Box<dyn SimilarityJudge>,
    /// Minimum similarity to consider two outputs as "the same".
    threshold: SimilarityScore,
    /// Number of oscillation cycles required.
    min_cycles: usize,
}

impl OscillationDetector {
    /// Create a new oscillation detector.
    ///
    /// - `judge`: similarity judge.
    /// - `threshold`: similarity score above which two outputs are "the same" (default 0.9).
    /// - `min_cycles`: how many A-B-A cycles needed (default 2).
    pub fn new(
        judge: Box<dyn SimilarityJudge>,
        threshold: SimilarityScore,
        min_cycles: usize,
    ) -> Self {
        Self {
            judge,
            threshold,
            min_cycles,
        }
    }
}

impl PatternDetector for OscillationDetector {
    fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection> {
        if outputs.len() < 4 {
            return None;
        }

        let mut cycles = 0_usize;
        let mut total_score = 0.0_f64;

        // Check A-B-A pattern: output[i] ~ output[i-2]
        for i in 2..outputs.len() {
            let score = self.judge.score(outputs[i], outputs[i - 2]);
            if score >= self.threshold {
                cycles += 1;
                total_score += score;
            }
        }

        if cycles >= self.min_cycles {
            let avg_score = total_score / cycles as f64;
            return Some(StagnationDetection {
                pattern: StagnationPattern::Oscillation,
                confidence: avg_score,
                reason: format!(
                    "{} oscillation cycles detected (avg similarity {:.3})",
                    cycles, avg_score
                ),
            });
        }

        None
    }

    fn target_pattern(&self) -> StagnationPattern {
        StagnationPattern::Oscillation
    }
}

/// Composite detector that runs multiple [`PatternDetector`]s and returns
/// the highest-confidence detection.
pub struct StagnationDetector {
    detectors: Vec<Box<dyn PatternDetector>>,
}

impl StagnationDetector {
    /// Create a composite from the given detectors.
    pub fn new(detectors: Vec<Box<dyn PatternDetector>>) -> Self {
        Self { detectors }
    }

    /// Run all detectors and return the detection with the highest confidence, if any.
    pub fn detect(&self, outputs: &[&str]) -> Option<StagnationDetection> {
        self.detectors
            .iter()
            .filter_map(|d| d.detect(outputs))
            .max_by(|a, b| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stagnation::similarity::ExactHash;

    #[test]
    fn stagnation_pattern_display() {
        assert_eq!(StagnationPattern::Spinning.to_string(), "spinning");
        assert_eq!(StagnationPattern::Oscillation.to_string(), "oscillation");
        assert_eq!(StagnationPattern::NoDrift.to_string(), "no_drift");
        assert_eq!(
            StagnationPattern::DiminishingReturns.to_string(),
            "diminishing_returns"
        );
    }

    #[test]
    fn stagnation_pattern_serde_roundtrip() {
        let pattern = StagnationPattern::Spinning;
        let json = serde_json::to_string(&pattern).unwrap();
        assert_eq!(json, "\"spinning\"");
        let parsed: StagnationPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, pattern);
    }

    #[test]
    fn spinning_detector_not_enough_outputs() {
        let detector = SpinningDetector::new(Box::new(ExactHash), 0.9, 2);
        assert!(detector.detect(&["only one"]).is_none());
    }

    #[test]
    fn spinning_detector_exact_repeats() {
        let detector = SpinningDetector::new(Box::new(ExactHash), 0.9, 2);
        let outputs = ["same", "same", "same"];
        let result = detector.detect(&outputs.iter().copied().collect::<Vec<_>>());
        assert!(result.is_some());
        let det = result.unwrap();
        assert_eq!(det.pattern, StagnationPattern::Spinning);
        assert!((det.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn spinning_detector_no_repeat() {
        let detector = SpinningDetector::new(Box::new(ExactHash), 0.9, 2);
        let outputs = ["a", "b", "c"];
        assert!(detector.detect(&outputs).is_none());
    }

    #[test]
    fn spinning_detector_interrupted_repeat() {
        let detector = SpinningDetector::new(Box::new(ExactHash), 0.9, 2);
        // "same", "same" is 1 consecutive pair, then "diff" breaks, "same", "same" is 1 pair
        let outputs = ["same", "same", "diff", "same", "same"];
        assert!(detector.detect(&outputs).is_none());
    }

    #[test]
    fn oscillation_detector_not_enough_outputs() {
        let detector = OscillationDetector::new(Box::new(ExactHash), 0.9, 2);
        assert!(detector.detect(&["a", "b", "c"]).is_none());
    }

    #[test]
    fn oscillation_detector_abab_pattern() {
        let detector = OscillationDetector::new(Box::new(ExactHash), 0.9, 2);
        // A, B, A, B — outputs[2]~outputs[0] and outputs[3]~outputs[1]
        let outputs = ["fix A", "fix B", "fix A", "fix B"];
        let result = detector.detect(&outputs);
        assert!(result.is_some());
        let det = result.unwrap();
        assert_eq!(det.pattern, StagnationPattern::Oscillation);
    }

    #[test]
    fn oscillation_detector_no_oscillation() {
        let detector = OscillationDetector::new(Box::new(ExactHash), 0.9, 2);
        let outputs = ["a", "b", "c", "d"];
        assert!(detector.detect(&outputs).is_none());
    }

    #[test]
    fn composite_stagnation_detector_picks_highest_confidence() {
        let spinning = SpinningDetector::new(Box::new(ExactHash), 0.9, 2);
        let oscillation = OscillationDetector::new(Box::new(ExactHash), 0.9, 2);
        let detector = StagnationDetector::new(vec![Box::new(spinning), Box::new(oscillation)]);

        // 3 identical outputs — spinning should fire
        let outputs = ["same", "same", "same"];
        let result = detector.detect(&outputs);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern, StagnationPattern::Spinning);
    }

    #[test]
    fn composite_stagnation_detector_empty() {
        let detector = StagnationDetector::new(vec![]);
        assert!(detector.detect(&["a", "b"]).is_none());
    }

    #[test]
    fn stagnation_detection_serde_roundtrip() {
        let det = StagnationDetection {
            pattern: StagnationPattern::NoDrift,
            confidence: 0.85,
            reason: "no improvement".to_string(),
        };
        let json = serde_json::to_string(&det).unwrap();
        let parsed: StagnationDetection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pattern, det.pattern);
        assert!((parsed.confidence - det.confidence).abs() < f64::EPSILON);
        assert_eq!(parsed.reason, det.reason);
    }
}
