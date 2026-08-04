//! Black-box coverage for the published `mcp-repl` process boundary.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

// Each process normally finishes in well under a second, but beta and Windows
// runners can be CPU-starved while the all-target workspace job is active.
// Keep hangs bounded without treating scheduler stalls as product failures.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(180);
const SUITE_TIMEOUT: Duration = Duration::from_secs(600);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

async fn run(mut command: Command, label: &str, timeout: Duration) -> Output {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));
    tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded {timeout:?}"))
        .unwrap_or_else(|error| panic!("wait for {label}: {error}"))
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_status(output: &Output, expected: i32, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{label} had unexpected status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json_lines(output: &Output, label: &str) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "{label} stdout line {} is not JSON: {error}: {line}",
                    index + 1
                )
            })
        })
        .collect()
}

async fn build_fixture() -> PathBuf {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(workspace_root()).args([
        "build",
        "--quiet",
        "-p",
        "tower-mcp-examples",
        "--example",
        "mcp_repl_fixture",
        "--features",
        "http,protocol-2026-07-28",
        "--message-format=json-render-diagnostics",
    ]);
    // Coverage and beta jobs may need to compile the repository-only fixture
    // with a distinct target configuration. Keep that budget independent of
    // the much tighter timeout used to detect hung mcp-repl processes.
    let output = run(command, "fixture build", BUILD_TIMEOUT).await;
    assert_success(&output, "fixture build");

    // The outer test runner may select a different target directory (notably
    // cargo-llvm-cov). Cargo's artifact record is authoritative; deriving the
    // fixture path from the integration-test executable only works when both
    // Cargo invocations happen to share a target directory.
    let fixture = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact"
                && message["target"]["name"] == "mcp_repl_fixture")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("Cargo did not report the mcp_repl_fixture executable");
    assert!(
        fixture.is_file(),
        "fixture was not built at {}",
        fixture.display()
    );
    fixture
}

fn repl_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-repl"));
    command.current_dir(workspace_root());
    command
}

async fn wait_for_file(path: &Path, label: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match std::fs::read_to_string(path) {
                Ok(contents) => break contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("read {label}: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}

async fn run_stdio(fixture: &Path, temp: &TempDir, case: &str, repl_args: &[&str]) -> Output {
    let exit_file = temp.path().join(format!("{case}.exit"));
    let mut command = repl_command();
    command
        .args(repl_args)
        .arg(fixture)
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, case, CASE_TIMEOUT).await;
    assert_eq!(
        wait_for_file(&exit_file, "stdio fixture shutdown").await,
        "clean",
        "mcp-repl left its stdio child running"
    );
    output
}

struct HttpFixture {
    child: Option<Child>,
    url: String,
    subscription_file: PathBuf,
}

impl HttpFixture {
    async fn start(fixture: &Path, temp: &TempDir) -> Self {
        let ready_file = temp.path().join("http.ready");
        let subscription_file = temp.path().join("http.subscription");
        let mut command = Command::new(fixture);
        command
            .arg("--http")
            .env("MCP_REPL_FIXTURE_READY_FILE", &ready_file)
            .env("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", &subscription_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().expect("spawn HTTP fixture");
        let url = wait_for_file(&ready_file, "HTTP fixture readiness").await;
        Self {
            child: Some(child),
            url,
            subscription_file,
        }
    }

    async fn shutdown(mut self) {
        let mut child = self.child.take().expect("HTTP fixture child");
        child.start_kill().expect("stop HTTP fixture");
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("HTTP fixture did not exit")
            .expect("wait for HTTP fixture");
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

async fn run_http(url: &str, case: &str, repl_args: &[&str]) -> Output {
    let mut command = repl_command();
    command.args(repl_args).args(["--http", url]);
    run(command, case, CASE_TIMEOUT).await
}

async fn auth_failure_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth failure server");
    let url = format!(
        "http://{}/",
        listener.local_addr().expect("auth server address")
    );
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0_u8; 8 * 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\n\
                          Content-Length: 0\r\n\
                          WWW-Authenticate: Bearer\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    (url, task)
}

async fn exercise_json_contract(fixture: &Path, temp: &TempDir) {
    // Keep one round trip after `announce` so the asynchronous notification
    // handler drains before the one-shot process exits, including on Windows.
    let multiple = run_stdio(
        fixture,
        temp,
        "json-multiple",
        &[
            "--json",
            "--verbose",
            "--trace",
            "--exec",
            "tools",
            "--exec",
            "announce",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_success(&multiple, "multiple JSON commands");
    let values = json_lines(&multiple, "multiple JSON commands");
    assert_eq!(values.len(), 3, "one JSON line must be emitted per command");
    assert!(
        values[0].is_array(),
        "tools returns the raw MCP list: {values:?}"
    );
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("announced"))
    );
    assert_eq!(
        values[2].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
    assert!(
        !String::from_utf8_lossy(&multiple.stdout).contains("connected:"),
        "--verbose must not contaminate JSON stdout"
    );
    let stderr = String::from_utf8_lossy(&multiple.stderr);
    assert!(stderr.contains("fixture announcement"), "{stderr}");
    assert!(
        stderr.contains("tools/list"),
        "wire tracing stayed off: {stderr}"
    );

    let no_match = run_stdio(
        fixture,
        temp,
        "json-no-match",
        &["--json", "--exec", "find definitely-not-on-the-surface"],
    )
    .await;
    assert_status(&no_match, 1, "no-match outcome");
    assert_eq!(
        json_lines(&no_match, "no-match outcome"),
        [serde_json::json!([])]
    );

    let continued = run_stdio(
        fixture,
        temp,
        "json-continued",
        &[
            "--json",
            "--exec",
            "no_such_command",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_status(&continued, 2, "usage error");
    let values = json_lines(&continued, "continued JSON commands");
    assert_eq!(values.len(), 2, "later commands must run after a failure");
    assert_eq!(values[0]["kind"], "usage");
    assert_eq!(values[0]["exitStatus"], 2);
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );

    let server_error = run_stdio(
        fixture,
        temp,
        "json-server-error",
        &["--json", "--exec", "fail"],
    )
    .await;
    assert_status(&server_error, 3, "tool error");
    let values = json_lines(&server_error, "tool error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["isError"], true);

    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve unavailable endpoint");
    let unavailable_url = format!(
        "http://{}/",
        unavailable.local_addr().expect("unavailable address")
    );
    drop(unavailable);
    let transport_error = run_http(
        &unavailable_url,
        "JSON transport error",
        &["--json", "--exec", "tools"],
    )
    .await;
    assert_status(&transport_error, 4, "transport error");
    let values = json_lines(&transport_error, "transport error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "transport");

    let (auth_url, auth_server) = auth_failure_server().await;
    let auth_error = run_http(&auth_url, "JSON auth error", &["--json", "--exec", "tools"]).await;
    auth_server.abort();
    assert_status(&auth_error, 5, "authentication error");
    let values = json_lines(&auth_error, "authentication error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "auth");
}

async fn exercise_imported_stdio_config(fixture: &Path, temp: &TempDir) {
    let workspace = temp.path().join("import-workspace");
    let cwd = workspace.join("work");
    std::fs::create_dir_all(&cwd).expect("create imported fixture cwd");
    let config = workspace.join(".mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "mcpServers": {
                "fixture": {
                    "command": fixture,
                    "env": {
                        "MCP_REPL_IMPORTED_VALUE": "${env:MCP_REPL_HOST_VALUE}"
                    },
                    "cwd": "${workspaceFolder}/work"
                }
            }
        })
        .to_string(),
    )
    .expect("write imported stdio config");
    let exit_file = temp.path().join("import-stdio.exit");
    let selector = format!("{}:fixture", config.display());
    let mut command = repl_command();
    command
        .args(["--json", "--exec", "process_info", &selector])
        .env("MCP_REPL_HOST_VALUE", "from-host")
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "imported stdio config", CASE_TIMEOUT).await;
    assert_success(&output, "imported stdio config");
    assert_eq!(
        wait_for_file(&exit_file, "imported stdio fixture shutdown").await,
        "clean"
    );
    let values = json_lines(&output, "imported stdio config");
    assert_eq!(values.len(), 1);
    let process: serde_json::Value = serde_json::from_str(
        values[0]
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .expect("process_info text result"),
    )
    .expect("process_info JSON");
    assert_eq!(process["imported"], "from-host");
    assert_eq!(
        PathBuf::from(process["cwd"].as_str().expect("process cwd"))
            .canonicalize()
            .expect("canonical process cwd"),
        cwd.canonicalize().expect("canonical expected cwd")
    );
}

async fn exercise_imported_http_config(http: &HttpFixture, temp: &TempDir) {
    let config = temp.path().join("vscode-mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "servers": {
                "fixture": {
                    "type": "http",
                    "url": "http://127.0.0.1:1/"
                }
            }
        })
        .to_string(),
    )
    .expect("write imported HTTP config");
    let selector = format!("{}:fixture", config.display());
    let mut command = repl_command();
    command.args([
        "--json",
        "--exec",
        "add a=20 b=22",
        "--http",
        &http.url,
        &selector,
    ]);
    let output = run(command, "imported HTTP config", CASE_TIMEOUT).await;
    assert_success(&output, "imported HTTP config");
    let values = json_lines(&output, "imported HTTP config");
    assert_eq!(values.len(), 1);
    assert_eq!(
        values[0].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
}

async fn exercise_stdio(fixture: &Path, temp: &TempDir) {
    let stable = run_stdio(
        fixture,
        temp,
        "stable-stdio",
        &[
            "--protocol",
            "stable",
            "--verbose",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_success(&stable, "stable stdio");
    let stdout = String::from_utf8_lossy(&stable.stdout);
    let stderr = String::from_utf8_lossy(&stable.stderr);
    assert!(stdout.contains("protocol 2025-11-25"), "{stdout}");
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stderr.contains("mcp-repl fixture ready"), "{stderr}");

    let final_ = run_stdio(
        fixture,
        temp,
        "final-stdio",
        &[
            "--protocol",
            "2026-07-28",
            "--verbose",
            "--exec",
            "add a=20 b=22",
            "--exec",
            "prompt greet name=Ada",
            "--exec",
            "read fixture://guide",
        ],
    )
    .await;
    assert_success(&final_, "final stdio");
    let stdout = String::from_utf8_lossy(&final_.stdout);
    assert!(stdout.contains("protocol 2026-07-28"), "{stdout}");
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stdout.contains("Please greet Ada warmly."), "{stdout}");
    assert!(stdout.contains("fixture resource body"), "{stdout}");

    let error = run_stdio(
        fixture,
        temp,
        "json-error",
        &["--json", "--exec", "no_such_command"],
    )
    .await;
    assert!(!error.status.success(), "unknown command should fail");
    let stdout = String::from_utf8_lossy(&error.stdout);
    let stderr = String::from_utf8_lossy(&error.stderr);
    assert!(stdout.contains("\"error\""), "{stdout}");
    assert!(!stdout.contains("fixture ready"), "{stdout}");
    assert!(stderr.contains("mcp-repl fixture ready"), "{stderr}");
}

async fn exercise_interactive_final_task(http: &HttpFixture) {
    let mut command = repl_command();
    command
        .args([
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--http",
            &http.url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn interactive final mcp-repl");
    let mut stdin = child.stdin.take().expect("interactive stdin");
    stdin
        .write_all(b"slow_add a=2 b=3 &\n")
        .await
        .expect("write task command");
    wait_for_file(&http.subscription_file, "final subscription").await;
    // The subscription is immediate, while the bounded task poller remains a
    // fallback. Leave enough room for either path to observe completion before
    // asking the editor thread to exit.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    stdin
        .write_all(b"jobs\nquit\n")
        .await
        .expect("write task status and quit commands");
    drop(stdin);
    let output = tokio::time::timeout(CASE_TIMEOUT, child.wait_with_output())
        .await
        .expect("interactive final case timed out")
        .expect("wait for interactive final case");
    assert_success(&output, "interactive final task");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("started"), "{stdout}");
    assert!(stdout.contains("completed"), "{stdout}");
}

async fn exercise_http(fixture: &Path, temp: &TempDir) {
    let http = HttpFixture::start(fixture, temp).await;

    exercise_imported_http_config(&http, temp).await;

    let stable = run_http(
        &http.url,
        "stable HTTP",
        &[
            "--protocol",
            "stable",
            "--verbose",
            "--exec",
            "prompt greet name=Grace",
            "--exec",
            "announce",
        ],
    )
    .await;
    assert_success(&stable, "stable HTTP");
    let stdout = String::from_utf8_lossy(&stable.stdout);
    let stderr = String::from_utf8_lossy(&stable.stderr);
    assert!(stdout.contains("protocol 2025-11-25"), "{stdout}");
    assert!(stdout.contains("Please greet Grace warmly."), "{stdout}");
    assert!(stderr.contains("fixture announcement"), "{stderr}");

    let final_ = run_http(
        &http.url,
        "final HTTP",
        &[
            "--protocol",
            "2026-07-28",
            "--json",
            "--exec",
            "add a=40 b=2",
            "--exec",
            "read fixture://guide",
        ],
    )
    .await;
    assert_success(&final_, "final HTTP");
    let stdout = String::from_utf8_lossy(&final_.stdout);
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stdout.contains("fixture resource body"), "{stdout}");

    exercise_interactive_final_task(&http).await;
    http.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn published_cli_covers_transports_and_protocol_lifecycles() {
    tokio::time::timeout(SUITE_TIMEOUT, async {
        let temp = TempDir::new().expect("temporary fixture directory");
        let fixture = build_fixture().await;
        exercise_json_contract(&fixture, &temp).await;
        exercise_imported_stdio_config(&fixture, &temp).await;
        exercise_stdio(&fixture, &temp).await;
        exercise_http(&fixture, &temp).await;
    })
    .await
    .expect("mcp-repl E2E suite exceeded its job-level timeout");
}
