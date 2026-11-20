//! Rewrites tool parameter schemas into the dialect providers accept.
//!
//! `schemars` 0.8 emits draft-07 shaped schemas: a root `$schema` dialect
//! declaration, `definitions` with `#/definitions/...` references, and no
//! `required` key when every argument is optional. Anthropic validates tool
//! `input_schema`s against JSON Schema draft 2020-12 and rejects the whole
//! request on the first violation, and it requires `required` whenever
//! `properties` is present. Every provider therefore runs schemas through
//! [`normalize_tool_schema`] before embedding them on the wire.

use schemars::schema::RootSchema;
use serde_json::{Map, Value};

/// Keywords holding one subschema (`items` may also be a draft-07 tuple).
const SINGLE_SUBSCHEMAS: [&str; 12] = [
    "items",
    "additionalItems",
    "additionalProperties",
    "contains",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
    "unevaluatedItems",
    "unevaluatedProperties",
    "contentSchema",
];

/// Keywords holding an array of subschemas.
const LIST_SUBSCHEMAS: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// Keywords holding a name-to-subschema map.
const MAP_SUBSCHEMAS: [&str; 6] = [
    "properties",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "$defs",
    "definitions",
];

/// Serializes a tool `RootSchema` into a wire-ready parameter schema.
pub fn tool_schema_value(schema: &RootSchema) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(schema)?;
    normalize_tool_schema(&mut value);
    Ok(value)
}

/// Drops the declared dialect, migrates `definitions` into `$defs`, rewrites
/// draft-07 tuple validation into `prefixItems`, and guarantees `required`
/// wherever `properties` is present.
pub fn normalize_tool_schema(schema: &mut Value) {
    if !schema.is_object() {
        // Boolean or missing roots are legal JSON Schema but not a valid
        // tool `input_schema`; accept any object instead of failing.
        *schema = Value::Object(Map::new());
        schema["type"] = Value::String("object".to_string());
    }
    walk(schema, &mut normalize_node);
}

fn normalize_node(map: &mut Map<String, Value>) {
    map.remove("$schema");

    if let Some(Value::String(reference)) = map.get_mut("$ref")
        && let Some(name) = reference.strip_prefix("#/definitions/")
    {
        *reference = format!("#/$defs/{name}");
    }

    if let Some(Value::Object(definitions)) = map.remove("definitions") {
        match map
            .entry("$defs")
            .or_insert_with(|| Value::Object(Map::new()))
        {
            Value::Object(defs) => {
                for (name, definition) in definitions {
                    defs.entry(name).or_insert(definition);
                }
            }
            defs => *defs = Value::Object(definitions),
        }
    }

    // Draft-07 tuple validation put the per-position schemas in `items`; from
    // 2019-09 on they belong in `prefixItems` and `items` must be a single
    // schema. Anthropic rejects the array form outright.
    match map.remove("items") {
        Some(Value::Array(tuple)) => {
            match map.get_mut("prefixItems") {
                Some(Value::Array(prefix)) => prefix.extend(tuple),
                Some(_) => {}
                None => {
                    map.insert("prefixItems".to_string(), Value::Array(tuple));
                }
            }
            // `additionalItems: true` and an absent value both mean "extra
            // items are unconstrained", which is what a missing `items` says.
            if let Some(tail @ (Value::Bool(false) | Value::Object(_))) =
                map.remove("additionalItems")
            {
                map.insert("items".to_string(), tail);
            }
        }
        // A single schema (or boolean) is already 2020-12 valid; put it back
        // and drop the draft-07 `additionalItems` sibling, which no longer
        // applies.
        other => {
            if let Some(items) = other {
                map.insert("items".to_string(), items);
            }
            map.remove("additionalItems");
        }
    }

    if matches!(map.get("properties"), Some(Value::Object(_))) {
        // Anthropic requires `required` whenever `properties` is present; an
        // empty array is valid. A non-array value is invalid everywhere.
        if !matches!(map.get("required"), Some(Value::Array(_))) {
            map.insert("required".to_string(), Value::Array(Vec::new()));
        }
    } else if map.contains_key("required") && !matches!(map.get("required"), Some(Value::Array(_)))
    {
        map.remove("required");
    }
}

/// Applies `visit` to `schema` and every subschema reachable from it.
fn walk(schema: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    match schema {
        Value::Object(map) => {
            visit(map);
            for keyword in SINGLE_SUBSCHEMAS {
                if let Some(child) = map.get_mut(keyword) {
                    walk(child, visit);
                }
            }
            for keyword in LIST_SUBSCHEMAS {
                if let Some(Value::Array(items)) = map.get_mut(keyword) {
                    for item in items {
                        walk(item, visit);
                    }
                }
            }
            for keyword in MAP_SUBSCHEMAS {
                if let Some(Value::Object(entries)) = map.get_mut(keyword) {
                    for child in entries.values_mut() {
                        walk(child, visit);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, visit);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_dialect_and_adds_missing_required() {
        let mut schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "ListArgs",
            "type": "object",
            "properties": {"path": {"type": ["string", "null"]}}
        });

        normalize_tool_schema(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["required"], json!([]));
        assert_eq!(schema["title"], "ListArgs");
        assert_eq!(
            schema["properties"]["path"],
            json!({"type": ["string", "null"]})
        );
    }

    #[test]
    fn keeps_required_and_fixes_nested_objects() {
        let mut schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {"inner": {"type": "string"}}
                },
                "list": {
                    "type": "array",
                    "items": {"type": "object", "properties": {}}
                }
            },
            "required": ["outer"]
        });

        normalize_tool_schema(&mut schema);

        assert_eq!(schema["required"], json!(["outer"]));
        assert_eq!(schema["properties"]["outer"]["required"], json!([]));
        assert_eq!(schema["properties"]["list"]["items"]["required"], json!([]));
    }

    #[test]
    fn migrates_definitions_and_rewrites_refs() {
        let mut schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"questions": {"type": "array", "items": {"$ref": "#/definitions/Question"}}},
            "definitions": {
                "Question": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            }
        });

        normalize_tool_schema(&mut schema);

        assert!(schema.get("definitions").is_none());
        assert_eq!(
            schema["properties"]["questions"]["items"]["$ref"],
            json!("#/$defs/Question")
        );
        assert_eq!(
            schema["$defs"]["Question"],
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            })
        );
    }

    #[test]
    fn converts_draft07_tuple_items_to_prefix_items() {
        // The shape schemars emits for the workflow spec's
        // `edges: Option<Vec<(String, String)>>`.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "edges": {
                    "default": null,
                    "type": ["array", "null"],
                    "items": {
                        "type": "array",
                        "items": [{"type": "string"}, {"type": "string"}],
                        "minItems": 2,
                        "maxItems": 2
                    }
                }
            }
        });

        normalize_tool_schema(&mut schema);

        let tuple = &schema["properties"]["edges"]["items"];
        assert_eq!(
            tuple["prefixItems"],
            json!([{"type": "string"}, {"type": "string"}])
        );
        assert!(tuple.get("items").is_none());
        assert_eq!(tuple["minItems"], 2);
        assert_eq!(tuple["maxItems"], 2);
        assert!(!has_tuple_items(&schema));
    }

    #[test]
    fn translates_additional_items() {
        let mut closed = json!({
            "type": "array",
            "items": [{"type": "string"}],
            "additionalItems": false
        });
        normalize_tool_schema(&mut closed);
        assert_eq!(closed["prefixItems"], json!([{"type": "string"}]));
        assert_eq!(closed["items"], json!(false));
        assert!(closed.get("additionalItems").is_none());

        let mut typed = json!({
            "type": "array",
            "items": [{"type": "string"}],
            "additionalItems": {"type": "integer"}
        });
        normalize_tool_schema(&mut typed);
        assert_eq!(typed["items"], json!({"type": "integer"}));
        assert!(typed.get("additionalItems").is_none());

        let mut open = json!({
            "type": "array",
            "items": [{"type": "string"}],
            "additionalItems": true
        });
        normalize_tool_schema(&mut open);
        assert!(open.get("items").is_none());
        assert!(open.get("additionalItems").is_none());
    }

    fn has_tuple_items(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                matches!(map.get("items"), Some(Value::Array(_)))
                    || map
                        .iter()
                        .filter(|(key, _)| {
                            !matches!(key.as_str(), "enum" | "const" | "default" | "examples")
                        })
                        .any(|(_, child)| has_tuple_items(child))
            }
            Value::Array(items) => items.iter().any(has_tuple_items),
            _ => false,
        }
    }

    #[test]
    fn replaces_non_array_required() {
        let mut schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": true
        });

        normalize_tool_schema(&mut schema);

        assert_eq!(schema["required"], json!([]));
    }

    #[test]
    fn leaves_property_names_that_shadow_keywords_alone() {
        let mut schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "$schema": {"type": "string"},
                "definitions": {"type": "string"},
                "required": {"type": "string"},
                "properties": {"type": "string"}
            }
        });

        normalize_tool_schema(&mut schema);

        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 4);
        assert_eq!(properties["$schema"], json!({"type": "string"}));
        assert_eq!(properties["definitions"], json!({"type": "string"}));
        assert_eq!(properties["required"], json!({"type": "string"}));
        assert_eq!(properties["properties"], json!({"type": "string"}));
        assert_eq!(schema["required"], json!([]));
    }

    #[test]
    fn replaces_boolean_roots_with_an_object_schema() {
        let mut schema = Value::Bool(true);

        normalize_tool_schema(&mut schema);

        assert_eq!(schema, json!({"type": "object"}));
    }

    #[derive(schemars::JsonSchema)]
    struct AllOptional {
        #[allow(dead_code)]
        path: Option<String>,
    }

    #[test]
    fn serializes_root_schema_ready_for_anthropic() {
        let value = tool_schema_value(&schemars::schema_for!(AllOptional)).unwrap();

        assert!(value.get("$schema").is_none());
        assert!(value["properties"].is_object());
        assert_eq!(value["required"], json!([]));
    }
}
