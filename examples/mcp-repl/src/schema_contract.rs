//! Saved compatibility contracts for tool schemas and prompt arguments.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_mcp::protocol::{PromptDefinition, ToolDefinition};

const FORMAT_VERSION: u32 = 1;

/// How closely the current server definition must match a saved snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    /// Require a byte-independent canonical JSON match.
    Strict,
    /// Allow changes that preserve existing callers and consumers.
    #[default]
    Compatible,
    /// Load the contract but do not enforce it.
    Ignore,
}

impl std::fmt::Display for ValidationMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => formatter.write_str("strict"),
            Self::Compatible => formatter.write_str("compatible"),
            Self::Ignore => formatter.write_str("ignore"),
        }
    }
}

/// Surface definition represented by a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    Tool,
    Prompt,
}

impl std::fmt::Display for ContractKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool => formatter.write_str("tool"),
            Self::Prompt => formatter.write_str("prompt"),
        }
    }
}

fn string_type() -> String {
    "string".to_string()
}

/// Prompt arguments are strings in MCP. Keeping the type explicit makes a
/// future protocol change (or a hand-edited incompatible snapshot) visible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptArgumentContract {
    pub name: String,
    #[serde(rename = "type", default = "string_type")]
    pub value_type: String,
    #[serde(default)]
    pub required: bool,
}

/// Versioned, metadata-free schema contract for one tool or prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub format_version: u32,
    pub kind: ContractKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgumentContract>,
}

impl Snapshot {
    pub fn tool(definition: &ToolDefinition) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            kind: ContractKind::Tool,
            name: definition.name.clone(),
            input_schema: Some(definition.input_schema.clone()),
            output_schema: definition.output_schema.clone(),
            arguments: Vec::new(),
        }
    }

    pub fn prompt(definition: &PromptDefinition) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            kind: ContractKind::Prompt,
            name: definition.name.clone(),
            input_schema: None,
            output_schema: None,
            arguments: definition
                .arguments
                .iter()
                .map(|argument| PromptArgumentContract {
                    name: argument.name.clone(),
                    value_type: string_type(),
                    required: argument.required,
                })
                .collect(),
        }
    }

    pub fn from_surface(
        tools: &[ToolDefinition],
        prompts: &[PromptDefinition],
        selector: &str,
    ) -> Result<Option<Self>, String> {
        if let Some(name) = selector.strip_prefix("tool:") {
            return Ok(tools
                .iter()
                .find(|definition| definition.name == name)
                .map(Self::tool));
        }
        if let Some(name) = selector.strip_prefix("prompt:") {
            return Ok(prompts
                .iter()
                .find(|definition| definition.name == name)
                .map(Self::prompt));
        }
        let tool = tools.iter().find(|definition| definition.name == selector);
        let prompt = prompts
            .iter()
            .find(|definition| definition.name == selector);
        match (tool, prompt) {
            (Some(_), Some(_)) => Err(format!(
                "both a tool and prompt are named {selector:?}; use `tool:{selector}` or `prompt:{selector}`"
            )),
            (Some(definition), None) => Ok(Some(Self::tool(definition))),
            (None, Some(definition)) => Ok(Some(Self::prompt(definition))),
            (None, None) => Ok(None),
        }
    }

    pub fn matching_surface(
        &self,
        tools: &[ToolDefinition],
        prompts: &[PromptDefinition],
    ) -> Option<Self> {
        match self.kind {
            ContractKind::Tool => tools
                .iter()
                .find(|definition| definition.name == self.name)
                .map(Self::tool),
            ContractKind::Prompt => prompts
                .iter()
                .find(|definition| definition.name == self.name)
                .map(Self::prompt),
        }
    }

    fn validate_shape(&self) -> Result<(), String> {
        if self.format_version != FORMAT_VERSION {
            return Err(format!(
                "unsupported schema snapshot formatVersion {}; expected {FORMAT_VERSION}",
                self.format_version
            ));
        }
        if self.name.is_empty() {
            return Err("schema snapshot has an empty name".to_string());
        }
        match self.kind {
            ContractKind::Tool if self.input_schema.is_none() => {
                Err("tool schema snapshot has no inputSchema".to_string())
            }
            ContractKind::Tool if !self.arguments.is_empty() => {
                Err("tool schema snapshot unexpectedly contains prompt arguments".to_string())
            }
            ContractKind::Prompt if self.input_schema.is_some() || self.output_schema.is_some() => {
                Err("prompt schema snapshot unexpectedly contains tool schemas".to_string())
            }
            _ => Ok(()),
        }
    }

    /// Canonical JSON sorts every object key and the scalar arrays whose JSON
    /// Schema order is semantically irrelevant.
    pub fn canonical_value(&self) -> Value {
        canonicalize(
            serde_json::to_value(self).expect("schema snapshot serialization is infallible"),
            None,
        )
    }

    pub fn to_pretty_json(&self) -> String {
        let mut rendered = serde_json::to_string_pretty(&self.canonical_value())
            .expect("canonical schema snapshot serialization is infallible");
        rendered.push('\n');
        rendered
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.to_pretty_json())
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let snapshot: Self = serde_json::from_str(&source)
            .map_err(|error| format!("{}: invalid schema snapshot: {error}", path.display()))?;
        snapshot
            .validate_shape()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(snapshot)
    }
}

fn canonicalize(value: Value, parent_key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .into_iter()
                .map(|(key, value)| {
                    let value = canonicalize(value, Some(&key));
                    (key, value)
                })
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) if matches!(parent_key, Some("required" | "enum" | "type")) => {
            let mut values = values
                .into_iter()
                .map(|value| canonicalize(value, None))
                .collect::<Vec<_>>();
            values.sort_by_key(Value::to_string);
            Value::Array(values)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| canonicalize(value, None))
                .collect(),
        ),
        value => value,
    }
}

/// One stable, machine-readable compatibility finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub path: String,
    pub code: String,
    pub message: String,
}

/// Result returned by both explicit validation and pre-invocation checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationReport {
    pub compatible: bool,
    pub mode: ValidationMode,
    pub kind: ContractKind,
    pub name: String,
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    fn new(snapshot: &Snapshot, mode: ValidationMode, issues: Vec<ValidationIssue>) -> Self {
        Self {
            compatible: issues.is_empty(),
            mode,
            kind: snapshot.kind,
            name: snapshot.name.clone(),
            issues,
        }
    }
}

fn issue(path: impl Into<String>, code: &str, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        path: path.into(),
        code: code.to_string(),
        message: message.into(),
    }
}

/// Compare a saved contract with the current definition of the same name.
pub fn validate(
    expected: &Snapshot,
    current: Option<&Snapshot>,
    mode: ValidationMode,
) -> ValidationReport {
    if mode == ValidationMode::Ignore {
        return ValidationReport::new(expected, mode, Vec::new());
    }
    let Some(current) = current else {
        return ValidationReport::new(
            expected,
            mode,
            vec![issue(
                "$",
                "definition_removed",
                format!(
                    "{} {:?} is no longer advertised",
                    expected.kind, expected.name
                ),
            )],
        );
    };
    if expected.kind != current.kind || expected.name != current.name {
        return ValidationReport::new(
            expected,
            mode,
            vec![issue(
                "$",
                "identity_changed",
                format!(
                    "expected {} {:?}, found {} {:?}",
                    expected.kind, expected.name, current.kind, current.name
                ),
            )],
        );
    }
    if mode == ValidationMode::Strict {
        let issues = (expected.canonical_value() != current.canonical_value())
            .then(|| {
                issue(
                    "$",
                    "definition_changed",
                    "current definition does not exactly match the canonical snapshot",
                )
            })
            .into_iter()
            .collect();
        return ValidationReport::new(expected, mode, issues);
    }

    let mut issues = Vec::new();
    match expected.kind {
        ContractKind::Tool => compare_tools(expected, current, &mut issues),
        ContractKind::Prompt => compare_prompts(expected, current, &mut issues),
    }
    ValidationReport::new(expected, mode, issues)
}

fn compare_tools(expected: &Snapshot, current: &Snapshot, issues: &mut Vec<ValidationIssue>) {
    let expected_input = expected
        .input_schema
        .as_ref()
        .expect("validated tool snapshot input");
    let current_input = current
        .input_schema
        .as_ref()
        .expect("current tool snapshot input");
    compare_schema(
        expected_input,
        current_input,
        expected_input,
        current_input,
        Direction::Input,
        "$.inputSchema",
        issues,
        &mut HashSet::new(),
    );

    match (&expected.output_schema, &current.output_schema) {
        (Some(expected_output), Some(current_output)) => compare_schema(
            expected_output,
            current_output,
            expected_output,
            current_output,
            Direction::Output,
            "$.outputSchema",
            issues,
            &mut HashSet::new(),
        ),
        (Some(_), None) => issues.push(issue(
            "$.outputSchema",
            "output_schema_removed",
            "tool no longer advertises its expected output schema",
        )),
        (None, _) => {}
    }
}

fn compare_prompts(expected: &Snapshot, current: &Snapshot, issues: &mut Vec<ValidationIssue>) {
    let current_arguments = current
        .arguments
        .iter()
        .map(|argument| (argument.name.as_str(), argument))
        .collect::<BTreeMap<_, _>>();
    let expected_required = expected
        .arguments
        .iter()
        .filter(|argument| argument.required)
        .map(|argument| argument.name.as_str())
        .collect::<BTreeSet<_>>();

    for argument in &expected.arguments {
        let path = format!("$.arguments.{}", argument.name);
        let Some(current) = current_arguments.get(argument.name.as_str()) else {
            issues.push(issue(
                path,
                "argument_removed",
                format!("prompt argument {:?} was removed", argument.name),
            ));
            continue;
        };
        if argument.value_type != current.value_type {
            issues.push(issue(
                &path,
                "argument_retyped",
                format!(
                    "prompt argument {:?} changed type from {:?} to {:?}",
                    argument.name, argument.value_type, current.value_type
                ),
            ));
        }
        if !argument.required && current.required {
            issues.push(issue(
                path,
                "argument_newly_required",
                format!("prompt argument {:?} is now required", argument.name),
            ));
        }
    }
    for argument in &current.arguments {
        if argument.required && !expected_required.contains(argument.name.as_str()) {
            let existed = expected
                .arguments
                .iter()
                .any(|expected| expected.name == argument.name);
            if !existed {
                issues.push(issue(
                    format!("$.arguments.{}", argument.name),
                    "argument_newly_required",
                    format!("new prompt argument {:?} is required", argument.name),
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Direction {
    Input,
    Output,
}

#[allow(clippy::too_many_arguments)]
fn compare_schema(
    expected_root: &Value,
    current_root: &Value,
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
    seen: &mut HashSet<(usize, usize, Direction)>,
) {
    let expected = match resolve_local_ref(expected_root, expected) {
        Ok(value) => value,
        Err(message) => {
            issues.push(issue(path, "invalid_expected_ref", message));
            return;
        }
    };
    let current = match resolve_local_ref(current_root, current) {
        Ok(value) => value,
        Err(message) => {
            issues.push(issue(path, "invalid_current_ref", message));
            return;
        }
    };
    if expected.is_boolean() || current.is_boolean() {
        let expected_class = schema_class(expected);
        let current_class = schema_class(current);
        let compatible = match direction {
            Direction::Input => {
                matches!(expected_class, SchemaClass::None)
                    || matches!(current_class, SchemaClass::Any)
                    || expected_class == current_class
            }
            Direction::Output => {
                matches!(expected_class, SchemaClass::Any)
                    || matches!(current_class, SchemaClass::None)
                    || expected_class == current_class
            }
        };
        if !compatible {
            issues.push(issue(
                path,
                "boolean_schema_changed",
                format!("{} boolean schema changed incompatibly", direction.label()),
            ));
        }
        return;
    }
    let pair = (
        expected as *const Value as usize,
        current as *const Value as usize,
        direction,
    );
    if !seen.insert(pair) {
        return;
    }

    compare_types(expected, current, direction, path, issues);
    compare_allowed_values(expected, current, direction, path, issues);
    compare_constraints(expected, current, direction, path, issues);
    compare_object(
        expected_root,
        current_root,
        expected,
        current,
        direction,
        path,
        issues,
        seen,
    );
    compare_array(
        expected_root,
        current_root,
        expected,
        current,
        direction,
        path,
        issues,
        seen,
    );

    for keyword in [
        "anyOf",
        "oneOf",
        "allOf",
        "not",
        "prefixItems",
        "contains",
        "dependentRequired",
        "propertyNames",
        "patternProperties",
    ] {
        if expected.get(keyword).map(canonical_ref) != current.get(keyword).map(canonical_ref) {
            issues.push(issue(
                format!("{path}.{keyword}"),
                "schema_composition_changed",
                format!("JSON Schema keyword {keyword:?} changed"),
            ));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaClass {
    Any,
    None,
    Concrete,
}

fn schema_class(schema: &Value) -> SchemaClass {
    match schema {
        Value::Bool(true) => SchemaClass::Any,
        Value::Bool(false) => SchemaClass::None,
        _ => SchemaClass::Concrete,
    }
}

fn canonical_ref(value: &Value) -> Value {
    canonicalize(value.clone(), None)
}

fn resolve_local_ref<'a>(root: &'a Value, value: &'a Value) -> Result<&'a Value, String> {
    let mut value = value;
    let mut visited = HashSet::new();
    for _ in 0..64 {
        let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
            return Ok(value);
        };
        if !reference.starts_with('#') {
            return Err(format!(
                "external JSON Schema reference {reference:?} cannot be compared offline"
            ));
        }
        if !visited.insert(reference.to_string()) {
            return Ok(value);
        }
        let pointer = reference.strip_prefix('#').unwrap_or_default();
        value = root
            .pointer(pointer)
            .ok_or_else(|| format!("unresolved local JSON Schema reference {reference:?}"))?;
    }
    Err("JSON Schema reference chain exceeds 64 entries".to_string())
}

fn schema_types(schema: &Value) -> Option<BTreeSet<&str>> {
    match schema.get("type") {
        Some(Value::String(value)) => Some(BTreeSet::from([value.as_str()])),
        Some(Value::Array(values)) => Some(values.iter().filter_map(Value::as_str).collect()),
        _ => schema
            .get("const")
            .map(|value| BTreeSet::from([json_type(value)]))
            .or_else(|| {
                schema
                    .get("enum")
                    .and_then(Value::as_array)
                    .map(|values| values.iter().map(json_type).collect())
            }),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn accepts_type(allowed: &BTreeSet<&str>, candidate: &str) -> bool {
    allowed.contains(candidate) || (candidate == "integer" && allowed.contains("number"))
}

fn compare_types(
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let expected_types = schema_types(expected);
    let current_types = schema_types(current);
    let compatible = match (direction, &expected_types, &current_types) {
        (_, None, None) => true,
        (Direction::Input, Some(_), None) => true,
        (Direction::Input, None, Some(_)) => false,
        (Direction::Input, Some(expected), Some(current)) => expected
            .iter()
            .all(|candidate| accepts_type(current, candidate)),
        (Direction::Output, None, Some(_)) => true,
        (Direction::Output, Some(_), None) => false,
        (Direction::Output, Some(expected), Some(current)) => current
            .iter()
            .all(|candidate| accepts_type(expected, candidate)),
    };
    if !compatible {
        issues.push(issue(
            format!("{path}.type"),
            "schema_retyped",
            format!(
                "schema type changed incompatibly from {} to {}",
                type_label(expected_types.as_ref()),
                type_label(current_types.as_ref())
            ),
        ));
    }
}

fn type_label(types: Option<&BTreeSet<&str>>) -> String {
    types
        .map(|types| types.iter().copied().collect::<Vec<_>>().join(" | "))
        .unwrap_or_else(|| "any".to_string())
}

fn allowed_values(schema: &Value) -> Option<BTreeSet<String>> {
    if let Some(value) = schema.get("const") {
        return Some(BTreeSet::from([canonical_ref(value).to_string()]));
    }
    schema.get("enum").and_then(Value::as_array).map(|values| {
        values
            .iter()
            .map(|value| canonical_ref(value).to_string())
            .collect()
    })
}

fn compare_allowed_values(
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let expected = allowed_values(expected);
    let current = allowed_values(current);
    let compatible = match (direction, &expected, &current) {
        (_, None, None) => true,
        (Direction::Input, Some(_), None) => true,
        (Direction::Input, None, Some(_)) => false,
        (Direction::Input, Some(expected), Some(current)) => expected.is_subset(current),
        (Direction::Output, None, Some(_)) => true,
        (Direction::Output, Some(_), None) => false,
        (Direction::Output, Some(expected), Some(current)) => current.is_subset(expected),
    };
    if !compatible {
        issues.push(issue(
            path,
            "allowed_values_changed",
            "enum/const values changed incompatibly",
        ));
    }
}

fn compare_constraints(
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for keyword in [
        "minimum",
        "exclusiveMinimum",
        "minLength",
        "minItems",
        "minProperties",
        "minContains",
    ] {
        compare_bound(expected, current, direction, path, keyword, true, issues);
    }
    for keyword in [
        "maximum",
        "exclusiveMaximum",
        "maxLength",
        "maxItems",
        "maxProperties",
        "maxContains",
    ] {
        compare_bound(expected, current, direction, path, keyword, false, issues);
    }
    for keyword in ["pattern", "format", "multipleOf", "uniqueItems"] {
        let expected_value = constraint_value(expected, keyword);
        let current_value = constraint_value(current, keyword);
        let compatible = match (direction, &expected_value, &current_value) {
            (_, None, None) => true,
            (Direction::Input, Some(_), None) => true,
            (Direction::Input, None, Some(_)) => false,
            (Direction::Input, Some(expected), Some(current)) => expected == current,
            (Direction::Output, None, Some(_)) => true,
            (Direction::Output, Some(_), None) => false,
            (Direction::Output, Some(expected), Some(current)) => expected == current,
        };
        if !compatible {
            issues.push(issue(
                format!("{path}.{keyword}"),
                "schema_constraint_changed",
                format!(
                    "{} constraint {keyword:?} changed incompatibly",
                    direction.label()
                ),
            ));
        }
    }
}

fn constraint_value(schema: &Value, keyword: &str) -> Option<Value> {
    let value = schema.get(keyword)?;
    if keyword == "uniqueItems" && value == &Value::Bool(false) {
        None
    } else {
        Some(canonical_ref(value))
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_bound(
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    keyword: &str,
    lower: bool,
    issues: &mut Vec<ValidationIssue>,
) {
    let expected_value = bound_value(expected, keyword);
    let current_value = bound_value(current, keyword);
    let compatible = match (direction, expected_value, current_value, lower) {
        (_, None, None, _) => true,
        (Direction::Input, Some(_), None, _) => true,
        (Direction::Input, None, Some(_), _) => false,
        (Direction::Input, Some(expected), Some(current), true) => current <= expected,
        (Direction::Input, Some(expected), Some(current), false) => current >= expected,
        (Direction::Output, None, Some(_), _) => true,
        (Direction::Output, Some(_), None, _) => false,
        (Direction::Output, Some(expected), Some(current), true) => current >= expected,
        (Direction::Output, Some(expected), Some(current), false) => current <= expected,
    };
    if !compatible {
        issues.push(issue(
            format!("{path}.{keyword}"),
            "schema_constraint_changed",
            format!(
                "{} constraint {keyword:?} changed incompatibly",
                direction.label()
            ),
        ));
    }
}

fn bound_value(schema: &Value, keyword: &str) -> Option<f64> {
    let value = schema.get(keyword).and_then(Value::as_f64)?;
    if matches!(keyword, "minLength" | "minItems" | "minProperties") && value == 0.0 {
        None
    } else {
        Some(value)
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_object(
    expected_root: &Value,
    current_root: &Value,
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
    seen: &mut HashSet<(usize, usize, Direction)>,
) {
    let expected_properties = expected.get("properties").and_then(Value::as_object);
    let current_properties = current.get("properties").and_then(Value::as_object);
    let expected_required = required(expected);
    let current_required = required(current);

    for (name, expected_property) in expected_properties.into_iter().flatten() {
        let property_path = format!("{path}.properties.{name}");
        let Some(current_property) = current_properties.and_then(|properties| properties.get(name))
        else {
            issues.push(issue(
                property_path,
                match direction {
                    Direction::Input => "input_removed",
                    Direction::Output => "output_removed",
                },
                format!("{} property {name:?} was removed", direction.label()),
            ));
            continue;
        };
        compare_schema(
            expected_root,
            current_root,
            expected_property,
            current_property,
            direction,
            &property_path,
            issues,
            seen,
        );
    }

    match direction {
        Direction::Input => {
            for name in current_required.difference(&expected_required) {
                issues.push(issue(
                    format!("{path}.properties.{name}"),
                    "input_newly_required",
                    format!("input property {name:?} is newly required"),
                ));
            }
        }
        Direction::Output => {
            for name in expected_required.difference(&current_required) {
                issues.push(issue(
                    format!("{path}.properties.{name}"),
                    "required_output_no_longer_guaranteed",
                    format!("output property {name:?} is no longer required"),
                ));
            }
        }
    }

    compare_additional_properties(
        expected_root,
        current_root,
        expected,
        current,
        direction,
        path,
        issues,
        seen,
    );
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

fn required(schema: &Value) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

enum AdditionalProperties<'a> {
    Any,
    None,
    Schema(&'a Value),
}

fn additional_properties(schema: &Value) -> AdditionalProperties<'_> {
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => AdditionalProperties::None,
        Some(Value::Object(object)) if object.is_empty() => AdditionalProperties::Any,
        Some(value @ Value::Object(_)) => AdditionalProperties::Schema(value),
        _ => AdditionalProperties::Any,
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_additional_properties(
    expected_root: &Value,
    current_root: &Value,
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
    seen: &mut HashSet<(usize, usize, Direction)>,
) {
    let expected = additional_properties(expected);
    let current = additional_properties(current);
    let incompatible = match (direction, expected, current) {
        (Direction::Input, AdditionalProperties::Any, AdditionalProperties::Any)
        | (Direction::Input, AdditionalProperties::None, _)
        | (Direction::Input, AdditionalProperties::Schema(_), AdditionalProperties::Any)
        | (Direction::Output, AdditionalProperties::Any, _)
        | (Direction::Output, AdditionalProperties::None, AdditionalProperties::None)
        | (Direction::Output, AdditionalProperties::Schema(_), AdditionalProperties::None) => false,
        (_, AdditionalProperties::Schema(expected), AdditionalProperties::Schema(current)) => {
            compare_schema(
                expected_root,
                current_root,
                expected,
                current,
                direction,
                &format!("{path}.additionalProperties"),
                issues,
                seen,
            );
            false
        }
        _ => true,
    };
    if incompatible {
        issues.push(issue(
            format!("{path}.additionalProperties"),
            "additional_properties_changed",
            format!(
                "{} additionalProperties changed incompatibly",
                direction.label()
            ),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_array(
    expected_root: &Value,
    current_root: &Value,
    expected: &Value,
    current: &Value,
    direction: Direction,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
    seen: &mut HashSet<(usize, usize, Direction)>,
) {
    match (expected.get("items"), current.get("items"), direction) {
        (Some(expected), Some(current), _) => compare_schema(
            expected_root,
            current_root,
            expected,
            current,
            direction,
            &format!("{path}.items"),
            issues,
            seen,
        ),
        (None, Some(_), Direction::Input) | (Some(_), None, Direction::Output) => {
            issues.push(issue(
                format!("{path}.items"),
                "array_items_changed",
                format!("{} array items became less compatible", direction.label()),
            ));
        }
        _ => {}
    }
}

/// Preloaded snapshots selected with repeated `--schema-contract` flags.
#[derive(Default)]
pub struct ContractSet {
    mode: ValidationMode,
    snapshots: BTreeMap<(ContractKind, String), (PathBuf, Snapshot)>,
}

impl ContractSet {
    pub fn load(paths: &[PathBuf], mode: ValidationMode) -> Result<Self, String> {
        let mut snapshots = BTreeMap::new();
        for path in paths {
            let snapshot = Snapshot::load(path)?;
            let key = (snapshot.kind, snapshot.name.clone());
            if let Some((previous, _)) = snapshots.insert(key, (path.clone(), snapshot)) {
                return Err(format!(
                    "{} duplicates schema contract {}",
                    path.display(),
                    previous.display()
                ));
            }
        }
        Ok(Self { mode, snapshots })
    }

    pub fn mode(&self) -> ValidationMode {
        self.mode
    }

    pub fn check_tool(&self, definition: &ToolDefinition) -> Option<ValidationReport> {
        self.snapshots
            .get(&(ContractKind::Tool, definition.name.clone()))
            .map(|(_, expected)| validate(expected, Some(&Snapshot::tool(definition)), self.mode))
    }

    pub fn check_prompt(&self, definition: &PromptDefinition) -> Option<ValidationReport> {
        self.snapshots
            .get(&(ContractKind::Prompt, definition.name.clone()))
            .map(|(_, expected)| validate(expected, Some(&Snapshot::prompt(definition)), self.mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::protocol::{PromptArgument, ToolDefinition};

    fn tool(name: &str, input: Value, output: Option<Value>) -> Snapshot {
        Snapshot::tool(&ToolDefinition {
            name: name.to_string(),
            title: None,
            description: None,
            input_schema: input,
            output_schema: output,
            icons: None,
            annotations: None,
            execution: None,
            meta: None,
        })
    }

    fn prompt(arguments: &[(&str, bool)]) -> Snapshot {
        Snapshot::prompt(&PromptDefinition {
            name: "greet".to_string(),
            title: None,
            description: None,
            icons: None,
            arguments: arguments
                .iter()
                .map(|(name, required)| PromptArgument {
                    name: (*name).to_string(),
                    description: None,
                    required: *required,
                })
                .collect(),
            meta: None,
        })
    }

    #[test]
    fn canonical_export_sorts_schema_keys_and_unordered_scalar_arrays() {
        let snapshot = tool(
            "x",
            serde_json::json!({
                "required": ["z", "a"],
                "properties": {
                    "z": {"type": ["null", "string"]},
                    "a": {"type": "integer"}
                },
                "type": "object"
            }),
            None,
        );
        let rendered = snapshot.to_pretty_json();
        assert!(rendered.find("formatVersion").unwrap() < rendered.find("inputSchema").unwrap());
        assert!(rendered.find("\"a\"").unwrap() < rendered.find("\"z\"").unwrap());
        assert!(rendered.find("\"null\"").unwrap() < rendered.find("\"string\"").unwrap());
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn compatible_tool_allows_additive_optional_input_and_output() {
        let expected = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {"count": {"type": "number"}},
                "required": ["count"]
            })),
        );
        let current = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"]
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer"},
                    "next": {"type": "string"}
                },
                "required": ["count", "next"]
            })),
        );
        assert!(validate(&expected, Some(&current), ValidationMode::Compatible).compatible);
    }

    #[test]
    fn compatible_tool_reports_removed_retyped_and_newly_required_fields() {
        let expected = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                }
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer"},
                    "label": {"type": "string"}
                },
                "required": ["count", "label"]
            })),
        );
        let current = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "number"},
                    "page": {"type": "integer"}
                },
                "required": ["query", "page"]
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {"count": {"type": "string"}},
                "required": ["count"]
            })),
        );
        let report = validate(&expected, Some(&current), ValidationMode::Compatible);
        let codes = report
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<BTreeSet<_>>();
        assert!(!report.compatible);
        assert!(codes.contains("input_removed"));
        assert!(codes.contains("input_newly_required"));
        assert!(codes.contains("schema_retyped"));
        assert!(codes.contains("output_removed"));
        assert!(codes.contains("required_output_no_longer_guaranteed"));
    }

    #[test]
    fn compatible_tool_applies_constraints_in_the_safe_direction() {
        let expected = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string", "maxLength": 20}}
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {"score": {"type": "number", "minimum": 0}}
            })),
        );
        let breaking = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string", "maxLength": 10}}
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {"score": {"type": "number", "minimum": -1}}
            })),
        );
        let report = validate(&expected, Some(&breaking), ValidationMode::Compatible);
        assert_eq!(
            report
                .issues
                .iter()
                .filter(|issue| issue.code == "schema_constraint_changed")
                .count(),
            2
        );

        let safe = tool(
            "x",
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string", "maxLength": 30}}
            }),
            Some(serde_json::json!({
                "type": "object",
                "properties": {"score": {"type": "number", "minimum": 1}}
            })),
        );
        assert!(validate(&expected, Some(&safe), ValidationMode::Compatible).compatible);
    }

    #[test]
    fn boolean_schemas_follow_input_and_output_variance() {
        let expected = tool("x", Value::Bool(true), Some(Value::Bool(true)));
        let input_narrowed = tool("x", Value::Bool(false), Some(Value::Bool(true)));
        assert!(!validate(&expected, Some(&input_narrowed), ValidationMode::Compatible).compatible);

        let output_narrowed = tool("x", Value::Bool(true), Some(Value::Bool(false)));
        assert!(
            validate(
                &expected,
                Some(&output_narrowed),
                ValidationMode::Compatible
            )
            .compatible
        );
    }

    #[test]
    fn nested_local_references_are_compared_recursively() {
        let expected = tool(
            "x",
            serde_json::json!({
                "$defs": {
                    "filter": {
                        "type": "object",
                        "properties": {"term": {"type": "string"}}
                    }
                },
                "type": "object",
                "properties": {"filter": {"$ref": "#/$defs/filter"}}
            }),
            None,
        );
        let current = tool(
            "x",
            serde_json::json!({
                "$defs": {
                    "filter": {
                        "type": "object",
                        "properties": {"term": {"type": "number"}}
                    }
                },
                "type": "object",
                "properties": {"filter": {"$ref": "#/$defs/filter"}}
            }),
            None,
        );
        let report = validate(&expected, Some(&current), ValidationMode::Compatible);
        assert!(!report.compatible);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "schema_retyped"
                && issue.path == "$.inputSchema.properties.filter.properties.term.type"
        }));
    }

    #[test]
    fn prompt_validation_allows_optional_additions_and_rejects_breaks() {
        let expected = prompt(&[("name", false), ("tone", true)]);
        let additive = prompt(&[("name", false), ("tone", false), ("language", false)]);
        assert!(validate(&expected, Some(&additive), ValidationMode::Compatible).compatible);

        let breaking = prompt(&[("name", true), ("language", true)]);
        let report = validate(&expected, Some(&breaking), ValidationMode::Compatible);
        assert!(!report.compatible);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "argument_removed")
        );
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "argument_newly_required")
        );

        let mut retyped = expected.clone();
        retyped.arguments[0].value_type = "number".to_string();
        let report = validate(&expected, Some(&retyped), ValidationMode::Compatible);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "argument_retyped")
        );
    }

    #[test]
    fn strict_and_ignore_modes_are_explicit() {
        let expected = prompt(&[("name", false)]);
        let current = prompt(&[("name", false), ("tone", false)]);
        assert!(!validate(&expected, Some(&current), ValidationMode::Strict).compatible);
        assert!(validate(&expected, None, ValidationMode::Ignore).compatible);
    }

    #[test]
    fn snapshot_round_trip_and_contract_set_are_file_backed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("greet.schema.json");
        let expected = prompt(&[("name", true)]);
        expected.write(&path).unwrap();
        assert_eq!(Snapshot::load(&path).unwrap(), expected);

        let set = ContractSet::load(&[path], ValidationMode::Compatible).unwrap();
        let definition = PromptDefinition {
            name: "greet".to_string(),
            title: None,
            description: None,
            icons: None,
            arguments: vec![PromptArgument {
                name: "name".to_string(),
                description: None,
                required: true,
            }],
            meta: None,
        };
        assert!(set.check_prompt(&definition).unwrap().compatible);
    }
}
