//! Compare the stdio pipe modes: throughput, latency, and shutdown.
//!
//! `StdioTransport::run()` can read and write through tokio's blocking thread
//! pool or through the I/O reactor (`StdioPipes`). This example spawns itself
//! as a stdio server in each mode and drives it over pipes, the way an MCP
//! client does. It is a measurement aid, not a pass/fail test.
//!
//! Run with: cargo run --release --example stdio_pipe_bench
//!
//! Options: `--total N` (requests per throughput round), `--concurrency N`
//! (requests in flight), `--rounds N`.
//!
//! Three things are reported per mode:
//!
//! - throughput with many requests in flight
//! - round-trip latency with one request in flight
//! - how long the server process lingers after `run()` returns because the
//!   transport was stopped while the client still holds stdin open. A blocking
//!   stdin read cannot be cancelled, so tokio's runtime waits for it.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tower_mcp::{CallToolResult, McpRouter, NoParams, StdioPipes, StdioTransport, ToolBuilder};

const MODES: [(&str, StdioPipes); 3] = [
    ("blocking", StdioPipes::Blocking),
    ("auto", StdioPipes::Auto),
    ("shared", StdioPipes::Shared),
];

fn parse_mode(name: &str) -> StdioPipes {
    MODES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, mode)| *mode)
        .expect("unknown mode")
}

fn arg_value(args: &[String], flag: &str, default: usize) -> usize {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--server") {
        return serve(
            parse_mode(&args[i + 1]),
            arg_value(&args, "--stop-after-ms", 0),
        )
        .await;
    }

    let total = arg_value(&args, "--total", 20_000);
    let concurrency = arg_value(&args, "--concurrency", 32);
    let rounds = arg_value(&args, "--rounds", 3);

    println!("{total} requests per round, {concurrency} in flight, {rounds} rounds (best shown)");
    println!(
        "{:<10} {:>14} {:>16} {:>22}",
        "mode", "req/s", "latency (us)", "exit after stop (ms)"
    );
    for (name, _) in MODES {
        let mut best_rate = 0.0f64;
        let mut best_latency = f64::MAX;
        for _ in 0..rounds {
            best_rate = best_rate.max(throughput(name, concurrency, total).await?);
            best_latency = best_latency.min(throughput_latency(name, total / 4).await?);
        }
        let exit = exit_after_stop(name).await?;
        println!(
            "{:<10} {:>14.0} {:>16.1} {:>22}",
            name, best_rate, best_latency, exit
        );
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Server role
// ----------------------------------------------------------------------------

async fn serve(mode: StdioPipes, stop_after_ms: usize) -> Result<(), tower_mcp::BoxError> {
    let noop = ToolBuilder::new("noop")
        .description("Return a constant")
        .handler(|_: NoParams| async { Ok(CallToolResult::text("ok")) })
        .build();
    let router = McpRouter::new()
        .server_info("stdio-pipe-bench", "0.0.0")
        .tool(noop);

    let mut transport = StdioTransport::new(router).pipes(mode);
    if stop_after_ms > 0 {
        let handle = transport.handle();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(stop_after_ms as u64)).await;
            let _ = handle.shutdown();
        });
    }
    transport.run().await?;
    if stop_after_ms > 0 {
        // The protocol stream is finished, so stdout is free for a marker.
        println!("run-returned");
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Client role
// ----------------------------------------------------------------------------

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

async fn spawn_server(mode: &str, stop_after_ms: usize) -> std::io::Result<Server> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--server", mode])
        .args(["--stop-after-ms", &stop_after_ms.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    Ok(Server {
        child,
        stdin,
        stdout,
    })
}

impl Server {
    async fn handshake(&mut self) -> std::io::Result<()> {
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"bench","version":"0"}}}"#;
        self.stdin.write_all(init.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        let mut line = String::new();
        self.stdout.read_line(&mut line).await?;
        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        self.stdin.write_all(initialized.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        Ok(())
    }

    async fn send_call(&mut self, id: usize) -> std::io::Result<()> {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"noop","arguments":{{}}}}}}"#
        );
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await
    }

    /// Keep `concurrency` calls in flight until `total` have been answered.
    async fn drive(&mut self, concurrency: usize, total: usize) -> std::io::Result<Duration> {
        let started = Instant::now();
        let mut sent = 0;
        while sent < concurrency.min(total) {
            self.send_call(sent + 1).await?;
            sent += 1;
        }
        let mut line = String::new();
        for _ in 0..total {
            line.clear();
            self.stdout.read_line(&mut line).await?;
            if sent < total {
                self.send_call(sent + 1).await?;
                sent += 1;
            }
        }
        Ok(started.elapsed())
    }
}

/// Requests per second with `concurrency` in flight.
async fn throughput(mode: &str, concurrency: usize, total: usize) -> std::io::Result<f64> {
    let mut server = spawn_server(mode, 0).await?;
    server.handshake().await?;
    let elapsed = server.drive(concurrency, total).await?;
    Ok(total as f64 / elapsed.as_secs_f64())
}

/// Mean round-trip microseconds with one call in flight.
async fn throughput_latency(mode: &str, total: usize) -> std::io::Result<f64> {
    let mut server = spawn_server(mode, 0).await?;
    server.handshake().await?;
    let elapsed = server.drive(1, total).await?;
    Ok(elapsed.as_secs_f64() * 1e6 / total as f64)
}

/// Stop the transport while the client keeps stdin open, and report how long
/// the process takes to exit once `run()` has returned.
async fn exit_after_stop(mode: &str) -> std::io::Result<String> {
    let mut server = spawn_server(mode, 300).await?;
    server.handshake().await?;

    let mut line = String::new();
    while !line.starts_with("run-returned") {
        line.clear();
        if server.stdout.read_line(&mut line).await? == 0 {
            break;
        }
    }
    let returned = Instant::now();
    match tokio::time::timeout(Duration::from_secs(2), server.child.wait()).await {
        Ok(status) => {
            status?;
            Ok(format!("{:.1}", returned.elapsed().as_secs_f64() * 1e3))
        }
        Err(_) => {
            // Still alive: it is waiting on a read that cannot be cancelled.
            drop(server.stdin);
            server.child.wait().await?;
            Ok("blocked until stdin closed".to_string())
        }
    }
}
