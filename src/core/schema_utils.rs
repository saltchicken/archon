//! Shared JSON Schema helpers used by provider tool-schema flattening
//! and skill hook argument validation.

use serde_json::{Map, Value};

/// Clones top-level `$defs` and `definitions` entries from a JSON Schema object.
pub(crate) fn extract_root_defs(schema: &Value) -> Map<String, Value> {
    let mut defs = Map::new();
    if let Some(obj) = schema.as_object() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(d)) = obj.get(key) {
                defs.extend(d.clone());
            }
        }
    }
    defs
}

/// Removes and returns top-level `$defs` and `definitions` entries from a JSON Schema object.
pub(crate) fn take_root_defs(schema: &mut Value) -> Map<String, Value> {
    let mut defs = Map::new();
    if let Some(obj) = schema.as_object_mut() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(d)) = obj.remove(key) {
                defs.extend(d);
            }
        }
    }
    defs
}

/// Extracts the definition name from a local JSON Pointer (`#/$defs/<name>` or `#/definitions/<name>`).
pub(crate) fn resolve_ref_name(ref_str: &str) -> Option<&str> {
    ref_str
        .strip_prefix("#/$defs/")
        .or_else(|| ref_str.strip_prefix("#/definitions/"))
}

/// Extracts all string values when every branch in a `oneOf`/`anyOf` array is a string `const` or `enum`.
pub(crate) fn extract_enum_strings(branches: &[Value]) -> Option<Vec<String>> {
    let mut vals = Vec::new();
    for branch in branches {
        let obj = branch.as_object()?;
        if let Some(c) = obj.get("const").and_then(Value::as_str) {
            vals.push(c.to_string());
        } else {
            let arr = obj.get("enum").and_then(Value::as_array)?;
            for item in arr {
                vals.push(item.as_str()?.to_string());
            }
        }
    }
    (!vals.is_empty()).then_some(vals)
}
