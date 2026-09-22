# Compaction

Compaction is an explicit, transactional context replacement owned by
tea-core. A host provides the Compactor, model access, context capacity, and
policy; the core validates a proposed replacement and commits it only while the
owning operation remains active.

Automatic compaction is opt-in through AutomaticCompactionPolicy. It runs only
at a safe boundary after an assistant/tool turn and before the next provider
request. Typed context-overflow recovery may compact and retry a single
incomplete continuation within the configured bound.

A compaction that could not shrink the request is declined before it costs
anything. The policy's `recent_tokens` selects an intact recent suffix, and a
transcript that fits entirely inside that suffix leaves nothing to summarize;
asking a compactor anyway would spend a provider request and come back as a
replacement with an empty checkpoint, which is a fatal rejection. Small context
capacities reach this state immediately, because fixed instructions and tool
schemas can exceed the threshold on their own.

The current provider-backed host strategy is
cache_replay_summary_v1. It preserves an exact source prefix when possible and
records a v1 strategy descriptor, request-layout observations, provider usage,
and compaction lifecycle records.

The durable harness adds those lifecycle records to the session and redacted
trace. It never stores a prompt or checkpoint in the trace artifact itself.
