#!/usr/bin/env python3
"""Reject workspace dependency edges that violate Tea's crate boundaries."""

from __future__ import annotations

import json
import subprocess
import sys
from collections import deque


def descendants(start: str, edges: dict[str, set[str]]) -> set[str]:
    seen: set[str] = set()
    pending = deque(edges.get(start, ()))
    while pending:
        package = pending.popleft()
        if package in seen:
            continue
        seen.add(package)
        pending.extend(edges.get(package, ()))
    return seen


def main() -> int:
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1"], text=True
        )
    )
    package_names = {package["id"]: package["name"] for package in metadata["packages"]}
    edges = {
        node["id"]: {
            dependency["pkg"]
            for dependency in node["deps"]
            if dependency.get("dep_kinds")
        }
        for node in metadata["resolve"]["nodes"]
    }
    names_to_ids: dict[str, str] = {
        package_names[package_id]: package_id for package_id in metadata["workspace_members"]
    }
    required_workspace = {
        "tea-protocol",
        "tea-session",
        "tea-trace",
        "tea-http",
        "tea-core",
        "tea-providers",
        "tea-luau",
        "tea-tui",
        "tea-agent",
    }
    transitive_prohibited = {
        "tea-core": {
            "h12tiny-client-sync",
            "http",
            "percent-encoding",
            "mlua",
            "tea-providers",
            "tea-luau",
            "tea-agent",
            "tea-tui",
        },
        "tea-session": {"tea-core"},
        "tea-trace": {"tea-core"},
    }
    direct_prohibited = {
        # Provider diagnostics are intentionally converted into the durable
        # `tea-session::ProviderErrorRecord` at the adapter boundary. The
        # feature-gated `tea-eval` binary also owns a direct `tea-luau` edge.
        # Adapters must not otherwise reach the terminal or trace layers.
        "tea-providers": {"tea-trace", "tea-agent"},
        "tea-luau": {"tea-session", "tea-trace", "tea-providers", "tea-agent"},
    }
    failures: list[str] = []
    workspace_names = set(names_to_ids)
    for package in sorted(required_workspace - workspace_names):
        failures.append(f"required workspace package is missing: {package}")
    for package in sorted(workspace_names - required_workspace):
        failures.append(f"unexpected workspace package remains: {package}")
    for package, forbidden in transitive_prohibited.items():
        package_id = names_to_ids.get(package)
        if package_id is None:
            failures.append(f"required workspace package is missing: {package}")
            continue
        reachable = {package_names[item] for item in descendants(package_id, edges)}
        for dependency in sorted(reachable & forbidden):
            failures.append(f"{package} reaches prohibited dependency {dependency}")

    for package, forbidden in direct_prohibited.items():
        package_id = names_to_ids.get(package)
        if package_id is None:
            failures.append(f"required workspace package is missing: {package}")
            continue
        direct = {package_names[item] for item in edges.get(package_id, ())}
        for dependency in sorted(direct & forbidden):
            failures.append(f"{package} directly depends on prohibited dependency {dependency}")

    tui_id = names_to_ids.get("tea-tui")
    if tui_id is None:
        failures.append("required workspace package is missing: tea-tui")
    elif edges.get(tui_id):
        dependencies = sorted(package_names[item] for item in edges[tui_id])
        failures.append(f"tea-tui must be zero-dependency; found {', '.join(dependencies)}")

    if failures:
        for failure in failures:
            print(f"crate graph check failed: {failure}", file=sys.stderr)
        return 1
    print("crate graph check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
