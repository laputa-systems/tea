//! Immutable model-harness profiles and explicit tool-schema diagnostics.
//!
//! A profile captures provider/model-facing variation without changing the
//! executor-neutral session or core contracts. It is deliberately a value
//! object: a profile identity is derived from canonical fields, and a later
//! profile edit is a new profile rather than an in-place mutation.

use crate::HarnessError;
use std::collections::{BTreeMap, BTreeSet};
use tea_protocol::JsonValue;
use tea_session::{
    ArtifactId, CanonicalHashWriter, Digest, ModelHarnessProfileId,
};

const PROFILE_SCHEMA_VERSION: u16 = 1;
const PROFILE_ABI_VERSION: u16 = 1;

/// Immutable model-specific harness profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelHarnessProfile {
    /// Canonical content-derived identity.
    pub profile_id: ModelHarnessProfileId,
    /// Host-selected provider family, never credentials or endpoint data.
    pub provider_family: String,
    /// Model identifier requested by the host.
    pub requested_model: String,
    /// Returned provider revision when the adapter exposes one.
    pub returned_model_revision: Option<String>,
    /// Immutable trusted base-prompt variant identifier.
    pub base_prompt_variant: String,
    /// Immutable tool-presentation variant identifier.
    pub tool_presentation_variant: String,
    /// Registered default compaction strategy identifier.
    pub default_compaction_strategy: String,
    /// Registered default tool-result projection strategy identifier.
    pub default_projection_strategy: String,
}

impl ModelHarnessProfile {
    /// Construct and content-address one immutable profile.
    pub fn new(
        provider_family: impl Into<String>,
        requested_model: impl Into<String>,
        returned_model_revision: Option<String>,
        base_prompt_variant: impl Into<String>,
        tool_presentation_variant: impl Into<String>,
        default_compaction_strategy: impl Into<String>,
        default_projection_strategy: impl Into<String>,
    ) -> Result<Self, HarnessError> {
        let profile = Self {
            profile_id: ModelHarnessProfileId::new("pending")
                .expect("fixed profile placeholder is a valid opaque ID"),
            provider_family: provider_family.into(),
            requested_model: requested_model.into(),
            returned_model_revision,
            base_prompt_variant: base_prompt_variant.into(),
            tool_presentation_variant: tool_presentation_variant.into(),
            default_compaction_strategy: default_compaction_strategy.into(),
            default_projection_strategy: default_projection_strategy.into(),
        };
        profile.validate_fields()?;
        let profile_id = ModelHarnessProfileId::new(profile.identity_digest().to_hex())
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        Ok(Self { profile_id, ..profile })
    }

    /// Recompute the identity and reject a forged or stale profile record.
    pub fn verify_identity(&self) -> Result<(), HarnessError> {
        self.validate_fields()?;
        let expected = self.identity_digest().to_hex();
        if self.profile_id.as_str() != expected {
            return Err(HarnessError::invalid_state(format!(
                "model-harness profile ID {} does not match canonical content identity {expected}",
                self.profile_id,
            )));
        }
        Ok(())
    }

    /// Return the canonical immutable profile digest.
    pub fn identity_digest(&self) -> Digest {
        let mut writer = CanonicalHashWriter::new(
            "tea-model-harness-profile",
            PROFILE_SCHEMA_VERSION,
            PROFILE_ABI_VERSION,
        );
        writer.string("provider_family", &self.provider_family);
        writer.string("requested_model", &self.requested_model);
        writer.string(
            "returned_model_revision",
            self.returned_model_revision.as_deref().unwrap_or_default(),
        );
        writer.string("base_prompt_variant", &self.base_prompt_variant);
        writer.string("tool_presentation_variant", &self.tool_presentation_variant);
        writer.string("default_compaction_strategy", &self.default_compaction_strategy);
        writer.string("default_projection_strategy", &self.default_projection_strategy);
        writer.finish()
    }

    fn validate_fields(&self) -> Result<(), HarnessError> {
        for (field, value) in [
            ("provider_family", self.provider_family.as_str()),
            ("requested_model", self.requested_model.as_str()),
            ("base_prompt_variant", self.base_prompt_variant.as_str()),
            ("tool_presentation_variant", self.tool_presentation_variant.as_str()),
            (
                "default_compaction_strategy",
                self.default_compaction_strategy.as_str(),
            ),
            (
                "default_projection_strategy",
                self.default_projection_strategy.as_str(),
            ),
        ] {
            if value.is_empty() || value.len() > 240 || value.chars().any(char::is_control) {
                return Err(HarnessError::invalid_state(format!(
                    "model-harness profile {field} must be bounded non-control text",
                )));
            }
        }
        if self
            .returned_model_revision
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > 240 || value.chars().any(char::is_control))
        {
            return Err(HarnessError::invalid_state(
                "model-harness profile returned_model_revision must be bounded non-control text",
            ));
        }
        Ok(())
    }
}

/// One field whose supplied JSON kind differs from the canonical tool schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldMismatch {
    /// JSON-pointer-like field location.
    pub field: String,
    /// Expected canonical schema type name.
    pub expected: String,
    /// Actual supplied JSON type name.
    pub actual: String,
}

/// Structured model/profile evidence for a rejected tool argument object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSchemaDeviation {
    /// Immutable model-harness profile used for the request.
    pub profile_id: ModelHarnessProfileId,
    /// Registered tool whose canonical schema was checked.
    pub tool_name: String,
    /// Fields outside a closed object schema.
    pub unknown_fields: Vec<String>,
    /// Required object fields not supplied.
    pub missing_fields: Vec<String>,
    /// Fields with a visible JSON type mismatch.
    pub type_mismatches: Vec<FieldMismatch>,
    /// Retained raw-arguments artifact. The diagnostic never embeds arguments.
    pub raw_arguments_artifact: ArtifactId,
}

impl ToolSchemaDeviation {
    /// Return whether this evidence captures any schema deviation.
    pub fn is_empty(&self) -> bool {
        self.unknown_fields.is_empty()
            && self.missing_fields.is_empty()
            && self.type_mismatches.is_empty()
    }
}

/// Compare an object argument value to the stable, common closed-schema
/// vocabulary and return content-free structured evidence. The full raw
/// object must already live in `raw_arguments_artifact` before this function
/// is called; callers never attach raw arguments to telemetry.
pub fn inspect_tool_schema_deviation(
    profile_id: ModelHarnessProfileId,
    tool_name: impl Into<String>,
    schema: &JsonValue,
    arguments: &JsonValue,
    raw_arguments_artifact: ArtifactId,
) -> Result<Option<ToolSchemaDeviation>, HarnessError> {
    let tool_name = tool_name.into();
    let JsonValue::Object(schema) = schema else {
        return Err(HarnessError::invalid_state(
            "tool-schema deviation inspection requires an object schema",
        ));
    };
    let JsonValue::Object(arguments) = arguments else {
        return Ok(Some(ToolSchemaDeviation {
            profile_id,
            tool_name,
            unknown_fields: Vec::new(),
            missing_fields: required_field_names(schema)?,
            type_mismatches: vec![FieldMismatch {
                field: "".into(),
                expected: "object".into(),
                actual: json_kind_name(arguments).into(),
            }],
            raw_arguments_artifact,
        }));
    };
    let properties = match schema.get("properties") {
        None => BTreeMap::new(),
        Some(JsonValue::Object(properties)) => properties.clone(),
        Some(_) => {
            return Err(HarnessError::invalid_state(
                "tool-schema deviation inspection requires object properties when present",
            ));
        }
    };
    let additional_properties = schema
        .get("additionalProperties")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    let mut unknown_fields = if additional_properties {
        Vec::new()
    } else {
        arguments
            .keys()
            .filter(|field| !properties.contains_key(*field))
            .cloned()
            .collect()
    };
    unknown_fields.sort();
    let mut missing_fields = required_field_names(schema)?
        .into_iter()
        .filter(|field| !arguments.contains_key(field))
        .collect::<Vec<_>>();
    missing_fields.sort();
    let mut type_mismatches = Vec::new();
    for (field, property_schema) in &properties {
        let Some(value) = arguments.get(field) else {
            continue;
        };
        let Some(expected) = property_schema
            .get("type")
            .and_then(JsonValue::as_str)
        else {
            continue;
        };
        let actual = json_kind_name(value);
        if expected != actual {
            type_mismatches.push(FieldMismatch {
                field: field.clone(),
                expected: expected.into(),
                actual: actual.into(),
            });
        }
    }
    type_mismatches.sort_by(|left, right| left.field.cmp(&right.field));
    let deviation = ToolSchemaDeviation {
        profile_id,
        tool_name,
        unknown_fields,
        missing_fields,
        type_mismatches,
        raw_arguments_artifact,
    };
    Ok((!deviation.is_empty()).then_some(deviation))
}

fn required_field_names(schema: &BTreeMap<String, JsonValue>) -> Result<Vec<String>, HarnessError> {
    let Some(required) = schema.get("required") else {
        return Ok(Vec::new());
    };
    let JsonValue::Array(required) = required else {
        return Err(HarnessError::invalid_state(
            "tool-schema deviation inspection requires required to be an array",
        ));
    };
    let mut fields = BTreeSet::new();
    for field in required {
        let Some(field) = field.as_str() else {
            return Err(HarnessError::invalid_state(
                "tool-schema deviation inspection requires string required fields",
            ));
        };
        fields.insert(field.to_owned());
    }
    Ok(fields.into_iter().collect())
}

fn json_kind_name(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}
