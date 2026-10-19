//! `wire-schema-check`: validate a conformance-server frame log against the
//! published MCP schemas.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use wire_schema_check::baseline::{Baseline, compare};
use wire_schema_check::check::{Options, Report, check_lines, parse_line};
use wire_schema_check::schemas::Schemas;

#[derive(Parser)]
#[command(about = "Validate captured MCP frames against the published JSON schemas")]
struct Args {
    /// Frame log (JSONL) written by conformance-server with TOWER_MCP_FRAME_LOG.
    /// May be given more than once.
    #[arg(long, required = true)]
    log: Vec<PathBuf>,

    /// Known failures (YAML). Failures outside it make the run fail.
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// Also validate frames the server received, not only those it sent.
    #[arg(long)]
    include_inbound: bool,

    /// Print every failure instead of the first per (version, definition).
    #[arg(long)]
    verbose: bool,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Args) -> Result<ExitCode, String> {
    // Each log is checked on its own: exchange numbers and request ids restart
    // with every server process, so lines from different logs must not
    // correlate with one another.
    let mut logs = Vec::new();
    for path in &args.log {
        let mut lines = Vec::new();
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for (n, raw) in text.lines().enumerate() {
            if raw.trim().is_empty() {
                continue;
            }
            lines.push(parse_line(raw).map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))?);
        }
        logs.push(lines);
    }
    let baseline = match &args.baseline {
        Some(path) => Baseline::parse(
            &std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        )
        .map_err(|e| format!("{}: {e}", path.display()))?,
        None => Baseline::default(),
    };

    let mut schemas = Schemas::bundled();
    let options = Options {
        include_inbound: args.include_inbound,
    };
    let mut report = Report::default();
    for lines in &logs {
        report.merge(check_lines(lines, &mut schemas, options));
    }

    println!(
        "wire-schema-check: {} log lines in {} log(s)",
        logs.iter().map(Vec::len).sum::<usize>(),
        logs.len()
    );
    for (version, stats) in &report.by_version {
        println!("\n== {version}");
        println!(
            "frames {}  validated {}  passed {}  failed {}  skipped inbound {}  uncorrelated responses {}",
            stats.frames,
            stats.validated,
            stats.passed,
            stats.failed,
            stats.skipped_inbound,
            stats.uncorrelated
        );
        if !stats.unknown_methods.is_empty() {
            let list: Vec<String> = stats
                .unknown_methods
                .iter()
                .map(|(m, n)| format!("{m} x{n}"))
                .collect();
            println!(
                "methods without a definition (reported, not failures): {}",
                list.join(", ")
            );
        }
    }
    if !report.unversioned.is_empty() {
        let list: Vec<String> = report
            .unversioned
            .iter()
            .map(|(v, n)| format!("{v} x{n}"))
            .collect();
        println!(
            "\nframes with no usable schema version (skipped): {}",
            list.join(", ")
        );
    }

    let comparison = compare(&report, &baseline);
    let baselined: BTreeMap<_, _> = comparison
        .baselined
        .iter()
        .map(|f| ((f.version.as_str(), f.def.as_str()), f.count))
        .collect();

    if !report.failures.is_empty() {
        println!("\nfailures by definition:");
        let mut shown: BTreeMap<(&str, &str), u64> = BTreeMap::new();
        for failure in &report.failures {
            let key = (failure.version.as_str(), failure.def.as_str());
            let seen = shown.entry(key).or_insert(0);
            *seen += 1;
            if *seen > 1 && !args.verbose {
                continue;
            }
            let status = if baselined.contains_key(&key) {
                "baselined"
            } else {
                "NEW"
            };
            println!(
                "  [{status}] {} {} (method {}, id {})",
                failure.version,
                failure.def,
                failure.method.as_deref().unwrap_or("?"),
                failure
                    .id
                    .as_ref()
                    .map_or_else(|| "none".to_string(), |v| v.to_string()),
            );
            for error in failure.errors.iter().take(3) {
                println!("      {error}");
            }
        }
        println!();
        for f in comparison.baselined.iter().chain(&comparison.unbaselined) {
            let status = if baselined.contains_key(&(f.version.as_str(), f.def.as_str())) {
                "baselined"
            } else {
                "NEW"
            };
            println!("  {} {} x{} [{status}]", f.version, f.def, f.count);
        }
    }

    for (version, def) in &comparison.stale {
        println!("note: baseline entry {version} {def} no longer fails and can be removed");
    }

    if comparison.unbaselined.is_empty() {
        println!(
            "\nOK: no failures outside the baseline ({} baselined definitions failing)",
            comparison.baselined.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "\nFAIL: {} definitions fail outside the baseline",
            comparison.unbaselined.len()
        );
        Ok(ExitCode::from(1))
    }
}
