//! Shared pieces of tool input schemas.

use serde_json::{Value, json};

/// A closed object schema.
pub fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

pub fn describe(mut schema: Value, description: &str) -> Value {
    schema["description"] = description.into();
    schema
}

/// A rectangle in logical window pixels.
pub fn region() -> Value {
    object(
        json!({
            "x": { "type": "number", "minimum": 0 },
            "y": { "type": "number", "minimum": 0 },
            "width": { "type": "number", "exclusiveMinimum": 0 },
            "height": { "type": "number", "exclusiveMinimum": 0 }
        }),
        &["x", "y", "width", "height"],
    )
}

pub fn easing() -> Value {
    json!({ "enum": ["linear", "ease_in", "ease_out", "ease_in_out", "step"] })
}
