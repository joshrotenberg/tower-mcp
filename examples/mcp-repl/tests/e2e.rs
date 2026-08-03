//! Black-box coverage for the published `mcp-repl` process boundary.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

const CASE_TIMEOUT: Duration = Duration::from_secs(20);
const BUILD_TIMEOUT: Duration = Duration::from_secs(180);
const SUITE_TIMEOUT: Duration = Duration::from_secs(300);

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
    ]);
    // Coverage and beta jobs may need to compile the repository-only fixture
    // with a distinct target configuration. Keep that budget independent of
    // the much tighter timeout used to detect hung mcp-repl processes.
    let output = run(command, "fixture build", BUILD_TIMEOUT).await;
    assert_success(&output, "fixture build");

    let test_binary = std::env::current_exe().expect("current test executable");
    let profile_dir = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("target profile directory");
    let fixture = profile_dir
        .join("examples")
        .join(format!("mcp_repl_fixture{}", std::env::consts::EXE_SUFFIX));
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
        exercise_stdio(&fixture, &temp).await;
        exercise_http(&fixture, &temp).await;
    })
    .await
    .expect("mcp-repl E2E suite exceeded its job-level timeout");
}
