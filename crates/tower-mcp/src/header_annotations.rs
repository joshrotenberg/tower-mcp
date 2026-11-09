//! SEP-2243 `x-mcp-header` extraction from a tool input schema.
//!
//! The client uses this to learn which arguments to mirror into `Mcp-Param-*`
//! headers, and the server to validate those headers, so both have to agree
//! on which schemas are valid.

use std::collections::HashSet;

use serde_json::Value;

/// One `x-mcp-header` annotation and the argument it mirrors.
#[derive(Debug, Clone)]
pub(crate) struct CustomHeaderMapping {
    pub(crate) suffix: String,
    pub(crate) property_path: Vec<String>,
}

/// Collect the statically reachable `x-mcp-header` annotations from a tool
/// input schema. Traversal follows only `properties` chains, matching
/// SEP-2243's extraction rule. An annotation anywhere else in the schema is
/// an error.
pub(crate) fn custom_header_mappings(schema: &Value) -> Result<Vec<CustomHeaderMapping>, String> {
    let mut mappings = Vec::new();
    walk(schema, &mut Vec::new(), &mut HashSet::new(), &mut mappings)?;
    if annotation_count(schema) != mappings.len() {
        return Err(
            "x-mcp-header annotation is not statically reachable through properties".to_string(),
        );
    }
    Ok(mappings)
}

// Counts annotations at real schema locations only. Keywords holding literal
// data (`const`, `enum`, `default`, `examples`, `dependentRequired`) and
// unknown keywords are not schemas, and the keys of a property-name map are
// names, so none of them can be annotations.
fn annotation_count(schema: &Value) -> usize {
    let Some(object) = schema.as_object() else {
        return 0;
    };
    let nested = object
        .iter()
        .map(|(keyword, value)| match keyword.as_str() {
            // A subschema, or a list of them (`items` may be either).
            "allOf"
            | "anyOf"
            | "oneOf"
            | "prefixItems"
            | "items"
            | "additionalItems"
            | "contains"
            | "additionalProperties"
            | "propertyNames"
            | "unevaluatedItems"
            | "unevaluatedProperties"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => match value {
                Value::Array(subschemas) => subschemas.iter().map(annotation_count).sum(),
                subschema => annotation_count(subschema),
            },
            // Maps from a name to a subschema. A `dependencies` entry may
            // also be an array of names, which counts as nothing.
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
            | "dependencies" => value
                .as_object()
                .map_or(0, |map| map.values().map(annotation_count).sum()),
            _ => 0,
        })
        .sum::<usize>();
    usize::from(object.contains_key("x-mcp-header")) + nested
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

// SEP-2243 allows integer, string and boolean; `number` is not permitted.
fn primitive_header_type(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => matches!(kind.as_str(), "string" | "integer" | "boolean"),
        Some(Value::Array(kinds)) => {
            let mut primitive = false;
            for kind in kinds {
                match kind.as_str() {
                    Some("string" | "integer" | "boolean") if !primitive => primitive = true,
                    Some("null") => {}
                    _ => return false,
                }
            }
            primitive
        }
        _ => false,
    }
}

fn walk(
    schema: &Value,
    path: &mut Vec<String>,
    seen: &mut HashSet<String>,
    mappings: &mut Vec<CustomHeaderMapping>,
) -> Result<(), String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };

    for (property_name, property_schema) in properties {
        path.push(property_name.clone());
        if let Some(annotation) = property_schema.get("x-mcp-header") {
            let suffix = annotation
                .as_str()
                .ok_or_else(|| format!("x-mcp-header at {} must be a string", path.join(".")))?;
            if suffix.is_empty() || !suffix.bytes().all(is_tchar) {
                return Err(format!(
                    "invalid x-mcp-header value {suffix:?} at {}",
                    path.join(".")
                ));
            }
            if !primitive_header_type(property_schema) {
                return Err(format!(
                    "x-mcp-header at {} requires a string, integer, or boolean property",
                    path.join(".")
                ));
            }
            if !seen.insert(suffix.to_ascii_lowercase()) {
                return Err(format!("duplicate x-mcp-header value {suffix:?}"));
            }
            mappings.push(CustomHeaderMapping {
                suffix: suffix.to_string(),
                property_path: path.clone(),
            });
        }
        walk(property_schema, path, seen, mappings)?;
        path.pop();
    }
    Ok(())
}
