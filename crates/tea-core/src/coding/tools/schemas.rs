//! JSON schemas for the pinned standard coding tools.

use std::collections::BTreeMap;
use tea_protocol::{JsonNumber, JsonValue};

pub(crate) fn schema_object(
    required: &[&str],
    properties: impl IntoIterator<Item = (&'static str, JsonValue)>,
) -> JsonValue {
    let mut schema = BTreeMap::from([
        ("type".to_owned(), JsonValue::String("object".to_owned())),
        ("properties".to_owned(), JsonValue::object(properties)),
    ]);
    if !required.is_empty() {
        schema.insert(
            "required".to_owned(),
            JsonValue::Array(
                required
                    .iter()
                    .map(|name| JsonValue::String((*name).to_owned()))
                    .collect(),
            ),
        );
    }
    JsonValue::Object(schema)
}

pub(crate) fn schema_string(description: &'static str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("string".to_owned())),
        ("description", JsonValue::String(description.to_owned())),
    ])
}

pub(crate) fn schema_number(description: &'static str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("number".to_owned())),
        ("description", JsonValue::String(description.to_owned())),
    ])
}

pub(crate) fn schema_boolean(description: &'static str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("boolean".to_owned())),
        ("description", JsonValue::String(description.to_owned())),
    ])
}

pub(crate) fn read_schema() -> JsonValue {
    schema_object(
        &["path"],
        [
            (
                "path",
                schema_string("Path to the file to read (relative or absolute)"),
            ),
            (
                "offset",
                schema_number("Line number to start reading from (1-indexed)"),
            ),
            ("limit", schema_number("Maximum number of lines to read")),
        ],
    )
}

/// Tea v2 read schema. Pi's captured v1 schema remains immutable in
/// [`read_schema`]; the digest option belongs only to the explicit v2 profile.
pub(crate) fn read_v2_schema() -> JsonValue {
    schema_object(
        &["path"],
        [
            (
                "path",
                schema_string("Path to the file to read (relative or absolute)"),
            ),
            (
                "offset",
                schema_number("Line number to start reading from (1-indexed)"),
            ),
            ("limit", schema_number("Maximum number of lines to read")),
            (
                "includeDigest",
                schema_boolean(
                    "Include the complete-file BLAKE3 digest in the result (default: false)",
                ),
            ),
        ],
    )
}

pub(crate) fn bash_schema() -> JsonValue {
    schema_object(
        &["command"],
        [
            ("command", schema_string("Bash command to execute")),
            (
                "timeout",
                schema_number("Timeout in seconds (optional, no default timeout)"),
            ),
        ],
    )
}

pub(crate) fn edit_schema() -> JsonValue {
    schema_object(
        &["path", "edits"],
        [
            ("path", schema_string("Path to the file to edit (relative or absolute)")),
            (
                "edits",
                JsonValue::object([
                    ("type", JsonValue::String("array".to_owned())),
                    (
                        "items",
                        schema_object(
                            &["oldText", "newText"],
                            [
                                ("oldText", schema_string("Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.")),
                                ("newText", schema_string("Replacement text for this targeted edit.")),
                            ],
                        ),
                    ),
                    ("description", JsonValue::String("One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.".to_owned())),
                ]),
            ),
        ],
    )
}

/// Tea v2 multi-file exact-edit schema.
///
/// This intentionally has no v1 `path`/`edits` alternative. A Tea v2 profile
/// is an immutable composition, so a model cannot accidentally believe one
/// `edit` invocation accepts two incompatible shapes.
pub(crate) fn edit_v2_schema() -> JsonValue {
    let edit = JsonValue::object([
        ("type", JsonValue::String("object".to_owned())),
        (
            "required",
            JsonValue::Array(vec![
                JsonValue::String("oldText".to_owned()),
                JsonValue::String("newText".to_owned()),
            ]),
        ),
        ("additionalProperties", JsonValue::Bool(false)),
        (
            "properties",
            JsonValue::object([
                (
                    "oldText",
                    schema_string(
                        "Exact non-empty original text. It must occur exactly once in this file's original snapshot.",
                    ),
                ),
                (
                    "newText",
                    schema_string("Replacement text for this targeted edit."),
                ),
            ]),
        ),
    ]);
    let file = JsonValue::object([
        ("type", JsonValue::String("object".to_owned())),
        (
            "required",
            JsonValue::Array(vec![
                JsonValue::String("path".to_owned()),
                JsonValue::String("edits".to_owned()),
            ]),
        ),
        (
            "additionalProperties",
            JsonValue::Bool(false),
        ),
        (
            "properties",
            JsonValue::object([
                (
                    "path",
                    JsonValue::object([
                        ("type", JsonValue::String("string".to_owned())),
                        ("maxLength", JsonValue::Number(JsonNumber::Unsigned(4096))),
                        ("description", JsonValue::String("Existing in-workspace UTF-8 file path (relative or absolute). The same canonical file may appear only once.".to_owned())),
                    ]),
                ),
                (
                    "expectedDigest",
                    JsonValue::object([
                        ("type", JsonValue::String("string".to_owned())),
                        ("minLength", JsonValue::Number(JsonNumber::Unsigned(64))),
                        ("maxLength", JsonValue::Number(JsonNumber::Unsigned(64))),
                        ("description", JsonValue::String("Optional complete-file BLAKE3 digest returned by read(includeDigest=true). Rejects a stale snapshot before any file is written.".to_owned())),
                    ]),
                ),
                (
                    "edits",
                    JsonValue::object([
                        ("type", JsonValue::String("array".to_owned())),
                        ("minItems", JsonValue::Number(JsonNumber::Unsigned(1))),
                        ("maxItems", JsonValue::Number(JsonNumber::Unsigned(64))),
                        ("items", edit),
                        ("description", JsonValue::String("Exact replacements, all matched against this file's original snapshot. Every oldText must match exactly once and replacements must not overlap.".to_owned())),
                    ]),
                ),
            ]),
        ),
    ]);
    JsonValue::object([
        ("type", JsonValue::String("object".to_owned())),
        (
            "required",
            JsonValue::Array(vec![JsonValue::String("files".to_owned())]),
        ),
        ("additionalProperties", JsonValue::Bool(false)),
        (
            "properties",
            JsonValue::object([(
                "files",
                JsonValue::object([
                    ("type", JsonValue::String("array".to_owned())),
                    ("minItems", JsonValue::Number(JsonNumber::Unsigned(1))),
                    ("maxItems", JsonValue::Number(JsonNumber::Unsigned(32))),
                    ("items", file),
                    ("description", JsonValue::String("A complete coordinated edit plan for one to 32 existing files. All paths and exact-match preconditions are validated before the host transaction is requested; a host that cannot establish rollback reports an indeterminate outcome.".to_owned())),
                ]),
            )]),
        ),
    ])
}

pub(crate) fn write_schema() -> JsonValue {
    schema_object(
        &["path", "content"],
        [
            (
                "path",
                schema_string("Path to the file to write (relative or absolute)"),
            ),
            ("content", schema_string("Content to write to the file")),
        ],
    )
}

pub(crate) fn grep_schema() -> JsonValue {
    schema_object(
        &["pattern"],
        [
            (
                "pattern",
                schema_string("Search pattern (regex or literal string)"),
            ),
            (
                "path",
                schema_string("Directory or file to search (default: current directory)"),
            ),
            (
                "glob",
                schema_string("Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"),
            ),
            (
                "ignoreCase",
                schema_boolean("Case-insensitive search (default: false)"),
            ),
            (
                "literal",
                schema_boolean("Treat pattern as literal string instead of regex (default: false)"),
            ),
            (
                "context",
                schema_number("Number of lines to show before and after each match (default: 0)"),
            ),
            (
                "limit",
                schema_number("Maximum number of matches to return (default: 100)"),
            ),
        ],
    )
}

pub(crate) fn find_schema() -> JsonValue {
    schema_object(
        &["pattern"],
        [
            (
                "pattern",
                schema_string(
                    "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'",
                ),
            ),
            (
                "path",
                schema_string("Directory to search in (default: current directory)"),
            ),
            (
                "limit",
                schema_number("Maximum number of results (default: 1000)"),
            ),
        ],
    )
}

pub(crate) fn ls_schema() -> JsonValue {
    schema_object(
        &[],
        [
            (
                "path",
                schema_string("Directory to list (default: current directory)"),
            ),
            (
                "limit",
                schema_number("Maximum number of entries to return (default: 500)"),
            ),
        ],
    )
}
