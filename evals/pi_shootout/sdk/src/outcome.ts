export type AssistantOutcome = { role?: unknown; stopReason?: unknown; errorMessage?: unknown };

function isRateLimitError(errorMessage: unknown): boolean {
	if (typeof errorMessage !== "string") return false;
	const normalized = errorMessage.toLowerCase();
	return /\b429\b|rate[\s_-]*limit|too many requests/.test(normalized);
}

/** Pi settles a provider-error assistant message without throwing `prompt()`. */
export function terminalFailure(session: { messages: AssistantOutcome[] }): string | null {
	const assistant = [...session.messages].reverse().find((message) => message.role === "assistant");
	if (assistant?.stopReason !== "error") return null;
	return isRateLimitError(assistant.errorMessage) ? "openrouter_response_429" : "pi_model_error";
}
