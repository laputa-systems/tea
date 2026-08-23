import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import test from "node:test";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("accepts the array capability manifest before checking the live credential", async () => {
	const directory = await mkdtemp(join(tmpdir(), "tea-pi-adapter-"));
	const task = join(directory, "task.json");
	const capabilities = join(directory, "capabilities.json");
	const adapter = fileURLToPath(new URL("../src/pi-adapter.ts", import.meta.url));
	await writeFile(task, JSON.stringify({ prompt: "fix it", capabilities: [{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "write" }] }));
	await writeFile(capabilities, JSON.stringify([{ name: "read" }, { name: "bash" }, { name: "edit" }, { name: "write" }]));
	let output = "";
	try {
		execFileSync(process.execPath, [adapter, "--task-json", task, "--workspace", directory, "--capabilities-json", capabilities, "--result-json", join(directory, "result.json"), "--evidence-dir", directory, "--attempt-id", "attempt", "--baseline-id", "pi-static", "--provider", "openrouter", "--model", "poolside/laguna-s-2.1:free", "--thinking-level", "high", "--max-output-tokens", "unlimited"], { env: { PATH: process.env.PATH ?? "" }, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
		assert.fail("adapter should reject its missing live credential")
	} catch (error) {
		output = String((error as { stderr?: string }).stderr ?? "");
	}
	assert.match(output, /OPENROUTER_API_KEY must be injected by vault/);
	assert.doesNotMatch(output, /invalid JSON object/);
});
