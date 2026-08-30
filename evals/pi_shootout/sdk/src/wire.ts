import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { canonical, sha256 } from "./canonical.ts";

type JsonObject = Record<string, unknown>;

export type AttemptPath = { value: string; replacement: string };

export type ReturnedRoute = {
	model: string | null;
	provider: string | null;
	provenance: string | null;
};

const SENSITIVE_FIELD = /(?:authorization|api[_-]?key|token|credential|secret|password)/iu;
const KNOWN_FIELDS = new Set([
	"model", "messages", "tools", "reasoning", "reasoning_effort", "temperature", "seed",
	"max_tokens", "max_completion_tokens", "tool_choice", "parallel_tool_calls", "stream",
	"stream_options", "provider",
]);

function object(value: unknown): JsonObject | null {
	return value !== null && typeof value === "object" && !Array.isArray(value) ? value as JsonObject : null;
}

function normalizeText(value: string, paths: readonly AttemptPath[]): string {
	return paths.reduce((current, path) => current.split(path.value).join(path.replacement), value);
}

/** Remove credentials and normalize only attempt-local paths in retained evidence.
 * This deliberately operates on the persisted copy; it never changes the
 * object returned by Pi to its provider. */
export function sanitizeWireValue(value: unknown, paths: readonly AttemptPath[]): unknown {
	if (typeof value === "string") return normalizeText(value, paths);
	if (Array.isArray(value)) return value.map((entry) => sanitizeWireValue(entry, paths));
	const record = object(value);
	if (!record) return value;
	return Object.fromEntries(Object.entries(record).map(([name, entry]) => [
		name,
		SENSITIVE_FIELD.test(name) ? "[redacted]" : sanitizeWireValue(entry, paths),
	]));
}

function field(payload: JsonObject, name: string): { present: boolean; value: unknown | null } {
	return Object.hasOwn(payload, name) ? { present: true, value: payload[name] ?? null } : { present: false, value: null };
}

function systemDigest(messages: unknown[]): string | null {
	const system = messages.filter((message) => {
		const item = object(message);
		return item?.role === "system" || item?.role === "developer";
	});
	return system.length === 0 ? null : sha256(canonical(system));
}

function toolFacts(value: unknown): { names: string[]; schemaSha256: string } {
	if (!Array.isArray(value)) return { names: [], schemaSha256: sha256(canonical([])) };
	const tools = value.map((tool) => object(tool) ?? {});
	const names = tools.map((tool) => {
		const functionTool = object(tool.function);
		return typeof functionTool?.name === "string" ? functionTool.name : typeof tool.name === "string" ? tool.name : "<unnamed>";
	});
	return { names, schemaSha256: sha256(canonical(tools)) };
}

/** Derive a canonical, content-safe witness from the actual provider payload. */
export function summarizeWireRequest(payloadValue: unknown, ordinal: number, paths: readonly AttemptPath[]): JsonObject {
	const payload = object(payloadValue);
	if (!payload) throw new Error("Pi before_provider_request payload must be an object");
	const sanitized = sanitizeWireValue(payload, paths) as JsonObject;
	const messages = Array.isArray(sanitized.messages) ? sanitized.messages : [];
	const tools = toolFacts(sanitized.tools);
	const roles = messages.map((message) => {
		const item = object(message);
		return typeof item?.role === "string" ? item.role : "<missing>";
	});
	const assistantMessages = messages.filter((message) => object(message)?.role === "assistant");
	return {
		ordinal,
		canonical_request_sha256: sha256(canonical(sanitized)),
		model: typeof sanitized.model === "string" ? sanitized.model : null,
		message_count: messages.length,
		message_roles: roles,
		messages: messages.map((message, index) => {
			const item = object(message) ?? {};
			const content = Object.hasOwn(item, "content") ? item.content : null;
			return {
				ordinal: index + 1,
				role: roles[index],
				structural_sha256: sha256(canonical(item)),
				content_sha256: sha256(canonical(content)),
			};
		}),
		system_prompt_sha256: systemDigest(messages),
		assistant_reasoning_content: assistantMessages.length === 0 ? null : assistantMessages.every((message) => Object.hasOwn(object(message) ?? {}, "reasoning_content")),
		tool_count: tools.names.length,
		tool_names: tools.names,
		tool_schema_sha256: tools.schemaSha256,
		reasoning: Object.hasOwn(sanitized, "reasoning") ? sanitized.reasoning : Object.hasOwn(sanitized, "reasoning_effort") ? sanitized.reasoning_effort : null,
		temperature: field(sanitized, "temperature"),
		seed: field(sanitized, "seed"),
		max_tokens: field(sanitized, "max_tokens"),
		max_completion_tokens: field(sanitized, "max_completion_tokens"),
		tool_choice: field(sanitized, "tool_choice"),
		parallel_tool_calls: field(sanitized, "parallel_tool_calls"),
		stream: field(sanitized, "stream"),
		stream_options: field(sanitized, "stream_options"),
		provider_routing: object(sanitized.provider),
		other_model_affecting_top_level_fields: Object.fromEntries(
			Object.entries(sanitized).filter(([name]) => !KNOWN_FIELDS.has(name)),
		),
		// Private attempt evidence intentionally retains the sanitized model-facing
		// request. Shareable reports consume the structural fields above instead.
		canonical_payload: sanitized,
	};
}

export class WireEvidence {
	readonly requests: JsonObject[] = [];
	readonly paths: readonly AttemptPath[];
	returnedRoute: ReturnedRoute = { model: null, provider: null, provenance: null };

	constructor(paths: readonly AttemptPath[]) {
		this.paths = paths;
	}

	capture(payload: unknown): void {
		this.requests.push(summarizeWireRequest(payload, this.requests.length + 1, this.paths));
	}

	captureResponse(headers: Record<string, string>): void {
		const lowered = Object.fromEntries(Object.entries(headers).map(([name, value]) => [name.toLowerCase(), value]));
		const provider = lowered["x-openrouter-provider"] ?? null;
		const model = lowered["x-openrouter-model"] ?? null;
		if (provider !== null || model !== null) {
			this.returnedRoute = { model, provider, provenance: "OpenRouter response header" };
		}
	}

	summary(routingPolicy: JsonObject): JsonObject {
		return {
			source: "direct-final-openrouter-boundary",
			request_count: this.requests.length,
			requests: this.requests.map(({ canonical_payload: _payload, ...summary }) => summary),
			routing_policy: routingPolicy,
			returned_route: this.returnedRoute,
		};
	}

	async write(evidenceDir: string): Promise<void> {
		await mkdir(evidenceDir, { recursive: true });
		await writeFile(
			join(evidenceDir, "wire-requests.json"),
			`${JSON.stringify({ schema_version: "tea-pi-wire-request-evidence/v1", requests: this.requests, returned_route: this.returnedRoute }, null, 2)}\n`,
			"utf8",
		);
	}
}
