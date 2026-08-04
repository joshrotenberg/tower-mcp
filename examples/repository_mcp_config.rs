//! Guards the checked-in MCP client configuration against Cargo target drift.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, McpServer>,
}

#[derive(Deserialize)]
struct McpServer {
    command: String,
    args: Vec<String>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    targets: Vec<CargoTarget>,
}

#[derive(Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
}

fn value_after<'a>(args: &'a [String], flags: &[&str]) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| flags.contains(&pair[0].as_str()))
        .map(|pair| pair[1].as_str())
}

#[test]
fn configured_cargo_servers_name_existing_packages_and_targets() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("examples package must live under the workspace root");
    let config: McpConfig = serde_json::from_slice(
        &std::fs::read(workspace.join(".mcp.json")).expect("read repository .mcp.json"),
    )
    .expect("parse repository .mcp.json");

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");

    for (server_name, server) in config.servers {
        assert_eq!(
            server.command, "cargo",
            "repository server {server_name:?} needs an explicit validator for its command"
        );
        assert_eq!(
            server.args.first().map(String::as_str),
            Some("run"),
            "repository server {server_name:?} must use `cargo run`"
        );
        let package_name = value_after(&server.args, &["-p", "--package"])
            .unwrap_or_else(|| panic!("repository server {server_name:?} must select a package"));
        let package = metadata
            .packages
            .iter()
            .find(|package| package.name == package_name)
            .unwrap_or_else(|| {
                panic!("repository server {server_name:?} names missing package {package_name:?}")
            });

        if let Some(example_name) = value_after(&server.args, &["--example"]) {
            assert!(
                package.targets.iter().any(|target| {
                    target.name == example_name && target.kind.iter().any(|kind| kind == "example")
                }),
                "repository server {server_name:?} names missing example {example_name:?} in package {package_name:?}"
            );
        } else {
            assert!(
                package
                    .targets
                    .iter()
                    .any(|target| target.kind.iter().any(|kind| kind == "bin")),
                "repository server {server_name:?} names package {package_name:?} without a binary target"
            );
        }
    }
}
