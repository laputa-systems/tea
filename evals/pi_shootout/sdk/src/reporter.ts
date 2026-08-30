import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

import { boundedPreview, canonical, normalizeWorkspace, sha256 } from "./canonical.ts";
import { type WireEvidence } from "./wire.ts";

export type Terminal = { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null };

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
	providerRouting: Record<string, unknown>;
	workspace: string;
	evidenceDir: string;
	shellEnvironmentSha256: string;
	shellCurlAvailable: boolean;
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
		record.content = boundedPreview(event.result);
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
	if (typeof event.error === "string") record.error = boundedPreview(event.error);
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
		return {
			schema_version: "tea-coding-eval-result/v3",
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
				sampling: { temperature: null, seed: null, source: "provider-default" },
			},
			wire: this.options.wire.summary(this.options.providerRouting),
			effective_policy: {
				controlled: {
					automatic_compaction: false,
					compaction_threshold: null,
					provider_retry: { enabled: true, max_retries: 0 },
					request_timeout_seconds: null,
					idle_timeout_seconds: null,
					outer_attempt_timeout_seconds: this.options.outerTimeoutSeconds,
					model_reasoning: this.options.thinkingLevel,
					output_token_ceiling: this.options.maxOutputTokens,
					provider_routing: this.options.providerRouting,
					sampling: { temperature: null, seed: null },
				},
				native: { tool_execution: canonicalTools(session).map((tool) => ({ name: tool.name, execution_mode: tool.execution_mode })) },
				observability_unknown: ["request_timeout_seconds", "idle_timeout_seconds"],
			},
			surface: this.surface,
			timings: { agent_ms: Math.max(0, Date.now() - this.startedAt), candidate_validation_ms: 0, rollover_ms: 0 },
			counts: { turns: stats.userMessages, model_turns: this.trace.filter((event) => event.type === "turn_start").length, provider_requests: null, tool_calls: stats.toolCalls, retries: this.trace.filter((event) => event.type === "auto_retry_start").length, compactions: this.trace.filter((event) => event.type === "compaction_end").length },
			usage: { input: stats.tokens.input, prompt_total: stats.tokens.input + stats.tokens.cacheRead + stats.tokens.cacheWrite, output: stats.tokens.output, generation: stats.tokens.input + stats.tokens.output, all_tokens: stats.tokens.input + stats.tokens.cacheRead + stats.tokens.cacheWrite + stats.tokens.output, reasoning: null, cache_read: stats.tokens.cacheRead, cache_write: stats.tokens.cacheWrite },
			cost: { kind: "catalog-estimate", currency: "USD", total: Number.isFinite(stats.cost) ? stats.cost : null },
			harness: { mode: "static", base_snapshot_id: null, initial_snapshot_id: null, final_snapshot_id: null, decision: "not-applicable", candidate_count: 0, candidate_id: null, changed_surfaces: [], candidate_source_bytes: 0, hypothesis: null },
			trace: this.trace,
		};
	}

	async write(path: string, result: Record<string, unknown>): Promise<void> {
		await mkdir(dirname(path), { recursive: true });
		await writeFile(path, `${JSON.stringify(result, null, 2)}\n`, "utf8");
	}
}
