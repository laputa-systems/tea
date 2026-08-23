import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Reporter } from "../src/reporter.ts";
import { terminalFailure } from "../src/outcome.ts";

function session() {
	return {
		systemPrompt: "cwd /tmp/attempt",
		getActiveToolNames: () => ["read", "bash", "edit", "write"],
		getAllTools: () => [{ name: "read", description: "read", parameters: { type: "object" } }],
		getSessionStats: () => ({ userMessages: 2, toolCalls: 1, tokens: { input: 3, output: 4, cacheRead: 5, cacheWrite: 6 }, cost: 0 }),
		messages: [{ role: "assistant", content: "finished" }],
		model: { id: "poolside/laguna-s-2.1:free", provider: "openrouter" },
	};
}

test("normalizes successful session accounting and surfaces", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-reporter-"));
	const reporter = new Reporter({ attemptId: "a", baselineId: "pi-static", requestedModel: "poolside/laguna-s-2.1:free", thinkingLevel: "high", maxOutputTokens: null, workspace: "/tmp/attempt", evidenceDir: directory, shellEnvironmentSha256: "environment", shellCurlAvailable: true });
	reporter.start();
	await reporter.captureSurface(session());
	reporter.observe({ type: "tool_execution_update", toolName: "bash", result: "x".repeat(1000) });
	const result = reporter.finish(session(), { status: "completed", code: null });
	assert.equal((result.usage as { generation: number }).generation, 7);
	assert.equal((result.model as { max_output_tokens: null }).max_output_tokens, null);
	assert.equal(((result.trace as Array<{ content: { bytes: number } }>)[0].content.bytes), 1000);
	assert.match(await readFile(join(directory, "system-prompt.txt"), "utf8"), /cwd/);
});

test("trace never serializes an environment object", () => {
	const reporter = new Reporter({ attemptId: "a", baselineId: "pi-static", requestedModel: "poolside/laguna-s-2.1:free", thinkingLevel: "high", maxOutputTokens: null, workspace: "/tmp/attempt", evidenceDir: "/tmp/unused", shellEnvironmentSha256: "environment", shellCurlAvailable: true });
	reporter.observe({ type: "message_update", environment: { OPENROUTER_API_KEY: "secret" } });
	assert.equal(JSON.stringify(reporter.trace).includes("secret"), false);
});

test("retains a terminal model failure instead of relabeling it completed", () => {
	assert.equal(terminalFailure({ messages: [{ role: "assistant", stopReason: "error" }] }), "pi_model_error");
	assert.equal(terminalFailure({ messages: [{ role: "assistant", stopReason: "stop" }] }), null);
});
