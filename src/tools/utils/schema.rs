use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub fn parse_args<T: DeserializeOwned>(json_args: &Value, tool_name: &str) -> Result<T> {
    serde_json::from_value(json_args.clone())
        .with_context(|| format!("Invalid arguments provided to {tool_name}"))
}

pub fn auto_schema<T: JsonSchema>(name: &str, description: &str) -> Value {
    let mut parameters = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }

    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}
