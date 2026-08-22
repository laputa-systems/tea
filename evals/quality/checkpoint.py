"""Deterministic structured-checkpoint and workspace-ledger candidate.

This is an explicitly experimental, provider-free strategy component. It owns
only the schema, marker recognition, latest-wins merge, and exact-operation
ledger used by quality fixtures; it does not claim to generate a semantic
summary or bind itself as the runtime default.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
from typing import Iterable


MARKER_PREFIX = "<!-- tea-checkpoint:v1 generation="
HEADINGS = (
    "Goal",
    "Constraints and Preferences",
    "Current Checkpoint",
    "Decisions and Rationale",
    "Progress — Done",
    "Progress — In Progress",
    "Progress — Blocked",
    "Failed Attempts",
    "Verification",
    "Workspace Ledger",
    "Next Concrete Action",
    "Critical Context",
)
MAX_LEDGER_ENTRIES = 64


@dataclass(frozen=True)
class LedgerEntry:
    """One exact operation fact observed outside a model-generated summary."""

    kind: str
    target: str
    cwd: str | None
    status: str
    diagnostic_fingerprint: str | None = None
    generation: int = 0

    def key(self) -> tuple[str, str, str | None, str, str | None]:
        return (self.kind, self.target, self.cwd, self.status, self.diagnostic_fingerprint)


@dataclass
class WorkspaceLedger:
    """Bounded, ordered, de-duplicated operational facts."""

    entries: list[LedgerEntry] = field(default_factory=list)

    def merge(self, incoming: Iterable[LedgerEntry]) -> "WorkspaceLedger":
        merged = {entry.key(): entry for entry in self.entries}
        for entry in incoming:
            previous = merged.get(entry.key())
            if previous is None or entry.generation >= previous.generation:
                merged[entry.key()] = entry
        ordered = sorted(
            merged.values(),
            key=lambda entry: (entry.generation, entry.kind, entry.target, entry.cwd or "", entry.status),
        )[-MAX_LEDGER_ENTRIES:]
        return WorkspaceLedger(entries=ordered)


@dataclass
class StructuredCheckpoint:
    """Human-readable v1 checkpoint with exact marker-based recognition."""

    generation: int
    sections: dict[str, list[str]] = field(default_factory=dict)
    ledger: WorkspaceLedger = field(default_factory=WorkspaceLedger)

    @classmethod
    def empty(cls) -> "StructuredCheckpoint":
        return cls(generation=0, sections={heading: [] for heading in HEADINGS})

    def merge(self, updates: dict[str, Iterable[str]], ledger_delta: Iterable[LedgerEntry]) -> "StructuredCheckpoint":
        sections = {heading: list(self.sections.get(heading, ())) for heading in HEADINGS}
        for heading, values in updates.items():
            if heading not in sections:
                raise ValueError(f"unknown checkpoint heading: {heading}")
            # Latest-wins at a section boundary: the incoming generation is
            # authoritative, while exact duplicate lines remain collapsed.
            sections[heading] = list(dict.fromkeys(value.strip() for value in values if value.strip()))
        return StructuredCheckpoint(
            generation=self.generation + 1,
            sections=sections,
            ledger=self.ledger.merge(ledger_delta),
        )

    def render(self) -> str:
        lines = [f"{MARKER_PREFIX}{self.generation} -->"]
        for heading in HEADINGS:
            if heading == "Workspace Ledger":
                continue
            lines.extend((f"## {heading}", *(f"- {value}" for value in self.sections.get(heading, ()))))
        lines.append("## Workspace Ledger")
        lines.extend(
            f"- {entry.kind} | {entry.target} | {entry.status} | {entry.cwd or '-'} | "
            f"{entry.diagnostic_fingerprint or '-'}"
            for entry in self.ledger.entries
        )
        return "\n".join(lines)


def checkpoint_fingerprint(text: str) -> str:
    """Stable non-secret fingerprint used by quality artifacts."""

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def parse_checkpoint(text: str) -> StructuredCheckpoint | None:
    """Recognize only the exact v1 marker; never fuzzy-match ordinary prose."""

    first, _, remainder = text.partition("\n")
    if not first.startswith(MARKER_PREFIX) or not first.endswith(" -->"):
        return None
    try:
        generation = int(first[len(MARKER_PREFIX) : -4])
    except ValueError:
        return None
    checkpoint = StructuredCheckpoint.empty()
    checkpoint.generation = generation
    current: str | None = None
    for line in remainder.splitlines():
        if line.startswith("## "):
            candidate = line[3:]
            current = candidate if candidate in checkpoint.sections else None
        elif current and line.startswith("- "):
            checkpoint.sections[current].append(line[2:])
    return checkpoint
