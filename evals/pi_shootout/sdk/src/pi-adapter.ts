import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AgentSession, createAgentSessionFromServices, createAgentSessionServices, createCodingTools, ModelRuntime, SessionManager, SettingsManager } from "@earendil-works/pi-coding-agent";
import { getModel } from "@earendil-works/pi-ai/compat";

import { canonical, normalizeWorkspace, sha256 } from "./canonical.ts";
import { terminalFailure } from "./outcome.ts";
import { Reporter } from "./reporter.ts";

type Arguments = {
	taskJson: string; workspace: string; capabilitiesJson: string; resultJson: string; evidenceDir: string;
	attemptId: string; baselineId: "pi-static"; provider: "openrouter"; model: string; thinkingLevel: "high"; maxOutputTokens: number | null; shell: Record<string, string>;
};

function parse(): Arguments {
	const permitted = new Set(["--task-json", "--workspace", "--capabilities-json", "--result-json", "--evidence-dir", "--attempt-id", "--baseline-id", "--provider", "--model", "--thinking-level", "--max-output-tokens", "--shell-env"]);
	const values = new Map<string, string>();
	const shell: Record<string, string> = {};
	for (let index = 2; index < process.argv.length; index += 2) {
		const flag = process.argv[index];
		const value = process.argv[index + 1];
		if (!permitted.has(flag) || value === undefined) throw new Error(`invalid adapter argument ${flag ?? "<missing>"}`);
		if (flag === "--shell-env") {
			const equals = value.indexOf("=");
			if (equals < 1) throw new Error("--shell-env requires NAME=VALUE");
			const name = value.slice(0, equals);
			if (Object.hasOwn(shell, name)) throw new Error(`duplicate shell environment variable ${name}`);
			shell[name] = value.slice(equals + 1);
			continue;
		}
		if (values.has(flag)) throw new Error(`duplicate adapter argument ${flag}`);
		values.set(flag, value);
	}
	const required = (flag: string) => {
		const value = values.get(flag);
		if (!value) throw new Error(`missing required adapter argument ${flag}`);
		return value;
	};
	const maximum = required("--max-output-tokens");
	if (maximum !== "unlimited") throw new Error("Pi shootout requires unlimited max output tokens");
	return {
		taskJson: required("--task-json"), workspace: required("--workspace"), capabilitiesJson: required("--capabilities-json"), resultJson: required("--result-json"), evidenceDir: required("--evidence-dir"),
		attemptId: required("--attempt-id"), baselineId: required("--baseline-id") as "pi-static", provider: required("--provider") as "openrouter", model: required("--model"), thinkingLevel: required("--thinking-level") as "high",
		maxOutputTokens: null, shell,
	};
}

async function readJson(path: string): Promise<unknown> {
	const source = await (await import("node:fs/promises")).readFile(path, "utf8");
	return JSON.parse(source) as unknown;
}

function assertInputs(args: Arguments, task: Record<string, unknown>, capabilities: unknown): void {
	if (args.baselineId !== "pi-static" || args.provider !== "openrouter" || args.thinkingLevel !== "high") throw new Error("unsupported Pi shootout condition")
	if (args.model !== "poolside/laguna-s-2.1:free") throw new Error("Pi shootout requires poolside/laguna-s-2.1:free")
	if (task.capabilities === undefined || canonical(task.capabilities) !== canonical(capabilities)) throw new Error("task and capability manifest disagree")
	if (!Array.isArray(capabilities) || capabilities.map((item) => (item as { name?: unknown }).name).join(",") !== "read,bash,edit,find") throw new Error("Pi shootout requires read/bash/edit/find")
	if (args.maxOutputTokens !== null) throw new Error("Pi shootout requires unlimited max output tokens")
}

function sanitizeParentEnvironment(shell: Record<string, string>): void {
	for (const name of Object.keys(process.env)) delete process.env[name];
	Object.assign(process.env, shell);
}

async function main(): Promise<void> {
	const args = parse();
	const [taskValue, capabilities] = await Promise.all([readJson(args.taskJson), readJson(args.capabilitiesJson)]);
	if (!taskValue || typeof taskValue !== "object" || Array.isArray(taskValue)) throw new Error(`invalid task JSON object ${args.taskJson}`);
	const task = taskValue as Record<string, unknown>;
	assertInputs(args, task, capabilities);
	const prompt = task.prompt;
	if (typeof prompt !== "string" || !prompt) throw new Error("task prompt is missing")
	const key = process.env.OPENROUTER_API_KEY;
	if (!key) throw new Error("OPENROUTER_API_KEY must be injected by vault")
	const agentDir = await mkdtemp(join(tmpdir(), "tea-pi-sdk-"));
	const settings = SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: true, maxRetries: 0 } });
	const modelRuntime = await ModelRuntime.create({ authPath: join(agentDir, "auth.json"), modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
	await modelRuntime.setRuntimeApiKey("openrouter", key);
	sanitizeParentEnvironment(args.shell);
	const services = await createAgentSessionServices({ cwd: args.workspace, agentDir, settingsManager: settings, modelRuntime, resourceLoaderOptions: { noExtensions: true, noSkills: true, noPromptTemplates: true, noContextFiles: true, noThemes: true } });
	if (services.diagnostics.some((diagnostic) => diagnostic.type === "error") || services.resourceLoader.getExtensions().extensions.length || services.resourceLoader.getSkills().skills.length || services.resourceLoader.getPrompts().prompts.length || services.resourceLoader.getAgentsFiles().agentsFiles.length) throw new Error("isolated Pi runtime discovered a repository resource")
	const model = modelRuntime.getModel("openrouter", args.model as never) ?? getModel("openrouter", args.model as never);
	if (!model) throw new Error(`pinned Pi SDK lacks model ${args.model}`)
	const manager = SessionManager.inMemory(args.workspace);
	const first = await createAgentSessionFromServices({ services, sessionManager: manager, model, thinkingLevel: args.thinkingLevel, tools: [] });
	first.session.dispose();
	const tools = createCodingTools(args.workspace, { bash: { exposeSessionEnvironment: false, spawnHook: ({ command, cwd }) => ({ command, cwd, env: { ...args.shell } }) } });
	const second = new AgentSession({ agent: first.session.agent, sessionManager: manager, settingsManager: settings, cwd: args.workspace, resourceLoader: services.resourceLoader, modelRuntime, initialActiveToolNames: ["read", "bash", "edit", "find"], allowedToolNames: ["read", "bash", "edit", "find"], baseToolsOverride: Object.fromEntries(tools.map((tool) => [tool.name, tool])) });
	const shellHash = sha256(canonical(Object.fromEntries(Object.entries(args.shell).map(([name, value]) => [name, name === "HOME" ? "{HOME}" : name === "TMPDIR" ? "{TMPDIR}" : name === "npm_config_cache" ? "{NPM_CACHE}" : normalizeWorkspace(value, args.workspace)]))));
	const reporter = new Reporter({ attemptId: args.attemptId, baselineId: args.baselineId, requestedModel: args.model, thinkingLevel: args.thinkingLevel, maxOutputTokens: args.maxOutputTokens, workspace: args.workspace, evidenceDir: args.evidenceDir, shellEnvironmentSha256: shellHash, shellCurlAvailable: true });
	reporter.start();
	const unsubscribe = second.subscribe((event) => reporter.observe(event));
	let terminal: { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null } = { status: "completed", code: null };
	try {
		await reporter.captureSurface(second);
		if (second.getActiveToolNames().join(",") !== "read,bash,edit,find") throw new Error("Pi active tool surface drifted")
		await second.prompt(prompt, { expandPromptTemplates: false });
		const failure = terminalFailure(second);
		if (failure) terminal = { status: "failed", code: failure };
	} catch (error) {
		terminal = { status: "failed", code: error instanceof Error ? "pi_sdk_error" : "pi_sdk_failure" };
	} finally {
		unsubscribe();
		const result = reporter.finish(second, terminal);
		await reporter.write(args.resultJson, result);
		second.dispose();
	}
	if (terminal.status !== "completed") process.exitCode = 1;
}

await main();
