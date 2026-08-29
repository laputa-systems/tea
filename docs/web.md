# Web retrieval

Tea exposes one model-visible `web` tool for web evidence.

```json
{"query":"why did rustls change CryptoProvider defaults"}
```

A query discovers relevant sources and returns cleaned Markdown from those
pages in the same tool call. It defaults to `kind: "developer"`, which is
suited to documentation, READMEs, GitHub issues, pull requests, and
implementation research. Use `kind: "web"` for broader research. A query
returns five results by default and accepts at most eight.

```json
{"urls":["https://docs.rs/example","https://github.com/example/project/issues/42"]}
```

Use `urls` for sources already known. Tea retrieves the array concurrently in
one call, preserving its input order and retaining successful pages when one
member fails. There is deliberately no `action`, separate search/fetch tool,
or singular `url` field: both forms mean “return useful source content now.”
Batch independent known URLs instead of making sequential calls.

`web` uses Firecrawl Keyless through `POST /v2/search` and `POST /v2/scrape`.
It needs no API key or `[web]` configuration. Tea does not include a local
browser; Firecrawl performs remote JS-heavy retrieval. Returned source bodies
are visibly marked as untrusted external content and are bounded fairly before
they reach the model.

The checked-in Luau bundle owns Firecrawl request shape, developer-search
policy, missing-content repair, response normalization, failure handling, and
truncation. `tea-http` owns generic pooled HTTP/1+HTTP/2 TLS transport,
deadlines, retries, route scheduling, cancellation, and `request_many`.
The host grants the bundle only a route-scoped `network.http` capability for
`https://api.firecrawl.dev/v2/search` and `/v2/scrape`; provider policy cannot
select an arbitrary origin.

When Firecrawl is unavailable, `web` reports a bounded error. For a known
public URL or API, use `bash` with `curl` for direct retrieval where practical.
That is not a replacement for search discovery.
