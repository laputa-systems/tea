export type AssistantOutcome = { role?: unknown; stopReason?: unknown; errorMessage?: unknown };

/** Pi settles a provider-error assistant message without throwing `prompt()`. */
export function terminalFailure(session: { messages: AssistantOutcome[] }): string | null {
	const assistant = [...session.messages].reverse().find((message) => message.role === "assistant");
	return assistant?.stopReason === "error" ? "pi_model_error" : null;
}
