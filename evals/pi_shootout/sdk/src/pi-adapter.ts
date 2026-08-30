import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { createAgentSessionFromServices, createAgentSessionServices, createBashToolDefinition, createEditToolDefinition, createFindToolDefinition, createReadToolDefinition, ModelRuntime, SessionManager, SettingsManager, type ExtensionAPI, type ToolDefinition } from "@earendil-works/pi-coding-agent";
import { getModel } from "@earendil-works/pi-ai/compat";

import { canonical, normalizeWorkspace, sha256 } from "./canonical.ts";
import { terminalFailure } from "./outcome.ts";
import { Reporter } from "./reporter.ts";
import { WireEvidence, type AttemptPath } from "./wire.ts";

const SHOOTOUT_TEMPERATURE = 0;
const SHOOTOUT_SEED = 20260829;

type Arguments = {
	taskJson: string; workspace: string; capabilitiesJson: string; resultJson: string; evidenceDir: string;
	attemptId: string; baselineId: "pi-static"; provider: "openrouter"; model: string; thinkingLevel: "high"; maxOutputTokens: number | null; outerTimeoutSeconds: number; providerRouting: Record<string, unknown>; shell: Record<string, string>;
};

function parse(): Arguments {
	const permitted = new Set(["--task-json", "--workspace", "--capabilities-json", "--result-json", "--evidence-dir", "--attempt-id", "--baseline-id", "--provider", "--model", "--thinking-level", "--max-output-tokens", "--outer-timeout-seconds", "--provider-routing-json", "--shell-env"]);
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
	const outerTimeoutSeconds = Number.parseInt(required("--outer-timeout-seconds"), 10);
	if (!Number.isSafeInteger(outerTimeoutSeconds) || outerTimeoutSeconds < 1) throw new Error("--outer-timeout-seconds must be positive");
	let providerRouting: unknown;
	try {
		providerRouting = JSON.parse(required("--provider-routing-json")) as unknown;
	} catch {
		throw new Error("--provider-routing-json must be JSON");
	}
	if (!providerRouting || typeof providerRouting !== "object" || Array.isArray(providerRouting)) throw new Error("--provider-routing-json must be an object");
	return {
		taskJson: required("--task-json"), workspace: required("--workspace"), capabilitiesJson: required("--capabilities-json"), resultJson: required("--result-json"), evidenceDir: required("--evidence-dir"),
		attemptId: required("--attempt-id"), baselineId: required("--baseline-id") as "pi-static", provider: required("--provider") as "openrouter", model: required("--model"), thinkingLevel: required("--thinking-level") as "high",
		maxOutputTokens: null, outerTimeoutSeconds, providerRouting: providerRouting as Record<string, unknown>, shell,
	};
}

async function readJson(path: string): Promise<unknown> {
	const source = await (await import("node:fs/promises")).readFile(path, "utf8");
	return JSON.parse(source) as unknown;
}

function assertInputs(args: Arguments, task: Record<string, unknown>, capabilities: unknown): void {
	if (args.baselineId !== "pi-static" || args.provider !== "openrouter" || args.thinkingLevel !== "high") throw new Error("unsupported Pi shootout condition")
	if (args.model !== "deepseek/deepseek-v4-flash-0731") throw new Error("Pi shootout requires deepseek/deepseek-v4-flash-0731")
	if (task.capabilities === undefined || canonical(task.capabilities) !== canonical(capabilities)) throw new Error("task and capability manifest disagree")
	if (!Array.isArray(capabilities) || capabilities.map((item) => (item as { name?: unknown }).name).join(",") !== "read,bash,edit,find") throw new Error("Pi shootout requires read/bash/edit/find")
	if (args.maxOutputTokens !== null) throw new Error("Pi shootout requires unlimited max output tokens")
}

/** Return the complete environment for a coding-tool child. Pi session state
 * and the OpenRouter key are never authority for this boundary. */
export function codingToolEnvironment(shell: Record<string, string>): Record<string, string> {
	return Object.fromEntries(Object.entries(shell).filter(([name]) => name !== "OPENROUTER_API_KEY" && !name.startsWith("PI_")));
}

function sanitizeParentEnvironment(shell: Record<string, string>): void {
	for (const name of Object.keys(process.env)) delete process.env[name];
	Object.assign(process.env, codingToolEnvironment(shell));
}

export type PiSessionInput = {
	workspace: string;
	shell: Record<string, string>;
	model: string;
	thinkingLevel: "high";
	providerRouting: Record<string, unknown>;
	apiKey: string;
	abortAfterFirstRequest?: boolean;
};

export type IsolatedPiSession = {
	session: Awaited<ReturnType<typeof createAgentSessionFromServices>>["session"];
	services: Awaited<ReturnType<typeof createAgentSessionServices>>;
	wire: WireEvidence;
	tools: ToolDefinition[];
};

function attemptPaths(workspace: string, shell: Record<string, string>): AttemptPath[] {
	const paths: AttemptPath[] = [{ value: workspace, replacement: "{WORKSPACE}" }];
	for (const [name, replacement] of [["HOME", "{HOME}"], ["TMPDIR", "{TMPDIR}"], ["npm_config_cache", "{NPM_CACHE}"]] as const) {
		const value = shell[name];
		if (value) paths.push({ value, replacement });
	}
	return paths.sort((left, right) => right.value.length - left.value.length);
}

function routedPayload(payload: unknown, routing: Record<string, unknown>): Record<string, unknown> {
	if (!payload || typeof payload !== "object" || Array.isArray(payload)) throw new Error("Pi provider payload is not an object");
	// The shootout contract uses an unlimited output ceiling. Pi's pinned
	// OpenRouter converter otherwise supplies its own large
	// `max_completion_tokens` default, which is still a real provider limit and
	// can cause a request to be rejected before inference starts.
	const routed: Record<string, unknown> = { ...(payload as Record<string, unknown>), provider: routing };
	routed.temperature = SHOOTOUT_TEMPERATURE;
	routed.seed = SHOOTOUT_SEED;
	delete routed.max_tokens;
	delete routed.max_completion_tokens;
	return routed;
}

/** Construct the real pinned Pi SDK session, including the public request
 * interception path, without requiring an inference response. Tests can ask
 * the observer to abort after the first constructed payload. */
export async function createIsolatedPiSession(input: PiSessionInput): Promise<IsolatedPiSession> {
	const agentDir = await mkdtemp(join(tmpdir(), "tea-pi-sdk-"));
	const settings = SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: true, maxRetries: 0 } });
	const modelRuntime = await ModelRuntime.create({ authPath: join(agentDir, "auth.json"), modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
	await modelRuntime.setRuntimeApiKey("openrouter", input.apiKey);
	const wire = new WireEvidence(attemptPaths(input.workspace, input.shell));
	// The observer is a hidden, inline SDK extension. It is instrumentation, not
	// a discovered project extension, and it has no prompt/tool contribution.
	const observer = {
		name: "tea-shootout-wire-observer",
		hidden: true,
		factory: (pi: ExtensionAPI) => {
			pi.on("before_provider_request", (event, context) => {
				const payload = routedPayload(event.payload, input.providerRouting);
				wire.capture(payload);
				if (input.abortAfterFirstRequest) context.abort();
				return payload;
			});
			pi.on("after_provider_response", (event) => {
				if (event.headers) wire.captureResponse(event.headers);
			});
		},
	};
	sanitizeParentEnvironment(input.shell);
	const services = await createAgentSessionServices({
		cwd: input.workspace,
		agentDir,
		settingsManager: settings,
		modelRuntime,
		resourceLoaderOptions: {
			noExtensions: true, noSkills: true, noPromptTemplates: true, noContextFiles: true, noThemes: true,
			extensionFactories: [observer],
		},
	});
	const extensions = services.resourceLoader.getExtensions().extensions;
	if (services.diagnostics.some((diagnostic) => diagnostic.type === "error") || extensions.length !== 1 || !extensions[0]?.path.startsWith("<inline:tea-shootout-wire-observer>") || services.resourceLoader.getSkills().skills.length || services.resourceLoader.getPrompts().prompts.length || services.resourceLoader.getAgentsFiles().agentsFiles.length) throw new Error("isolated Pi runtime discovered a repository resource");
	const model = modelRuntime.getModel("openrouter", input.model as never) ?? getModel("openrouter", input.model as never);
	if (!model) throw new Error(`pinned Pi SDK lacks model ${input.model}`);
	const manager = SessionManager.inMemory(input.workspace);
	const tools = [
		createReadToolDefinition(input.workspace),
	createBashToolDefinition(input.workspace, {
		exposeSessionEnvironment: false,
		spawnHook: ({ command, cwd }) => ({ command, cwd, env: codingToolEnvironment(input.shell) }),
	}),
		createEditToolDefinition(input.workspace),
		createFindToolDefinition(input.workspace),
	] as unknown as ToolDefinition[];
	const { session } = await createAgentSessionFromServices({ services, sessionManager: manager, model, thinkingLevel: input.thinkingLevel, tools: ["read", "bash", "edit", "find"], customTools: tools });
	return { session, services, wire, tools };
}

export async function captureFirstPiRequest(input: Omit<PiSessionInput, "abortAfterFirstRequest">, prompt = "provider-free request construction"): Promise<IsolatedPiSession> {
	const isolated = await createIsolatedPiSession({ ...input, abortAfterFirstRequest: true });
	try {
		await isolated.session.prompt(prompt, { expandPromptTemplates: false });
	} catch {
		// The observer aborts the run after the payload reaches the public
		// interception hook. A provider-free construction is expected to settle
		// as cancelled or errored depending on this pinned SDK version.
	} finally {
		if (isolated.wire.requests.length === 0) {
			isolated.session.dispose();
			throw new Error("Pi did not construct a provider request before the provider-free abort");
		}
	}
	return isolated;
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
	const isolated = await createIsolatedPiSession({ workspace: args.workspace, shell: args.shell, model: args.model, thinkingLevel: args.thinkingLevel, providerRouting: args.providerRouting, apiKey: key });
	const { session, wire } = isolated;
	const shellHash = sha256(canonical(Object.fromEntries(Object.entries(args.shell).map(([name, value]) => [
		name,
		name === "HOME" ? "{HOME}"
			: name === "TMPDIR" ? "{TMPDIR}"
				: name === "npm_config_cache" ? "{NPM_CACHE}"
					: name === "NODE_PATH" ? "{NODE_PATH}"
						: normalizeWorkspace(value, args.workspace),
	]))));
	const reporter = new Reporter({ attemptId: args.attemptId, baselineId: args.baselineId, requestedModel: args.model, thinkingLevel: args.thinkingLevel, maxOutputTokens: args.maxOutputTokens, outerTimeoutSeconds: args.outerTimeoutSeconds, providerRouting: args.providerRouting, samplingTemperature: SHOOTOUT_TEMPERATURE, samplingSeed: SHOOTOUT_SEED, samplingSource: "adapter-set", workspace: args.workspace, evidenceDir: args.evidenceDir, shellEnvironmentSha256: shellHash, shellCurlAvailable: true, wire });
	reporter.start();
	const unsubscribe = session.subscribe((event) => reporter.observe(event));
	let terminal: { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null } = { status: "completed", code: null };
	try {
		await reporter.captureSurface(session);
		if (session.getActiveToolNames().join(",") !== "read,bash,edit,find") throw new Error("Pi active tool surface drifted")
		await session.prompt(prompt, { expandPromptTemplates: false });
		const failure = terminalFailure(session);
		if (failure) terminal = { status: "failed", code: failure };
	} catch (error) {
		terminal = { status: "failed", code: error instanceof Error ? "pi_sdk_error" : "pi_sdk_failure" };
	} finally {
		unsubscribe();
		await wire.write(args.evidenceDir);
		const result = reporter.finish(session, terminal);
		await reporter.write(args.resultJson, result);
		session.dispose();
	}
	if (terminal.status !== "completed") process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
	await main();
}
