//! Typed MCP Apps server with an interactive raw-HTML resource.
//!
//! Compile or run with:
//!
//! ```bash
//! cargo run --example mcp_apps --features mcp-apps
//! ```
//!
//! The Cargo feature exposes the typed API. `with_mcp_apps()` is the separate
//! runtime opt-in that advertises `io.modelcontextprotocol/ui`.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{
    CallToolResult, McpAppResourceBuilder, McpAppUri, McpRouter, McpUiResourceMeta, McpUiToolMeta,
    StdioTransport, ToolBuilder, mcp_app_tool_result,
};

const DASHBOARD_URI: &str = "ui://weather/dashboard";

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Weather dashboard</title>
  <style>
    body { font: 16px system-ui; margin: 1rem; }
    output { display: block; font-size: 2rem; font-weight: 700; }
  </style>
</head>
<body>
  <h1>Weather</h1>
  <output id="weather">Waiting for tool result…</output>
  <script>
    window.addEventListener("message", (event) => {
      if (event.data?.method !== "ui/notifications/tool-result") return;
      const data = event.data.params?.structuredContent;
      if (data) document.querySelector("#weather").textContent =
        `${data.temperatureF} °F · ${data.conditions}`;
    });
  </script>
</body>
</html>
"##;

#[derive(Debug, Deserialize, JsonSchema)]
struct WeatherInput {
    city: String,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let dashboard_uri = McpAppUri::new(DASHBOARD_URI)?;

    // Omitting CSP and permissions intentionally selects the Apps
    // specification's restrictive host defaults.
    let dashboard = McpAppResourceBuilder::new(DASHBOARD_URI, "Weather dashboard", DASHBOARD_HTML)?
        .description("Interactive rendering for weather tool results")
        .metadata(McpUiResourceMeta::default().prefers_border(true))
        .build()?;

    let weather = ToolBuilder::new("get_weather")
        .description("Get current weather for a city")
        .read_only()
        .handler(|input: WeatherInput| async move {
            // The text is useful to non-App hosts and the model. The structured
            // object is optimized for the negotiated UI.
            Ok(mcp_app_tool_result(
                format!("{}: 72 °F and sunny", input.city),
                json!({
                    "city": input.city,
                    "temperatureF": 72,
                    "conditions": "sunny"
                }),
            )
            .expect("serde_json::Value serialization is infallible"))
        })
        .build()
        .with_mcp_app(McpUiToolMeta::new(dashboard_uri.clone()))?;

    // App-only tools remain hidden from the model by host policy but can be
    // called by the App on the same server connection.
    let refresh = ToolBuilder::new("refresh_weather")
        .description("Refresh the dashboard's weather data")
        .read_only()
        .no_params_handler(|| async {
            Ok(CallToolResult::text(
                "Dashboard weather data has been refreshed.",
            ))
        })
        .build()
        .with_mcp_app(McpUiToolMeta::app_only(dashboard_uri))?;

    let router = McpRouter::new()
        .server_info("mcp-apps-example", env!("CARGO_PKG_VERSION"))
        .with_mcp_apps()
        .tool(weather)
        .tool(refresh)
        .resource(dashboard);

    StdioTransport::new(router).run().await?;
    Ok(())
}
