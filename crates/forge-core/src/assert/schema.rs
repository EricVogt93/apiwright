//! Thin wrapper around the `jsonschema` crate producing readable, collected
//! error strings instead of an error iterator tied to borrowed state.

use serde_json::Value;

/// Cap on how many individual validation errors are reported; schemas that
/// fail wildly (e.g. validating the wrong document) should not flood the UI.
const MAX_ERRORS: usize = 20;

/// Validate `instance` against `schema`.
///
/// Returns `Ok(())` when valid. Returns `Err` with up to
/// [`MAX_ERRORS`] human-readable `"<instance path>: <message>"` strings
/// when invalid, or a single-element `Err` if `schema` itself does not
/// compile as a valid JSON Schema.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<String>> {
    let validator = match jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
    {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("invalid JSON schema: {e}")]),
    };

    let errors: Vec<String> = validator
        .iter_errors(instance)
        .take(MAX_ERRORS)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate against one named definition while retaining the complete schema
/// document as the resolution root for internal `#/$defs/...` references.
pub fn validate_definition(
    schema: &Value,
    definition: &str,
    instance: &Value,
) -> Result<(), Vec<String>> {
    if !schema
        .get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|definitions| definitions.contains_key(definition))
    {
        return Err(vec![format!(
            "unknown JSON Schema definition {definition:?}; validation was not run"
        )]);
    }
    let mut selected = schema.clone();
    let Some(object) = selected.as_object_mut() else {
        return Err(vec![
            "invalid JSON schema: document must be an object".to_string()
        ]);
    };
    object.insert(
        "$ref".to_string(),
        Value::String(format!("#/$defs/{}", escape_json_pointer(definition))),
    );
    validate(&selected, instance)
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn valid_instance_passes() {
        let schema = json!({
            "type": "object",
            "required": ["id"],
            "properties": { "id": { "type": "number" } }
        });
        let instance = json!({ "id": 1 });
        assert_eq!(validate(&schema, &instance), Ok(()));
    }

    #[test]
    fn invalid_instance_collects_errors() {
        let schema = json!({
            "type": "object",
            "required": ["id", "name"],
            "properties": {
                "id": { "type": "number" },
                "name": { "type": "string" }
            }
        });
        let instance = json!({ "id": "not-a-number" });
        let errors = validate(&schema, &instance).unwrap_err();
        assert!(!errors.is_empty());
        assert!(errors.len() <= MAX_ERRORS);
        // At least one error should mention the `id` field's path.
        assert!(errors.iter().any(|e| e.contains("id")));
    }

    #[test]
    fn invalid_schema_itself_reports_single_error() {
        // `type` must be a string or array of strings, not a number.
        let schema = json!({ "type": 123 });
        let result = validate(&schema, &json!({}));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().len(), 1);
    }

    #[test]
    fn many_errors_are_capped() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" },
                "c": { "type": "string" },
            },
            "additionalProperties": false
        });
        // 30 unexpected properties -> way more than MAX_ERRORS violations.
        let mut map = serde_json::Map::new();
        for i in 0..30 {
            map.insert(format!("extra{i}"), json!(1));
        }
        let instance = Value::Object(map);
        let errors = validate(&schema, &instance).unwrap_err();
        assert!(errors.len() <= MAX_ERRORS);
    }

    #[test]
    fn named_definition_keeps_internal_refs_and_fails_unknown_names_closed() {
        let schema = json!({
            "$defs": {
                "item": {
                    "type": "object",
                    "required": ["child"],
                    "properties": {"child": {"$ref": "#/$defs/child"}}
                },
                "child": {
                    "type": "object",
                    "required": ["id"],
                    "properties": {"id": {"type": "integer"}},
                    "additionalProperties": false
                }
            }
        });
        assert!(validate_definition(&schema, "item", &json!({"child": {"id": 7}})).is_ok());
        assert!(validate_definition(&schema, "item", &json!({"child": {"id": "7"}})).is_err());
        assert!(
            validate_definition(&schema, "missing", &json!({})).unwrap_err()[0]
                .contains("unknown JSON Schema definition")
        );
    }

    #[test]
    fn enforces_contract_keywords_and_supported_formats() {
        let schema = json!({
            "type": "object",
            "required": ["dates", "contact", "asciiContact", "labels"],
            "properties": {
                "dates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["date", "createdAt"],
                        "properties": {
                            "date": {"type": "string", "format": "date"},
                            "createdAt": {"type": "string", "format": "date-time"}
                        },
                        "additionalProperties": false
                    }
                },
                "contact": {"type": "string", "format": "idn-email"},
                "asciiContact": {"type": "string", "format": "email"},
                "labels": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                }
            },
            "additionalProperties": false
        });
        assert!(validate(
            &schema,
            &json!({
                "dates": [{"date": "2026-07-28", "createdAt": "2026-07-28T10:00:00Z"}],
                "contact": "user@b\u{00fc}cher.example",
                "asciiContact": "user@example.test",
                "labels": {"kind": "primary"}
            })
        )
        .is_ok());
        for invalid in [
            json!({"dates": [], "contact": 7, "asciiContact": "user@example.test", "labels": {}}),
            json!({"dates": [{"date": "not-a-date", "createdAt": "nope"}], "contact": "bad", "asciiContact": "bad", "labels": {}}),
            json!({"dates": [], "contact": "user@example.test", "asciiContact": "user@example.test", "labels": {"kind": 7}}),
            json!({"dates": [], "contact": "user@example.test", "asciiContact": "user@example.test", "labels": {}, "extra": true}),
        ] {
            assert!(validate(&schema, &invalid).is_err(), "{invalid}");
        }
    }
}
