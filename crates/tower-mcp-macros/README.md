# tower-mcp-macros

Optional procedural macros for [`tower-mcp`](https://crates.io/crates/tower-mcp).

The crate provides `#[tool_fn]`, `#[prompt_fn]`, `#[resource_fn]`, and
`#[resource_template_fn]`. Most users should enable the `macros` feature on
`tower-mcp` instead of depending on this crate directly.

See the [tower-mcp documentation](https://docs.rs/tower-mcp) for usage and the
builder APIs that these macros generate.
