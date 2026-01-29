//! HTTP SSE client example demonstrating stream resumption
//!
//! This example shows how to connect to an MCP server's SSE stream and use
//! the Last-Event-ID header for stream resumption (SEP-1699).
//!
//! Run the server first:
//!   cargo run --example http_server --features http
//!
//! Then run this client:
//!   cargo run --example http_sse_client --features http
//!
//! The example demonstrates:
//! 1. Initializing an MCP session
//! 2. Connecting to the SSE stream
//! 3. Calling a tool that generates progress events
//! 4. Tracking event IDs as they arrive
//! 5. Reconnecting with Last-Event-ID to resume the stream

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Mutex;

const SERVER_URL: &str = "http://127.0.0.1:3000";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("http_sse_client=debug")
        .init();

    let client = Client::new();

    // Step 1: Initialize MCP session
    println!("Initializing MCP session...");
    let init_response = client
        .post(SERVER_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "sse-client-example",
                    "version": "1.0.0"
                }
            }
        }))
        .send()
        .await?;

    let session_id = init_response
        .headers()
        .get("mcp-session-id")
        .ok_or("No session ID in response")?
        .to_str()?
        .to_string();

    println!("Session ID: {}", session_id);

    // Parse the response to confirm initialization
    let init_result: serde_json::Value = init_response.json().await?;
    println!(
        "Connected to: {}",
        init_result["result"]["serverInfo"]["name"]
    );
    println!();

    // Step 2: Start slow_task (10 steps) but only listen for a few events
    // This simulates a client that disconnects mid-stream
    println!("=== PHASE 1: Connect and receive some events ===\n");
    println!(
        "Starting slow_task with 10 steps, but we'll disconnect after receiving ~3 events...\n"
    );

    let last_event_id = connect_and_disconnect_early(&client, &session_id).await?;

    println!("\n=== PHASE 2: Client is 'offline' ===\n");
    println!("Client disconnected! But the server is still sending events...");
    println!("These events are being buffered on the server.\n");

    // Wait while more events are generated (they'll be buffered)
    println!("Waiting 2 seconds while server continues generating events...\n");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 3: Reconnect with Last-Event-ID to see replay
    if let Some(id) = last_event_id {
        println!("=== PHASE 3: Reconnect with Last-Event-ID ===\n");
        println!("Reconnecting with Last-Event-ID: {}", id);
        println!(
            "The server will REPLAY any buffered events with ID > {}\n",
            id
        );

        reconnect_and_show_replay(&client, &session_id, id).await?;
    }

    println!("\n=== Demo complete! ===");
    println!("Notice how missed events were replayed before new events arrived.");

    Ok(())
}

/// Connect to SSE, start slow_task, but disconnect after receiving a few events
async fn connect_and_disconnect_early(
    client: &Client,
    session_id: &str,
) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let response = client
        .get(SERVER_URL)
        .header("Accept", "text/event-stream")
        .header("MCP-Session-Id", session_id)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        return Err(format!("SSE connection failed: {} - {}", status, body).into());
    }

    println!("SSE stream connected.");

    // Track the last event ID and count
    let last_id: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let event_count = Arc::new(Mutex::new(0u32));

    // Spawn a task to call slow_task with 10 steps (takes ~3 seconds total)
    let tool_client = client.clone();
    let tool_session = session_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = tool_client
            .post(SERVER_URL)
            .header("Content-Type", "application/json")
            .header("MCP-Session-Id", &tool_session)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "tools/call",
                "params": {
                    "name": "slow_task",
                    "arguments": {
                        "steps": 10,
                        "delay_ms": 300
                    },
                    "_meta": {
                        "progressToken": "demo-progress"
                    }
                }
            }))
            .send()
            .await;
    });

    // Read the SSE stream but disconnect after 3 events
    let mut stream = response.bytes_stream();
    let disconnect_after = 3;

    use futures::StreamExt;

    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                if let Some(id) = parse_sse_events(&text) {
                    *last_id.lock().await = Some(id);
                    let mut count = event_count.lock().await;
                    *count += 1;

                    if *count >= disconnect_after {
                        println!("\n  ** Simulating disconnect after {} events! **", count);
                        break;
                    }
                }
            }
            Some(Err(e)) => {
                println!("Stream error: {}", e);
                break;
            }
            None => {
                println!("Stream ended");
                break;
            }
        }
    }

    let result = *last_id.lock().await;
    Ok(result)
}

/// Reconnect with Last-Event-ID and show replayed + new events
async fn reconnect_and_show_replay(
    client: &Client,
    session_id: &str,
    last_event_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = client
        .get(SERVER_URL)
        .header("Accept", "text/event-stream")
        .header("MCP-Session-Id", session_id)
        .header("Last-Event-ID", last_event_id.to_string())
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        return Err(format!("SSE connection failed: {} - {}", status, body).into());
    }

    println!("Reconnected! Watching for replayed events...\n");

    let mut stream = response.bytes_stream();
    let timeout = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(timeout);

    let mut replay_count = 0;
    let mut seen_ids = Vec::new();

    use futures::StreamExt;

    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes);
                        if let Some((id, is_replay)) = parse_sse_events_with_replay_detection(&text, last_event_id, &mut seen_ids) {
                            if is_replay {
                                replay_count += 1;
                                println!("  >> REPLAYED Event (id: {}) - this was buffered while we were offline!", id);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        println!("Stream error: {}", e);
                        break;
                    }
                    None => {
                        println!("Stream ended");
                        break;
                    }
                }
            }
            _ = &mut timeout => {
                break;
            }
        }
    }

    println!(
        "\nReplay summary: {} events were replayed from the buffer",
        replay_count
    );

    Ok(())
}

/// Parse SSE events and detect if they're replayed (ID <= last_event_id we sent)
fn parse_sse_events_with_replay_detection(
    text: &str,
    last_sent_id: u64,
    seen_ids: &mut Vec<u64>,
) -> Option<(u64, bool)> {
    let mut result: Option<(u64, bool)> = None;
    let mut current_id: Option<u64> = None;
    let mut current_data: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            if let Some(id) = current_id {
                if !seen_ids.contains(&id) {
                    seen_ids.push(id);
                    // Events replayed have IDs that came after our last_sent_id
                    // but arrived immediately on reconnect (before any new events)
                    let is_replay = id > last_sent_id && seen_ids.len() <= 10; // heuristic
                    result = Some((id, is_replay));

                    if !current_data.is_empty() {
                        let data = current_data.join("");
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                            if let Some(method) = json.get("method").and_then(|m| m.as_str()) {
                                if !is_replay {
                                    println!("  Event (id: {}): {}", id, method);
                                }
                            }
                        }
                    }
                }
            }
            current_id = None;
            current_data.clear();
        } else if let Some(value) = line.strip_prefix("id:") {
            current_id = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("data:") {
            current_data.push(value.trim().to_string());
        }
    }

    result
}

/// Parse SSE events from a chunk of text
/// Returns the last event ID found, if any
fn parse_sse_events(text: &str) -> Option<u64> {
    let mut last_id: Option<u64> = None;
    let mut current_id: Option<u64> = None;
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            // Empty line = end of event
            if !current_data.is_empty() {
                let data = current_data.join("\n");
                let event_type = current_event.as_deref().unwrap_or("message");
                let id_str = current_id
                    .map(|id| format!(" (id: {})", id))
                    .unwrap_or_default();

                println!("  Event{}: type={}", id_str, event_type);

                // Try to parse the data as JSON for nicer output
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                    if let Some(method) = json.get("method").and_then(|m| m.as_str()) {
                        println!("    method: {}", method);
                    }
                } else {
                    // Not JSON, print as-is (might be a ping)
                    if data != "ping" {
                        println!("    data: {}", data);
                    }
                }

                if let Some(id) = current_id {
                    last_id = Some(id);
                }
            }

            // Reset for next event
            current_id = None;
            current_event = None;
            current_data.clear();
        } else if let Some(value) = line.strip_prefix("id:") {
            current_id = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("event:") {
            current_event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            current_data.push(value.trim().to_string());
        } else if line.starts_with(':') {
            // Comment line (used for keep-alive)
            let comment = line.trim_start_matches(':').trim();
            if !comment.is_empty() {
                println!("  (keep-alive: {})", comment);
            }
        }
    }

    last_id
}
