import { lstat, mkdtemp } from "node:fs/promises";
import { AsyncLocalStorage } from "node:async_hooks";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { createAgentSessionFromServices, createAgentSessionServices, createBashToolDefinition, createEditToolDefinition, createFindToolDefinition, createLocalBashOperations, createReadToolDefinition, ModelRuntime, SessionManager, SettingsManager, type BashOperations, type ExtensionAPI, type ToolDefinition } from "@earendil-works/pi-coding-agent";
import { getModel } from "@earendil-works/pi-ai/compat";

import { canonical, normalizeWorkspace, sha256 } from "./canonical.ts";
import { terminalFailure } from "./outcome.ts";
import {
	POST_EDIT_VALIDATION_REMINDER,
	PostEditValidationGate,
	PRE_EDIT_TOOL_GATE_BLOCK_REASON,
	Reporter,
	SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON,
	type PostEditValidationGateMode,
	type PreEditToolGateMode,
} from "./reporter.ts";
import { WireEvidence, type AttemptPath } from "./wire.ts";

const SHOOTOUT_TEMPERATURE = 0;
const SHOOTOUT_SEED = 20260829;
// Paired static runs must not replay a pre-output provider failure. The result
// reporter reads the effective setting below rather than repeating this value.
const PAIRED_PROVIDER_RETRY = Object.freeze({ enabled: true, maxRetries: 0 });
// The uncapped diagnostic still needs a finite transport guard. Match Tea's
// adapter-level policy rather than leaving Pi's SDK default idle timeout as an
// unobserved, potentially shorter control.
const DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS = 86_400;

type Arguments = {
	taskJson: string; workspace: string; capabilitiesJson: string; resultJson: string; evidenceDir: string;
	attemptId: string; baselineId: "pi-static"; provider: "openrouter"; model: string; thinkingLevel: "high"; maxOutputTokens: number | null; outerTimeoutSeconds: number; providerRouting: Record<string, unknown>; shell: Record<string, string>; preEditToolGate: PreEditToolGateMode; postEditValidationGate: PostEditValidationGateMode;
};

function parse(): Arguments {
	const permitted = new Set(["--task-json", "--workspace", "--capabilities-json", "--result-json", "--evidence-dir", "--attempt-id", "--baseline-id", "--provider", "--model", "--thinking-level", "--max-output-tokens", "--outer-timeout-seconds", "--provider-routing-json", "--pre-edit-tool-gate", "--post-edit-validation-gate", "--shell-env"]);
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
	if (!Number.isSafeInteger(outerTimeoutSeconds) || outerTimeoutSeconds < 0) throw new Error("--outer-timeout-seconds must be non-negative");
	let providerRouting: unknown;
	try {
		providerRouting = JSON.parse(required("--provider-routing-json")) as unknown;
	} catch {
		throw new Error("--provider-routing-json must be JSON");
	}
	if (!providerRouting || typeof providerRouting !== "object" || Array.isArray(providerRouting)) throw new Error("--provider-routing-json must be an object");
	const preEditToolGate = values.get("--pre-edit-tool-gate") ?? "none";
	if (preEditToolGate !== "none" && preEditToolGate !== "direct-edit-v1" && preEditToolGate !== "source-local-v1") throw new Error("--pre-edit-tool-gate must be none, direct-edit-v1, or source-local-v1");
	const postEditValidationGate = values.get("--post-edit-validation-gate") ?? "none";
	if (postEditValidationGate !== "none" && postEditValidationGate !== "unmasked-evidence-v1") throw new Error("--post-edit-validation-gate must be none or unmasked-evidence-v1");
	if (postEditValidationGate === "unmasked-evidence-v1" && preEditToolGate !== "source-local-v1") throw new Error("unmasked-evidence-v1 requires --pre-edit-tool-gate source-local-v1");
	return {
		taskJson: required("--task-json"), workspace: required("--workspace"), capabilitiesJson: required("--capabilities-json"), resultJson: required("--result-json"), evidenceDir: required("--evidence-dir"),
		attemptId: required("--attempt-id"), baselineId: required("--baseline-id") as "pi-static", provider: required("--provider") as "openrouter", model: required("--model"), thinkingLevel: required("--thinking-level") as "high",
		maxOutputTokens: null, outerTimeoutSeconds, providerRouting: providerRouting as Record<string, unknown>, shell, preEditToolGate, postEditValidationGate,
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

function safeSourceLocalTarget(value: unknown): value is string {
	return typeof value === "string"
		&& value.length > 0
		&& !value.includes("\\0")
		&& !value.startsWith("/")
		&& !value.includes("\\")
		&& value.split("/").every((part) => part.length > 0 && part !== "." && part !== "..");
}

/** Read, validate, and copy the versioned task declaration without modifying
 * the parsed task object or its target array. The runner has already witnessed
 * a clean checkout; both adapters independently confirm these targets still
 * name regular files before the model receives the task. */
async function sourceLocalTaskTargets(
	task: Readonly<Record<string, unknown>>,
	prompt: string,
	workspace: string,
	mode: PreEditToolGateMode,
): Promise<string[]> {
	if (mode !== "source-local-v1") return [];
	const metadata = task.source_local_v1;
	if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) throw new Error("source-local-v1 requires versioned task metadata");
	const record = metadata as Readonly<Record<string, unknown>>;
	if (record.schema_version !== "tea-coding-eval-source-local/v1") throw new Error("source-local-v1 task metadata schema is unsupported");
	if (!Array.isArray(record.targets) || !record.targets.length || !record.targets.every(safeSourceLocalTarget)) throw new Error("source-local-v1 task targets are invalid");
	const targets = [...record.targets];
	if (new Set(targets).size !== targets.length) throw new Error("source-local-v1 task targets must be unique");
	for (const target of targets) {
		if (!prompt.includes(target)) throw new Error("source-local-v1 task target must occur literally in the prompt");
		const entry = await lstat(join(workspace, target)).catch(() => undefined);
		if (!entry || entry.isSymbolicLink() || !entry.isFile()) throw new Error(`source-local-v1 target is not a regular workspace file: ${target}`);
	}
	return targets;
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
	outerTimeoutSeconds: number;
	providerRouting: Record<string, unknown>;
	apiKey: string;
	abortAfterFirstRequest?: boolean;
	preEditToolGate?: PreEditToolGateMode;
	postEditValidationGate?: PostEditValidationGateMode;
	sourceLocalTargets?: readonly string[];
};

export type IsolatedPiSession = {
	session: Awaited<ReturnType<typeof createAgentSessionFromServices>>["session"];
	services: Awaited<ReturnType<typeof createAgentSessionServices>>;
	wire: WireEvidence;
	tools: ToolDefinition[];
	validationGate: PostEditValidationGate;
};

/** Pi's public tool-result event exposes no process exit status. The existing
 * native bash execution seam is therefore wrapped only to correlate the
 * process outcome with the already-public call ID. This transient witness is
 * never serialized; the result exports only hashes and the explicit
 * `exited-zero` receipt claim. A missing witness is deliberately
 * non-qualifying. */
export class BashExitStatusWitness {
	private readonly outcomes = new Map<string, boolean>();

	record(toolCallId: string | undefined, exitCode: number | null): void {
		if (toolCallId) this.outcomes.set(toolCallId, exitCode === 0);
	}

	take(toolCallId: string | undefined): boolean | undefined {
		if (!toolCallId) return undefined;
		const outcome = this.outcomes.get(toolCallId);
		this.outcomes.delete(toolCallId);
		return outcome;
	}
}

/** The adapter-local bridge binds Pi's asynchronous native bash execution to
 * the core-owned tool-call ID. Its only durable-adjacent output is the
 * content-free exit-zero witness; commands and process output stay inside
 * Pi's native tool implementation. */
export function createBashExitStatusBridge(operations: BashOperations, witness: BashExitStatusWitness): {
	operations: BashOperations;
	run<T>(toolCallId: string, operation: () => Promise<T>): Promise<T>;
} {
	const executionContext = new AsyncLocalStorage<string>();
	return {
		operations: {
			exec: async (command, cwd, options) => {
				try {
					const result = await operations.exec(command, cwd, options);
					witness.record(executionContext.getStore(), result.exitCode);
					return result;
				} catch (error) {
					// A thrown process operation has no explicit zero receipt.
					witness.record(executionContext.getStore(), null);
					throw error;
				}
			},
		},
		run: (toolCallId, operation) => executionContext.run(toolCallId, operation),
	};
}

/** The gate advances only from a completed, successful edit result. In
 * particular, sibling calls in the same assistant batch remain gated. */
export class PreEditToolGate {
	private successfulEditResult = false;
	private readonly declaredTargets: ReadonlySet<string>;
	private readonly targetEditCallIds = new Set<string>();
	readonly mode: PreEditToolGateMode;

	constructor(mode: PreEditToolGateMode, sourceLocalTargets: readonly string[] = []) {
		this.mode = mode;
		this.declaredTargets = new Set(sourceLocalTargets);
		if (mode === "source-local-v1" && this.declaredTargets.size === 0) {
			throw new Error("source-local-v1 requires declared task targets");
		}
	}

	beforeToolCall(toolName: string, argumentsValue?: unknown, toolCallId?: string): { block: true; reason: string } | undefined {
		if (this.mode === "direct-edit-v1" && !this.successfulEditResult && (toolName === "bash" || toolName === "find")) {
			return { block: true, reason: PRE_EDIT_TOOL_GATE_BLOCK_REASON };
		}
		if (this.mode === "source-local-v1" && !this.successfulEditResult) {
			if (toolName === "bash" || toolName === "find") return { block: true, reason: SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON };
			if (toolName === "read") return this.isTargetRead(argumentsValue) ? undefined : { block: true, reason: SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON };
			if (toolName === "edit") {
				if (!toolCallId || !this.isTargetEdit(argumentsValue)) return { block: true, reason: SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON };
				// This records only an admitted target-local call ID. A later
				// successful result must carry this exact ID to unlock the gate.
				this.targetEditCallIds.add(toolCallId);
			}
		}
		return undefined;
	}

	recordToolResult(toolName: string, isError: boolean, toolCallId?: string): void {
		if (this.mode === "direct-edit-v1" && toolName === "edit" && !isError) this.successfulEditResult = true;
		if (this.mode === "source-local-v1" && toolName === "edit" && toolCallId && this.targetEditCallIds.delete(toolCallId) && !isError) {
			this.successfulEditResult = true;
		}
	}

	private isTargetRead(argumentsValue: unknown): boolean {
		return this.targetPath(argumentsValue) !== undefined;
	}

	private isTargetEdit(argumentsValue: unknown): boolean {
		// Pi's native edit tool edits one top-level `path`; Tea's independently
		// constrained transactional ABI carries `files[]`. Each adapter therefore
		// admits an edit only when every path exposed by its own public ABI is a
		// declared target.
		return this.targetPath(argumentsValue) !== undefined;
	}

	private targetPath(value: unknown): string | undefined {
		if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
		const path = (value as Record<string, unknown>).path;
		return typeof path === "string" && this.declaredTargets.has(path) ? path : undefined;
	}
}

export function createShootoutObserver(
	input: Pick<PiSessionInput, "providerRouting" | "abortAfterFirstRequest" | "preEditToolGate" | "postEditValidationGate" | "sourceLocalTargets">,
	wire: WireEvidence,
	validationGate = new PostEditValidationGate(input.postEditValidationGate ?? "none", input.sourceLocalTargets),
	bashExitStatus = new BashExitStatusWitness(),
) {
	const preEditToolGate = new PreEditToolGate(input.preEditToolGate ?? "none", input.sourceLocalTargets);
	return {
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
			pi.on("tool_call", (event) => {
				const preEditBlock = preEditToolGate.beforeToolCall(event.toolName, event.input, event.toolCallId);
				if (preEditBlock) return preEditBlock;
				const validationBlock = validationGate.beforeToolCall(event.toolName, event.input, event.toolCallId);
				if (validationBlock) return validationBlock;
				return undefined;
			});
			pi.on("tool_result", (event) => {
				preEditToolGate.recordToolResult(
					event.toolName,
					event.isError,
					event.toolCallId,
				);
				validationGate.recordToolResult(
					event.toolName,
					event.isError,
					event.toolCallId,
					event.toolName === "bash" ? bashExitStatus.take(event.toolCallId) : undefined,
				);
			});
		},
	};
}

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

export function providerTimeoutMilliseconds(outerTimeoutSeconds: number): number {
	if (!Number.isSafeInteger(outerTimeoutSeconds) || outerTimeoutSeconds < 0) {
		throw new Error("outer timeout seconds must be a non-negative safe integer");
	}
	return (outerTimeoutSeconds === 0 ? DIAGNOSTIC_REQUEST_TIMEOUT_SECONDS : outerTimeoutSeconds) * 1_000;
}

export type OuterDeadline = { expiresAtMilliseconds: number } | undefined;

export type OuterDeadlineClock = {
	now(): number;
};

export type OuterDeadlineTimer = {
	setTimeout(callback: () => void, milliseconds: number): unknown;
	clearTimeout(handle: unknown): void;
};

export type PiAbortableSession = {
	abort(): Promise<void>;
};

const systemDeadlineClock: OuterDeadlineClock = {
	now: () => performance.now(),
};

const systemDeadlineTimer: OuterDeadlineTimer = {
	setTimeout: (callback, milliseconds) => setTimeout(callback, milliseconds),
	clearTimeout: (handle) => clearTimeout(handle as NodeJS.Timeout),
};

/** Begin the scored whole-attempt deadline. Zero is the explicit diagnostic
 * lane: it leaves the outer deadline disabled while retaining its transport
 * guard. The deadline is created before local session setup so a slow setup
 * cannot consume model time outside the scored attempt. */
export function outerDeadline(
	outerTimeoutSeconds: number,
	clock: OuterDeadlineClock = systemDeadlineClock,
): OuterDeadline {
	if (!Number.isSafeInteger(outerTimeoutSeconds) || outerTimeoutSeconds < 0) {
		throw new Error("outer timeout seconds must be a non-negative safe integer");
	}
	if (outerTimeoutSeconds === 0) return undefined;
	return { expiresAtMilliseconds: clock.now() + outerTimeoutSeconds * 1_000 };
}

/** Run one Pi prompt under its whole-attempt deadline. Once that deadline
 * expires, use the pinned public session abort, await the ordinary prompt
 * settlement it triggers, and report the timeout even when cancellation makes
 * the prompt reject. The runner's separate grace is finalization time only. */
export async function settlePiPromptWithinOuterDeadline(
	session: PiAbortableSession,
	run: () => Promise<void>,
	deadline: OuterDeadline,
	timer: OuterDeadlineTimer = systemDeadlineTimer,
	clock: OuterDeadlineClock = systemDeadlineClock,
): Promise<boolean> {
	if (!deadline) {
		await run();
		return false;
	}
	const remainingMilliseconds = deadline.expiresAtMilliseconds - clock.now();
	if (remainingMilliseconds <= 0) {
		await session.abort();
		return true;
	}

	type PromptOutcome = { kind: "settled" } | { kind: "failed"; error: unknown };
	let timedOut = false;
	let timerHandle: unknown;
	let promptOutcome: Promise<PromptOutcome> | undefined;
	let resolveDeadline: ((outcome: { kind: "deadline" }) => void) | undefined;
	const deadlineOutcome = new Promise<{ kind: "deadline" }>((resolve) => {
		resolveDeadline = resolve;
	});
	timerHandle = timer.setTimeout(() => {
		// The assignment is synchronous. A prompt rejection caused by this
		// abort must not win the race and relabel the terminal as SDK failure.
		timedOut = true;
		void (async () => {
			try {
				await session.abort();
			} catch {
				// The terminal remains outer_timeout. Still wait for the prompt
				// settlement below so no model work survives the deadline.
			}
			if (promptOutcome) await promptOutcome;
			resolveDeadline?.({ kind: "deadline" });
		})();
	}, remainingMilliseconds);
	if (timedOut) {
		await deadlineOutcome;
		return true;
	}
	promptOutcome = run().then(
		() => ({ kind: "settled" }),
		(error: unknown) => ({ kind: "failed", error }),
	);
	const outcome = await Promise.race([promptOutcome, deadlineOutcome]);
	timer.clearTimeout(timerHandle);
	if (timedOut) {
		await deadlineOutcome;
		return true;
	}
	if (outcome.kind === "failed") throw outcome.error;
	return false;
}

/** Construct the real pinned Pi SDK session, including the public request
 * interception path, without requiring an inference response. Tests can ask
 * the observer to abort after the first constructed payload. */
export async function createIsolatedPiSession(input: PiSessionInput): Promise<IsolatedPiSession> {
	const agentDir = await mkdtemp(join(tmpdir(), "tea-pi-sdk-"));
	const transportTimeoutMs = providerTimeoutMilliseconds(input.outerTimeoutSeconds);
	const settings = SettingsManager.inMemory({
		compaction: { enabled: false },
		retry: { ...PAIRED_PROVIDER_RETRY, provider: { timeoutMs: transportTimeoutMs } },
		httpIdleTimeoutMs: transportTimeoutMs,
	});
	const modelRuntime = await ModelRuntime.create({ authPath: join(agentDir, "auth.json"), modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
	await modelRuntime.setRuntimeApiKey("openrouter", input.apiKey);
	const wire = new WireEvidence(attemptPaths(input.workspace, input.shell));
	// The observer is a hidden, inline SDK extension. It is instrumentation and
	// a paired execution policy; it has no prompt or tool-definition contribution.
	const validationGate = new PostEditValidationGate(input.postEditValidationGate ?? "none", input.sourceLocalTargets);
	const bashExitStatus = new BashExitStatusWitness();
	const observer = createShootoutObserver(input, wire, validationGate, bashExitStatus);
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
	const bashExitStatusBridge = createBashExitStatusBridge(createLocalBashOperations(), bashExitStatus);
	const bashTool = createBashToolDefinition(input.workspace, {
		exposeSessionEnvironment: false,
		operations: bashExitStatusBridge.operations,
		spawnHook: ({ command, cwd }) => ({ command, cwd, env: codingToolEnvironment(input.shell) }),
	});
	const nativeBashExecute = bashTool.execute.bind(bashTool);
	bashTool.execute = (toolCallId, parameters, signal, onUpdate, context) =>
		bashExitStatusBridge.run(toolCallId, () => nativeBashExecute(toolCallId, parameters, signal, onUpdate, context));
	const tools = [
		createReadToolDefinition(input.workspace),
		bashTool,
		createEditToolDefinition(input.workspace),
		createFindToolDefinition(input.workspace),
	] as unknown as ToolDefinition[];
	const { session } = await createAgentSessionFromServices({ services, sessionManager: manager, model, thinkingLevel: input.thinkingLevel, tools: ["read", "bash", "edit", "find"], customTools: tools });
	return { session, services, wire, tools, validationGate };
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
	const deadline = outerDeadline(args.outerTimeoutSeconds);
	const [taskValue, capabilities] = await Promise.all([readJson(args.taskJson), readJson(args.capabilitiesJson)]);
	if (!taskValue || typeof taskValue !== "object" || Array.isArray(taskValue)) throw new Error(`invalid task JSON object ${args.taskJson}`);
	const task = taskValue as Record<string, unknown>;
	assertInputs(args, task, capabilities);
	const prompt = task.prompt;
	if (typeof prompt !== "string" || !prompt) throw new Error("task prompt is missing")
	const sourceLocalTargets = await sourceLocalTaskTargets(task, prompt, args.workspace, args.preEditToolGate);
	const key = process.env.OPENROUTER_API_KEY;
	if (!key) throw new Error("OPENROUTER_API_KEY must be injected by vault")
	const isolated = await createIsolatedPiSession({ workspace: args.workspace, shell: args.shell, model: args.model, thinkingLevel: args.thinkingLevel, outerTimeoutSeconds: args.outerTimeoutSeconds, providerRouting: args.providerRouting, apiKey: key, preEditToolGate: args.preEditToolGate, postEditValidationGate: args.postEditValidationGate, sourceLocalTargets });
	const { session, wire, validationGate } = isolated;
	const shellHash = sha256(canonical(Object.fromEntries(Object.entries(args.shell).map(([name, value]) => [
		name,
		name === "HOME" ? "{HOME}"
			: name === "TMPDIR" ? "{TMPDIR}"
				: name === "npm_config_cache" ? "{NPM_CACHE}"
					: name === "NODE_PATH" ? "{NODE_PATH}"
						: normalizeWorkspace(value, args.workspace),
	]))));
	const transportTimeoutSeconds = providerTimeoutMilliseconds(args.outerTimeoutSeconds) / 1_000;
	const effectiveRetry = isolated.services.settingsManager.getRetrySettings();
	const reporter = new Reporter({ attemptId: args.attemptId, baselineId: args.baselineId, requestedModel: args.model, thinkingLevel: args.thinkingLevel, maxOutputTokens: args.maxOutputTokens, outerTimeoutSeconds: args.outerTimeoutSeconds, requestTimeoutSeconds: transportTimeoutSeconds, idleTimeoutSeconds: transportTimeoutSeconds, providerRouting: args.providerRouting, providerRetry: { enabled: effectiveRetry.enabled, maxRetries: effectiveRetry.maxRetries }, samplingTemperature: SHOOTOUT_TEMPERATURE, samplingSeed: SHOOTOUT_SEED, samplingSource: "adapter-set", workspace: args.workspace, evidenceDir: args.evidenceDir, shellEnvironmentSha256: shellHash, shellCurlAvailable: true, preEditToolGate: args.preEditToolGate, preEditToolGateTargets: sourceLocalTargets, postEditValidationGate: args.postEditValidationGate, validationGate, wire });
	reporter.start();
	const unsubscribe = session.subscribe((event) => reporter.observe(event));
	let terminal: { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null } = { status: "completed", code: null };
	try {
		await reporter.captureSurface(session);
		if (session.getActiveToolNames().join(",") !== "read,bash,edit,find") throw new Error("Pi active tool surface drifted")
		const outerTimedOut = await settlePiPromptWithinOuterDeadline(
			session,
			() => session.prompt(prompt, { expandPromptTemplates: false }),
			deadline,
		);
		if (outerTimedOut) {
			terminal = { status: "failed", code: "outer_timeout" };
		} else {
			const failure = terminalFailure(session);
			if (failure) {
				terminal = { status: "failed", code: failure };
			} else if (validationGate.issueReminder()) {
				const continuationTimedOut = await settlePiPromptWithinOuterDeadline(
					session,
					() => session.prompt(POST_EDIT_VALIDATION_REMINDER, { expandPromptTemplates: false }),
					deadline,
				);
				if (continuationTimedOut) {
					terminal = { status: "failed", code: "outer_timeout" };
				} else if (terminalFailure(session)) {
					terminal = { status: "failed", code: terminalFailure(session) };
				} else if (validationGate.pending()) {
					validationGate.markEvidenceMissing();
					terminal = { status: "failed", code: "post_edit_validation_evidence_missing" };
				}
			} else if (validationGate.pending()) {
				// A resumed observer state can only reach this branch after its one
				// allowed continuation; do not grant another model turn.
				validationGate.markEvidenceMissing();
				terminal = { status: "failed", code: "post_edit_validation_evidence_missing" };
			}
		}
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
