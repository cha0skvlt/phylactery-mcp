//! Write-side dedup: the circuit must not grow by cloning itself.
//!
//! Two decisions are made here and only here:
//! 1. a twin is rejected when cosine is high enough, or when cosine plus lexical overlap agree;
//! 2. a numeric disagreement cancels dedup entirely — "8848" and "9200" are different claims.
//!
//! The second rule protects corrections, the most valuable class of records.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::math;

fn numbers_regex() -> &'static Regex {
    static NUMBERS: OnceLock<Regex> = OnceLock::new();
    NUMBERS.get_or_init(|| Regex::new(r"\d+(?:[.,]\d+)?").expect("numbers regex"))
}

fn tokens_regex() -> &'static Regex {
    static TOKENS: OnceLock<Regex> = OnceLock::new();
    TOKENS.get_or_init(|| Regex::new(r"\w{3,}").expect("tokens regex"))
}

/// Every number in the text, normalized (`1,5` and `1.5` both become `1.5`).
pub fn numbers(text: &str) -> BTreeSet<String> {
    numbers_regex()
        .find_iter(text)
        .map(|hit| hit.as_str().replace(',', "."))
        .collect()
}

/// A numeric disagreement exists only when both sides carry numbers and the sets differ.
pub fn numeric_conflict(left: &str, right: &str) -> bool {
    let left_numbers = numbers(left);
    let right_numbers = numbers(right);
    !left_numbers.is_empty() && !right_numbers.is_empty() && left_numbers != right_numbers
}

/// Lowercased word tokens of three or more characters (`\w{3,}`, Unicode-aware).
pub fn tokens(text: &str) -> BTreeSet<String> {
    tokens_regex()
        .find_iter(text)
        .map(|hit| hit.as_str().to_lowercase())
        .collect()
}

/// `max(|a∩b| / |a|, |a∩b| / |b|)` — symmetric, tolerant of a short addition.
pub fn overlap(left: &str, right: &str) -> f64 {
    let left_tokens = tokens(left);
    let right_tokens = tokens(right);
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }
    let shared = left_tokens.intersection(&right_tokens).count() as f64;
    let left_ratio = shared / left_tokens.len() as f64;
    let right_ratio = shared / right_tokens.len() as f64;
    left_ratio.max(right_ratio)
}

/// The whole rule, in one place.
pub fn is_twin(similarity: f64, text_overlap: f64, conflict: bool) -> bool {
    if conflict {
        return false;
    }
    similarity >= math::DEDUP_COSINE
        || (similarity >= math::DEDUP_FUZZY_COSINE && text_overlap >= math::DEDUP_TEXT_OVERLAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_separators_are_normalized() {
        assert_eq!(numbers("1,5 and 1.5"), BTreeSet::from(["1.5".to_string()]));
    }

    #[test]
    fn numeric_disagreement_cancels_dedup() {
        assert!(numeric_conflict("Everest is 8848", "Everest is 9200"));
        assert!(!numeric_conflict("Everest is 8848", "Everest is 8848"));
        assert!(!numeric_conflict("no digits here", "Everest is 8848"));
    }

    #[test]
    fn corrections_survive_high_similarity() {
        let conflict = numeric_conflict("vault code is 4411", "vault code is 4412");
        assert!(
            !is_twin(0.999, 1.0, conflict),
            "a correction must be written"
        );
    }

    #[test]
    fn near_identical_text_is_a_twin() {
        assert!(is_twin(0.95, 0.0, false));
        assert!(is_twin(0.88, 0.9, false));
        assert!(!is_twin(0.88, 0.4, false));
        assert!(!is_twin(0.5, 1.0, false));
    }

    #[test]
    fn overlap_is_symmetric() {
        let left = "the phylactery lives in postgres";
        let right = "phylactery lives in postgres and in rust";
        let forward = overlap(left, right);
        let backward = overlap(right, left);
        assert!((forward - backward).abs() < 1e-12);
        assert!(forward >= 0.75, "expected at least 0.75, got {forward}");
    }

    #[test]
    fn empty_text_never_overlaps() {
        assert_eq!(overlap("", "anything at all"), 0.0);
    }
}
