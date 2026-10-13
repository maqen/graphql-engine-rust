//! Expected failures, per-area summaries and report files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::run::CaseResult;

/// Case ids known to fail on our engine (`conformance/expected-failures.txt`).
pub fn read_expected_failures(path: &Path) -> Result<BTreeSet<String>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn write_expected_failures(path: &Path, failing: &BTreeSet<String>) -> Result<()> {
    let mut out = String::from(
        "# Conformance cases our engine does not pass yet, one case id per line.\n\
         # A listed case that starts passing fails CI until it is removed from this list;\n\
         # a case missing from this list that fails is a regression.\n\
         # Regenerate with: hge-conformance ... --write-expected-failures\n",
    );
    for id in failing {
        out.push_str(id);
        out.push('\n');
    }
    std::fs::write(path, out).with_context(|| format!("{}", path.display()))
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct AreaStats {
    pub cases: usize,
    pub passed: usize,
    /// Failing and listed as expected failures.
    pub expected_failures: usize,
    /// Failing and not listed.
    pub regressions: usize,
    /// Passing but listed as an expected failure.
    pub newly_passing: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub target: String,
    pub total: AreaStats,
    pub areas: BTreeMap<String, AreaStats>,
    pub cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize)]
pub struct CaseReport {
    #[serde(flatten)]
    pub result: CaseResult,
    pub expected_failure: bool,
}

impl Report {
    /// `expected`: the expected-failure list, or `None` when every case must pass.
    pub fn new(
        target: &str,
        results: Vec<CaseResult>,
        expected: Option<&BTreeSet<String>>,
    ) -> Self {
        let mut areas: BTreeMap<String, AreaStats> = BTreeMap::new();
        let mut total = AreaStats::default();
        let mut cases = vec![];
        for r in results {
            let listed = expected.is_some_and(|e| e.contains(&r.id));
            for s in [areas.entry(r.area.clone()).or_default(), &mut total] {
                s.cases += 1;
                match (r.passed, listed) {
                    (true, false) => s.passed += 1,
                    (true, true) => {
                        s.passed += 1;
                        s.newly_passing += 1;
                    }
                    (false, true) => s.expected_failures += 1,
                    (false, false) => s.regressions += 1,
                }
            }
            cases.push(CaseReport {
                result: r,
                expected_failure: listed,
            });
        }
        cases.sort_by(|a, b| a.result.id.cmp(&b.result.id));
        Report {
            target: target.to_owned(),
            total,
            areas,
            cases,
        }
    }

    pub fn ok(&self) -> bool {
        self.total.regressions == 0 && self.total.newly_passing == 0
    }

    pub fn failing_ids(&self) -> BTreeSet<String> {
        self.cases
            .iter()
            .filter(|c| !c.result.passed)
            .map(|c| c.result.id.clone())
            .collect()
    }

    pub fn markdown(&self) -> String {
        let mut out = format!(
            "### Conformance: {}\n\n| Area | Cases | Passing | Pass rate | Expected failures | Regressions | Newly passing |\n|---|---:|---:|---:|---:|---:|---:|\n",
            self.target
        );
        let row = |name: &str, s: &AreaStats| {
            format!(
                "| {name} | {} | {} | {} | {} | {} | {} |\n",
                s.cases,
                s.passed,
                pct(s.passed, s.cases),
                s.expected_failures,
                s.regressions,
                s.newly_passing
            )
        };
        for (area, s) in &self.areas {
            out += &row(area, s);
        }
        out += &row("**total**", &self.total);
        out
    }
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "-".into()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / d as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, passed: bool) -> CaseResult {
        CaseResult {
            id: id.into(),
            area: id.split('/').next().unwrap().into(),
            passed,
            detail: None,
        }
    }

    #[test]
    fn classifies_cases() {
        let expected: BTreeSet<String> =
            ["q/s/listed_fail".into(), "q/s/listed_pass".into()].into();
        let report = Report::new(
            "rust",
            vec![
                r("q/s/pass", true),
                r("q/s/listed_fail", false),
                r("q/s/listed_pass", true),
                r("m/s/regression", false),
            ],
            Some(&expected),
        );
        assert_eq!(report.total.cases, 4);
        assert_eq!(report.total.passed, 2);
        assert_eq!(report.total.expected_failures, 1);
        assert_eq!(report.total.regressions, 1);
        assert_eq!(report.total.newly_passing, 1);
        assert_eq!(report.areas["m"].regressions, 1);
        assert!(!report.ok());
        assert!(
            report
                .markdown()
                .contains("| q | 3 | 2 | 66.7% | 1 | 0 | 1 |")
        );
    }

    #[test]
    fn without_a_list_every_failure_counts() {
        let report = Report::new("hasura", vec![r("q/s/a", false)], None);
        assert_eq!(report.total.regressions, 1);
        assert!(!report.ok());
    }

    #[test]
    fn reads_list_with_comments() {
        let dir = std::env::temp_dir().join(format!("hgeconf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("xf.txt");
        std::fs::write(&p, "# header\na/b/c  # reason\n\nd/e/f\n").unwrap();
        let set = read_expected_failures(&p).unwrap();
        assert_eq!(set, ["a/b/c".to_owned(), "d/e/f".to_owned()].into());
        write_expected_failures(&p, &set).unwrap();
        assert_eq!(read_expected_failures(&p).unwrap(), set);
    }
}
