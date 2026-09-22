"""Pinned Rust toolchain reader.

``rust-toolchain.toml`` at the repository root is the single source of truth
for the pinned nightly channel. Nothing else may hardcode the channel: shell
entry points use plain ``cargo`` (the rustup shim resolves the pin; the
Dockerfile derives the channel from the toml for its install step), and
Python entry points use ``pinned_toolchain()`` here when they need the
channel for recorded evidence.
"""

from __future__ import annotations

from pathlib import Path
import re


_CHANNEL_RE = re.compile(r'^channel\s*=\s*"([^"]+)"', re.MULTILINE)


def repository_root() -> Path:
    return Path(__file__).resolve().parents[2]


def pinned_toolchain(root: Path | None = None) -> str:
    """Return the pinned nightly channel from ``rust-toolchain.toml``."""

    base = root if root is not None else repository_root()
    path = base / "rust-toolchain.toml"
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"missing Rust toolchain pin: {path}: {error}") from error
    try:
        import tomllib
    except ImportError:  # Python < 3.11: fall through to the regex below.
        tomllib = None  # type: ignore[assignment]
    if tomllib is not None:
        try:
            data = tomllib.loads(text)
        except ValueError:
            data = None
        if isinstance(data, dict):
            section = data.get("toolchain")
            if isinstance(section, dict):
                channel = section.get("channel")
                if isinstance(channel, str) and channel:
                    return channel
    match = _CHANNEL_RE.search(text)
    if match:
        return match.group(1)
    raise ValueError(f"rust-toolchain.toml has no toolchain channel: {path}")
