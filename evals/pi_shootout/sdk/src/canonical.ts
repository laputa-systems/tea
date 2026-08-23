import { createHash } from "node:crypto";

export function canonical(value: unknown): string {
	return JSON.stringify(value, (_, item: unknown) => {
		if (item && typeof item === "object" && !Array.isArray(item)) {
			return Object.fromEntries(Object.entries(item).sort(([left], [right]) => left.localeCompare(right)));
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
