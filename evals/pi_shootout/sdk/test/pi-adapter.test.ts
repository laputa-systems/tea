import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import test from "node:test";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { canonical } from "../src/canonical.ts";
import { captureFirstPiRequest, codingToolEnvironment } from "../src/pi-adapter.ts";

test("accepts the array capability manifest before checking the live credential", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-adapter-"));
	const task = join(directory, "task.json");
	const capabilities = join(directory, "capabilities.json");
	const adapter = fileURLToPath(new URL("../src/pi-adapter.ts", import.meta.url));
	await writeFile(task, JSON.stringify({ prompt: "fix it", capabilities: [{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }] }));
	await writeFile(capabilities, JSON.stringify([{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }]));
	let output = "";
	try {
		execFileSync(process.execPath, [adapter, "--task-json", task, "--workspace", directory, "--capabilities-json", capabilities, "--result-json", join(directory, "result.json"), "--evidence-dir", directory, "--attempt-id", "attempt", "--baseline-id", "pi-static", "--provider", "openrouter", "--model", "deepseek/deepseek-v4-flash-0731", "--thinking-level", "high", "--max-output-tokens", "unlimited", "--outer-timeout-seconds", "900", "--provider-routing-json", "{\"require_parameters\":true}"], { env: { PATH: process.env.PATH ?? "" }, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
		assert.fail("adapter should reject its missing live credential")
	} catch (error) {
		output = String((error as { stderr?: string }).stderr ?? "");
	}
	assert.match(output, /OPENROUTER_API_KEY must be injected by vault/);
	assert.doesNotMatch(output, /invalid JSON object/);
});

test("constructs the real pinned Pi session and captures its four-tool request without inference", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-session-"));
	const shell = { PATH: process.env.PATH ?? "", HOME: join(directory, "home"), TMPDIR: join(directory, "tmp"), LANG: "C", LC_ALL: "C" };
	const isolated = await captureFirstPiRequest({ workspace: directory, shell, model: "deepseek/deepseek-v4-flash-0731", thinkingLevel: "high", providerRouting: { require_parameters: true }, apiKey: "provider-free-test-key" });
	try {
		assert.deepEqual(isolated.session.getActiveToolNames(), ["read", "bash", "edit", "find"]);
		assert.match(isolated.session.systemPrompt, /read/i);
		assert.match(isolated.session.systemPrompt, /bash/i);
		assert.match(isolated.session.systemPrompt, /edit/i);
		assert.match(isolated.session.systemPrompt, /find/i);
		assert.doesNotMatch(isolated.session.systemPrompt, /Available tools:\s*\(none\)/i);
		const request = isolated.wire.requests[0] as {
			tool_count: number; tool_names: string[]; provider_routing: unknown;
			canonical_payload: { tools: Array<{ function: { name: string; description: string; parameters: unknown; strict?: boolean } }> };
		};
		assert.equal(request.tool_count, 4);
		assert.deepEqual(request.tool_names, ["read", "bash", "edit", "find"]);
		assert.equal(new Set(request.tool_names).size, 4);
		assert.deepEqual(request.provider_routing, { require_parameters: true });
		const activeDefinitions = isolated.tools.map((tool) => ({
			name: tool.name,
			description: tool.description,
			parameters: tool.parameters,
		}));
		const wireDefinitions = request.canonical_payload.tools.map((tool) => tool.function);
		// Pi's OpenRouter converter adds the transport-level `strict: false`
		// marker, but the name, description, and parameter schema must be the
		// exact public ToolDefinition values registered in this session.
		assert.equal(canonical(wireDefinitions.map(({ strict: _strict, ...definition }) => definition)), canonical(activeDefinitions));
		assert.ok(wireDefinitions.every((definition) => definition.strict === false));
		assert.deepEqual(
			codingToolEnvironment({ ...shell, PI_SESSION_ID: "must-not-reach-bash", OPENROUTER_API_KEY: "must-not-reach-bash" }),
			shell,
		);
		assert.equal(isolated.services.resourceLoader.getSkills().skills.length, 0);
		assert.equal(isolated.services.resourceLoader.getPrompts().prompts.length, 0);
		assert.equal(isolated.services.resourceLoader.getAgentsFiles().agentsFiles.length, 0);
	} finally {
		isolated.session.dispose();
	}
});
