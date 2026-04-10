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
    CompositeSimilarity, ExactHash, SimilarityJudge, SimilarityScore, TokenFingerprint,
};
