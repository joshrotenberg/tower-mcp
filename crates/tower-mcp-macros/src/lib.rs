//! Proc macros for tower-mcp.
//!
//! Provides `#[tool_fn]` and `#[prompt_fn]` as optional syntactic sugar over
//! [`ToolBuilder`] and [`PromptBuilder`]. The macros generate builder code --
//! you can always eject to the builder pattern for full control.
//!
//! Enable via the `macros` feature on `tower-mcp`:
//!
//! ```toml
//! tower-mcp = { version = "0.8", features = ["macros"] }
//! ```
//!
//! # Tool Example
//!
//! ```rust,ignore
//! use tower_mcp::tool_fn;
//! use tower_mcp::CallToolResult;
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct AddInput { a: i64, b: i64 }
//!
//! #[tool_fn(description = "Add two numbers")]
//! async fn add(input: AddInput) -> Result<CallToolResult, tower_mcp::Error> {
//!     Ok(CallToolResult::text(format!("{}", input.a + input.b)))
//! }
//!
//! // Generates `add_tool() -> tower_mcp::tool::Tool`
//! ```
//!
//! # Prompt Example
//!
//! ```rust,ignore
//! use std::collections::HashMap;
//! use tower_mcp::prompt_fn;
//! use tower_mcp::protocol::GetPromptResult;
//!
//! #[prompt_fn(description = "Greet someone", args(name = "Name to greet"))]
//! async fn greet(args: HashMap<String, String>) -> Result<GetPromptResult, tower_mcp::Error> {
//!     let name = args.get("name").cloned().unwrap_or_default();
//!     Ok(GetPromptResult::user_message(format!("Hello, {name}!")))
//! }
//!
//! // Generates `greet_prompt() -> tower_mcp::prompt::Prompt`
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, LitStr, Token, parse_macro_input};

// ---------------------------------------------------------------------------
// tool_fn attrs
// ---------------------------------------------------------------------------

struct ToolAttrs {
    name: Option<String>,
    description: Option<String>,
}

impl syn::parse::Parse for ToolAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut description = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let lit: LitStr = input.parse()?;

            if ident == "name" {
                name = Some(lit.value());
            } else if ident == "description" {
                description = Some(lit.value());
            } else {
                return Err(syn::Error::new_spanned(
                    ident,
                    "unsupported attribute (expected `name` or `description`)",
                ));
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(ToolAttrs { name, description })
    }
}

// ---------------------------------------------------------------------------
// prompt_fn attrs
// ---------------------------------------------------------------------------

struct PromptArg {
    name: String,
    description: String,
    required: bool,
}

struct PromptAttrs {
    name: Option<String>,
    description: Option<String>,
    args: Vec<PromptArg>,
}

impl syn::parse::Parse for PromptAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut description = None;
        let mut args = Vec::new();

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;

            if ident == "args" {
                // args(name = "desc", ?optional_name = "desc")
                let content;
                syn::parenthesized!(content in input);
                while !content.is_empty() {
                    // Check for `?` prefix (optional)
                    let required = !content.peek(Token![?]);
                    if !required {
                        content.parse::<Token![?]>()?;
                    }

                    let arg_name: syn::Ident = content.parse()?;
                    content.parse::<Token![=]>()?;
                    let arg_desc: LitStr = content.parse()?;

                    args.push(PromptArg {
                        name: arg_name.to_string(),
                        description: arg_desc.value(),
                        required,
                    });

                    if !content.is_empty() {
                        content.parse::<Token![,]>()?;
                    }
                }
            } else {
                input.parse::<Token![=]>()?;
                let lit: LitStr = input.parse()?;

                if ident == "name" {
                    name = Some(lit.value());
                } else if ident == "description" {
                    description = Some(lit.value());
                } else {
                    return Err(syn::Error::new_spanned(
                        ident,
                        "unsupported attribute (expected `name`, `description`, or `args`)",
                    ));
                }
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(PromptAttrs {
            name,
            description,
            args,
        })
    }
}

/// Generates a tool constructor function from an async handler.
///
/// Applies to an `async fn` that takes a single typed input parameter
/// (implementing `Deserialize + JsonSchema`) and returns
/// `Result<CallToolResult, tower_mcp::Error>`.
///
/// # Attributes
///
/// - `description = "..."` -- tool description (required)
/// - `name = "..."` -- override the tool name (defaults to function name)
///
/// # Generated Code
///
/// For a function named `add`, generates `add_tool() -> tower_mcp::tool::Tool`.
///
/// The generated function calls `ToolBuilder` internally, so you can inspect
/// the expansion with `cargo expand`.
///
/// # Example
///
/// ```rust,ignore
/// #[tool_fn(description = "Add two numbers")]
/// async fn add(input: AddInput) -> Result<CallToolResult, tower_mcp::Error> {
///     Ok(CallToolResult::text(format!("{}", input.a + input.b)))
/// }
///
/// // Use it:
/// let router = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .tool(add_tool());
/// ```
#[proc_macro_attribute]
pub fn tool_fn(args: TokenStream, input: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(args as ToolAttrs);
    let func = parse_macro_input!(input as ItemFn);

    let fn_name = &func.sig.ident;
    let tool_name_str = attrs
        .name
        .unwrap_or_else(|| fn_name.to_string().replace('_', "-"));

    let description = match attrs.description {
        Some(d) => quote! { .description(#d) },
        None => quote! {},
    };

    // Extract the input type from the function signature
    let inputs = &func.sig.inputs;
    if inputs.len() != 1 {
        return syn::Error::new_spanned(
            &func.sig,
            "tool_fn handler must have exactly one parameter (the input type)",
        )
        .to_compile_error()
        .into();
    }

    let input_arg = inputs.first().unwrap();
    let input_type = match input_arg {
        syn::FnArg::Typed(pat_type) => &pat_type.ty,
        syn::FnArg::Receiver(_) => {
            return syn::Error::new_spanned(input_arg, "tool_fn handler cannot take self")
                .to_compile_error()
                .into();
        }
    };

    // Generate the constructor function name: {fn_name}_tool
    let constructor_name = syn::Ident::new(&format!("{fn_name}_tool"), fn_name.span());

    let expanded = quote! {
        // Keep the original function
        #func

        /// Creates a [`Tool`](tower_mcp::tool::Tool) from the `#[tool_fn]`-annotated handler.
        ///
        /// Generated by `tower-mcp-macros`. Equivalent to:
        /// ```ignore
        /// ToolBuilder::new(#tool_name_str)
        ///     #description
        ///     .handler(|input| #fn_name(input))
        ///     .build()
        /// ```
        pub fn #constructor_name() -> tower_mcp::tool::Tool {
            tower_mcp::ToolBuilder::new(#tool_name_str)
                #description
                .handler(|input: #input_type| #fn_name(input))
                .build()
        }
    };

    expanded.into()
}

/// Generates a prompt constructor function from an async handler.
///
/// Applies to an `async fn` that takes `HashMap<String, String>` and returns
/// `Result<GetPromptResult, tower_mcp::Error>`.
///
/// # Attributes
///
/// - `description = "..."` -- prompt description
/// - `name = "..."` -- override the prompt name (defaults to function name with `_` -> `-`)
/// - `args(name = "description", ?optional_name = "description")` -- declare arguments;
///   prefix with `?` to mark as optional (default is required)
///
/// # Generated Code
///
/// For a function named `greet`, generates `greet_prompt() -> tower_mcp::prompt::Prompt`.
///
/// # Example
///
/// ```rust,ignore
/// #[prompt_fn(description = "Greet someone", args(name = "Name to greet"))]
/// async fn greet(args: HashMap<String, String>) -> Result<GetPromptResult, tower_mcp::Error> {
///     let name = args.get("name").cloned().unwrap_or_default();
///     Ok(GetPromptResult::user_message(format!("Hello, {name}!")))
/// }
///
/// // Use it:
/// let router = McpRouter::new()
///     .server_info("my-server", "1.0.0")
///     .prompt(greet_prompt());
/// ```
#[proc_macro_attribute]
pub fn prompt_fn(args: TokenStream, input: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(args as PromptAttrs);
    let func = parse_macro_input!(input as ItemFn);

    let fn_name = &func.sig.ident;
    let prompt_name_str = attrs
        .name
        .unwrap_or_else(|| fn_name.to_string().replace('_', "-"));

    let description = match attrs.description {
        Some(d) => quote! { .description(#d) },
        None => quote! {},
    };

    // Validate: handler must take exactly one parameter (HashMap<String, String>)
    let inputs = &func.sig.inputs;
    if inputs.len() != 1 {
        return syn::Error::new_spanned(
            &func.sig,
            "prompt_fn handler must have exactly one parameter (args: HashMap<String, String>)",
        )
        .to_compile_error()
        .into();
    }

    let input_arg = inputs.first().unwrap();
    if let syn::FnArg::Receiver(_) = input_arg {
        return syn::Error::new_spanned(input_arg, "prompt_fn handler cannot take self")
            .to_compile_error()
            .into();
    }

    // Generate argument builder calls
    let arg_calls: Vec<_> = attrs
        .args
        .iter()
        .map(|arg| {
            let name = &arg.name;
            let desc = &arg.description;
            if arg.required {
                quote! { .required_arg(#name, #desc) }
            } else {
                quote! { .optional_arg(#name, #desc) }
            }
        })
        .collect();

    let constructor_name = syn::Ident::new(&format!("{fn_name}_prompt"), fn_name.span());

    let expanded = quote! {
        #func

        /// Creates a [`Prompt`](tower_mcp::prompt::Prompt) from the `#[prompt_fn]`-annotated handler.
        ///
        /// Generated by `tower-mcp-macros`.
        pub fn #constructor_name() -> tower_mcp::prompt::Prompt {
            tower_mcp::PromptBuilder::new(#prompt_name_str)
                #description
                #(#arg_calls)*
                .handler(|args: std::collections::HashMap<String, String>| #fn_name(args))
                .build()
        }
    };

    expanded.into()
}
