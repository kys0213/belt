//! Stagnation detection and lateral thinking module.
//!
//! Detects when an agent is stuck in a loop (spinning, oscillating, etc.)
//! and suggests alternative approaches via persona-based lateral analysis.
//!
//! # Architecture
//!
//! ```text
//! Agent outputs
//!   │
//!   ▼
//! SimilarityJudge ──► PatternDetector ──► StagnationDetector
//!                                               │
//!                                               ▼
//!                                        LateralAnalyzer ──► LateralPlan
//! ```
//!
//! - [`similarity`]: Judges how similar two outputs are (ExactHash, TokenFingerprint).
//! - [`pattern`]: Detects stagnation patterns (Spinning, Oscillation, NoDrift, DiminishingReturns).
//! - [`lateral`]: Selects a persona-based strategy to break out of stagnation.

pub mod lateral;
pub mod pattern;
pub mod similarity;

pub use lateral::{AnalyzeParams, LateralAnalyzer, LateralPlan, Persona};
pub use pattern::{
    OscillationDetector, PatternDetector, SpinningDetector, StagnationDetection,
    StagnationDetector, StagnationPattern,
};
pub use similarity::{
    CompositeSimilarity, ExactHash, NcdJudge, SimilarityJudge, SimilarityScore, TokenFingerprint,
};

use serde::{Deserialize, Serialize};

/// Lateral thinking analysis configuration.
///
/// Controls whether lateral plan generation runs when stagnation is detected.
/// When `enabled` is `false`, stagnation detection still runs but lateral plan
/// generation is skipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LateralConfig {
    /// Whether lateral thinking analysis is enabled. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for LateralConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Stagnation detection configuration.
///
/// Controls whether stagnation analysis runs and its parameters.
/// When `enabled` is `false`, the daemon skips stagnation detection entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StagnationConfig {
    /// Whether stagnation detection is enabled. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Lateral thinking analysis sub-configuration.
    #[serde(default)]
    pub lateral: LateralConfig,
}

fn default_enabled() -> bool {
    true
}

impl Default for StagnationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lateral: LateralConfig::default(),
        }
    }
}
