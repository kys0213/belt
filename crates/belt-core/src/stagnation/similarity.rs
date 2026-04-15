//! Similarity judgment for detecting repetitive agent outputs.
//!
//! Provides [`SimilarityJudge`] trait and implementations:
//! - [`ExactHash`]: byte-exact equality via hash comparison.
//! - [`TokenFingerprint`]: normalized token-level similarity using Jaccard index.
//! - [`NcdJudge`]: Normalized Compression Distance via flate2 GzEncoder.
//! - [`CompositeSimilarity`]: combines multiple judges using weighted average.

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

/// Combines multiple [`SimilarityJudge`]s using weighted average.
///
/// Each judge is paired with a weight. The composite score is computed as:
/// `sum(score_i * weight_i) / sum(weight_i)`, clamped to `[0.0, 1.0]`.
/// Returns 0.0 when there are no judges (total weight is zero).
pub struct CompositeSimilarity {
    judges: Vec<(Box<dyn SimilarityJudge>, f64)>,
}

impl CompositeSimilarity {
    /// Create a new composite from the given `(judge, weight)` pairs.
    pub fn new(judges: Vec<(Box<dyn SimilarityJudge>, f64)>) -> Self {
        Self { judges }
    }
}

impl Default for CompositeSimilarity {
    /// Default preset per spec R-016: ExactHash(0.5), TokenFingerprint(0.3), NcdJudge(0.2).
    fn default() -> Self {
        Self {
            judges: vec![
                (Box::new(ExactHash), 0.5),
                (Box::new(TokenFingerprint), 0.3),
                (Box::new(NcdJudge::default()), 0.2),
            ],
        }
    }
}

impl SimilarityJudge for CompositeSimilarity {
    fn score(&self, a: &str, b: &str) -> SimilarityScore {
        let (weighted_sum, total_weight) = self
            .judges
            .iter()
            .map(|(j, w)| (j.score(a, b) * w, w))
            .fold((0.0_f64, 0.0_f64), |(s, tw), (v, w)| (s + v, tw + w));

        if total_weight == 0.0 {
            return 0.0;
        }

        (weighted_sum / total_weight).clamp(0.0, 1.0)
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
        // Default preset: ExactHash(0.5) + TokenFingerprint(0.3) + NcdJudge(0.2)
        // For identical text, ExactHash=1.0, TokenFingerprint=1.0, NCD~=1.0 (short strings
        // have slight gz header overhead), so the weighted average is near 1.0.
        let score = composite.score("hello world", "hello world");
        assert!(
            score > 0.95,
            "identical text should score near 1.0, got {score}"
        );
    }

    #[test]
    fn composite_weighted_average() {
        // ExactHash returns 0.0 for different strings, TokenFingerprint returns >0.0
        let composite = CompositeSimilarity::new(vec![
            (Box::new(ExactHash), 0.5),
            (Box::new(TokenFingerprint), 0.5),
        ]);
        // "hello world" vs "hello earth": ExactHash=0.0, TokenFingerprint=1/3
        // weighted avg = (0.0*0.5 + (1.0/3.0)*0.5) / (0.5+0.5) = 1/6
        let score = composite.score("hello world", "hello earth");
        let token_score = TokenFingerprint.score("hello world", "hello earth");
        let expected = (0.0 * 0.5 + token_score * 0.5) / 1.0;
        assert!(
            (score - expected).abs() < 0.001,
            "expected {expected}, got {score}"
        );
    }

    #[test]
    fn composite_weighted_average_exact_match() {
        let composite = CompositeSimilarity::new(vec![
            (Box::new(ExactHash), 0.5),
            (Box::new(TokenFingerprint), 0.5),
        ]);
        // Exact match -- both return 1.0, weighted avg = 1.0
        let score = composite.score("hello", "hello");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_respects_weights() {
        // Give all weight to ExactHash (returns 0.0 for different strings)
        let composite = CompositeSimilarity::new(vec![
            (Box::new(ExactHash), 1.0),
            (Box::new(TokenFingerprint), 0.0),
        ]);
        let score = composite.score("hello world", "hello earth");
        // ExactHash=0.0 * 1.0 + TokenFingerprint * 0.0 = 0.0, total_weight=1.0
        assert!((score).abs() < f64::EPSILON);

        // Give all weight to TokenFingerprint
        let composite = CompositeSimilarity::new(vec![
            (Box::new(ExactHash), 0.0),
            (Box::new(TokenFingerprint), 1.0),
        ]);
        let score = composite.score("hello world", "hello earth");
        let expected = TokenFingerprint.score("hello world", "hello earth");
        assert!(
            (score - expected).abs() < f64::EPSILON,
            "expected {expected}, got {score}"
        );
    }

    #[test]
    fn composite_empty_judges() {
        let composite = CompositeSimilarity::new(vec![]);
        assert!((composite.score("a", "b")).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_result_clamped() {
        // Even with unusual inputs, result stays in [0.0, 1.0]
        let composite = CompositeSimilarity::default();
        let score = composite.score("abc", "xyz");
        assert!(
            (0.0..=1.0).contains(&score),
            "score must be in [0,1], got {score}"
        );
    }

    #[test]
    fn composite_name() {
        let composite = CompositeSimilarity::new(vec![]);
        assert_eq!(composite.name(), "composite");
    }
}
