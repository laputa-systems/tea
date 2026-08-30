import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Reporter } from "../src/reporter.ts";
import { terminalFailure } from "../src/outcome.ts";
import { WireEvidence } from "../src/wire.ts";

function session() {
	return {
		systemPrompt: "cwd /tmp/attempt",
		getActiveToolNames: () => ["read", "bash", "edit", "find"],
		getAllTools: () => ["read", "bash", "edit", "find"].map((name) => ({ name, description: name, parameters: { type: "object" } })),
		getSessionStats: () => ({ userMessages: 2, toolCalls: 1, tokens: { input: 3, output: 4, cacheRead: 5, cacheWrite: 6 }, cost: 0 }),
		messages: [{ role: "assistant", content: [{ type: "thinking", thinking: "hidden" }, { type: "text", text: "finished" }, { type: "toolCall", id: "call-1", name: "bash", arguments: { command: "secret command" } }] }],
		model: { id: "deepseek/deepseek-v4-flash-0731", provider: "openrouter" },
	};
}

test("normalizes successful session accounting and surfaces", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-reporter-"));
	const wire = new WireEvidence([{ value: "/tmp/attempt", replacement: "{WORKSPACE}" }]);
	wire.capture({ model: "deepseek/deepseek-v4-flash-0731", messages: [{ role: "system", content: "system" }], tools: [{ type: "function", function: { name: "read", description: "read", parameters: {} } }], reasoning: { effort: "high" }, stream: true, stream_options: { include_usage: true }, provider: { require_parameters: true } });
	const reporter = new Reporter({ attemptId: "a", baselineId: "pi-static", requestedModel: "deepseek/deepseek-v4-flash-0731", thinkingLevel: "high", maxOutputTokens: null, outerTimeoutSeconds: 900, providerRouting: { require_parameters: true }, workspace: "/tmp/attempt", evidenceDir: directory, shellEnvironmentSha256: "environment", shellCurlAvailable: true, wire });
	reporter.start();
	await reporter.captureSurface(session());
	reporter.observe({ type: "tool_execution_update", toolName: "bash", result: "x".repeat(1000) });
	const result = reporter.finish(session(), { status: "completed", code: null });
	assert.equal((result.usage as { generation: number }).generation, 7);
	assert.equal((result.usage as { prompt_total: number }).prompt_total, 14);
	assert.equal((result.usage as { all_tokens: number }).all_tokens, 18);
	assert.equal((result.counts as { turns: number }).turns, 2);
	assert.equal((result.model as { max_output_tokens: null }).max_output_tokens, null);
	assert.deepEqual((result.model as { sampling: unknown }).sampling, { temperature: null, seed: null, source: "provider-default" });
	assert.equal((result.runtime as { version: string }).version, "0.84.4");
	assert.equal((result.runtime as { revision: string }).revision, "npm:@earendil-works/pi-coding-agent@0.84.4");
	assert.equal(((result.trace as Array<{ content: { bytes: number } }>)[0].content.bytes), 1000);
	assert.equal(result.final_text, "finished");
	assert.equal((result.model as { returned_model: unknown }).returned_model, null);
	assert.equal(((result.wire as { requests: unknown[] }).requests).length, 1);
	assert.match(await readFile(join(directory, "system-prompt.txt"), "utf8"), /cwd/);
});

test("trace never serializes an environment object", () => {
	const reporter = new Reporter({ attemptId: "a", baselineId: "pi-static", requestedModel: "deepseek/deepseek-v4-flash-0731", thinkingLevel: "high", maxOutputTokens: null, outerTimeoutSeconds: 900, providerRouting: { require_parameters: true }, workspace: "/tmp/attempt", evidenceDir: "/tmp/unused", shellEnvironmentSha256: "environment", shellCurlAvailable: true, wire: new WireEvidence([]) });
	reporter.observe({ type: "tool_execution_start", toolName: "bash", args: { command: "npm install secret", environment: { OPENROUTER_API_KEY: "secret" } } });
	assert.equal(JSON.stringify(reporter.trace).includes("secret"), false);
	assert.match(JSON.stringify(reporter.trace), /arguments_sha256/);
	assert.equal(reporter.trace[0]?.category, "upstream_or_dependency");
});

test("retains a terminal model failure instead of relabeling it completed", () => {
	assert.equal(terminalFailure({ messages: [{ role: "assistant", stopReason: "error" }] }), "pi_model_error");
	assert.equal(terminalFailure({ messages: [{ role: "assistant", stopReason: "error", errorMessage: "HTTP 429: too many requests" }] }), "openrouter_response_429");
	assert.equal(terminalFailure({ messages: [{ role: "assistant", stopReason: "stop" }] }), null);
});
