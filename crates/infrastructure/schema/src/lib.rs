/*!
 * @file JsonValidator
 * @description JSON subset-schema validation with error lists.
 *
 * Responsibilities:
 * - Validate values against a small schema vocabulary.
 * - Report every violation with a JSON-pointerish path.
 * - Stay dependency-light for hot validation paths.
 *
 * This module must not depend on: any other workspace crate. Full
 * JSON Schema (references, formats, conditionals) stays out of scope.
 */

//! Validation as total functions: schema in, error list out.

use serde_json::Value;

/// Small schema vocabulary covering tool inputs and model outputs.
#[derive(Debug, Clone, PartialEq)]
pub enum Schema {
    /// Anything validates.
    Any,
    /// Only JSON null.
    Null,
    /// Only booleans.
    Bool,
    /// Only integers (rejects 1.5).
    Int,
    /// Any number.
    Num,
    /// Only strings.
    Str,
    /// Arrays with per-item schema.
    Arr {
        /// Item schema.
        items: Box<Schema>,
    },
    /// Objects with known properties and required keys.
    Obj {
        /// Known property schemas.
        props: Vec<(String, Schema)>,
        /// Keys that must be present.
        required: Vec<String>,
    },
    /// Strings limited to an enumeration.
    Enum(Vec<String>),
    /// Nullable wrapper around another schema.
    Nullable(Box<Schema>),
}

/// Validate `value` against `schema`, collecting every violation.
pub fn validate(schema: &Schema, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, value, "$", &mut errors);
    errors
}

fn check(schema: &Schema, value: &Value, path: &str, errors: &mut Vec<String>) {
    match (schema, value) {
        (Schema::Any, _) => {}
        (Schema::Null, Value::Null) => {}
        (Schema::Bool, Value::Bool(_)) => {}
        (Schema::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => {}
        (Schema::Num, Value::Number(_)) => {}
        (Schema::Str, Value::String(_)) => {}
        (Schema::Nullable(inner), Value::Null) => {
            let _ = inner;
        }
        (Schema::Nullable(inner), _) => check(inner, value, path, errors),
        (Schema::Arr { items }, Value::Array(values)) => {
            for (index, item) in values.iter().enumerate() {
                check(items, item, &format!("{path}[{index}]"), errors);
            }
        }
        (Schema::Obj { props, required }, Value::Object(map)) => {
            for key in required {
                if !map.contains_key(key) {
                    errors.push(format!("{path}: missing required key {key:?}"));
                }
            }
            for (key, prop) in props {
                if let Some(item) = map.get(key) {
                    check(prop, item, &format!("{path}.{key}"), errors);
                }
            }
        }
        (Schema::Enum(options), Value::String(value)) => {
            if !options.iter().any(|o| o == value) {
                errors.push(format!("{path}: {value:?} is not one of {options:?}"));
            }
        }
        _ => errors.push(format!("{path}: type mismatch for {value}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_schema() -> Schema {
        Schema::Obj {
            props: vec![
                ("command".to_string(), Schema::Str),
                ("timeout_ms".to_string(), Schema::Int),
                (
                    "mode".to_string(),
                    Schema::Enum(vec!["fast".to_string(), "safe".to_string()]),
                ),
            ],
            required: vec!["command".to_string()],
        }
    }

    #[test]
    fn valid_inputs_pass_and_violations_list_paths() {
        assert!(validate(&tool_schema(), &json!({"command": "ls"})).is_empty());
        let errors = validate(&tool_schema(), &json!({"timeout_ms": 1.5, "mode": "wild"}));
        assert!(errors.iter().any(|e| e.contains("missing required")));
        assert!(errors.iter().any(|e| e.contains("$.timeout_ms")));
        assert!(errors.iter().any(|e| e.contains("$.mode")));
    }

    #[test]
    fn nesting_and_nullability_compose() {
        let schema = Schema::Obj {
            props: vec![(
                "items".to_string(),
                Schema::Arr {
                    items: Box::new(Schema::Nullable(Box::new(Schema::Int))),
                },
            )],
            required: vec![],
        };
        assert!(validate(&schema, &json!({"items": [1, null, 3]})).is_empty());
        assert_eq!(validate(&schema, &json!({"items": [1, "x"]})).len(), 1);
        assert!(!validate(&Schema::Int, &json!(1.5)).is_empty());
    }
}
