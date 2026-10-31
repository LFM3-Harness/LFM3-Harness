//! Advisory grounding for numbers written into a deliverable.
//!
//! A 1.7B model will produce a plausible-looking figure that no source
//! contains. This check does not decide truth and it never blocks completion:
//! it points the review turn at numbers the model could not have learned from
//! what it actually read, so the model can verify or remove them. Human review
//! and the measured deliverable proof remain the only completion authorities.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

/// The result of comparing a written output with the evidence the model saw.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grounding {
    /// Distinct multi-digit numbers found in the output.
    pub checked: usize,
    /// Those that never appeared in the goal, plan, or a successful observation.
    pub unsupported: Vec<String>,
}

/// Compares the numbers in `output` with every number in `sources`.
#[must_use]
pub fn check(output: &str, sources: &[&str]) -> Grounding {
    let evidence = sources
        .iter()
        .flat_map(|source| numbers(source))
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut checked = 0;
    let mut unsupported = Vec::new();
    for number in numbers(output) {
        if !seen.insert(number.clone()) {
            continue;
        }
        checked += 1;
        if !evidence.contains(&number) {
            unsupported.push(number);
        }
    }
    Grounding {
        checked,
        unsupported,
    }
}

/// Distinct numeric tokens, normalized for comparison.
///
/// Single digits are skipped: they are usually prose counts ("3 steps") rather
/// than facts a reader would check. Decimals, versions, percentages, and
/// thousands-separated values are kept.
fn numbers(text: &str) -> Vec<String> {
    number_re()
        .find_iter(text)
        .filter_map(|matched| {
            let normalized = matched.as_str().replace(',', "");
            let digits = normalized.chars().filter(char::is_ascii_digit).count();
            (digits >= 2 || normalized.contains('.')).then_some(normalized)
        })
        .collect()
}

fn number_re() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\d[\d,]*(?:\.\d+)?").expect("the numeric token pattern is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_the_model_never_saw_is_flagged() {
        let grounding = check(
            "Revenue was 1,200 units in 2019.",
            &["Revenue was 1200 units last year."],
        );
        assert_eq!(grounding.checked, 2);
        assert_eq!(grounding.unsupported, vec!["2019".to_owned()]);
    }

    #[test]
    fn grounded_numbers_and_single_digit_counts_are_not_flagged() {
        let grounding = check(
            "The report has 3 sections and 24 crates at version 0.8.",
            &["The workspace holds 24 crates.", "Built at 0.8."],
        );
        assert_eq!(grounding.checked, 2, "the single-digit count is skipped");
        assert!(grounding.unsupported.is_empty());
    }

    #[test]
    fn every_distinct_number_is_checked_once() {
        let grounding = check("42 and 42 and 43", &["42"]);
        assert_eq!(grounding.checked, 2);
        assert_eq!(grounding.unsupported, vec!["43".to_owned()]);
    }
}
