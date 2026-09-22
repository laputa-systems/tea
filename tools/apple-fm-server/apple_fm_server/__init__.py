"""An OpenAI-compatible server for Apple's on-device Foundation Models.

The package exists so tea's `local` provider can talk to the model that ships
with macOS 26: it translates a Chat Completions request into one Apple session,
streams the response back as SSE, and bridges tool calls in both directions.

Modules, in dependency order:

* ``protocol`` — OpenAI wire shapes and their rendering into one Apple prompt.
* ``tools`` — OpenAI function tools as Apple tools, plus the call recorder.
* ``backend`` — the only module that imports ``apple_fm_sdk``.
* ``server`` — the stdlib HTTP transport that drives the backend's event loop.
"""

from __future__ import annotations

__all__ = ["backend", "protocol", "server", "tools"]
