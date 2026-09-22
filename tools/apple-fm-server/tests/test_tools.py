"""JSON Schema translation against the real Apple schema constructors.

These tests need ``apple_fm_sdk``. They are skipped where it is absent so the
wire-level tests still run on a non-macOS checkout.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from apple_fm_server.protocol import ToolSpec
from apple_fm_server.tools import ToolCallRecorder, convert_tools, generation_schema

fm = pytest.importorskip("apple_fm_sdk")

# The exact tool payload tea's terminal host sends, captured from a real request.
# See the fixture's `_provenance` block for how to regenerate it.
_HARNESS_TOOLS = Path(__file__).parent / "fixtures" / "tea-harness-tools.json"


def harness_tools() -> list[dict]:
    """Every tool function tea's default coding harness offers the model."""
    import json

    return json.loads(_HARNESS_TOOLS.read_text())["tools"]


def harness_spec(specs: list[dict], name: str) -> dict:
    """The parameter schema of one harness tool, by name."""
    for tool in specs:
        if tool["function"]["name"] == name:
            return tool["function"]["parameters"]
    raise AssertionError(f"the fixture has no {name} tool")


def test_every_harness_tool_converts_to_an_apple_tool():
    """The bridge must accept the exact schemas tea sends on a real request."""
    recorder = ToolCallRecorder()
    tools_sent = harness_tools()
    specs = [
        ToolSpec(
            name=tool["function"]["name"],
            description=tool["function"]["description"],
            parameters=tool["function"]["parameters"],
        )
        for tool in tools_sent
    ]

    tools, warnings = convert_tools(fm, specs, recorder)

    assert [tool.name for tool in tools] == [spec.name for spec in specs]
    # edit marks its object with oneOf to require either `edits` or `content`;
    # Apple has no equivalent, and the drop must be visible rather than silent.
    assert any("oneOf" in warning and "edit" in warning for warning in warnings)


def test_nested_object_arrays_survive_translation():
    """`edit` nests arrays of objects two levels deep; both levels must build."""
    schema = harness_spec(harness_tools(), "edit")
    converted = generation_schema(fm, "edit", "edit tool", schema, warnings=[])

    schema_dict = converted.to_dict()
    files = schema_dict["properties"]["files"]
    assert files["type"] == "array"

    # Walk files[] -> edits[] and confirm the deepest referenced schema is
    # registered. A missing registration is what the SDK rejects when the tool
    # is created ("contains undefined references").
    defs = schema_dict["$defs"]
    assert len(defs) == 2, f"both nesting levels must be registered, got {sorted(defs)}"
    files_item = files["items"]["$ref"].rsplit("/", 1)[-1]
    assert files_item in defs
    edits_item = defs[files_item]["properties"]["edits"]["items"]["$ref"].rsplit("/", 1)[-1]
    assert edits_item in defs


def test_a_pure_one_of_object_schema_merges_instead_of_losing_its_properties():
    """tea's `web` tool is exactly this shape: a choice of two object variants.

    Dropping the choice would leave an object with no properties, and the model
    called it with `{}`, which tea's host validation rejects.
    """
    warnings: list[str] = []
    converted = generation_schema(
        fm, "web", "Search the web.", harness_spec(harness_tools(), "web"), warnings=warnings
    )
    schema_dict = converted.to_dict()

    assert set(schema_dict["properties"]) == {"query", "kind", "limit", "urls"}
    # Only the fields required by every variant survive as required; neither
    # variant requires the other's, so nothing is forced.
    assert schema_dict["required"] == []
    assert "Provide exactly one of these shapes: {query} or {urls}." in schema_dict["description"]
    assert any("merged into one object schema" in warning for warning in warnings)


def test_a_nested_one_of_constraint_is_still_reported_as_dropped():
    """`edit` marks its own object with oneOf to require one of two fields."""
    warnings: list[str] = []
    converted = generation_schema(
        fm, "edit", "Edit files.", harness_spec(harness_tools(), "edit"), warnings=warnings
    )

    assert any("oneOf" in warning and "not enforced" not in warning for warning in warnings)
    files_item = converted.to_dict()["$defs"]
    assert files_item  # the nested schemas still build


def test_a_recorded_call_is_what_the_client_receives():
    recorder = ToolCallRecorder()
    specs = [
        ToolSpec(
            name="bash",
            description="Run a command.",
            parameters={
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            },
        )
    ]
    tools, warnings = convert_tools(fm, specs, recorder)

    assert warnings == []
    assert recorder.recorded is False
    placeholder = recorder.record("bash", '{"command":"ls"}')
    assert placeholder  # the session is handed something in place of a real result

    calls = recorder.calls
    assert recorder.recorded is True
    assert calls[0].name == "bash"
    assert calls[0].arguments == '{"command":"ls"}'
    assert calls[0].call_id.startswith("call_")
    assert tools  # the tool objects stay alive for the session


def test_required_properties_are_not_optional_and_optional_ones_are():
    recorder = ToolCallRecorder()
    specs = [
        ToolSpec(
            name="t",
            description="",
            parameters={
                "type": "object",
                "properties": {"required_one": {"type": "string"}, "optional_one": {"type": "integer"}},
                "required": ["required_one"],
            },
        )
    ]
    tools, _ = convert_tools(fm, specs, recorder)
    schema_dict = tools[0].arguments_schema.to_dict()

    # Apple expresses optionality through the schema's `required` list, which is
    # what the model is actually guided by.
    assert schema_dict["required"] == ["required_one"]


def test_primitive_types_map_onto_apple_types():
    converted = generation_schema(
        fm,
        "types",
        "",
        {
            "type": "object",
            "properties": {
                "s": {"type": "string"},
                "i": {"type": "integer"},
                "n": {"type": "number"},
                "b": {"type": "boolean"},
                "l": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["s", "i", "n", "b", "l"],
        },
        warnings=[],
    )
    properties = converted.to_dict()["properties"]

    assert properties["s"]["type"] == "string"
    assert properties["i"]["type"] == "integer"
    assert properties["n"]["type"] == "number"
    assert properties["b"]["type"] == "boolean"
    assert properties["l"]["type"] == "array"
    assert properties["l"]["items"]["type"] == "string"


def test_numeric_and_array_bounds_become_guides():
    warnings: list[str] = []
    converted = generation_schema(
        fm,
        "bounded",
        "",
        {
            "type": "object",
            "properties": {
                "count": {"type": "integer", "minimum": 1, "maximum": 10},
                "items": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 4},
            },
        },
        warnings=warnings,
    )

    guides = {prop.name: [guide.guide_type.name for guide in prop.guides] for prop in converted.properties}
    assert guides["count"] == ["minimum", "maximum"]
    assert guides["items"] == ["minItems", "maxItems"]
    assert warnings == []


def test_string_enums_become_any_of_guides():
    converted = generation_schema(
        fm,
        "choice",
        "",
        {"type": "object", "properties": {"mode": {"type": "string", "enum": ["a", "b"]}}},
        warnings=[],
    )

    assert [guide.guide_type.name for guide in converted.properties[0].guides] == ["anyOf"]


def test_unexpressible_keywords_are_reported_not_dropped_silently():
    warnings: list[str] = []
    generation_schema(
        fm,
        "lossy",
        "",
        {
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 10},
                "either": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
            },
        },
        warnings=warnings,
    )

    reported = " ".join(warnings)
    assert "minLength" in reported and "maxLength" in reported and "oneOf" in reported


def test_a_non_object_root_schema_is_rejected():
    from apple_fm_server.protocol import ProtocolError

    with pytest.raises(ProtocolError, match="JSON Schema object"):
        generation_schema(fm, "bad", "", {"type": "array"}, warnings=[])
