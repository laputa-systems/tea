//! Miniserde-native JSON Schema validation for tool arguments.
//!
//! The public kernel boundary uses tea_protocol::JsonValue and
//! SerializedJson. This adapter deliberately validates the small, explicit
//! JSON Schema vocabulary used by the core and default coding profile without
//! importing a Serde value tree or a draft-specific validator dependency.

use crate::error::ToolError;
use crate::state::SerializedJson;
use std::collections::BTreeSet;
use tea_protocol::JsonValue;

pub(crate) fn validate_tool_arguments(
    tool_name: &str,
    schema: &JsonValue,
    arguments: &SerializedJson,
) -> Result<(), ToolError> {
    validate_schema(schema).map_err(|message| ToolError::InvalidArguments {
        tool: tool_name.to_owned(),
        message: format!("tool schema is invalid: {message}"),
    })?;

    let arguments =
        JsonValue::parse(arguments.as_str()).map_err(|_| ToolError::InvalidArguments {
            tool: tool_name.to_owned(),
            message: "tool-call arguments are not valid JSON: invalid JSON".to_owned(),
        })?;
    let mut errors = Vec::new();
    validate_value(schema, &arguments, "", &mut errors);
    if errors.is_empty() {
        return Ok(());
    }

    let received =
        arguments
            .to_json_string_pretty()
            .map_err(|error| ToolError::InvalidArguments {
                tool: tool_name.to_owned(),
                message: format!("tool-call arguments cannot be rendered: {error}"),
            })?;
    Err(ToolError::InvalidArguments {
        tool: tool_name.to_owned(),
        message: format!(
            "Validation failed for tool {tool_name:?}:\n{}\n\nReceived arguments:\n{received}",
            errors
                .iter()
                .map(|error| format!("  - {}: {}", error.path, error.message))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidationFailure {
    path: String,
    message: String,
}

fn validate_value(
    schema: &JsonValue,
    value: &JsonValue,
    path: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    let object = schema
        .as_object()
        .expect("validate_schema checked that every schema is an object");

    if let Some(type_value) = object.get("type") {
        let matches = match type_value {
            JsonValue::String(type_name) => type_matches(type_name, value),
            JsonValue::Array(types) => types
                .iter()
                .filter_map(JsonValue::as_str)
                .any(|type_name| type_matches(type_name, value)),
            _ => false,
        };
        if !matches {
            let expected = type_value
                .to_json_string()
                .unwrap_or_else(|_| "valid JSON types".to_owned());
            errors.push(ValidationFailure {
                path: display_path(path),
                message: format!("must be of type {expected}"),
            });
            return;
        }
    }

    if let Some(enum_values) = object.get("enum").and_then(JsonValue::as_array)
        && !enum_values.iter().any(|candidate| candidate == value) {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must be one of the enumerated values".to_owned(),
            });
        }
    if let Some(const_value) = object.get("const")
        && const_value != value {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must equal the schema constant".to_owned(),
            });
        }

    if let Some(schemas) = object.get("allOf").and_then(JsonValue::as_array) {
        for schema in schemas {
            validate_value(schema, value, path, errors);
        }
    }
    if let Some(schemas) = object.get("anyOf").and_then(JsonValue::as_array)
        && !schemas.iter().any(|schema| is_valid(schema, value)) {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must match at least one schema".to_owned(),
            });
        }
    if let Some(schemas) = object.get("oneOf").and_then(JsonValue::as_array) {
        let matches = schemas
            .iter()
            .filter(|schema| is_valid(schema, value))
            .count();
        if matches != 1 {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must match exactly one schema".to_owned(),
            });
        }
    }
    if let Some(schema) = object.get("not")
        && is_valid(schema, value) {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must not match the schema".to_owned(),
            });
        }

    match value {
        JsonValue::Object(value) => validate_object(object, value, path, errors),
        JsonValue::Array(value) => validate_array(object, value, path, errors),
        JsonValue::String(value) => validate_string(object, value, path, errors),
        JsonValue::Number(value) => {
            validate_number(object, JsonValue::Number(*value).as_f64(), path, errors)
        }
        JsonValue::Null | JsonValue::Bool(_) => {}
    }
}

fn validate_object(
    schema: &std::collections::BTreeMap<String, JsonValue>,
    value: &std::collections::BTreeMap<String, JsonValue>,
    path: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    if let Some(required) = schema.get("required").and_then(JsonValue::as_array) {
        for property in required.iter().filter_map(JsonValue::as_str) {
            if !value.contains_key(property) {
                errors.push(ValidationFailure {
                    path: child_path(path, property),
                    message: format!("must have required properties {property}"),
                });
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(JsonValue::as_object) {
        for (property, property_schema) in properties {
            if let Some(property_value) = value.get(property) {
                validate_value(
                    property_schema,
                    property_value,
                    &child_path(path, property),
                    errors,
                );
            }
        }
    }

    for (property, property_value) in value {
        if schema
            .get("properties")
            .and_then(JsonValue::as_object)
            .is_some_and(|properties| properties.contains_key(property))
        {
            continue;
        }
        match schema.get("additionalProperties") {
            Some(JsonValue::Bool(false)) => errors.push(ValidationFailure {
                path: child_path(path, property),
                message: "must not have additional properties".to_owned(),
            }),
            Some(JsonValue::Object(property_schema)) => {
                validate_value(
                    &JsonValue::Object(property_schema.clone()),
                    property_value,
                    &child_path(path, property),
                    errors,
                );
            }
            _ => {}
        }
    }
}

fn validate_array(
    schema: &std::collections::BTreeMap<String, JsonValue>,
    value: &[JsonValue],
    path: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    if let Some(item_schema) = schema.get("items") {
        for (index, item) in value.iter().enumerate() {
            validate_value(
                item_schema,
                item,
                &child_path(path, &index.to_string()),
                errors,
            );
        }
    }
    if let Some(unique) = schema.get("uniqueItems").and_then(JsonValue::as_bool)
        && unique
            && value
                .iter()
                .enumerate()
                .any(|(index, item)| value[..index].contains(item))
        {
            errors.push(ValidationFailure {
                path: display_path(path),
                message: "must contain unique items".to_owned(),
            });
        }
    check_count(schema, "minItems", value.len(), path, "items", errors);
    check_count(schema, "maxItems", value.len(), path, "items", errors);
}

fn validate_string(
    schema: &std::collections::BTreeMap<String, JsonValue>,
    value: &str,
    path: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    check_count(
        schema,
        "minLength",
        value.chars().count(),
        path,
        "characters",
        errors,
    );
    check_count(
        schema,
        "maxLength",
        value.chars().count(),
        path,
        "characters",
        errors,
    );
}

fn validate_number(
    schema: &std::collections::BTreeMap<String, JsonValue>,
    value: Option<f64>,
    path: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    let Some(value) = value else { return };
    for (keyword, message) in [
        ("minimum", "must be greater than or equal to"),
        ("maximum", "must be less than or equal to"),
        ("exclusiveMinimum", "must be greater than"),
        ("exclusiveMaximum", "must be less than"),
    ] {
        if let Some(bound) = schema.get(keyword).and_then(JsonValue::as_f64) {
            let valid = match keyword {
                "minimum" => value >= bound,
                "maximum" => value <= bound,
                "exclusiveMinimum" => value > bound,
                "exclusiveMaximum" => value < bound,
                _ => unreachable!("keyword comes from the bounds list"),
            };
            if !valid {
                errors.push(ValidationFailure {
                    path: display_path(path),
                    message: format!("{message} {bound}"),
                });
            }
        }
    }
}

fn check_count(
    schema: &std::collections::BTreeMap<String, JsonValue>,
    keyword: &str,
    actual: usize,
    path: &str,
    noun: &str,
    errors: &mut Vec<ValidationFailure>,
) {
    let Some(bound) = schema.get(keyword).and_then(JsonValue::as_u64) else {
        return;
    };
    let valid = if keyword.starts_with("min") {
        (actual as u64) >= bound
    } else {
        (actual as u64) <= bound
    };
    if !valid {
        let comparison = if keyword.starts_with("min") {
            "at least"
        } else {
            "at most"
        };
        errors.push(ValidationFailure {
            path: display_path(path),
            message: format!("must contain {comparison} {bound} {noun}"),
        });
    }
}

fn is_valid(schema: &JsonValue, value: &JsonValue) -> bool {
    let mut errors = Vec::new();
    validate_value(schema, value, "", &mut errors);
    errors.is_empty()
}

fn type_matches(type_name: &str, value: &JsonValue) -> bool {
    match type_name {
        "null" => value.is_null(),
        "boolean" => matches!(value, JsonValue::Bool(_)),
        "integer" => matches!(
            value,
            JsonValue::Number(
                tea_protocol::JsonNumber::Signed(_) | tea_protocol::JsonNumber::Unsigned(_)
            )
        ),
        "number" => matches!(value, JsonValue::Number(_)),
        "string" => matches!(value, JsonValue::String(_)),
        "array" => matches!(value, JsonValue::Array(_)),
        "object" => value.is_object(),
        _ => false,
    }
}

fn validate_schema(schema: &JsonValue) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| "schema must be a JSON object".to_owned())?;
    let allowed = [
        "$comment",
        "$id",
        "$schema",
        "additionalProperties",
        "allOf",
        "anyOf",
        "const",
        "default",
        "deprecated",
        "description",
        "enum",
        "examples",
        "exclusiveMaximum",
        "exclusiveMinimum",
        "items",
        "maxItems",
        "maxLength",
        "maximum",
        "minItems",
        "minLength",
        "minimum",
        "not",
        "oneOf",
        "properties",
        "required",
        "title",
        "type",
        "uniqueItems",
    ];
    for keyword in object.keys() {
        if !allowed.contains(&keyword.as_str()) {
            return Err(format!("unsupported keyword {keyword:?}"));
        }
    }
    if let Some(type_value) = object.get("type") {
        match type_value {
            JsonValue::String(type_name) if supported_type(type_name) => {}
            JsonValue::Array(types)
                if !types.is_empty()
                    && types
                        .iter()
                        .all(|value| value.as_str().is_some_and(supported_type)) => {}
            _ => return Err("type must name one or more supported JSON types".to_owned()),
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| "properties must be an object".to_owned())?;
        for (name, property_schema) in properties {
            validate_schema(property_schema)
                .map_err(|error| format!("property {name:?}: {error}"))?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| "required must be an array of strings".to_owned())?;
        let mut names = BTreeSet::new();
        for property in required {
            let property = property
                .as_str()
                .ok_or_else(|| "required must be an array of strings".to_owned())?;
            if !names.insert(property) {
                return Err(format!("required contains duplicate property {property:?}"));
            }
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        match additional {
            JsonValue::Bool(_) => {}
            JsonValue::Object(_) => validate_schema(additional)?,
            _ => return Err("additionalProperties must be a boolean or schema".to_owned()),
        }
    }
    if let Some(items) = object.get("items") {
        if !items.is_object() {
            return Err("items must be a schema object".to_owned());
        }
        validate_schema(items)?;
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(value) = object.get(keyword) {
            let schemas = value
                .as_array()
                .ok_or_else(|| format!("{keyword} must be an array of schemas"))?;
            for schema in schemas {
                validate_schema(schema)?;
            }
        }
    }
    if let Some(not) = object.get("not") {
        validate_schema(not)?;
    }
    if let Some(enum_values) = object.get("enum")
        && enum_values.as_array().is_none() {
            return Err("enum must be an array".to_owned());
        }
    for keyword in ["minItems", "maxItems", "minLength", "maxLength"] {
        if let Some(value) = object.get(keyword)
            && value.as_u64().is_none() {
                return Err(format!("{keyword} must be a nonnegative integer"));
            }
    }
    for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if let Some(value) = object.get(keyword)
            && value.as_f64().is_none() {
                return Err(format!("{keyword} must be a number"));
            }
    }
    if let Some(unique_items) = object.get("uniqueItems")
        && unique_items.as_bool().is_none() {
            return Err("uniqueItems must be a boolean".to_owned());
        }
    Ok(())
}

fn supported_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "null" | "boolean" | "integer" | "number" | "string" | "array" | "object"
    )
}

fn child_path(path: &str, child: &str) -> String {
    if path.is_empty() {
        child.to_owned()
    } else {
        format!("{path}.{child}")
    }
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "root".to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::validate_tool_arguments;
    use crate::state::SerializedJson;
    use tea_protocol::JsonValue;

    #[test]
    fn validator_accepts_matching_arguments_and_rejects_a_missing_required_property() {
        let schema = JsonValue::parse(
            r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}"#,
        )
        .expect("schema JSON");

        validate_tool_arguments("echo", &schema, &SerializedJson::new(r#"{"text":"hello"}"#))
            .expect("matching arguments");
        let error = validate_tool_arguments("echo", &schema, &SerializedJson::new("{}"))
            .expect_err("required property must be enforced");
        assert_eq!(
            error,
            crate::error::ToolError::InvalidArguments {
                tool: "echo".into(),
                message: "Validation failed for tool \"echo\":\n  - text: must have required properties text\n\nReceived arguments:\n{}".into(),
            }
        );
    }

    #[test]
    fn validator_checks_nested_types_and_additional_properties() {
        let schema = JsonValue::parse(
            r#"{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","required":["name"],"additionalProperties":false,"properties":{"name":{"type":"string"}}}}},"additionalProperties":false}"#,
        )
        .expect("schema JSON");
        let error = validate_tool_arguments(
            "nested",
            &schema,
            &SerializedJson::new(r#"{"items":[{"name":false,"extra":1}]}"#),
        )
        .expect_err("nested schema violations");
        assert!(format!("{error:?}").contains("items.0.name"));
        assert!(format!("{error:?}").contains("items.0.extra"));
    }
}
