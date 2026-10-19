//! The known-failures baseline.
//!
//! A deliberately small YAML subset, so no YAML dependency is needed. Top-level
//! keys are protocol versions; each maps definition names to a short reason:
//!
//! ```yaml
//! "2025-11-25":
//!   ListToolsResult: why this definition is expected to fail
//! "2026-07-28": {}
//! ```
//!
//! Blank lines and lines starting with `#` are ignored. Reasons are plain text
//! to the end of the line (an `#` inside a reason is part of the reason).

use std::collections::BTreeMap;

use crate::check::Report;

/// Expected failures: version -> definition -> reason.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub entries: BTreeMap<String, BTreeMap<String, String>>,
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

impl Baseline {
    /// Parse the baseline text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut entries: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim_end();
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indented = line.len() != trimmed.len();
            let (key, rest) = trimmed
                .split_once(':')
                .ok_or_else(|| format!("line {}: expected `key: value`", n + 1))?;
            let key = unquote(key).to_string();
            let rest = rest.trim();
            if !indented {
                if !rest.is_empty() && rest != "{}" {
                    return Err(format!(
                        "line {}: a version key takes no value (got `{rest}`)",
                        n + 1
                    ));
                }
                entries.entry(key.clone()).or_default();
                current = Some(key);
            } else {
                let version = current
                    .clone()
                    .ok_or_else(|| format!("line {}: entry before any version key", n + 1))?;
                entries
                    .entry(version)
                    .or_default()
                    .insert(key, unquote(rest).to_string());
            }
        }
        Ok(Self { entries })
    }

    fn contains(&self, version: &str, def: &str) -> bool {
        self.entries
            .get(version)
            .is_some_and(|defs| defs.contains_key(def))
    }
}

/// A failing (version, definition) pair and how often it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefFailure {
    pub version: String,
    pub def: String,
    pub count: u64,
}

/// A report compared with a baseline.
#[derive(Debug, Default)]
pub struct Comparison {
    /// Failures not covered by the baseline. Any entry here fails the run.
    pub unbaselined: Vec<DefFailure>,
    /// Failures the baseline expects.
    pub baselined: Vec<DefFailure>,
    /// Baseline entries whose definition no longer fails, for versions present
    /// in the report. Informational: the entry can be removed.
    pub stale: Vec<(String, String)>,
}

/// Compare `report` with `baseline`.
pub fn compare(report: &Report, baseline: &Baseline) -> Comparison {
    let mut out = Comparison::default();
    let counts = report.failures_by_def();
    for ((version, def), count) in &counts {
        let failure = DefFailure {
            version: version.clone(),
            def: def.clone(),
            count: *count,
        };
        if baseline.contains(version, def) {
            out.baselined.push(failure);
        } else {
            out.unbaselined.push(failure);
        }
    }
    for (version, defs) in &baseline.entries {
        if !report.by_version.contains_key(version) {
            continue;
        }
        for def in defs.keys() {
            if !counts.contains_key(&(version.clone(), def.clone())) {
                out.stale.push((version.clone(), def.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_entries_and_comments() {
        let text =
            "# header\n\"2025-11-25\":\n  ListToolsResult: extra field # kept\n\n2026-07-28: {}\n";
        let baseline = Baseline::parse(text).unwrap();
        assert_eq!(
            baseline.entries["2025-11-25"]["ListToolsResult"],
            "extra field # kept"
        );
        assert!(baseline.entries["2026-07-28"].is_empty());
    }

    fn report_with_failure(version: &str, def: &str) -> Report {
        use crate::check::{Failure, VersionStats};
        let mut report = Report::default();
        report
            .by_version
            .insert(version.to_string(), VersionStats::default());
        report.failures.push(Failure {
            version: version.to_string(),
            def: def.to_string(),
            method: None,
            id: None,
            errors: vec![],
        });
        report
    }

    #[test]
    fn failure_outside_the_baseline_is_unbaselined() {
        let report = report_with_failure("2025-11-25", "CallToolResult");
        let cmp = compare(&report, &Baseline::default());
        assert_eq!(cmp.unbaselined.len(), 1);
        assert!(cmp.baselined.is_empty());
    }

    #[test]
    fn failure_in_the_baseline_is_tolerated_and_stale_entries_are_noted() {
        let baseline =
            Baseline::parse("\"2025-11-25\":\n  CallToolResult: known\n  ListToolsResult: fixed\n")
                .unwrap();
        let report = report_with_failure("2025-11-25", "CallToolResult");
        let cmp = compare(&report, &baseline);
        assert!(cmp.unbaselined.is_empty());
        assert_eq!(cmp.baselined.len(), 1);
        assert_eq!(
            cmp.stale,
            vec![("2025-11-25".to_string(), "ListToolsResult".to_string())]
        );
    }

    #[test]
    fn baseline_entry_for_another_versions_def_does_not_cover_this_one() {
        let baseline = Baseline::parse("\"2026-07-28\":\n  CallToolResult: other\n").unwrap();
        let report = report_with_failure("2025-11-25", "CallToolResult");
        assert_eq!(compare(&report, &baseline).unbaselined.len(), 1);
    }

    #[test]
    fn the_checked_in_baseline_parses() {
        let text = include_str!("../../../.github/conformance/wire-schema-baseline.yml");
        Baseline::parse(text).expect("baseline parses");
    }

    #[test]
    fn rejects_entry_without_version() {
        assert!(Baseline::parse("  Foo: bar\n").is_err());
    }
}
