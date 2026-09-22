"""Bridge OpenAI function tools onto Apple Foundation Models tools.

The on-device model executes tools *inside* its own session: there is no mode
that returns a proposed call to the caller. Every converted tool therefore
records the call it receives and hands the session a fixed placeholder result,
and the server reports the recorded call to the client as an OpenAI
``tool_calls`` entry. Text the model generates *after* a recorded call was
produced against that placeholder, so the backend discards it.

JSON Schema and Apple's ``GenerationSchema`` overlap but are not the same
language. Supported keywords are translated; the rest are dropped and named in
the returned warnings, because silently dropping a constraint would change what
the model is asked to produce. Dropping one never relaxes what the *host*
accepts: tea validates every tool call against its own authoritative schema
before executing it.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from typing import Any, List, Mapping, Sequence, Union

from .protocol import ProtocolError, ToolSpec

# Handed to the session in place of a real result. The client executes the call
# and returns the true output on its next request.
DEFERRED_TOOL_RESULT = "Deferred: the client executes this tool call."

# JSON Schema primitives that map onto a Python type Apple can constrain.
_PRIMITIVES: Mapping[str, Any] = {
    "string": str,
    "integer": int,
    "number": float,
    "boolean": bool,
}

# Keywords this bridge cannot express. Each is reported so a caller can see
# exactly which part of a tool's contract reached the model and which did not.
_UNSUPPORTED_KEYWORDS = (
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "const",
    "patternProperties",
    "propertyNames",
    "if",
    "then",
    "else",
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "uniqueItems",
    "contains",
    "additionalItems",
)


@dataclass(frozen=True, slots=True)
class RecordedCall:
    """One tool call the model made, in the form an OpenAI client expects."""

    name: str
    call_id: str
    arguments: str


class ToolCallRecorder:
    """Collects the calls the on-device model makes during one request.

    A recorded call is the signal that the *client* owns execution, so the
    backend can both report the call and stop trusting generated text.
    """

    def __init__(self) -> None:
        self._calls: list[RecordedCall] = []

    @property
    def calls(self) -> tuple[RecordedCall, ...]:
        return tuple(self._calls)

    @property
    def recorded(self) -> bool:
        """Whether any call has been recorded so far.

        The backend polls this between streamed chunks to stop emitting text the
        model produced against a placeholder result.
        """
        return bool(self._calls)

    def record(self, name: str, arguments: str) -> str:
        """Record one call and return the placeholder result for the session."""
        self._calls.append(
            RecordedCall(name=name, call_id=f"call_{uuid.uuid4().hex[:24]}", arguments=arguments)
        )
        return DEFERRED_TOOL_RESULT


def convert_tools(
    fm: Any, specs: Sequence[ToolSpec], recorder: ToolCallRecorder
) -> tuple[list[Any], list[str]]:
    """Convert *specs* into Apple tools bound to *recorder*.

    Returns the tools and the schema warnings, one line per dropped keyword.
    """
    tools: list[Any] = []
    warnings: list[str] = []
    for spec in specs:
        tools.append(_build_tool(fm, spec, recorder, warnings))
    return tools, warnings


def _build_tool(fm: Any, spec: ToolSpec, recorder: ToolCallRecorder, warnings: list[str]) -> Any:
    schema = generation_schema(fm, spec.name, spec.description, spec.parameters, warnings)

    async def call(_self: Any, args: Any) -> str:
        return recorder.record(spec.name, args.to_json())

    namespace = {
        "name": spec.name,
        "description": spec.description,
        # The property is evaluated once per tool construction and the SDK keeps
        # the result alive; returning a prebuilt schema keeps names stable.
        "arguments_schema": property(lambda _self: schema),
        "call": call,
    }
    return type(f"Tool_{_identifier(spec.name)}", (fm.Tool,), namespace)()


def generation_schema(
    fm: Any, name: str, description: str, schema: Mapping[str, Any], warnings: list[str]
) -> Any:
    """Translate one JSON Schema object into an Apple ``GenerationSchema``."""
    if schema.get("type") not in (None, "object"):
        raise ProtocolError(f"tool '{name}' parameters must be a JSON Schema object")
    root, _ = _object_schema(fm, _identifier(name), description, schema, warnings, path=name)
    return root


class _Optional:
    """The Apple optional marker for ``inner``, standing in for ``Optional[inner]``.

    ``apple_fm_sdk`` decides a property is optional by testing whether the
    literal text ``Optional`` appears in ``str(type_class)``. On Python 3.14
    ``str(Optional[str])`` is ``"str | None"``, so a property JSON Schema omits
    from ``required`` would otherwise be sent to the model as required — the
    model could then never leave it out and every optional argument would have
    to be invented. This wrapper keeps the union shape Apple's type mapping
    reads while spelling its name the way the optionality test expects.
    """

    __slots__ = ("_inner",)

    __origin__ = Union

    def __init__(self, inner: Any) -> None:
        self._inner = inner

    @property
    def __args__(self) -> tuple[Any, Any]:
        return (self._inner, type(None))

    def __str__(self) -> str:
        name = getattr(self._inner, "__name__", str(self._inner))
        return f"typing.Optional[{name}]"

    def __repr__(self) -> str:
        return str(self)


def _object_schema(
    fm: Any,
    type_name: str,
    description: str,
    schema: Mapping[str, Any],
    warnings: list[str],
    *,
    path: str,
) -> tuple[Any, list[Any]]:
    """Build an object schema and return it with every schema nested beneath it.

    Apple resolves nested references from the schema they are used in, so an
    intermediate schema and the root both carry the full descendant list.
    Registering only direct children leaves deeper references undefined, which
    the SDK rejects when the tool is created.
    """
    schema, note = _merge_object_variants(schema, warnings, path=path)
    if note:
        description = f"{description}\n\n{note}".strip()
    raw_properties = schema.get("properties")
    properties = raw_properties if isinstance(raw_properties, dict) else {}
    required = schema.get("required")
    required_names = set(required) if isinstance(required, list) else set()

    nested: list[Any] = []
    built: list[Any] = []
    for property_name, property_schema in properties.items():
        if not isinstance(property_schema, dict):
            continue
        child_path = f"{path}.{property_name}"
        type_class, child_schemas = _type_class(fm, property_schema, warnings, path=child_path)
        nested.extend(child_schemas)
        if property_name not in required_names:
            type_class = _Optional(type_class)
        built.append(
            _property_class()(
                name=property_name,
                type_class=type_class,
                description=_description(property_schema),
                guides=_guides(fm, property_schema, warnings, path=child_path),
            )
        )

    _report_unsupported(schema, warnings, path=path)
    return (
        fm.GenerationSchema(
            type_class=type(type_name, (), {}),
            description=description,
            properties=built,
            dynamic_nested_types=list(nested),
        ),
        nested,
    )


def _type_class(
    fm: Any, schema: Mapping[str, Any], warnings: list[str], *, path: str
) -> tuple[Any, list[Any]]:
    """Return (type class, every schema nested beneath this node)."""
    schema, note = _merge_object_variants(schema, warnings, path=path)
    _report_unsupported(schema, warnings, path=path)
    declared = schema.get("type")
    if isinstance(declared, list):
        # A union of primitives has no Apple equivalent; the first supported
        # member is used and the narrowing is reported.
        declared = next((entry for entry in declared if entry in _PRIMITIVES or entry == "object"), None)
        if declared is not None:
            warnings.append(f"{path}: union type narrowed to '{declared}'")
    if declared == "array":
        items = schema.get("items")
        if not isinstance(items, dict):
            warnings.append(f"{path}: array without an 'items' schema treated as array<string>")
            return List[str], []
        element_class, nested = _type_class(fm, items, warnings, path=f"{path}[]")
        return List[element_class], nested
    if declared == "object" or "properties" in schema:
        nested_schema, nested = _object_schema(
            fm, _identifier(f"{path}_object"), note, schema, warnings, path=path
        )
        # A nested object is referenced by the name of its synthetic type class,
        # which is how Apple links a property to a schema in dynamic_nested_types.
        return nested_schema.type_class, [nested_schema, *nested]
    if isinstance(declared, str) and declared in _PRIMITIVES:
        return _PRIMITIVES[declared], []
    if isinstance(schema.get("enum"), list):
        warnings.append(f"{path}: untyped enum treated as string")
        return str, []
    warnings.append(f"{path}: unrecognized type treated as string")
    return str, []


def _merge_object_variants(
    schema: Mapping[str, Any], warnings: list[str], *, path: str
) -> tuple[Mapping[str, Any], str]:
    """Rewrite a pure ``oneOf``/``anyOf`` choice of object shapes into one object schema.

    Apple's schema language has no "exactly one of" construct. Dropping the
    choice entirely is worse than merging it: the model is left with an object
    that declares no properties, so it calls the tool with ``{}`` — observed
    live on tea's ``web`` tool, whose entire schema is a choice between a
    ``query`` search and a ``urls`` fetch. The merged schema is the union of the
    alternatives, with only the fields required by *every* alternative required;
    that admits a superset of the valid calls, which is the most that can be
    said without the constraint.

    Returns the schema to use and a note describing the choice, or the schema
    unchanged with an empty note when there is nothing to merge.
    """
    if "properties" in schema or schema.get("type") not in (None, "object"):
        return schema, ""
    for keyword in ("oneOf", "anyOf"):
        variants = schema.get(keyword)
        if not isinstance(variants, list) or not variants:
            continue
        if not all(
            isinstance(variant, dict)
            and (isinstance(variant.get("properties"), dict) or variant.get("type") == "object")
            for variant in variants
        ):
            return schema, ""

        properties: dict[str, Any] = {}
        required_sets: list[set[str]] = []
        for variant in variants:
            properties.update(variant.get("properties") or {})
            declared_required = variant.get("required")
            required_sets.append(set(declared_required) if isinstance(declared_required, list) else set())
        merged = dict(schema)
        merged.pop(keyword, None)
        merged["type"] = "object"
        merged["properties"] = properties
        merged["required"] = sorted(set.intersection(*required_sets)) if required_sets else []

        shapes = " or ".join(
            "{" + ", ".join(sorted(required)) + "}" if required else "{no required fields}"
            for required in (sorted(required_set) for required_set in required_sets)
        )
        warnings.append(
            f"{path}: '{keyword}' merged into one object schema; the exactly-one-of requirement "
            "is not enforced"
        )
        return merged, f"Provide exactly one of these shapes: {shapes}."
    return schema, ""


def _guides(fm: Any, schema: Mapping[str, Any], warnings: list[str], *, path: str) -> list[Any]:
    """Translate the JSON Schema keywords Apple can constrain."""
    guides: list[Any] = []
    enum = schema.get("enum")
    if isinstance(enum, list) and enum and all(isinstance(value, str) for value in enum):
        guides.append(fm.GenerationGuide.anyOf(list(enum)))
    elif isinstance(enum, list) and enum:
        warnings.append(f"{path}: non-string enum dropped")
    minimum = schema.get("minimum")
    if isinstance(minimum, (int, float)) and not isinstance(minimum, bool):
        guides.append(fm.GenerationGuide.minimum(minimum))
    maximum = schema.get("maximum")
    if isinstance(maximum, (int, float)) and not isinstance(maximum, bool):
        guides.append(fm.GenerationGuide.maximum(maximum))
    min_items = schema.get("minItems")
    if isinstance(min_items, int) and not isinstance(min_items, bool):
        guides.append(fm.GenerationGuide.min_items(min_items))
    max_items = schema.get("maxItems")
    if isinstance(max_items, int) and not isinstance(max_items, bool):
        guides.append(fm.GenerationGuide.max_items(max_items))
    pattern = schema.get("pattern")
    if isinstance(pattern, str):
        guides.append(fm.GenerationGuide.regex(pattern))
    return guides


def _report_unsupported(schema: Mapping[str, Any], warnings: list[str], *, path: str) -> None:
    for keyword in _UNSUPPORTED_KEYWORDS:
        if keyword in schema:
            warnings.append(f"{path}: '{keyword}' has no Foundation Models equivalent and was dropped")


def _description(schema: Mapping[str, Any]) -> str | None:
    description = schema.get("description")
    return description if isinstance(description, str) and description else None


def _property_class() -> Any:
    """Apple's schema ``Property`` class.

    ``apple_fm_sdk`` re-exports ``GenerationSchema`` and ``GenerationGuide`` at
    package level but not ``Property``, so the schema submodule is imported
    explicitly rather than relying on it being loaded as a side effect.
    """
    from apple_fm_sdk import generation_property

    return generation_property.Property


def _identifier(name: str) -> str:
    """A type-name-safe identifier derived from *name*."""
    cleaned = "".join(character if character.isalnum() else "_" for character in name)
    return cleaned or "value"
