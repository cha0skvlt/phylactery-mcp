//! Golden-set evaluation for triple extraction.
//!
//! The set lives in `tests/golden/triples.tsv`: one sentence per line, expected triples after a
//! tab. Precision, recall and F1 are computed against the rule extractor, which is the only
//! extractor: the same sentences must produce the same triples on every run.

use std::collections::BTreeSet;

use phylactery_mcp::kg::{self, KnownEntity, RawTriple};

#[derive(Debug, Default)]
struct Score {
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
}

impl Score {
    fn precision(&self) -> f64 {
        let denominator = self.true_positive + self.false_positive;
        if denominator == 0 {
            1.0
        } else {
            self.true_positive as f64 / denominator as f64
        }
    }

    fn recall(&self) -> f64 {
        let denominator = self.true_positive + self.false_negative;
        if denominator == 0 {
            1.0
        } else {
            self.true_positive as f64 / denominator as f64
        }
    }

    fn f1(&self) -> f64 {
        let precision = self.precision();
        let recall = self.recall();
        if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        }
    }
}

fn key(triple: &RawTriple) -> String {
    format!(
        "{}|{}|{}",
        stem(&kg::normalize(&triple.subject)),
        triple.predicate,
        stem(&kg::normalize(&triple.object))
    )
}

/// Crude stemmer: enough to compare Russian case endings without a morphology dictionary.
fn stem(word: &str) -> String {
    const ENDINGS: &[&str] = &[
        "ами", "ями", "ов", "ев", "ах", "ях", "ой", "ей", "ый", "ий", "ая", "ое", "ые", "ам", "ям",
        "а", "я", "у", "ю", "ы", "и", "е", "о", "ь",
    ];
    let mut out = String::new();
    for token in word.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut stemmed = token.to_string();
        for ending in ENDINGS {
            if stemmed.len() > ending.len() + 2 && stemmed.ends_with(ending) {
                stemmed.truncate(stemmed.len() - ending.len());
                break;
            }
        }
        out.push_str(&stemmed);
    }
    out
}

fn expected_key(raw: &str) -> String {
    let mut parts = raw.split('|');
    let subject = parts.next().unwrap_or("");
    let predicate = parts.next().unwrap_or("");
    let object = parts.next().unwrap_or("");
    format!(
        "{}|{}|{}",
        stem(&kg::normalize(subject)),
        predicate.trim(),
        stem(&kg::normalize(object))
    )
}

fn load() -> Vec<(String, BTreeSet<String>)> {
    let raw = std::fs::read_to_string("tests/golden/triples.tsv").expect("golden set");
    raw.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            let sentence = parts.next().unwrap_or("").trim().to_string();
            let expected = parts
                .next()
                .unwrap_or("")
                .split(';')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(expected_key)
                .collect();
            (sentence, expected)
        })
        .collect()
}

fn score_rules(cases: &[(String, BTreeSet<String>)]) -> Score {
    let known: Vec<KnownEntity> = Vec::new();
    let mut score = Score::default();
    for (sentence, expected) in cases {
        let actual: BTreeSet<String> = kg::extract(sentence, &known).iter().map(key).collect();
        if std::env::var("GOLDEN_DEBUG").as_deref() == Ok("1") {
            println!("case: {sentence} -> {actual:?} expected {expected:?}");
        }
        for item in &actual {
            if expected.contains(item) {
                score.true_positive += 1;
            } else {
                score.false_positive += 1;
                println!("FP rules: {sentence} -> {item}");
            }
        }
        for item in expected {
            if !actual.contains(item) {
                score.false_negative += 1;
                println!("FN rules: {sentence} -> {item}");
            }
        }
    }
    score
}

#[test]
fn rules_meet_the_quality_bar() {
    let cases = load();
    if std::env::var("GOLDEN_DEBUG").as_deref() == Ok("1") {
        let probe = kg::extract("The MCP server uses Rust", &[]);
        println!("direct probe: {probe:?}");
        let probe2 = kg::extract("MCP server uses Rust", &[]);
        println!("probe without article: {probe2:?}");
    }
    assert!(cases.len() >= 30, "golden set is too small");
    let score = score_rules(&cases);
    println!(
        "rules: precision {:.2} recall {:.2} f1 {:.2} (tp {} fp {} fn {})",
        score.precision(),
        score.recall(),
        score.f1(),
        score.true_positive,
        score.false_positive,
        score.false_negative
    );
    assert!(
        score.precision() >= 0.75,
        "precision {:.2} below the bar",
        score.precision()
    );
    assert!(
        score.recall() >= 0.55,
        "recall {:.2} below the bar",
        score.recall()
    );
}
