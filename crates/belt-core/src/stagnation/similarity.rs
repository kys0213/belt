//! Similarity judgment for detecting repetitive agent outputs.
//!
//! Provides [`SimilarityJudge`] trait and two implementations:
//! - [`ExactHash`]: byte-exact equality via hash comparison.
//! - [`TokenFingerprint`]: normalized token-level similarity using Jaccard index.
//! - [`CompositeSimilarity`]: combines multiple judges, returning the maximum score.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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
