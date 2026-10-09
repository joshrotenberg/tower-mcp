//! Protocol shape checks and opt-in JSON Schema validation for tools.
//!
//! With the `schema-validation` feature, every tool checks `arguments` against
//! its advertised `inputSchema` before the handler runs, and a result's
//! `structuredContent` against its `outputSchema` before the result is
//! returned. Both schemas are compiled once, when the tool is built.
//!
//! The check wraps the tool's service, MRTR handler, and live handler, the same
//! places [`Tool::with_guard`] wraps, so it covers every handler kind and both
//! synchronous and task-backed calls. The structured-content shape check
//! runs independently of the feature and the declared schemas.

use super::*;

/// Per-tool switches for schema validation, set on [`ToolBuilder`].
///
/// Both default to on. Output also controls the always-on shape check.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "schema-validation"), allow(dead_code))]
pub(crate) struct SchemaValidation {
    pub(crate) input: bool,
    pub(crate) output: bool,
}

impl Default for SchemaValidation {
    fn default() -> Self {
        Self {
            input: true,
            output: true,
        }
    }
}

impl Tool {
    /// Enforce this tool's schemas around every handler it has.
    ///
    /// Compiles the input schema and, when one is declared, the output schema.
    /// A schema that does not compile is logged and left unchecked; it does
    /// not panic, because a tool built from a backend's schema (a proxy, a
    /// dynamic registration) must not take the process down.
    /// Always wraps because the structured-content shape check applies to
    /// every tool regardless of the feature or declared schemas (#1540), so
    /// no tool may bypass the wrapper.
    pub(crate) fn with_schema_validation(self, validation: SchemaValidation) -> Self {
        let schemas = Arc::new(engine::ToolChecks {
            // Like the schema check, diagnostics retain the build-time name;
            // later with_name_prefix calls (including nesting) are not reflected.
            tool: self.name.clone(),
            check_shape: validation.output,
            #[cfg(feature = "schema-validation")]
            schemas: engine::ToolSchemas::compile(
                &self.name,
                validation.input.then_some(&self.input_schema),
                validation
                    .output
                    .then_some(self.output_schema.as_ref())
                    .flatten(),
            ),
        });
        Tool {
            service: self.service.map(|inner| {
                BoxCloneService::new(engine::ValidateService {
                    inner,
                    schemas: schemas.clone(),
                })
            }),
            #[cfg(feature = "stateless")]
            mrtr_handler: self.mrtr_handler.map(|inner| {
                Arc::new(engine::ValidatedMrtrToolHandler {
                    inner,
                    schemas: schemas.clone(),
                }) as Arc<dyn MrtrToolHandler>
            }),
            live_handler: self.live_handler.map(|inner| {
                Arc::new(engine::ValidatedLiveToolHandler {
                    inner,
                    schemas: schemas.clone(),
                }) as Arc<dyn LiveToolHandler>
            }),
            ..self
        }
    }
}

mod engine {
    use super::*;

    #[cfg(feature = "schema-validation")]
    use jsonschema::Validator;

    /// Most violations named in one error result. A model needs the first few
    /// to correct a call; the rest would only bloat the message.
    #[cfg(feature = "schema-validation")]
    const MAX_REPORTED_ERRORS: usize = 5;

    /// Longest a single violation is allowed to run, in characters. A
    /// violation echoes the offending value, which can be arbitrarily large.
    #[cfg(feature = "schema-validation")]
    const MAX_ERROR_CHARS: usize = 300;

    /// The compiled schemas of one tool.
    #[cfg(feature = "schema-validation")]
    pub(super) struct ToolSchemas {
        tool: String,
        input: Option<Validator>,
        output: Option<Validator>,
    }

    #[cfg(feature = "schema-validation")]
    fn compile(tool: &str, which: &'static str, schema: &Value) -> Option<Validator> {
        // Formats are annotations by default in draft 2019-09 and later. The
        // schemas here are contracts with a caller, so `format` is enforced.
        match jsonschema::options()
            .should_validate_formats(true)
            .build(schema)
        {
            Ok(validator) => Some(validator),
            Err(error) => {
                tracing::error!(
                    target: "mcp::tools",
                    tool,
                    schema = which,
                    %error,
                    "tool schema does not compile; it will not be validated"
                );
                None
            }
        }
    }

    /// One line per violation, up to [`MAX_REPORTED_ERRORS`], or `None` when
    /// the instance is valid.
    #[cfg(feature = "schema-validation")]
    fn violations(validator: &Validator, instance: &Value) -> Option<String> {
        let mut errors = validator.iter_errors(instance);
        let lines: Vec<String> = errors
            .by_ref()
            .take(MAX_REPORTED_ERRORS)
            .map(|error| {
                let location = match error.instance_path().as_str() {
                    "" => "(root)",
                    path => path,
                };
                let mut message = error.to_string();
                if let Some((end, _)) = message.char_indices().nth(MAX_ERROR_CHARS) {
                    message.truncate(end);
                    message.push_str("...");
                }
                // The last segment of the schema path is the keyword that failed.
                match error.schema_path().as_str().rsplit('/').next() {
                    Some(rule) if !rule.is_empty() => {
                        format!("- at {location}: {message} ({rule})")
                    }
                    _ => format!("- at {location}: {message}"),
                }
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        let mut report = lines.join("\n");
        if errors.next().is_some() {
            report.push_str("\n- and more violations");
        }
        Some(report)
    }

    #[cfg(feature = "schema-validation")]
    impl ToolSchemas {
        /// `None` when neither schema is to be checked, or neither compiled.
        pub(super) fn compile(
            tool: &str,
            input: Option<&Value>,
            output: Option<&Value>,
        ) -> Option<Self> {
            let input = input.and_then(|schema| compile(tool, "inputSchema", schema));
            let output = output.and_then(|schema| compile(tool, "outputSchema", schema));
            if input.is_none() && output.is_none() {
                return None;
            }
            Some(Self {
                tool: tool.to_string(),
                input,
                output,
            })
        }

        /// The error message for arguments that violate the input schema.
        fn check_input(&self, arguments: &Value) -> Option<String> {
            let validator = self.input.as_ref()?;
            // `arguments` is optional in `tools/call`, and an omitted one
            // arrives as null. An input schema describes an object, so an
            // omitted value is checked as an empty one, unless the schema
            // takes null itself (a handler declared over `()`).
            let empty = Value::Object(Map::new());
            let instance = if arguments.is_null() {
                if validator.is_valid(arguments) {
                    return None;
                }
                &empty
            } else {
                arguments
            };
            let report = violations(validator, instance)?;
            tracing::debug!(
                target: "mcp::tools",
                tool = %self.tool,
                %report,
                "tool arguments do not match the input schema"
            );
            // Same prefix as the serde failure a typed handler reports, so a
            // caller matching on it sees one shape with or without the feature.
            Some(format!(
                "Invalid input: the arguments do not match the tool's input schema.\n{report}"
            ))
        }

        /// The result itself, or an error result when its `structuredContent`
        /// violates the output schema.
        fn check_output(&self, result: CallToolResult) -> CallToolResult {
            let Some(validator) = self.output.as_ref() else {
                return result;
            };
            // An error result is not a conforming structured result and
            // carries no structured content of its own to check.
            if result.is_error {
                return result;
            }
            let Some(structured) = result.structured_content.as_ref() else {
                return result;
            };
            let Some(report) = violations(validator, structured) else {
                return result;
            };
            // The caller cannot fix this: the handler produced a result that
            // contradicts the schema it advertises.
            tracing::warn!(
                target: "mcp::tools",
                tool = %self.tool,
                %report,
                "tool structuredContent does not match the output schema"
            );
            CallToolResult::error(format!(
                "Tool returned structured content that does not match its output schema.\n{report}"
            ))
        }
    }

    pub(super) struct ToolChecks {
        pub(super) tool: String,
        pub(super) check_shape: bool,
        #[cfg(feature = "schema-validation")]
        pub(super) schemas: Option<ToolSchemas>,
    }

    impl ToolChecks {
        fn check_input(&self, _arguments: &Value) -> Option<String> {
            #[cfg(feature = "schema-validation")]
            return self.schemas.as_ref()?.check_input(_arguments);
            #[cfg(not(feature = "schema-validation"))]
            None
        }

        fn check_output(&self, result: CallToolResult, final_protocol: bool) -> CallToolResult {
            // #1540: constructors cannot know the request revision. Older MCP
            // revisions require objects, while 2026-07-28 permits any JSON value.
            if self.check_shape
                && !final_protocol
                && !result.is_error
                && let Some(structured) = result.structured_content.as_ref()
            {
                let kind = match structured {
                    Value::Object(_) => None,
                    Value::Array(_) => Some("array"),
                    Value::String(_) => Some("string"),
                    Value::Number(_) => Some("number"),
                    Value::Bool(_) => Some("boolean"),
                    Value::Null => Some("null"),
                };
                if let Some(kind) = kind {
                    let message = format!(
                        "Tool '{}' returned structuredContent of type {kind}; MCP 2025-11-25 and earlier require a JSON object. Wrap a list with CallToolResult::from_list or return an object.",
                        self.tool,
                    );
                    tracing::warn!(
                        target: "mcp::tools",
                        tool = %self.tool,
                        %message,
                        "tool structuredContent has an invalid shape for the protocol revision"
                    );
                    return CallToolResult::error(message);
                }
            }
            #[cfg(feature = "schema-validation")]
            if let Some(schemas) = &self.schemas {
                return schemas.check_output(result);
            }
            result
        }
    }

    /// Validates a tool service's arguments before it runs and its result
    /// after.
    #[derive(Clone)]
    pub(super) struct ValidateService<S> {
        pub(super) inner: S,
        pub(super) schemas: Arc<ToolChecks>,
    }

    impl<S> Service<ToolRequest> for ValidateService<S>
    where
        S: Service<ToolRequest, Response = CallToolResult, Error = Infallible>,
        S::Future: Send + 'static,
    {
        type Response = CallToolResult;
        type Error = Infallible;
        type Future = BoxFuture<'static, std::result::Result<CallToolResult, Infallible>>;

        fn poll_ready(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, req: ToolRequest) -> Self::Future {
            if let Some(message) = self.schemas.check_input(&req.args) {
                return Box::pin(std::future::ready(Ok(CallToolResult::error(message))));
            }
            let final_protocol = crate::router::is_final_protocol_request(req.ctx.extensions());
            let future = self.inner.call(req);
            let schemas = self.schemas.clone();
            Box::pin(async move {
                future
                    .await
                    .map(|result| schemas.check_output(result, final_protocol))
            })
        }
    }

    /// The same check around an MRTR handler. Only a completed outcome has a
    /// result to check; an input-required outcome passes through.
    #[cfg(feature = "stateless")]
    pub(super) struct ValidatedMrtrToolHandler {
        pub(super) inner: Arc<dyn MrtrToolHandler>,
        pub(super) schemas: Arc<ToolChecks>,
    }

    #[cfg(feature = "stateless")]
    impl MrtrToolHandler for ValidatedMrtrToolHandler {
        fn call(
            &self,
            ctx: RequestContext,
            args: Value,
        ) -> BoxFuture<'_, Result<RequestOutcome<CallToolResult>>> {
            if let Some(message) = self.schemas.check_input(&args) {
                return Box::pin(async move {
                    Ok(RequestOutcome::Complete(CallToolResult::error(message)))
                });
            }
            let final_protocol = crate::router::is_final_protocol_request(ctx.extensions());
            let future = self.inner.call(ctx, args);
            Box::pin(async move {
                Ok(match future.await? {
                    RequestOutcome::Complete(result) => {
                        RequestOutcome::Complete(self.schemas.check_output(result, final_protocol))
                    }
                    other => other,
                })
            })
        }

        fn input_schema(&self) -> Value {
            self.inner.input_schema()
        }
    }

    /// The same check around a live handler. A rejected call completes the
    /// task with an error result, as a rejected guard does.
    pub(super) struct ValidatedLiveToolHandler {
        pub(super) inner: Arc<dyn LiveToolHandler>,
        pub(super) schemas: Arc<ToolChecks>,
    }

    #[async_trait::async_trait]
    impl LiveToolHandler for ValidatedLiveToolHandler {
        async fn call(
            &self,
            ctx: RequestContext,
            task: TaskContext,
            arguments: Value,
        ) -> Result<TaskOutcome> {
            if let Some(message) = self.schemas.check_input(&arguments) {
                return Ok(TaskOutcome::Completed(CallToolResult::error(message)));
            }
            let final_protocol = crate::router::is_final_protocol_request(ctx.extensions());
            Ok(match self.inner.call(ctx, task, arguments).await? {
                TaskOutcome::Completed(result) => {
                    TaskOutcome::Completed(self.schemas.check_output(result, final_protocol))
                }
                other => other,
            })
        }
    }
}

#[cfg(test)]
mod tests;
