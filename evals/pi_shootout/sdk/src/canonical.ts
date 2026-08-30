import { createHash } from "node:crypto";

export function canonical(value: unknown): string {
	return JSON.stringify(value, (_, item: unknown) => {
		if (item && typeof item === "object" && !Array.isArray(item)) {
			// Rust's JsonValue and Python's sort_keys use byte/code-unit ordering
			// for these ASCII protocol field names. `localeCompare` placed the
			// lowercase npm_config_cache between uppercase NPM_* keys on some
			// hosts, making otherwise identical cross-adapter fingerprints differ.
			return Object.fromEntries(Object.entries(item).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0));
		}
		return item;
	});
}

export function sha256(value: string | Uint8Array): string {
	return createHash("sha256").update(value).digest("hex");
}

export function normalizeWorkspace(value: string, workspace: string): string {
	return value.split(workspace).join("{WORKSPACE}");
}

export function boundedPreview(value: unknown, maximum = 512): { bytes: number; digest: string; preview?: string } {
	const text = typeof value === "string" ? value : canonical(value);
	const bytes = Buffer.byteLength(text);
	return { bytes, digest: sha256(text), ...(text.length <= maximum ? { preview: text } : { preview: text.slice(0, maximum) }) };
}
