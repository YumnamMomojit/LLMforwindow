# Local/remote API

Base URL: `http://127.0.0.1:8787/v1` locally; `https://<your-hostname>/v1` when sharing. Auth: `Authorization: Bearer sem_...`.
Model ID: the catalog id you served (`GET /v1/models` returns exactly that one, and only while the engine is ready).

| Endpoint | Notes |
|---|---|
| `GET /v1/models` | one entry; 503 `engine_unavailable` if the engine is not ready |
| `POST /v1/chat/completions` | JSON or SSE (`stream:true`) |

Supported: roles `system`/`user`/`assistant` (string or `[{type:"text"}]` content), `max_tokens`/`max_completion_tokens`
(default 1024, clamped to 4096; `semdok-edge serve --max-output-tokens N` sets the clamp, and a lower N lowers the default too), `temperature` 0–2, `top_p` 0–1, `stop` (≤4 strings), `stream`, `stream_options.include_usage`, `n=1`.
A trailing `assistant` message is *continued* by the upstream (documented upstream behaviour).
Accepted but **ignored**: `seed`, `user`, `store`, `metadata`, `service_tier`. **Rejected (400)**: everything else, incl. tools,
logprobs, response_format, images, audio, `n>1`, penalties. Not offered: embeddings, completions, Responses API, tool execution,
structured-output guarantees. This is **not** full OpenAI compatibility.
Usage numbers are the engine's own (`usage` in JSON; final chunk if `include_usage`); never inferred from SSE chunk counts.
Thinking-capable models stream `delta.reasoning_content` (upstream extension, passed through).

Errors: `{"error":{"message","type","code"}}`. 400 validation/`context_length_exceeded`, 401 `missing|invalid|revoked|expired_api_key`,
403 `host_not_allowed`/`insufficient_scope`, 404 unknown model/route, 408 `request_timeout` (body not received within 30 s), 413 `request_too_large` (1 MiB), 415, 429 `rate_limit_exceeded`
(per key, 30/min default) / `queue_full` (+`Retry-After`), 503 `engine_unavailable`/`queue_timeout`/`engine_busy`, 504 deadline/`stream_stalled`.
Mid-stream failures are reported as a final `data: {"error":...}` event followed by `[DONE]`. This includes an upstream stream that ends without `[DONE]` (`engine_error`, "The model stream ended unexpectedly."); clients must treat a stream as complete only after `[DONE]` with no preceding error event.
Request-log outcomes: `ok`, `rejected`, `cancelled` (client left), `deadline`, `idle_timeout`, `engine_error`, `upstream_error`, `truncated` (upstream ended without `[DONE]`), `client_stalled` (client stopped reading for the idle timeout; the slot is released and the response is aborted: the connection closes without the chunked terminator, so the client sees a failed transfer rather than a clean end).
Defaults: 1 active generation, 4 queued, queue wait 30 s, first-byte wait 600 s (streaming), stream idle 120 s, total 3600 s per reply (`serve --request-deadline`).
While waiting for the first token, streams emit `: keepalive` SSE comments every 15 s — clients must ignore comment lines.
A reply cut at its `max_tokens` limit ends with `finish_reason: "length"`; check it rather than assuming the answer is complete.
No HTTP keep-alive: every response carries `Connection: close` and the connection closes after it, so each request opens a new connection.

Retry guidance: retry only 429/503 with `Retry-After`, with backoff; never retry 4xx validation errors; set client timeouts ≥ prefill time
(examples use 600 s). Examples: `examples/` (Python OpenAI SDK, Python stdlib, Node, PowerShell/curl.exe, curl).
