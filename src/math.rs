//! Ranking mathematics of the circuit.
//!
//! Constants are carried over from the frontier-derived reference implementation
//! (`persistent-agent`) and are not to be tuned by feel. The formulas here are the
//! executable specification; SQL supplies cosine, this module supplies trust.
//!
//! Write-side constants (dedup, diary echo) are declared here so that ranking and writing share
//! one source of truth; they are consumed by the M1 write path.
#![allow(dead_code)]

use chrono::{DateTime, Utc};

/// HNSW shortlist size before reranking.
pub const ANN_CANDIDATES: i64 = 50;
/// Candidates below this query similarity are dropped.
pub const SIMILARITY_FLOOR: f64 = 0.3;
/// Multiplier for `memory_type = unverified` (and for an undeclared type).
pub const UNVERIFIED_FACTOR: f64 = 0.35;
/// Age decay half-life in days; `<= 0` disables decay.
pub const HALF_LIFE_DAYS: f64 = 180.0;
/// Assumed importance when the column is null.
pub const DEFAULT_IMPORTANCE: f64 = 0.5;
/// Age factor for an undeclared `created_at`.
pub const UNDECLARED_AGE_FACTOR: f64 = 0.5;

pub const TOP_K_MEMORIES: usize = 3;
pub const TOP_K_DIARY: usize = 4;
pub const TOP_K_LESSONS: usize = 2;

/// Write-side dedup constants (M1).
pub const DEDUP_NEIGHBORS: i64 = 5;
pub const DEDUP_COSINE: f64 = 0.92;
pub const DEDUP_FUZZY_COSINE: f64 = 0.85;
pub const DEDUP_TEXT_OVERLAP: f64 = 0.8;

/// Diary echo gate (M1).
pub const DIARY_ECHO_ROWS: i64 = 20;
pub const DIARY_ECHO_THRESHOLD: f64 = 0.92;

/// Importance values above 1.0 are stored on a 0..10 scale (a `9` is a standing order).
/// Ranking works on 0..1, so such rows are normalized instead of dominating by a factor of ten.
pub fn normalize_importance(raw: Option<f64>) -> f64 {
    match raw {
        None => DEFAULT_IMPORTANCE,
        Some(value) if !value.is_finite() => DEFAULT_IMPORTANCE,
        Some(value) if value > 1.0 => (value / 10.0).clamp(0.0, 1.0),
        Some(value) => value.clamp(0.0, 1.0),
    }
}

/// Memory types the circuit knows. An unknown type is ranked as `unverified` (fail-closed).
pub const KNOWN_MEMORY_TYPES: [&str; 6] = [
    "semantic",
    "identity",
    "unverified",
    "lesson",
    "episodic",
    "procedure",
];

/// Canonical memory type: trimmed, lowercased, unknown values degrade to `unverified`.
pub fn normalize_memory_type(raw: &str) -> String {
    let kind = raw.trim().to_ascii_lowercase();
    if KNOWN_MEMORY_TYPES.contains(&kind.as_str()) {
        kind
    } else {
        "unverified".to_string()
    }
}

/// Importance on the stored scale: finite, clamped to `0..=10`, `0.5` when unusable.
pub fn sanitize_importance(raw: f64) -> f64 {
    if !raw.is_finite() {
        return DEFAULT_IMPORTANCE;
    }
    raw.clamp(0.0, 10.0)
}

/// `0.5 ** (age_days / half_life)`.
pub fn age_factor(age_days: f64) -> f64 {
    if !age_days.is_finite() || age_days <= 0.0 {
        return 1.0;
    }
    if HALF_LIFE_DAYS <= 0.0 {
        return 1.0;
    }
    0.5_f64.powf(age_days / HALF_LIFE_DAYS)
}

/// Trust multiplier for a hit: unverified status and age, with identity exempt from aging.
pub fn distrust(memory_type: &str, created_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f64 {
    let kind = memory_type.trim().to_ascii_lowercase();
    let mut factor = 1.0;
    if kind.is_empty() || kind == "unverified" {
        factor *= UNVERIFIED_FACTOR;
    }
    if kind != "identity" {
        match created_at {
            Some(stamp) => {
                let age_days = (now - stamp).num_seconds().max(0) as f64 / 86_400.0;
                factor *= age_factor(age_days);
            }
            None => factor *= UNDECLARED_AGE_FACTOR,
        }
    }
    factor
}

/// Final ordering key: similarity, weighted importance, then trust.
pub fn retrieval_score(
    similarity: f64,
    importance: Option<f64>,
    memory_type: &str,
    created_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> f64 {
    similarity * normalize_importance(importance) * distrust(memory_type, created_at, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn age_factor_halves_at_one_half_life() {
        let factor = age_factor(HALF_LIFE_DAYS);
        assert!((factor - 0.5).abs() < 1e-9, "got {factor}");
    }

    #[test]
    fn age_factor_is_monotonic() {
        let mut previous = 1.0;
        for days in [0.0, 1.0, 90.0, 180.0, 360.0, 720.0] {
            let factor = age_factor(days);
            assert!(factor <= previous + 1e-12, "not monotonic at {days}");
            previous = factor;
        }
    }

    #[test]
    fn identity_never_ages() {
        let now = at(2026, 11, 13);
        let old = at(2020, 1, 1);
        assert!((distrust("identity", Some(old), now) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn unverified_hits_are_downranked() {
        let now = at(2026, 11, 13);
        let fresh = distrust("semantic", Some(now), now);
        let unverified = distrust("unverified", Some(now), now);
        assert!((fresh - 1.0).abs() < 1e-12);
        assert!((unverified - UNVERIFIED_FACTOR).abs() < 1e-12);
    }

    #[test]
    fn undeclared_age_is_not_fresh() {
        let now = at(2026, 11, 13);
        assert!((distrust("semantic", None, now) - UNDECLARED_AGE_FACTOR).abs() < 1e-12);
    }

    #[test]
    fn importance_scale_is_normalized() {
        assert!((normalize_importance(Some(9.0)) - 0.9).abs() < 1e-12);
        assert!((normalize_importance(Some(0.7)) - 0.7).abs() < 1e-12);
        assert!((normalize_importance(None) - DEFAULT_IMPORTANCE).abs() < 1e-12);
        assert!((normalize_importance(Some(f64::NAN)) - DEFAULT_IMPORTANCE).abs() < 1e-12);
    }

    #[test]
    fn memory_types_fail_closed() {
        assert_eq!(normalize_memory_type(" Semantic "), "semantic");
        assert_eq!(normalize_memory_type("identity"), "identity");
        assert_eq!(normalize_memory_type("garbage"), "unverified");
        assert_eq!(normalize_memory_type(""), "unverified");
    }

    #[test]
    fn importance_is_bounded_and_finite() {
        assert_eq!(sanitize_importance(42.0), 10.0);
        assert_eq!(sanitize_importance(-3.0), 0.0);
        assert_eq!(sanitize_importance(f64::NAN), DEFAULT_IMPORTANCE);
        assert_eq!(sanitize_importance(0.7), 0.7);
    }

    #[test]
    fn standing_order_outranks_weak_fact() {
        let now = at(2026, 11, 13);
        let order = retrieval_score(0.8, Some(10.0), "identity", Some(now), now);
        let fact = retrieval_score(0.8, Some(0.2), "semantic", Some(now), now);
        assert!(order > fact);
    }
}
