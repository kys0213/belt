//! Similarity judgment for detecting repetitive agent outputs.
//!
//! Provides [`SimilarityJudge`] trait and implementations:
//! - [`ExactHash`]: byte-exact equality via hash comparison.
//! - [`TokenFingerprint`]: normalized token-level similarity using Jaccard index.
//! - [`NcdJudge`]: Normalized Compression Distance via flate2 GzEncoder.
//! - [`CompositeSimilarity`]: combines multiple judges, returning the maximum score.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;

/// A similarity score in `[0.0, 1.0]` where 1.0 means identical.
pub type SimilarityScore = f64;

/// Judge how similar two text artifacts are.
pub trait SimilarityJudge: Send + Sync {
    /// Return a similarity score between `a` and `b`.
    ///
    /// The score MUST be in `[0.0, 1.0]`.
    fn score(&self, a: &str, b: &str) -> SimilarityScore;

    /// Human-readable name of this judge (for diagnostics).
    fn name(&self) -> &str;
}

/// Exact byte-level equality via hashing.
///
/// Returns 1.0 if inputs hash identically, 0.0 otherwise.
#[derive(Debug, Default)]
pub struct ExactHash;

impl ExactHash {
    fn hash_str(s: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

impl SimilarityJudge for ExactHash {
    fn score(&self, a: &str, b: &str) -> SimilarityScore {
        if Self::hash_str(a) == Self::hash_str(b) {
            1.0
        } else {
            0.0
        }
    }

    fn name(&self) -> &str {
        "exact_hash"
    }
}

/// Normalized token-level similarity using the Jaccard index.
///
/// Tokenizes by splitting on whitespace, lowercasing, and stripping
/// non-alphanumeric characters. The score is `|intersection| / |union|`.
#[derive(Debug, Default)]
pub struct TokenFingerprint;

impl TokenFingerprint {
    fn tokenize(s: &str) -> HashSet<String> {
        s.split_whitespace()
            .map(|w| {
                w.to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect::<String>()
            })
            .filter(|t| !t.is_empty())
            .collect()
    }
}

impl SimilarityJudge for TokenFingerprint {
    fn score(&self, a: &str, b: &str) -> SimilarityScore {
        let set_a = Self::tokenize(a);
        let set_b = Self::tokenize(b);

        if set_a.is_empty() && set_b.is_empty() {
            return 1.0;
        }

        let intersection = set_a.intersection(&set_b).count() as f64;
        let union = set_a.union(&set_b).count() as f64;

        if union == 0.0 {
            return 1.0;
        }

        intersection / union
    }

    fn name(&self) -> &str {
        "token_fingerprint"
    }
}

/// Normalized Compression Distance (NCD) similarity judge.
///
/// Uses flate2 GzEncoder to compute:
/// `NCD(X,Y) = (C(XY) - min(C(X), C(Y))) / max(C(X), C(Y))`
///
/// The similarity score is `1.0 - NCD`, so 1.0 means identical and 0.0 means
/// completely different. Inputs whose NCD exceeds `threshold` are considered
/// dissimilar (the score will be low).
#[derive(Debug)]
pub struct NcdJudge {
    /// NCD threshold — default 0.3 means similarity >= 0.7 is "similar".
    threshold: f64,
}

impl NcdJudge {
    /// Create a new `NcdJudge` with the given NCD threshold.
    ///
    /// The threshold controls what NCD value is considered "similar".
    /// A lower threshold means stricter matching. Default is 0.3.
    pub fn new(threshold: f64) -> Self {
        Self { threshold }
    }

    /// Return the configured NCD threshold.
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Compress the input bytes and return the compressed size.
    fn compressed_size(data: &[u8]) -> usize {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).expect("in-memory write");
        encoder.finish().expect("in-memory finish").len()
    }
}

impl Default for NcdJudge {
    fn default() -> Self {
        Self { threshold: 0.3 }
    }
}

impl SimilarityJudge for NcdJudge {
    fn score(&self, a: &str, b: &str) -> SimilarityScore {
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }

        let ca = Self::compressed_size(a.as_bytes()) as f64;
        let cb = Self::compressed_size(b.as_bytes()) as f64;
        let combined = format!("{a}{b}");
        let cab = Self::compressed_size(combined.as_bytes()) as f64;

        let max_c = ca.max(cb);
        if max_c == 0.0 {
            return 1.0;
        }

        let ncd = (cab - ca.min(cb)) / max_c;
        // Clamp to [0.0, 1.0] — compression artifacts can cause slight overshoot.
        (1.0 - ncd).clamp(0.0, 1.0)
    }

    fn name(&self) -> &str {
        "ncd"
    }
}

/// Combines multiple [`SimilarityJudge`]s, returning the maximum score.
pub struct CompositeSimilarity {
    judges: Vec<Box<dyn SimilarityJudge>>,
}

impl CompositeSimilarity {
    /// Create a new composite from the given judges.
    pub fn new(judges: Vec<Box<dyn SimilarityJudge>>) -> Self {
        Self { judges }
    }
}

impl Default for CompositeSimilarity {
    /// Default preset with ExactHash, TokenFingerprint, and NcdJudge.
    fn default() -> Self {
        Self {
            judges: vec![
                Box::new(ExactHash),
                Box::new(TokenFingerprint),
                Box::new(NcdJudge::default()),
            ],
        }
    }
}

impl SimilarityJudge for CompositeSimilarity {
    fn score(&self, a: &str, b: &str) -> SimilarityScore {
        self.judges
            .iter()
            .map(|j| j.score(a, b))
            .fold(0.0_f64, f64::max)
    }

    fn name(&self) -> &str {
        "composite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_hash_identical() {
        let judge = ExactHash;
        assert!((judge.score("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn exact_hash_different() {
        let judge = ExactHash;
        assert!((judge.score("hello", "world")).abs() < f64::EPSILON);
    }

    #[test]
    fn exact_hash_name() {
        assert_eq!(ExactHash.name(), "exact_hash");
    }

    #[test]
    fn token_fingerprint_identical() {
        let judge = TokenFingerprint;
        let score = judge.score("the quick brown fox", "the quick brown fox");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_fingerprint_partial_overlap() {
        let judge = TokenFingerprint;
        // {"the", "quick", "brown", "fox"} vs {"the", "slow", "brown", "dog"}
        // intersection = {"the", "brown"} = 2
        // union = {"the", "quick", "brown", "fox", "slow", "dog"} = 6
        let score = judge.score("the quick brown fox", "the slow brown dog");
        assert!((score - 2.0 / 6.0).abs() < 0.001);
    }

    #[test]
    fn token_fingerprint_empty_both() {
        let judge = TokenFingerprint;
        assert!((judge.score("", "") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_fingerprint_empty_one() {
        let judge = TokenFingerprint;
        assert!((judge.score("hello", "")).abs() < f64::EPSILON);
    }

    #[test]
    fn token_fingerprint_case_insensitive() {
        let judge = TokenFingerprint;
        let score = judge.score("Hello World", "hello world");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_fingerprint_strips_punctuation() {
        let judge = TokenFingerprint;
        let score = judge.score("hello, world!", "hello world");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_fingerprint_name() {
        assert_eq!(TokenFingerprint.name(), "token_fingerprint");
    }

    #[test]
    fn ncd_identical_text() {
        let judge = NcdJudge::default();
        // Use a longer string to reduce gz header overhead effect on NCD.
        let text = "the quick brown fox jumps over the lazy dog repeatedly in a loop";
        let score = judge.score(text, text);
        assert!(
            score > 0.90,
            "identical text should score near 1.0, got {score}"
        );
    }

    #[test]
    fn ncd_different_text() {
        let judge = NcdJudge::default();
        let score = judge.score(
            "the quick brown fox jumps over the lazy dog",
            "1234567890 abcdefghij !@#$%^&*() zyxwvutsrq",
        );
        assert!(
            score < 0.7,
            "very different text should score low, got {score}"
        );
    }

    #[test]
    fn ncd_empty_both() {
        let judge = NcdJudge::default();
        assert!((judge.score("", "") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ncd_threshold_default() {
        let judge = NcdJudge::default();
        assert!((judge.threshold() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn ncd_custom_threshold() {
        let judge = NcdJudge::new(0.5);
        assert!((judge.threshold() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn ncd_score_in_range() {
        let judge = NcdJudge::default();
        let score = judge.score("abc", "xyz");
        assert!(
            (0.0..=1.0).contains(&score),
            "score must be in [0,1], got {score}"
        );
    }

    #[test]
    fn ncd_name() {
        assert_eq!(NcdJudge::default().name(), "ncd");
    }

    #[test]
    fn composite_default_includes_ncd() {
        let composite = CompositeSimilarity::default();
        // Default preset should have 3 judges
        let score = composite.score("hello world", "hello world");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_returns_max() {
        let composite =
            CompositeSimilarity::new(vec![Box::new(ExactHash), Box::new(TokenFingerprint)]);
        // Different strings but overlapping tokens — ExactHash=0.0, TokenFingerprint>0.0
        let score = composite.score("hello world", "hello earth");
        assert!(score > 0.0);
        // Exact match — both return 1.0
        let score = composite.score("hello", "hello");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_empty_judges() {
        let composite = CompositeSimilarity::new(vec![]);
        assert!((composite.score("a", "b")).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_name() {
        let composite = CompositeSimilarity::new(vec![]);
        assert_eq!(composite.name(), "composite");
    }
}
