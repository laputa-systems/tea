import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

import { canonical, normalizeWorkspace, sha256 } from "./canonical.ts";
import { type WireEvidence } from "./wire.ts";

export type Terminal = { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null };

export type PreEditToolGateMode = "none" | "direct-edit-v1" | "source-local-v1";
export type PostEditValidationGateMode = "none" | "unmasked-evidence-v1";

/** This exact model-visible result is shared with Tea's paired policy. */
export const PRE_EDIT_TOOL_GATE_BLOCK_REASON = "Pre-edit direct workflow policy: before a successful edit result, bash and find are unavailable. Read the named source and make the smallest edit to the named target; after a successful edit, use bash or find only for focused validation.";
/** This generic result is shared with Tea's target-constrained paired policy. */
export const SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON = "Pre-edit source-local workflow policy: before a successful edit to a declared task target, only read and edit calls whose paths are declared task targets are available. Bash, find, and non-target read/edit calls are unavailable; after a successful target-local edit, use other tools only for focused validation.";
/** This exact model-visible result is shared with Tea's paired policy. */
export const POST_EDIT_VALIDATION_BLOCK_REASON = "Validation evidence requires a direct foreground command whose exit status is visible. Avoid pipelines and status-suppression wrappers; choose an appropriate workspace-local check.";
/** This exact continuation is shared with Tea's paired policy. */
export const POST_EDIT_VALIDATION_REMINDER = "Before finalizing, run an appropriate workspace-local check after the most recent successful edit. Run it directly so its exit status is visible; avoid pipelines and status-suppression wrappers. Choose the check from the task and workspace, address any failure, then finish.";

export type PreEditToolGatePolicy = {
	mode: "none";
	blocked_tools: [];
	target_restricted_tools: [];
	source_local_targets: [];
	unlocks_after: null;
	same_batch_rule: null;
	block_reason_sha256: null;
} | {
	mode: "direct-edit-v1";
	blocked_tools: ["bash", "find"];
	target_restricted_tools: [];
	source_local_targets: [];
	unlocks_after: "prior-successful-edit-result";
	same_batch_rule: "block-until-prior-successful-edit-result";
	block_reason_sha256: string;
} | {
	mode: "source-local-v1";
	blocked_tools: ["bash", "find"];
	target_restricted_tools: ["read", "edit"];
	source_local_targets: string[];
	unlocks_after: "prior-successful-target-local-edit-result";
	same_batch_rule: "block-until-prior-successful-target-local-edit-result";
	block_reason_sha256: string;
};

export function preEditToolGatePolicy(mode: PreEditToolGateMode, sourceLocalTargets: readonly string[] = []): PreEditToolGatePolicy {
	if (mode === "none") {
		return {
			mode,
			blocked_tools: [],
			target_restricted_tools: [],
			source_local_targets: [],
			unlocks_after: null,
			same_batch_rule: null,
			block_reason_sha256: null,
		};
	}
	if (mode === "direct-edit-v1") return {
		mode,
		blocked_tools: ["bash", "find"],
		target_restricted_tools: [],
		source_local_targets: [],
		unlocks_after: "prior-successful-edit-result",
		same_batch_rule: "block-until-prior-successful-edit-result",
		block_reason_sha256: sha256(PRE_EDIT_TOOL_GATE_BLOCK_REASON),
	};
	if (!sourceLocalTargets.length) throw new Error("source-local-v1 requires declared task targets");
	return {
		mode,
		blocked_tools: ["bash", "find"],
		target_restricted_tools: ["read", "edit"],
		source_local_targets: [...sourceLocalTargets],
		unlocks_after: "prior-successful-target-local-edit-result",
		same_batch_rule: "block-until-prior-successful-target-local-edit-result",
		block_reason_sha256: sha256(SOURCE_LOCAL_PRE_EDIT_TOOL_GATE_BLOCK_REASON),
	};
}

/** A deliberately closed syntax profile. It only establishes that a direct
 * foreground shell command exposed its process status; it makes no claim that
 * the command is task-correct, a test, or a hidden validator. */
export function directForegroundShellV1(command: unknown): command is string {
	if (typeof command !== "string" || command.trim().length === 0) return false;
	if (command.includes("\0") || command.includes("\n") || command.includes("\r")) return false;
	if (/[;&|$`()<>"'\\\\]/u.test(command)) return false;
	// With composition and quoting rejected, whitespace-delimited words are a
	// conservative enough evaluator screen. Checking every word also rejects
	// `env bash` without attempting to recognize task-specific command names.
	return !command.split(/\s+/u).some((word) => {
		const base = word.split("/").at(-1)?.toLowerCase();
		return base !== undefined && ["sh", "bash", "zsh", "dash", "fish", ".", "source", "eval", "exec"].includes(base);
	});
}

export type PostEditValidationGatePolicy = {
	mode: "none";
	applies_after: null;
	qualifies_with: null;
	resets_after: null;
	same_batch_rule: null;
	command_profile: null;
	completion_reminder_limit: 0;
	block_reason_sha256: null;
	reminder_sha256: null;
} | {
	mode: "unmasked-evidence-v1";
	applies_after: "prior-successful-declared-target-edit-result";
	qualifies_with: "prior-successful-unmasked-direct-foreground-bash-result";
	resets_after: "later-successful-edit-result";
	same_batch_rule: "evidence-requires-prior-successful-bash-result";
	command_profile: "unmasked-direct-foreground-bash/v1";
	completion_reminder_limit: 1;
	block_reason_sha256: string;
	reminder_sha256: string;
};

export function postEditValidationGatePolicy(mode: PostEditValidationGateMode): PostEditValidationGatePolicy {
	if (mode === "none") return {
		mode,
		applies_after: null,
		qualifies_with: null,
		resets_after: null,
		same_batch_rule: null,
		command_profile: null,
		completion_reminder_limit: 0,
		block_reason_sha256: null,
		reminder_sha256: null,
	};
	return {
		mode,
		applies_after: "prior-successful-declared-target-edit-result",
		qualifies_with: "prior-successful-unmasked-direct-foreground-bash-result",
		resets_after: "later-successful-edit-result",
		same_batch_rule: "evidence-requires-prior-successful-bash-result",
		command_profile: "unmasked-direct-foreground-bash/v1",
		completion_reminder_limit: 1,
		block_reason_sha256: sha256(POST_EDIT_VALIDATION_BLOCK_REASON),
		reminder_sha256: sha256(POST_EDIT_VALIDATION_REMINDER),
	};
}

type ValidationTransition = {
	transition: "edit-pending" | "evidence-satisfied" | "candidate-failed" | "masked-bash-blocked" | "completion-reminder-issued" | "evidence-missing";
	generation: number;
	qualifying_call_id_sha256: string | null;
	qualifying_arguments_sha256: string | null;
	process_exit: "exited-zero" | null;
};

export type ValidationEvidence = {
	state: "not_required" | "satisfied" | "missing";
	edit_generation: number | null;
	qualifying_call_id_sha256: string | null;
	qualifying_arguments_sha256: string | null;
	qualifying_process_exit: "exited-zero" | null;
	candidate_failures: number;
	masked_call_blocks: number;
	reminders_issued: number;
	transitions_sha256: string;
};

type Candidate = {
	generation: number;
	callIdSha256: string;
	argumentsSha256: string;
	eligible: boolean;
};

/** State retained by Pi's hidden observer. Only hashes and generic transition
 * names are exported; commands, results, and validator identity never cross
 * the adapter boundary. */
export class PostEditValidationGate {
	private readonly declaredTargets: ReadonlySet<string>;
	private readonly targetEditCallIds = new Set<string>();
	private readonly unsettledEditCallIds = new Set<string>();
	private readonly bashCandidates = new Map<string, Candidate>();
	private readonly transitions: ValidationTransition[] = [];
	private generation = 0;
	private qualifying: Candidate | undefined;
	private candidateFailures = 0;
	private maskedCallBlocks = 0;
	private remindersIssued = 0;
	readonly mode: PostEditValidationGateMode;

	constructor(mode: PostEditValidationGateMode, sourceLocalTargets: readonly string[] = []) {
		this.mode = mode;
		this.declaredTargets = new Set(sourceLocalTargets);
		if (mode === "unmasked-evidence-v1" && this.declaredTargets.size === 0) {
			throw new Error("unmasked-evidence-v1 requires declared source-local targets");
		}
	}

	beforeToolCall(toolName: string, argumentsValue?: unknown, toolCallId?: string): { block: true; reason: string } | undefined {
		if (this.mode === "none") return undefined;
		if (toolName === "edit" && toolCallId) {
			// Pi emits every tool_call in an assistant batch before any matching
			// tool_result settles. If a bash event arrived earlier in that same
			// stream, a later edit must still make the candidate ineligible—even
			// when that edit eventually fails and therefore does not reset the
			// generation.
			for (const candidate of this.bashCandidates.values()) candidate.eligible = false;
			this.unsettledEditCallIds.add(toolCallId);
			if (this.isTargetEdit(argumentsValue)) this.targetEditCallIds.add(toolCallId);
			return undefined;
		}
		if (toolName !== "bash" || !this.pending()) return undefined;
		const command = this.bashCommand(argumentsValue);
		if (!directForegroundShellV1(command)) {
			this.maskedCallBlocks += 1;
			this.transitions.push({
				transition: "masked-bash-blocked", generation: this.generation,
				qualifying_call_id_sha256: null, qualifying_arguments_sha256: null, process_exit: null,
			});
			return { block: true, reason: POST_EDIT_VALIDATION_BLOCK_REASON };
		}
		if (toolCallId) {
			this.bashCandidates.set(toolCallId, {
				generation: this.generation,
				callIdSha256: sha256(toolCallId),
				argumentsSha256: sha256(canonical(argumentsValue)),
				// Pi preflights one assistant batch before results settle. A
				// target edit still awaiting its result makes a sibling bash
				// ineligible even when an older edit is already pending.
				eligible: this.unsettledEditCallIds.size === 0,
			});
		}
		return undefined;
	}

	recordToolResult(toolName: string, isError: boolean, toolCallId?: string, bashProcessSucceeded?: boolean): void {
		if (this.mode === "none") return;
		if (toolName === "edit" && toolCallId) {
			const declaredTarget = this.targetEditCallIds.delete(toolCallId);
			const admittedEdit = this.unsettledEditCallIds.delete(toolCallId);
			if (!isError && admittedEdit && (declaredTarget || this.generation > 0)) {
				this.generation += 1;
				this.qualifying = undefined;
				this.transitions.push({
					transition: "edit-pending", generation: this.generation,
					qualifying_call_id_sha256: null, qualifying_arguments_sha256: null, process_exit: null,
				});
			}
			return;
		}
		if (toolName !== "bash" || !toolCallId) return;
		const candidate = this.bashCandidates.get(toolCallId);
		if (!candidate) return;
		this.bashCandidates.delete(toolCallId);
		if (!candidate.eligible || candidate.generation !== this.generation || !this.pending()) return;
		if (isError || bashProcessSucceeded !== true) {
			this.candidateFailures += 1;
			this.transitions.push({
				transition: "candidate-failed", generation: this.generation,
				qualifying_call_id_sha256: null, qualifying_arguments_sha256: null, process_exit: null,
			});
			return;
		}
		this.qualifying = candidate;
		this.transitions.push({
			transition: "evidence-satisfied", generation: this.generation,
			qualifying_call_id_sha256: candidate.callIdSha256,
			qualifying_arguments_sha256: candidate.argumentsSha256,
			process_exit: "exited-zero",
		});
	}

	pending(): boolean {
		return this.mode === "unmasked-evidence-v1" && this.generation > 0 && this.qualifying === undefined;
	}

	issueReminder(): boolean {
		if (!this.pending() || this.remindersIssued !== 0) return false;
		this.remindersIssued = 1;
		this.transitions.push({
			transition: "completion-reminder-issued", generation: this.generation,
			qualifying_call_id_sha256: null, qualifying_arguments_sha256: null, process_exit: null,
		});
		return true;
	}

	markEvidenceMissing(): void {
		if (!this.pending() || this.remindersIssued !== 1) return;
		this.transitions.push({
			transition: "evidence-missing", generation: this.generation,
			qualifying_call_id_sha256: null, qualifying_arguments_sha256: null, process_exit: null,
		});
	}

	evidence(): ValidationEvidence {
		const state = this.mode === "none" || this.generation === 0
			? "not_required"
			: this.qualifying === undefined ? "missing" : "satisfied";
		return {
			state,
			edit_generation: state === "not_required" ? null : this.generation,
			qualifying_call_id_sha256: this.qualifying?.callIdSha256 ?? null,
			qualifying_arguments_sha256: this.qualifying?.argumentsSha256 ?? null,
			qualifying_process_exit: this.qualifying ? "exited-zero" : null,
			candidate_failures: this.candidateFailures,
			masked_call_blocks: this.maskedCallBlocks,
			reminders_issued: this.remindersIssued,
			transitions_sha256: sha256(canonical(this.transitions)),
		};
	}

	trace(): Array<Record<string, unknown>> {
		return this.transitions.map((transition) => ({ type: "post_edit_validation_transition", ...transition }));
	}

	private isTargetEdit(value: unknown): boolean {
		if (!value || typeof value !== "object" || Array.isArray(value)) return false;
		const path = (value as Record<string, unknown>).path;
		return typeof path === "string" && this.declaredTargets.has(path);
	}

	private bashCommand(value: unknown): unknown {
		if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
		return (value as Record<string, unknown>).command;
	}
}

type SessionLike = {
	systemPrompt: string;
	getActiveToolNames(): string[];
	getAllTools(): Array<{ name: string; description: string; parameters: unknown; promptGuidelines?: unknown; executionMode?: unknown }>;
	getToolDefinition?(name: string): { name: string; description: string; parameters: unknown; promptGuidelines?: unknown; executionMode?: unknown } | undefined;
	getSessionStats(): { userMessages: number; toolCalls: number; tokens: { input: number; output: number; cacheRead: number; cacheWrite: number }; cost: number };
	messages: Array<{ role?: string; content?: unknown }>;
	model?: { id?: string; provider?: string };
};

export type ReporterOptions = {
	attemptId: string;
	baselineId: "pi-static";
	requestedModel: string;
	thinkingLevel: string;
	maxOutputTokens: number | null;
	outerTimeoutSeconds: number;
	requestTimeoutSeconds: number;
	idleTimeoutSeconds: number;
	providerRouting: Record<string, unknown>;
	providerRetry: { enabled: boolean; maxRetries: number };
	samplingTemperature?: number | null;
	samplingSeed?: number | null;
	samplingSource?: string;
	workspace: string;
	evidenceDir: string;
	shellEnvironmentSha256: string;
	shellCurlAvailable: boolean;
	preEditToolGate?: PreEditToolGateMode;
	preEditToolGateTargets?: readonly string[];
	postEditValidationGate?: PostEditValidationGateMode;
	validationGate?: PostEditValidationGate;
	wire: WireEvidence;
};

function visibleText(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((block): block is { type?: unknown; text?: unknown } => block !== null && typeof block === "object")
		.filter((block) => block.type === "text" && typeof block.text === "string")
		.map((block) => block.text as string)
		.join("");
}

/** Classify a tool action while the public Pi event still has its arguments.
 * The shareable trace retains only this category and a canonical argument hash,
 * never the command text itself. Keep these names aligned with compare.py. */
function toolCategory(toolName: string | undefined, argumentsValue: unknown): string {
	if (toolName === "read" || toolName === "find") return "inspection";
	if (toolName === "edit") return "edit";
	if (toolName !== "bash" || argumentsValue === null || typeof argumentsValue !== "object" || Array.isArray(argumentsValue)) return toolName ? "other" : "unknown";
	const command = (argumentsValue as Record<string, unknown>).command;
	if (typeof command !== "string") return "shell";
	const lowered = command.toLowerCase();
	if (/\b(curl|npm\s+(?:view|pack|install)|git\s+fetch)\b|github\.com/u.test(lowered)) return "upstream_or_dependency";
	if (/\b(npm\s+(?:test|run)|(?:npx|npm|yarn|pnpm)\s+.*(?:mocha|jest)|pytest|cargo\s+test)\b/u.test(lowered)) return "validation";
	if (/\b(eslint|clippy|rustfmt|lint)\b/u.test(lowered)) return "lint";
	if (/\bgit\s+(?:status|log|diff|show|stash|branch)\b/u.test(lowered)) return "repository_state";
	return "shell";
}

function eventRecord(event: Record<string, unknown>, sequence: number): Record<string, unknown> {
	const type = typeof event.type === "string" ? event.type : "unknown";
	const record: Record<string, unknown> = { sequence, type };
	const toolName = typeof event.toolName === "string" ? event.toolName : undefined;
	const toolCallId = typeof event.toolCallId === "string" ? event.toolCallId : typeof event.id === "string" ? event.id : undefined;
	if (toolName) record.tool_name = toolName;
	if (toolCallId) record.tool_call_id = toolCallId;
	if (type === "tool_execution_start" && "args" in event) {
		record.arguments_sha256 = sha256(canonical(event.args));
		record.category = toolCategory(toolName, event.args);
	}
	if (type === "tool_execution_end") {
		record.success = event.isError !== true;
	}
	if (type.includes("tool") && "result" in event) {
		const result = typeof event.result === "string" ? event.result : canonical(event.result);
		record.content = { bytes: Buffer.byteLength(result), digest: sha256(result) };
	}
	const message = event.message;
	if (message !== null && typeof message === "object" && !Array.isArray(message)) {
		const assistant = message as Record<string, unknown>;
		if (assistant.role === "assistant") {
			record.stop_reason = typeof assistant.stopReason === "string" ? assistant.stopReason : null;
			record.visible_text_bytes = Buffer.byteLength(visibleText(assistant.content));
			const usage = assistant.usage;
			if (usage !== null && typeof usage === "object" && !Array.isArray(usage)) {
				record.usage = Object.fromEntries(["input", "output", "cacheRead", "cacheWrite"].map((name) => [name, (usage as Record<string, unknown>)[name] ?? null]));
			}
		}
	}
	if (typeof event.error === "string") record.error = { bytes: Buffer.byteLength(event.error), digest: sha256(event.error) };
	return record;
}

function canonicalTools(session: SessionLike): Array<Record<string, unknown>> {
	const registered = new Map(session.getAllTools().map((tool) => [tool.name, tool]));
	return session.getActiveToolNames().map((name) => {
		const tool = session.getToolDefinition?.(name) ?? registered.get(name);
		if (!tool) throw new Error(`active Pi tool ${name} has no definition`);
		return {
			name: tool.name,
			description: tool.description,
			parameters: tool.parameters,
			prompt_guidelines: tool.promptGuidelines ?? null,
			// The pinned public ToolDefinition does not promise this metadata for
			// every native factory. Unknown is more faithful than a parallel guess.
			execution_mode: typeof tool.executionMode === "string" ? tool.executionMode : null,
		};
	});
}

export class Reporter {
	readonly trace: Array<Record<string, unknown>> = [];
	private startedAt = 0;
	private surface: Record<string, unknown> | undefined;
	readonly options: ReporterOptions;

	constructor(options: ReporterOptions) {
		this.options = options;
	}

	start(): void {
		this.startedAt = Date.now();
	}

	observe(event: unknown): void {
		if (!event || typeof event !== "object") return;
		this.trace.push(eventRecord(event as Record<string, unknown>, this.trace.length + 1));
	}

	async captureSurface(session: SessionLike): Promise<Record<string, unknown>> {
		const prompt = session.systemPrompt;
		const tools = canonicalTools(session);
		this.surface = {
			system_prompt_bytes: Buffer.byteLength(prompt),
			system_prompt_sha256: sha256(prompt),
			workspace_normalized_system_prompt_sha256: sha256(normalizeWorkspace(prompt, this.options.workspace)),
			tool_surface_sha256: sha256(canonical(tools)),
			prompt_tool_surface_sha256: sha256(canonical(tools.map((tool) => ({ name: tool.name, description: tool.description, prompt_guidelines: tool.prompt_guidelines })))),
			wire_tool_surface_sha256: null,
			execution_surface_sha256: sha256(canonical(tools.map((tool) => ({ name: tool.name, execution_mode: tool.execution_mode })))),
			active_tools: session.getActiveToolNames(),
			authority: { tools: session.getActiveToolNames(), shell: true, secret_boundary: "explicit shootout shell allowlist" },
			research_tools: [],
			subagents: false,
			shell_curl_available: this.options.shellCurlAvailable,
			shell_environment_sha256: this.options.shellEnvironmentSha256,
			pre_edit_tool_gate: preEditToolGatePolicy(this.options.preEditToolGate ?? "none", this.options.preEditToolGateTargets),
			post_edit_validation_gate: postEditValidationGatePolicy(this.options.postEditValidationGate ?? "none"),
		};
		await mkdir(this.options.evidenceDir, { recursive: true });
		await writeFile(join(this.options.evidenceDir, "system-prompt.txt"), prompt, "utf8");
		await writeFile(join(this.options.evidenceDir, "tool-surface.json"), `${JSON.stringify(tools, null, 2)}\n`, "utf8");
		return this.surface;
	}

	finish(session: SessionLike, terminal: Terminal): Record<string, unknown> {
		if (!this.surface) throw new Error("captureSurface must precede finish");
		const stats = session.getSessionStats();
		const final = [...session.messages].reverse().find((message) => message.role === "assistant");
		const finalText = visibleText(final?.content);
		const firstRequest = this.options.wire.requests[0];
		this.surface.wire_tool_surface_sha256 = typeof firstRequest?.tool_schema_sha256 === "string" ? firstRequest.tool_schema_sha256 : null;
		const returned = this.options.wire.returnedRoute;
		const validationGate = this.options.validationGate ?? new PostEditValidationGate(this.options.postEditValidationGate ?? "none", this.options.preEditToolGateTargets);
		const validationEvidence = validationGate.evidence();
		return {
			schema_version: "tea-coding-eval-result/v4",
			attempt_id: this.options.attemptId,
			baseline_id: this.options.baselineId,
			terminal,
			final_text: finalText,
			runtime: { implementation: "pi-sdk", version: "0.84.4", revision: "npm:@earendil-works/pi-coding-agent@0.84.4", dirty: false, dirty_digest: null },
			model: {
				provider: "openrouter", requested_model: this.options.requestedModel,
				returned_model: returned.model, returned_provider: returned.provider,
				returned_model_provenance: returned.model === null ? null : returned.provenance,
				returned_provider_provenance: returned.provider === null ? null : returned.provenance,
				thinking_level: this.options.thinkingLevel, max_output_tokens: this.options.maxOutputTokens,
				sampling: { temperature: this.options.samplingTemperature ?? null, seed: this.options.samplingSeed ?? null, source: this.options.samplingSource ?? "provider-default" },
			},
			wire: this.options.wire.summary(this.options.providerRouting),
				effective_policy: {
				controlled: {
					automatic_compaction: false,
					compaction_threshold: null,
					provider_retry: {
						enabled: this.options.providerRetry.enabled,
						max_retries: this.options.providerRetry.maxRetries,
					},
					request_timeout_seconds: this.options.requestTimeoutSeconds,
					idle_timeout_seconds: this.options.idleTimeoutSeconds,
					outer_attempt_timeout_seconds: this.options.outerTimeoutSeconds,
					model_reasoning: this.options.thinkingLevel,
					output_token_ceiling: this.options.maxOutputTokens,
					provider_routing: this.options.providerRouting,
					sampling: { temperature: this.options.samplingTemperature ?? null, seed: this.options.samplingSeed ?? null },
					pre_edit_tool_gate: preEditToolGatePolicy(this.options.preEditToolGate ?? "none", this.options.preEditToolGateTargets),
					post_edit_validation_gate: postEditValidationGatePolicy(this.options.postEditValidationGate ?? "none"),
				},
				native: { tool_execution: canonicalTools(session).map((tool) => ({ name: tool.name, execution_mode: tool.execution_mode })) },
				observability_unknown: [],
			},
			surface: this.surface,
			timings: { agent_ms: Math.max(0, Date.now() - this.startedAt), candidate_validation_ms: 0, rollover_ms: 0 },
			counts: { turns: stats.userMessages, model_turns: this.trace.filter((event) => event.type === "turn_start").length, provider_requests: null, tool_calls: stats.toolCalls, retries: this.trace.filter((event) => event.type === "auto_retry_start").length, compactions: this.trace.filter((event) => event.type === "compaction_end").length },
			usage: { input: stats.tokens.input, prompt_total: stats.tokens.input + stats.tokens.cacheRead + stats.tokens.cacheWrite, output: stats.tokens.output, generation: stats.tokens.input + stats.tokens.output, all_tokens: stats.tokens.input + stats.tokens.cacheRead + stats.tokens.cacheWrite + stats.tokens.output, reasoning: null, cache_read: stats.tokens.cacheRead, cache_write: stats.tokens.cacheWrite },
			cost: { kind: "catalog-estimate", currency: "USD", total: Number.isFinite(stats.cost) ? stats.cost : null },
			harness: { mode: "static", base_snapshot_id: null, initial_snapshot_id: null, final_snapshot_id: null, decision: "not-applicable", candidate_count: 0, candidate_id: null, changed_surfaces: [], candidate_source_bytes: 0, hypothesis: null },
			validation_evidence: validationEvidence,
			trace: [...this.trace, ...validationGate.trace()],
		};
	}

	async write(path: string, result: Record<string, unknown>): Promise<void> {
		await mkdir(dirname(path), { recursive: true });
		await writeFile(path, `${JSON.stringify(result, null, 2)}\n`, "utf8");
	}
}
