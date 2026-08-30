import assert from "node:assert/strict";
import { mkdtemp, readFile } from "node:fs/promises";
import test from "node:test";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { WireEvidence } from "../src/wire.ts";

test("persists a redacted, path-normalized direct provider request witness", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-wire-"));
	const wire = new WireEvidence([
		{ value: "/private/attempt", replacement: "{WORKSPACE}" },
		{ value: "/private/cache", replacement: "{NPM_CACHE}" },
	]);
	wire.capture({
		model: "deepseek/deepseek-v4-flash-0731",
		messages: [{ role: "system", content: "work in /private/attempt" }],
		tools: [{ type: "function", function: { name: "read", description: "read", parameters: { type: "object" } } }],
		stream: true,
		provider: { require_parameters: true },
		authorization: "Bearer should-not-persist",
		nested: { api_key: "also-secret", cache: "/private/cache" },
	});
	await wire.write(directory);
	const persisted = await readFile(join(directory, "wire-requests.json"), "utf8");
	assert.doesNotMatch(persisted, /should-not-persist|also-secret|\/private\/attempt|\/private\/cache/);
	assert.match(persisted, /\[redacted\]/);
	assert.match(persisted, /\{WORKSPACE\}/);
	assert.match(persisted, /\{NPM_CACHE\}/);
	const summary = wire.summary({ require_parameters: true });
	assert.equal(JSON.stringify(summary).includes("canonical_payload"), false);
});
