import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

import { boundedPreview, canonical, normalizeWorkspace, sha256 } from "./canonical.ts";

export type Terminal = { status: "completed" | "failed" | "cancelled" | "aborted"; code: string | null };

type SessionLike = {
	systemPrompt: string;
	getActiveToolNames(): string[];
	getAllTools(): Array<{ name: string; description: string; parameters: unknown; promptGuidelines?: unknown }>;
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
	workspace: string;
	evidenceDir: string;
	shellEnvironmentSha256: string;
	shellCurlAvailable: boolean;
};

function eventRecord(event: Record<string, unknown>, sequence: number): Record<string, unknown> {
	const type = typeof event.type === "string" ? event.type : "unknown";
	const record: Record<string, unknown> = { sequence, type };
	const toolName = typeof event.toolName === "string" ? event.toolName : undefined;
	const toolCallId = typeof event.toolCallId === "string" ? event.toolCallId : typeof event.id === "string" ? event.id : undefined;
	if (toolName) record.tool_name = toolName;
	if (toolCallId) record.tool_call_id = toolCallId;
	if (type.includes("tool") && "result" in event) {
		record.content = boundedPreview(event.result);
	}
	if (typeof event.error === "string") record.error = boundedPreview(event.error);
	return record;
}

function canonicalTools(session: SessionLike): Array<Record<string, unknown>> {
	return session.getAllTools().map((tool) => ({
		name: tool.name,
		description: tool.description,
		parameters: tool.parameters,
		prompt_guidelines: tool.promptGuidelines ?? null,
		execution_mode: "parallel",
	}));
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
			active_tools: session.getActiveToolNames(),
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
		const finalText = typeof final?.content === "string" ? final.content : "";
		return {
			schema_version: "tea-coding-eval-result/v2",
			attempt_id: this.options.attemptId,
			baseline_id: this.options.baselineId,
			terminal,
			final_text: finalText,
			runtime: { implementation: "pi-sdk", version: "0.84.2", revision: "npm:@earendil-works/pi-coding-agent@0.84.2", dirty: false, dirty_digest: null },
			model: {
				provider: "openrouter", requested_model: this.options.requestedModel,
				returned_model: session.model?.id ?? null, returned_provider: session.model?.provider ?? null,
				thinking_level: this.options.thinkingLevel, max_output_tokens: this.options.maxOutputTokens,
				sampling: { temperature: null, seed: null, source: "provider-default" },
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
