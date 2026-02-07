//! Weather MCP server example
//!
//! A tower-mcp implementation of the official MCP quickstart weather server.
//! Uses the National Weather Service API to provide weather forecasts and alerts.
//!
//! Run with: cargo run --example weather_server
//!
//! Configure in Claude Desktop:
//! ```json
//! {
//!   "mcpServers": {
//!     "weather": {
//!       "command": "cargo",
//!       "args": ["run", "--example", "weather_server", "--manifest-path", "/path/to/tower-mcp/Cargo.toml"]
//!     }
//!   }
//! }
//! ```
//!
//! Example queries:
//! - "What's the weather forecast for San Francisco?" (lat: 37.7749, lon: -122.4194)
//! - "Are there any weather alerts in California?"

use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, McpRouter, StdioTransport, ToolBuilder};

const NWS_API_BASE: &str = "https://api.weather.gov";
const USER_AGENT: &str = "tower-mcp-weather/1.0";

// --- NWS API Response Types ---

#[derive(Debug, Deserialize)]
struct AlertsResponse {
    features: Vec<AlertFeature>,
}

#[derive(Debug, Deserialize)]
struct AlertFeature {
    properties: AlertProperties,
}

#[derive(Debug, Deserialize)]
struct AlertProperties {
    event: Option<String>,
    #[serde(rename = "areaDesc")]
    area_desc: Option<String>,
    severity: Option<String>,
    description: Option<String>,
    instruction: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PointsResponse {
    properties: PointsProperties,
}

#[derive(Debug, Deserialize)]
struct PointsProperties {
    forecast: String,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
struct ForecastProperties {
    periods: Vec<ForecastPeriod>,
}

#[derive(Debug, Deserialize)]
struct ForecastPeriod {
    name: String,
    temperature: i32,
    #[serde(rename = "temperatureUnit")]
    temperature_unit: String,
    #[serde(rename = "windSpeed")]
    wind_speed: String,
    #[serde(rename = "windDirection")]
    wind_direction: String,
    #[serde(rename = "detailedForecast")]
    detailed_forecast: String,
}

// --- Tool Input Types ---

#[derive(Debug, Deserialize, JsonSchema)]
struct GetForecastInput {
    /// Latitude of the location
    latitude: f64,
    /// Longitude of the location
    longitude: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetAlertsInput {
    /// Two-letter US state code (e.g., "CA", "NY")
    state: String,
}

// --- Helper Functions ---

async fn make_nws_request<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let client = Client::new();
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/geo+json")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("API error: {e}"))?;

    response
        .json::<T>()
        .await
        .map_err(|e| format!("Parse error: {e}"))
}

fn format_alert(feature: &AlertFeature) -> String {
    let props = &feature.properties;
    format!(
        "Event: {}\nArea: {}\nSeverity: {}\nDescription: {}\nInstructions: {}",
        props.event.as_deref().unwrap_or("Unknown"),
        props.area_desc.as_deref().unwrap_or("Unknown"),
        props.severity.as_deref().unwrap_or("Unknown"),
        props.description.as_deref().unwrap_or("No description"),
        props.instruction.as_deref().unwrap_or("No instructions")
    )
}

fn format_period(period: &ForecastPeriod) -> String {
    format!(
        "{}:\nTemperature: {}{}F\nWind: {} {}\nForecast: {}",
        period.name,
        period.temperature,
        period.temperature_unit.chars().next().unwrap_or('F'),
        period.wind_speed,
        period.wind_direction,
        period.detailed_forecast
    )
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Tracing to stderr (stdout is for JSON-RPC)
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug,weather_server=debug")
        .with_writer(std::io::stderr)
        .init();

    // Build tools
    let get_forecast = ToolBuilder::new("get_forecast")
        .description("Get weather forecast for a location using latitude and longitude")
        .read_only()
        .handler(|input: GetForecastInput| async move {
            let points_url = format!(
                "{}/points/{},{}",
                NWS_API_BASE, input.latitude, input.longitude
            );

            let points_data = match make_nws_request::<PointsResponse>(&points_url).await {
                Ok(data) => data,
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "Failed to get location data: {e}"
                    )));
                }
            };

            let forecast_data = match make_nws_request::<ForecastResponse>(
                &points_data.properties.forecast,
            )
            .await
            {
                Ok(data) => data,
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "Failed to get forecast: {e}"
                    )));
                }
            };

            let forecast = forecast_data
                .properties
                .periods
                .iter()
                .take(5)
                .map(format_period)
                .collect::<Vec<_>>()
                .join("\n---\n");

            Ok(CallToolResult::text(forecast))
        })
        .build();

    let get_alerts = ToolBuilder::new("get_alerts")
        .description("Get weather alerts for a US state (use two-letter state code)")
        .read_only()
        .handler(|input: GetAlertsInput| async move {
            let url = format!(
                "{}/alerts/active/area/{}",
                NWS_API_BASE,
                input.state.to_uppercase()
            );

            match make_nws_request::<AlertsResponse>(&url).await {
                Ok(data) => {
                    if data.features.is_empty() {
                        Ok(CallToolResult::text("No active alerts for this state."))
                    } else {
                        let alerts = data
                            .features
                            .iter()
                            .map(format_alert)
                            .collect::<Vec<_>>()
                            .join("\n---\n");
                        Ok(CallToolResult::text(alerts))
                    }
                }
                Err(e) => Ok(CallToolResult::error(format!(
                    "Failed to fetch alerts: {e}"
                ))),
            }
        })
        .build();

    // Create router
    let router = McpRouter::new()
        .server_info("weather", "1.0.0")
        .instructions(
            "Weather forecast and alerts server using the National Weather Service API. \
             Use get_forecast with latitude/longitude for weather forecasts. \
             Use get_alerts with a two-letter US state code for active weather alerts.",
        )
        .tool(get_forecast)
        .tool(get_alerts);

    // Run stdio transport
    tracing::info!("Starting weather MCP server");
    StdioTransport::new(router).run().await?;

    Ok(())
}
