//! End-to-end checks through `TestClient`, which drives the router the same
//! way a transport does, without a process or a pipe.
//!
//! The Lua task needs a `LocalSet`, so each test runs inside one.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use lua_tools_example::LuaTools;
use serde_json::{Value, json};
use tower_mcp::protocol::RequestId;
use tower_mcp::{McpRouter, RequestContext, TestClient};

/// The sample tools the binary serves by default.
const SAMPLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/lua");

/// Run `test` on a `LocalSet`, as the binary does.
async fn local<F: Future>(test: F) -> F::Output {
    tokio::task::LocalSet::new().run_until(test).await
}

/// A client on a router whose tools come from `dir`.
async fn serve(dir: &Path) -> (TestClient, LuaTools) {
    let router = McpRouter::new().server_info("lua-tools-test", "0.0.0");
    let (router, lua) = LuaTools::new(router).expect("start the Lua task");
    lua.load_dir(dir).await.expect("load the tools");
    let mut client = TestClient::from_router(router);
    client.initialize().await;
    (client, lua)
}

/// Two files with this source define the same tool name.
const DUP: &str =
    r#"return { name = "dup", description = "d", input = { type = "object" }, run = print }"#;

/// A temp directory holding the given `(file name, Lua source)` pairs.
fn tool_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    dir
}

fn names(tools: &[Value]) -> Vec<&str> {
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    names
}

#[tokio::test(flavor = "current_thread")]
async fn lists_the_sample_tools_with_their_schemas() {
    local(async {
        let (mut client, _lua) = serve(Path::new(SAMPLES)).await;
        let tools = client.list_tools().await;
        assert_eq!(names(&tools), ["countdown", "greet"]);

        let greet = tools.iter().find(|t| t["name"] == "greet").unwrap();
        assert_eq!(greet["inputSchema"]["required"], json!(["name"]));
        assert_eq!(greet["inputSchema"]["properties"]["name"]["minLength"], 1);
        let countdown = tools.iter().find(|t| t["name"] == "countdown").unwrap();
        assert_eq!(
            countdown["inputSchema"]["properties"]["seconds"]["maximum"],
            30
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn greet_returns_the_greeting() {
    local(async {
        let (mut client, _lua) = serve(Path::new(SAMPLES)).await;
        let result = client.call_tool("greet", json!({ "name": "world" })).await;
        assert!(!result.is_error);
        assert_eq!(
            result.structured_content,
            Some(json!({ "greeting": "hello, world" }))
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_arguments_are_rejected_before_lua_runs() {
    local(async {
        // `counted` bumps a global each time its Lua body runs; `calls` reads it.
        let dir = tool_dir(&[
            (
                "counted.lua",
                r#"calls = 0
                return { name = "counted", description = "d",
                  input = { type = "object", required = { "name" },
                    properties = { name = { type = "string", minLength = 1 } } },
                  run = function() calls = calls + 1; return "ran" end }"#,
            ),
            (
                "calls.lua",
                r#"return { name = "calls", description = "d", input = { type = "object" },
                  run = function() return { calls = calls } end }"#,
            ),
        ]);
        let (mut client, _lua) = serve(dir.path()).await;

        let result = client.call_tool("counted", json!({ "name": "" })).await;
        assert!(result.is_error);
        assert!(
            result.all_text().contains("Invalid input"),
            "{}",
            result.all_text()
        );
        assert!(result.all_text().contains("/name"), "{}", result.all_text());
        assert_eq!(client.call_tool_json("calls", json!({})).await["calls"], 0);

        // The same tool runs once the arguments are valid.
        client.call_tool("counted", json!({ "name": "x" })).await;
        assert_eq!(client.call_tool_json("calls", json!({})).await["calls"], 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reload_swaps_the_whole_set_with_one_notification() {
    local(async {
        let (mut client, lua) = serve(Path::new(SAMPLES)).await;
        client.drain_notifications();

        let dir = tool_dir(&[
            (
                "greet.lua",
                r#"return { name = "greet", description = "changed", input = { type = "object" },
                  run = function() return "hi" end }"#,
            ),
            (
                "extra.lua",
                r#"return { name = "extra", description = "new", input = { type = "object" },
                  run = function() return 42 end }"#,
            ),
        ]);
        let change = lua.load_dir(dir.path()).await.unwrap();
        assert_eq!(change.added, ["extra"]);
        assert_eq!(change.removed, ["countdown"]);
        assert_eq!(change.replaced, ["greet"]);

        let tools = client.list_tools().await;
        assert_eq!(names(&tools), ["extra", "greet"]);
        assert_eq!(client.drain_notifications().len(), 1);

        // New code runs, and a non-string, non-table return becomes text.
        assert_eq!(
            client.call_tool("greet", json!({})).await.first_text(),
            Some("hi")
        );
        assert_eq!(
            client.call_tool("extra", json!({})).await.first_text(),
            Some("42")
        );
        // A tool the reload removed is an unknown tool: a JSON-RPC error.
        let gone = client
            .call_tool_expect_error("countdown", json!({ "seconds": 1 }))
            .await;
        assert_eq!(gone["code"], -32602);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_bad_directory_names_every_problem_and_changes_nothing() {
    local(async {
        let (mut client, lua) = serve(Path::new(SAMPLES)).await;
        client.drain_notifications();

        let dir = tool_dir(&[
            ("fine.lua", r#"return { name = "fine", description = "d", input = { type = "object" }, run = print }"#),
            ("schema.lua", r#"return { name = "s", description = "d", input = { type = "nope" }, run = print }"#),
            ("syntax.lua", "return {"),
            ("dup_a.lua", DUP),
            ("dup_b.lua", DUP),
            ("missing.lua", r#"return { name = "m", input = { type = "object" }, run = print }"#),
            ("name.lua", r#"return { name = "bad name", description = "d", input = { type = "object" }, run = print }"#),
        ]);
        let error = lua.load_dir(dir.path()).await.unwrap_err();
        let text = error.to_string();
        for file in ["schema.lua", "syntax.lua", "missing.lua", "name.lua"] {
            assert!(text.contains(file), "{file} not named in:\n{text}");
        }
        assert!(text.contains("dup_b.lua") && text.contains("already defined"), "{text}");
        assert!(!text.contains("fine.lua"), "{text}");
        assert!(text.contains("/type"), "the schema location is missing:\n{text}");
        assert!(text.contains("description"), "the missing field is not named:\n{text}");

        // The previous set is still served, and nothing was announced.
        assert_eq!(names(&client.list_tools().await), ["countdown", "greet"]);
        let result = client.call_tool("greet", json!({ "name": "again" })).await;
        assert_eq!(result.structured_content, Some(json!({ "greeting": "hello, again" })));
        assert!(client.drain_notifications().is_empty());

        // A directory that cannot be read is an error too.
        let missing = dir.path().join("absent");
        assert!(lua.load_dir(&missing).await.is_err());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lua_errors_become_error_results() {
    local(async {
        let dir = tool_dir(&[(
            "fail.lua",
            r#"return { name = "fail", description = "d", input = { type = "object" },
              run = function(args)
                if args.table then error({ kind = "not_found", message = "no such thing", id = 7 }) end
                error("plain failure")
              end }"#,
        )]);
        let (mut client, _lua) = serve(dir.path()).await;

        let plain = client.call_tool("fail", json!({})).await;
        assert!(plain.is_error);
        assert!(plain.all_text().contains("plain failure"), "{}", plain.all_text());

        let table = client.call_tool("fail", json!({ "table": true })).await;
        assert!(table.is_error);
        assert_eq!(table.first_text(), Some("no such thing"));
        assert_eq!(
            table.structured_content,
            Some(json!({ "id": 7, "kind": "not_found", "message": "no such thing" }))
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_call_stops_its_coroutine() {
    local(async {
        // `spin` bumps `ticks` every 20 ms for two seconds; `peek` reads it.
        let dir = tool_dir(&[
            (
                "spin.lua",
                r#"ticks = 0
                return { name = "spin", description = "d", input = { type = "object" },
                  run = function()
                    for _ = 1, 100 do ticks = ticks + 1; sleep(0.02) end
                    return "finished"
                  end }"#,
            ),
            (
                "peek.lua",
                r#"return { name = "peek", description = "d", input = { type = "object" },
                  run = function() return { ticks = ticks } end }"#,
            ),
        ]);
        let (mut client, lua) = serve(dir.path()).await;

        // Start the call with a context we control, as a transport would, then
        // cancel it the way `notifications/cancelled` does.
        let spin = lua
            .registry()
            .list()
            .into_iter()
            .find(|t| t.name == "spin")
            .unwrap();
        let ctx = RequestContext::new(RequestId::Number(1));
        let canceller = ctx.clone();
        let call = tokio::task::spawn_local(spin.call_with_context(ctx, json!({})));
        tokio::time::sleep(Duration::from_millis(100)).await;
        canceller.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .expect("the cancelled call should end promptly")
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.first_text(), Some("cancelled"));

        // The coroutine is gone: the counter stops moving.
        let stopped = client.call_tool_json("peek", json!({})).await["ticks"]
            .as_i64()
            .unwrap();
        assert!((1..100).contains(&stopped), "ticks = {stopped}");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let later = client.call_tool_json("peek", json!({})).await["ticks"]
            .as_i64()
            .unwrap();
        assert_eq!(
            stopped, later,
            "the call kept running after it was cancelled"
        );
    })
    .await;
}
