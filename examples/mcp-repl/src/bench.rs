//! The `bench` built-in: issue a tool call N times and report the latency
//! distribution.
//!
//! The per-call `[142ms]` annotation answers "how slow was that one call".
//! `bench` answers "how slow is this tool", which is the question you have
//! when a server is behind a network, a cold cache, or a rate limiter.
//!
//! ```text
//! demo> bench echo message=hi --n 50
//! 50 calls  ok=50 err=0  min=0.2ms p50=0.3ms p95=0.6ms max=1.1ms
//! ```
//!
//! Percentiles are computed over the calls that succeeded; failures are
//! reported as a count (with the first error message) rather than folded into
//! the distribution, since a fast rejection would otherwise read as a fast
//! call.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tower_mcp::client::McpClient;

/// Calls issued when `--n` is not given.
pub const DEFAULT_N: usize = 20;

/// Refuse absurd run lengths: `--n 1000000` against a real server is a typo,
/// not a benchmark, and the REPL is blocked until it finishes.
pub const MAX_N: usize = 100_000;

/// A parsed `bench` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The tool to call.
    pub tool: String,
    /// The `key=value` tokens, passed through to the normal argument
    /// coercion so `bench` takes arguments exactly like a direct call does.
    pub args: Vec<String>,
    /// How many calls to issue.
    pub n: usize,
    /// How many calls may be in flight at once (1 is serial).
    pub concurrency: usize,
}

/// Parse `bench <tool> [k=v...] [--n N] [--concurrency C]`.
///
/// Flags may appear anywhere after the command word and in either spelling
/// (`--n 50` or `--n=50`); the first token that is not a flag or a flag value
/// is the tool name, and the rest are its arguments.
pub fn parse(tokens: &[&str]) -> Result<Plan, String> {
    let mut n = DEFAULT_N;
    let mut concurrency = 1usize;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        let flag = match token.split_once('=') {
            // `--n=50`: the value is attached. A bare `k=v` is an argument,
            // not a flag, so only `--`-prefixed names are read this way.
            Some((name, value)) if name.starts_with("--") => Some((name, Some(value.to_string()))),
            _ if token.starts_with("--") => Some((token, None)),
            _ => None,
        };
        let Some((name, attached)) = flag else {
            positional.push(token.to_string());
            i += 1;
            continue;
        };
        let value = match attached {
            Some(v) => v,
            None => {
                i += 1;
                tokens
                    .get(i)
                    .ok_or_else(|| format!("{name} needs a value"))?
                    .to_string()
            }
        };
        match name {
            "--n" => n = parse_count(name, &value)?,
            "--concurrency" => concurrency = parse_count(name, &value)?,
            other => {
                return Err(format!(
                    "unknown option `{other}` (bench takes --n and --concurrency)"
                ));
            }
        }
        i += 1;
    }

    if n > MAX_N {
        return Err(format!("--n {n} is above the {MAX_N} call limit"));
    }
    let mut positional = positional.into_iter();
    let tool = positional
        .next()
        .ok_or_else(|| "usage: bench <tool> [k=v...] [--n N] [--concurrency C]".to_string())?;
    Ok(Plan {
        tool,
        args: positional.collect(),
        // More workers than calls would just idle.
        concurrency: concurrency.min(n),
        n,
    })
}

fn parse_count(flag: &str, raw: &str) -> Result<usize, String> {
    match raw.parse::<usize>() {
        Ok(0) => Err(format!("{flag} must be at least 1")),
        Ok(v) => Ok(v),
        Err(_) => Err(format!("{flag} expects a number, got `{raw}`")),
    }
}

/// What a run produced: one latency per successful call, plus the failures.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Latency of each call that returned a non-error result.
    pub latencies: Vec<Duration>,
    /// Calls that failed, either at the protocol level or with `isError`.
    pub errors: usize,
    /// The first failure's message, so a run that fails says why.
    pub first_error: Option<String>,
    /// Wall-clock time for the whole run.
    pub total: Duration,
}

impl Outcome {
    fn record(&mut self, call: Call) {
        match call {
            Call::Ok(elapsed) => self.latencies.push(elapsed),
            Call::Err(message) => {
                self.errors += 1;
                if self.first_error.is_none() {
                    self.first_error = Some(message);
                }
            }
        }
    }

    /// The latency distribution, or `None` when nothing succeeded.
    pub fn stats(&self) -> Option<Stats> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        Some(Stats {
            min: sorted[0],
            p50: percentile(&sorted, 50.0),
            p95: percentile(&sorted, 95.0),
            max: sorted[sorted.len() - 1],
        })
    }
}

/// The reported latency distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
}

/// Nearest-rank percentile over an ascending slice: the smallest sample at or
/// above the requested share of the run. No interpolation, so every reported
/// number is a latency that actually happened.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

enum Call {
    Ok(Duration),
    Err(String),
}

/// One call, timed. A tool that returns `isError` counts as a failure: it did
/// not do the work, so its latency does not describe the work.
async fn call_once(client: &McpClient, tool: &str, arguments: serde_json::Value) -> Call {
    let started = Instant::now();
    match client.call_tool(tool, arguments).await {
        Ok(result) if result.is_error => Call::Err(error_text(&result)),
        Ok(_) => Call::Ok(started.elapsed()),
        Err(e) => Call::Err(e.to_string()),
    }
}

/// The text of a tool-reported error, for the `first error:` line.
fn error_text(result: &tower_mcp::CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .find_map(|c| match c {
            tower_mcp::protocol::Content::Text { text, .. } => Some(text.trim().to_string()),
            _ => None,
        })
        .unwrap_or_default();
    if text.is_empty() {
        "tool reported an error".to_string()
    } else {
        text
    }
}

/// Issue `n` calls, at most `concurrency` in flight, and time each one.
pub async fn run(
    client: &Arc<McpClient>,
    tool: &str,
    arguments: serde_json::Value,
    n: usize,
    concurrency: usize,
) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome::default();

    if concurrency <= 1 {
        for _ in 0..n {
            outcome.record(call_once(client, tool, arguments.clone()).await);
        }
        outcome.total = started.elapsed();
        return outcome;
    }

    // Workers pull from a shared counter rather than taking a fixed slice
    // each, so one slow call does not leave its worker's remaining share
    // queued behind it while others sit idle.
    let remaining = Arc::new(AtomicUsize::new(n));
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let tool = tool.to_string();
        let arguments = arguments.clone();
        let remaining = remaining.clone();
        workers.push(tokio::spawn(async move {
            let mut calls = Vec::new();
            while remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                calls.push(call_once(&client, &tool, arguments.clone()).await);
            }
            calls
        }));
    }
    for worker in workers {
        match worker.await {
            Ok(calls) => {
                for call in calls {
                    outcome.record(call);
                }
            }
            Err(e) => outcome.record(Call::Err(format!("bench worker failed: {e}"))),
        }
    }
    outcome.total = started.elapsed();
    outcome
}

/// A duration at a readable precision: sub-10ms keeps a decimal, since an
/// in-process or LAN call is otherwise reported as `0ms`.
pub fn dur(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        return format!("{secs:.2}s");
    }
    let ms = secs * 1000.0;
    if ms < 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{ms:.0}ms")
    }
}

/// Milliseconds as a JSON number.
fn ms(d: Duration) -> serde_json::Value {
    serde_json::json!(d.as_secs_f64() * 1000.0)
}

/// The one-line summary: call counts, then the distribution.
pub fn render(plan: &Plan, outcome: &Outcome) -> String {
    let ok = outcome.latencies.len();
    let mut line = format!(
        "{} calls  ok={ok} err={}",
        ok + outcome.errors,
        outcome.errors
    );
    if plan.concurrency > 1 {
        line.push_str(&format!(" concurrency={}", plan.concurrency));
    }
    match outcome.stats() {
        Some(s) => line.push_str(&format!(
            "  min={} p50={} p95={} max={}",
            dur(s.min),
            dur(s.p50),
            dur(s.p95),
            dur(s.max)
        )),
        None => line.push_str("  no successful calls"),
    }
    line
}

/// The `--json` form. Latency fields are null when nothing succeeded, rather
/// than zero, so a failed run cannot be read as an instant one.
pub fn render_json(plan: &Plan, outcome: &Outcome) -> serde_json::Value {
    let stats = outcome.stats();
    serde_json::json!({
        "tool": plan.tool,
        "calls": outcome.latencies.len() + outcome.errors,
        "ok": outcome.latencies.len(),
        "errors": outcome.errors,
        "concurrency": plan.concurrency,
        "firstError": outcome.first_error,
        "minMs": stats.map(|s| ms(s.min)),
        "p50Ms": stats.map(|s| ms(s.p50)),
        "p95Ms": stats.map(|s| ms(s.p95)),
        "maxMs": stats.map(|s| ms(s.max)),
        "totalMs": ms(outcome.total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(line: &str) -> Result<Plan, String> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        parse(&tokens)
    }

    fn outcome(millis: &[u64], errors: usize) -> Outcome {
        Outcome {
            latencies: millis.iter().map(|m| Duration::from_millis(*m)).collect(),
            errors,
            first_error: (errors > 0).then(|| "boom".to_string()),
            total: Duration::from_millis(millis.iter().sum::<u64>()),
        }
    }

    #[test]
    fn defaults_to_twenty_serial_calls() {
        let p = plan("echo").unwrap();
        assert_eq!(p.tool, "echo");
        assert_eq!(p.n, DEFAULT_N);
        assert_eq!(p.concurrency, 1);
        assert!(p.args.is_empty());
    }

    #[test]
    fn reads_both_flag_spellings_anywhere_in_the_line() {
        let attached = plan("--n=50 echo message=hi --concurrency=4").unwrap();
        let separate = plan("bench_me_not --n 50 --concurrency 4").unwrap();
        assert_eq!((attached.n, attached.concurrency), (50, 4));
        assert_eq!((separate.n, separate.concurrency), (50, 4));
        assert_eq!(attached.tool, "echo");
        assert_eq!(attached.args, ["message=hi"]);
    }

    #[test]
    fn keeps_arguments_in_order_and_does_not_read_them_as_flags() {
        // A value containing `=` or looking like a flag belongs to the tool.
        let p = plan("get_crate_info name=serde version=1.0 --n 3").unwrap();
        assert_eq!(p.args, ["name=serde", "version=1.0"]);
        assert_eq!(p.n, 3);
    }

    #[test]
    fn concurrency_never_exceeds_the_call_count() {
        let p = plan("echo --n 2 --concurrency 8").unwrap();
        assert_eq!(p.concurrency, 2);
    }

    #[test]
    fn rejects_bad_invocations() {
        let cases = [
            ("", "usage"),
            ("echo --n 0", "at least 1"),
            ("echo --n abc", "expects a number"),
            ("echo --n", "needs a value"),
            ("echo --nope 2", "unknown option"),
            ("echo --n 100001", "call limit"),
        ];
        for (line, expected) in cases {
            let err = plan(line).unwrap_err();
            assert!(
                err.contains(expected),
                "`bench {line}` should mention {expected:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn percentiles_are_nearest_rank_samples() {
        let sorted: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&sorted, 50.0), Duration::from_millis(50));
        assert_eq!(percentile(&sorted, 95.0), Duration::from_millis(95));
        // A single sample is every percentile of itself.
        let one = [Duration::from_millis(7)];
        assert_eq!(percentile(&one, 50.0), Duration::from_millis(7));
        assert_eq!(percentile(&one, 95.0), Duration::from_millis(7));
    }

    #[test]
    fn stats_span_the_successful_calls() {
        let s = outcome(&[30, 10, 20, 40, 50], 0).stats().unwrap();
        assert_eq!(s.min, Duration::from_millis(10));
        assert_eq!(s.p50, Duration::from_millis(30));
        assert_eq!(s.p95, Duration::from_millis(50));
        assert_eq!(s.max, Duration::from_millis(50));
    }

    #[test]
    fn a_run_with_no_successes_has_no_distribution() {
        let o = outcome(&[], 5);
        assert!(o.stats().is_none());
        let p = plan("echo --n 5").unwrap();
        assert!(render(&p, &o).contains("no successful calls"));
        let v = render_json(&p, &o);
        assert!(v["minMs"].is_null(), "{v}");
        assert_eq!(v["errors"], 5);
        assert_eq!(v["ok"], 0);
        assert_eq!(v["firstError"], "boom");
    }

    #[test]
    fn durations_keep_a_decimal_below_ten_milliseconds() {
        assert_eq!(dur(Duration::from_micros(240)), "0.2ms");
        assert_eq!(dur(Duration::from_millis(42)), "42ms");
        assert_eq!(dur(Duration::from_millis(2500)), "2.50s");
    }

    #[test]
    fn the_summary_reports_counts_and_the_distribution() {
        let p = plan("echo --n 4").unwrap();
        let line = render(&p, &outcome(&[10, 20, 30], 1));
        assert!(line.starts_with("4 calls  ok=3 err=1"), "{line}");
        assert!(line.contains("min=10ms"), "{line}");
        assert!(line.contains("max=30ms"), "{line}");
        // Serial runs say nothing about concurrency.
        assert!(!line.contains("concurrency"), "{line}");
        let overlapped = plan("echo --n 4 --concurrency 2").unwrap();
        assert!(render(&overlapped, &outcome(&[10, 20, 30], 1)).contains("concurrency=2"));
    }

    #[test]
    fn json_carries_the_distribution_in_milliseconds() {
        let p = plan("echo --n 3 --concurrency 3").unwrap();
        let v = render_json(&p, &outcome(&[10, 20, 30], 0));
        assert_eq!(v["tool"], "echo");
        assert_eq!(v["calls"], 3);
        assert_eq!(v["concurrency"], 3);
        assert_eq!(v["minMs"], 10.0);
        assert_eq!(v["p95Ms"], 30.0);
        assert!(v["firstError"].is_null());
    }

    // The run path against the in-process demo router: no socket, no child
    // process, but a real client and real `tools/call` round trips.
    async fn demo_client() -> Arc<McpClient> {
        use crate::elicit::ReplClientHandler;
        use std::sync::atomic::AtomicBool;
        use tower_mcp::client::{ChannelTransport, NotificationHandler};

        let handler =
            ReplClientHandler::new(NotificationHandler::new(), Arc::new(AtomicBool::new(false)));
        let client = McpClient::builder()
            .connect(ChannelTransport::new(crate::demo_router()), handler)
            .await
            .unwrap();
        client.initialize("bench-test", "0").await.unwrap();
        Arc::new(client)
    }

    #[tokio::test]
    async fn every_call_is_issued_and_timed() {
        let client = demo_client().await;
        let args = serde_json::json!({ "message": "hi" });
        for concurrency in [1, 3] {
            let o = run(&client, "echo", args.clone(), 6, concurrency).await;
            assert_eq!(o.latencies.len(), 6, "concurrency {concurrency}");
            assert_eq!(o.errors, 0);
            assert!(o.first_error.is_none());
            assert!(o.total > Duration::ZERO);
        }
    }

    #[tokio::test]
    async fn failures_are_counted_and_the_first_one_is_kept() {
        let client = demo_client().await;
        let o = run(&client, "no_such_tool", serde_json::json!({}), 3, 2).await;
        assert_eq!(o.errors, 3);
        assert!(o.latencies.is_empty());
        assert!(
            o.first_error.is_some_and(|e| e.contains("no_such_tool")),
            "the failure should name the tool"
        );
    }
}
