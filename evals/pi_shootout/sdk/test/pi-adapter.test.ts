import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import test from "node:test";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { canonical } from "../src/canonical.ts";
import { BashExitStatusWitness, captureFirstPiRequest, codingToolEnvironment, createBashExitStatusBridge, createShootoutObserver, outerDeadline, providerTimeoutMilliseconds, settlePiPromptWithinOuterDeadline, type OuterDeadlineTimer } from "../src/pi-adapter.ts";
import type { BashOperations, ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
	directForegroundShellV1,
	POST_EDIT_VALIDATION_BLOCK_REASON,
	PostEditValidationGate,
	PRE_EDIT_TOOL_GATE_BLOCK_REASON,
} from "../src/reporter.ts";
import { WireEvidence } from "../src/wire.ts";

test("accepts the array capability manifest before checking the live credential", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-adapter-"));
	const task = join(directory, "task.json");
	const capabilities = join(directory, "capabilities.json");
	const adapter = fileURLToPath(new URL("../src/pi-adapter.ts", import.meta.url));
	await writeFile(task, JSON.stringify({ prompt: "fix it", capabilities: [{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }] }));
	await writeFile(capabilities, JSON.stringify([{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }]));
	let output = "";
	try {
		execFileSync(process.execPath, [adapter, "--task-json", task, "--workspace", directory, "--capabilities-json", capabilities, "--result-json", join(directory, "result.json"), "--evidence-dir", directory, "--attempt-id", "attempt", "--baseline-id", "pi-static", "--provider", "openrouter", "--model", "deepseek/deepseek-v4-flash-0731", "--thinking-level", "high", "--max-output-tokens", "unlimited", "--outer-timeout-seconds", "900", "--provider-routing-json", "{\"require_parameters\":true}", "--pre-edit-tool-gate", "direct-edit-v1"], { env: { PATH: process.env.PATH ?? "" }, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
		assert.fail("adapter should reject its missing live credential")
	} catch (error) {
		output = String((error as { stderr?: string }).stderr ?? "");
	}
	assert.match(output, /OPENROUTER_API_KEY must be injected by vault/);
	assert.doesNotMatch(output, /invalid JSON object/);
});

test("rejects an unsupported pre-edit tool gate before checking credentials", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-adapter-"));
	const task = join(directory, "task.json");
	const capabilities = join(directory, "capabilities.json");
	const adapter = fileURLToPath(new URL("../src/pi-adapter.ts", import.meta.url));
	await writeFile(task, JSON.stringify({ prompt: "fix it", capabilities: [{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }] }));
	await writeFile(capabilities, JSON.stringify([{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }]));
	let output = "";
	try {
		execFileSync(process.execPath, [adapter, "--task-json", task, "--workspace", directory, "--capabilities-json", capabilities, "--result-json", join(directory, "result.json"), "--evidence-dir", directory, "--attempt-id", "attempt", "--baseline-id", "pi-static", "--provider", "openrouter", "--model", "deepseek/deepseek-v4-flash-0731", "--thinking-level", "high", "--max-output-tokens", "unlimited", "--outer-timeout-seconds", "900", "--provider-routing-json", "{\"require_parameters\":true}", "--pre-edit-tool-gate", "unknown"], { env: { PATH: process.env.PATH ?? "" }, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
		assert.fail("adapter should reject an unknown pre-edit gate")
	} catch (error) {
		output = String((error as { stderr?: string }).stderr ?? "");
	}
	assert.match(output, /--pre-edit-tool-gate must be none, direct-edit-v1, or source-local-v1/);
	assert.doesNotMatch(output, /OPENROUTER_API_KEY must be injected by vault/);
});

test("source-local mode rejects missing versioned target metadata before checking credentials", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-adapter-"));
	const task = join(directory, "task.json");
	const capabilities = join(directory, "capabilities.json");
	const adapter = fileURLToPath(new URL("../src/pi-adapter.ts", import.meta.url));
	await writeFile(task, JSON.stringify({ prompt: "fix lib/response.js", capabilities: [{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }] }));
	await writeFile(capabilities, JSON.stringify([{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "find" }]));
	let output = "";
	try {
		execFileSync(process.execPath, [adapter, "--task-json", task, "--workspace", directory, "--capabilities-json", capabilities, "--result-json", join(directory, "result.json"), "--evidence-dir", directory, "--attempt-id", "attempt", "--baseline-id", "pi-static", "--provider", "openrouter", "--model", "deepseek/deepseek-v4-flash-0731", "--thinking-level", "high", "--max-output-tokens", "unlimited", "--outer-timeout-seconds", "900", "--provider-routing-json", "{\"require_parameters\":true}", "--pre-edit-tool-gate", "source-local-v1"], { env: { PATH: process.env.PATH ?? "" }, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
		assert.fail("adapter should reject source-local mode without task metadata")
	} catch (error) {
		output = String((error as { stderr?: string }).stderr ?? "");
	}
	assert.match(output, /source-local-v1 requires versioned task metadata/);
	assert.doesNotMatch(output, /OPENROUTER_API_KEY must be injected by vault/);
});

test("derives the Pi transport timeout from the shared outer budget", () => {
	assert.equal(providerTimeoutMilliseconds(1800), 1_800_000);
	assert.equal(providerTimeoutMilliseconds(0), 86_400_000);
	assert.throws(() => providerTimeoutMilliseconds(-1), /non-negative safe integer/);
});

test("Pi outer deadline aborts then awaits normal settlement before reporting outer_timeout", async () => {
	let fire: (() => void) | undefined;
	let scheduledMilliseconds: number | undefined;
	let cleared = false;
	const timer: OuterDeadlineTimer = {
		setTimeout: (callback, milliseconds) => {
			fire = callback;
			scheduledMilliseconds = milliseconds;
			return "deadline";
		},
		clearTimeout: (handle) => {
			assert.equal(handle, "deadline");
			cleared = true;
		},
	};
	const events: string[] = [];
	let settlePrompt: (() => void) | undefined;
	const promptSettled = new Promise<void>((resolve) => {
		settlePrompt = resolve;
	});
	const session = {
		abort: async () => {
			events.push("abort");
			settlePrompt?.();
		},
	};
	const outer = settlePiPromptWithinOuterDeadline(
		session,
		async () => {
			await promptSettled;
			events.push("prompt-settled");
		},
		outerDeadline(5, { now: () => 1_000 }),
		timer,
		{ now: () => 1_000 },
	);
	assert.equal(scheduledMilliseconds, 5_000);
	assert.ok(fire);
	fire();
	assert.equal(await outer, true);
	assert.deepEqual(events, ["abort", "prompt-settled"]);
	assert.equal(cleared, true);
});

test("Pi diagnostic zero does not install or invoke an outer deadline", async () => {
	let timerUsed = false;
	const timer: OuterDeadlineTimer = {
		setTimeout: () => {
			timerUsed = true;
			return undefined;
		},
		clearTimeout: () => {
			timerUsed = true;
		},
	};
	let aborted = false;
	const timedOut = await settlePiPromptWithinOuterDeadline(
		{ abort: async () => { aborted = true; } },
		async () => undefined,
		outerDeadline(0, { now: () => 1_000 }),
		timer,
		{ now: () => 1_000 },
	);
	assert.equal(timedOut, false);
	assert.equal(aborted, false);
	assert.equal(timerUsed, false);
});

test("Pi ALS bridge attributes concurrent same-command exits to their own tool IDs", async () => {
	const completions: Array<(result: { exitCode: number | null }) => void> = [];
	const operations: BashOperations = {
		exec: () => new Promise<{ exitCode: number | null }>((resolve) => {
			completions.push(resolve);
		}),
	};
	const witness = new BashExitStatusWitness();
	const bridge = createBashExitStatusBridge(operations, witness);
	const execute = (toolCallId: string) => bridge.run(
		toolCallId,
		() => bridge.operations.exec("same-command", "/workspace", { onData: () => undefined }),
	);

	const first = execute("bash-first");
	const second = execute("bash-second");
	assert.equal(completions.length, 2);
	// Reverse completion order: a command-keyed or FIFO correlation would
	// attribute this zero receipt to the wrong identical invocation.
	completions[1]!({ exitCode: 0 });
	await second;
	assert.equal(witness.take("bash-second"), true);
	assert.equal(witness.take("bash-first"), undefined);
	completions[0]!({ exitCode: 1 });
	await first;
	assert.equal(witness.take("bash-first"), false);
});

test("hidden paired gate blocks only pre-edit bash and find until a successful edit result", async () => {
	const handlers = new Map<string, (event: unknown) => unknown>();
	const observer = createShootoutObserver({ providerRouting: {}, preEditToolGate: "direct-edit-v1" }, new WireEvidence([]));
	observer.factory({
		on: (event: string, handler: unknown) => {
			handlers.set(event, handler as (event: unknown) => unknown);
			},
	} as unknown as ExtensionAPI);
	const toolCall = handlers.get("tool_call");
	const toolResult = handlers.get("tool_result");
	assert.ok(toolCall);
	assert.ok(toolResult);
	const blocked = { block: true, reason: PRE_EDIT_TOOL_GATE_BLOCK_REASON };
	assert.deepEqual(await toolCall({ toolName: "bash" }), blocked);
	assert.deepEqual(await toolCall({ toolName: "find" }), blocked);
	assert.equal(await toolCall({ toolName: "read" }), undefined);
	assert.equal(await toolCall({ toolName: "edit" }), undefined);
	// Pi preflights sibling calls before their tool results settle, so an edit
	// proposed in this assistant batch cannot open its sibling bash call.
	assert.deepEqual(await toolCall({ toolName: "bash" }), blocked);
	await toolResult({ toolName: "edit", isError: true });
	assert.deepEqual(await toolCall({ toolName: "bash" }), blocked);
	await toolResult({ toolName: "edit", isError: false });
	assert.equal(await toolCall({ toolName: "bash" }), undefined);
	assert.equal(await toolCall({ toolName: "find" }), undefined);
});

test("source-local gate preserves inputs, blocks non-target calls, and unlocks only for the admitted edit ID", async () => {
	const handlers = new Map<string, (event: unknown) => unknown>();
	const observer = createShootoutObserver({ providerRouting: {}, preEditToolGate: "source-local-v1", sourceLocalTargets: ["lib/response.js"] }, new WireEvidence([]));
	observer.factory({
		on: (event: string, handler: unknown) => {
			handlers.set(event, handler as (event: unknown) => unknown);
		},
	} as unknown as ExtensionAPI);
	const toolCall = handlers.get("tool_call");
	const toolResult = handlers.get("tool_result");
	assert.ok(toolCall);
	assert.ok(toolResult);
	const blocked = { block: true, reason: "Pre-edit source-local workflow policy: before a successful edit to a declared task target, only read and edit calls whose paths are declared task targets are available. Bash, find, and non-target read/edit calls are unavailable; after a successful target-local edit, use other tools only for focused validation." };
	const targetRead = Object.freeze({ path: "lib/response.js" });
	// Pi's native edit ABI carries one target at the top level. The paired Tea
	// adapter accepts its own transactional `files[]` envelope, so this test
	// keeps the source-local policy honest at the Pi boundary.
	const targetEdit = Object.freeze({ path: "lib/response.js", edits: Object.freeze([]) });
	assert.equal(await toolCall({ toolName: "read", input: targetRead, toolCallId: "read-target" }), undefined);
	assert.deepEqual(await toolCall({ toolName: "read", input: { path: "test/response.js" }, toolCallId: "read-other" }), blocked);
	assert.deepEqual(await toolCall({ toolName: "edit", input: { path: "test/response.js", edits: [] }, toolCallId: "edit-other" }), blocked);
	assert.equal(await toolCall({ toolName: "edit", input: targetEdit, toolCallId: "edit-target" }), undefined);
	// The target edit and this bash call are in one preflight batch. No result
	// has settled yet, so the shell remains blocked.
	assert.deepEqual(await toolCall({ toolName: "bash", input: { command: "node fast-validator.js" }, toolCallId: "same-batch-bash" }), blocked);
	await toolResult({ toolName: "edit", isError: false, toolCallId: "different-edit" });
	assert.deepEqual(await toolCall({ toolName: "find", input: { pattern: "*.js" }, toolCallId: "find-before-target-result" }), blocked);
	await toolResult({ toolName: "edit", isError: false, toolCallId: "edit-target" });
	assert.equal(await toolCall({ toolName: "bash", input: { command: "node fast-validator.js" }, toolCallId: "after-target-result" }), undefined);
	assert.deepEqual(targetRead, { path: "lib/response.js" });
	assert.deepEqual(targetEdit, { path: "lib/response.js", edits: [] });
});

test("constructs the real pinned Pi session and captures its four-tool request without inference", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-session-"));
	const shell = { PATH: process.env.PATH ?? "", HOME: join(directory, "home"), TMPDIR: join(directory, "tmp"), LANG: "C", LC_ALL: "C" };
	await mkdir(shell.TMPDIR, { recursive: true });
	const isolated = await captureFirstPiRequest({ workspace: directory, shell, model: "deepseek/deepseek-v4-flash-0731", thinkingLevel: "high", providerRouting: { require_parameters: true }, outerTimeoutSeconds: 1800, apiKey: "provider-free-test-key" });
	const gated = await captureFirstPiRequest({ workspace: directory, shell, model: "deepseek/deepseek-v4-flash-0731", thinkingLevel: "high", providerRouting: { require_parameters: true }, outerTimeoutSeconds: 1800, apiKey: "provider-free-test-key", preEditToolGate: "direct-edit-v1" });
	try {
		assert.equal(isolated.services.settingsManager.getHttpIdleTimeoutMs(), 1_800_000);
		assert.equal(isolated.services.settingsManager.getProviderRetrySettings().timeoutMs, 1_800_000);
		assert.equal(isolated.services.settingsManager.getRetrySettings().enabled, true);
		assert.equal(isolated.services.settingsManager.getRetrySettings().maxRetries, 0);
		assert.deepEqual(isolated.session.getActiveToolNames(), ["read", "bash", "edit", "find"]);
		assert.match(isolated.session.systemPrompt, /read/i);
		assert.match(isolated.session.systemPrompt, /bash/i);
		assert.match(isolated.session.systemPrompt, /edit/i);
		assert.match(isolated.session.systemPrompt, /find/i);
		assert.doesNotMatch(isolated.session.systemPrompt, /Available tools:\s*\(none\)/i);
		const request = isolated.wire.requests[0] as {
			tool_count: number; tool_names: string[]; provider_routing: unknown;
			canonical_payload: {
				temperature?: unknown;
				seed?: unknown;
				max_tokens?: unknown;
				max_completion_tokens?: unknown;
				tools: Array<{ function: { name: string; description: string; parameters: unknown; strict?: boolean } }>;
			};
		};
		assert.equal(request.tool_count, 4);
		assert.deepEqual(request.tool_names, ["read", "bash", "edit", "find"]);
		assert.equal(new Set(request.tool_names).size, 4);
		assert.deepEqual(request.provider_routing, { require_parameters: true });
		assert.deepEqual(request.canonical_payload.temperature, 0);
		assert.deepEqual(request.canonical_payload.seed, 20260829);
		assert.equal(request.canonical_payload.max_tokens, undefined);
		assert.equal(request.canonical_payload.max_completion_tokens, undefined);
		assert.deepEqual(gated.session.getActiveToolNames(), isolated.session.getActiveToolNames());
		assert.equal(gated.session.systemPrompt, isolated.session.systemPrompt);
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
		assert.equal(canonical(gated.tools.map((tool) => ({ name: tool.name, description: tool.description, parameters: tool.parameters }))), canonical(activeDefinitions));
		assert.equal(canonical((gated.wire.requests[0] as { canonical_payload: { tools: unknown } }).canonical_payload.tools), canonical(request.canonical_payload.tools));
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
		gated.session.dispose();
	}
});

test("post-edit direct-foreground syntax profile is deliberately conservative", () => {
	for (const command of ["npm test", "node scripts/check.js", "cargo test -p crate"]) {
		assert.equal(directForegroundShellV1(command), true, command);
	}
	for (const command of ["", "npm test; echo ok", "npm test | tail", "npm test && echo ok", "npm test > out", "npm test $(pwd)", "npm test `pwd`", "npm test 'x'", "npm test \\", "bash check.sh", "env sh check.sh", "npm test\ntrue"]) {
		assert.equal(directForegroundShellV1(command), false, command);
	}
});

test("post-edit evidence requires an exact successful process receipt and resets after a later edit", () => {
	const gate = new PostEditValidationGate("unmasked-evidence-v1", ["lib/response.js"]);
	const edit = (id: string, path: string) => gate.beforeToolCall("edit", { path, edits: [] }, id);
	const bash = (id: string, command: string) => gate.beforeToolCall("bash", { command }, id);

	assert.equal(edit("target-edit", "lib/response.js"), undefined);
	gate.recordToolResult("edit", false, "target-edit");
	assert.equal(gate.pending(), true);
	assert.deepEqual(bash("masked", "npm test | tail"), { block: true, reason: POST_EDIT_VALIDATION_BLOCK_REASON });

	assert.equal(bash("nonzero", "npm test"), undefined);
	// A Pi tool can be non-error even when a cached or compatibility layer
	// obscures the child status. That result remains non-qualifying.
	gate.recordToolResult("bash", false, "nonzero", false);
	assert.equal(gate.pending(), true);

	assert.equal(edit("same-batch-edit", "lib/response.js"), undefined);
	assert.equal(bash("same-batch-bash", "npm test"), undefined);
	gate.recordToolResult("bash", false, "same-batch-bash", true);
	gate.recordToolResult("edit", false, "same-batch-edit");
	assert.equal(gate.evidence().edit_generation, 2);
	assert.equal(gate.evidence().qualifying_call_id_sha256, null);

	assert.equal(bash("qualifying", "npm test"), undefined);
	gate.recordToolResult("bash", false, "qualifying", true);
	assert.deepEqual(gate.evidence().qualifying_process_exit, "exited-zero");
	assert.equal(gate.pending(), false);

	// Source-local opens ordinary native edits after the first target edit.
	// A later successful edit invalidates the older validation receipt.
	assert.equal(edit("later-edit", "test/response.js"), undefined);
	gate.recordToolResult("edit", false, "later-edit");
	assert.equal(gate.evidence().edit_generation, 3);
	assert.equal(gate.evidence().qualifying_call_id_sha256, null);
	assert.equal(gate.evidence().qualifying_process_exit, null);
	assert.equal(gate.issueReminder(), true);
	assert.equal(gate.issueReminder(), false);
	gate.markEvidenceMissing();
	const evidence = gate.evidence();
	assert.equal(evidence.state, "missing");
	assert.equal(evidence.reminders_issued, 1);
	assert.equal(JSON.stringify({ evidence, trace: gate.trace() }).includes("npm test"), false);
});

test("post-edit evidence rejects a bash that precedes a failed edit in its own batch", () => {
	const gate = new PostEditValidationGate("unmasked-evidence-v1", ["lib/response.js"]);
	const edit = (id: string, path: string) => gate.beforeToolCall("edit", { path, edits: [] }, id);
	const bash = (id: string) => gate.beforeToolCall("bash", { command: "npm test" }, id);

	assert.equal(edit("initial-target-edit", "lib/response.js"), undefined);
	gate.recordToolResult("edit", false, "initial-target-edit");
	assert.equal(gate.pending(), true);

	// Pi preflights an entire assistant batch before tool results settle. The
	// bash arrives first in the observer stream, but a later failed edit still
	// makes it a same-batch call and therefore ineligible evidence.
	assert.equal(bash("same-batch-bash"), undefined);
	assert.equal(edit("same-batch-failed-edit", "lib/response.js"), undefined);
	gate.recordToolResult("edit", true, "same-batch-failed-edit");
	gate.recordToolResult("bash", false, "same-batch-bash", true);

	assert.equal(gate.pending(), true);
	assert.equal(gate.evidence().qualifying_call_id_sha256, null);
});
