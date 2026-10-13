# Security model

## Boundaries
1. **Public gateway** (`127.0.0.1:8787`, reached via tunnel): inference scope only. `GET /v1/models`,
   `POST /v1/chat/completions`; everything else 404/405 (upstream UI, `/v1/systemone`, `/v1/completions`, Anthropic routes,
   health detail, files, admin). Tested.
2. **Raw engine server**: loopback, ephemeral port, separate random 256-bit internal key delivered via environment (not argv),
   stored with DPAPI. Gateway refuses a non-loopback or non-http upstream. Public keys are never valid upstream; the internal
   key is never valid on the gateway (tested).
3. **Request shaping**: the body is parsed and re-built from an allowlist (model, messages[system|user|assistant, text], stream,
   stream_options.include_usage, max_tokens/max_completion_tokens (clamped), temperature, top_p, stop ≤4, n=1). `tools`,
   `logprobs`, `response_format`, images/audio, `x_colibri_*` and unknown fields → 400. `seed/user/store/metadata/service_tier`
   are dropped (accepted, ignored). Model must equal the served id.
4. **No forwarded-header trust**: `X-Forwarded-*` is ignored; limits are per key, not per IP. Optional `allowed_hosts` Host check.
5. **Keys**: `sem_<id>_<secret>` (keys made before the rename, `lah_<id>_<secret>`, keep working); secret 256-bit CSPRNG, shown once, only SHA-256 stored; expiry/revoke/rotate; scope `inference` only.
   Revocation is a flag and ignores the clock; expiry and rotation grace periods follow the system clock. The upgrade to schema v3
   decided once, by the clock at that moment, which old grace periods had ended (STORAGE.md).
6. **Logs**: time, key id, model, status, outcome, queue/first-byte/total ms, upstream-reported usage. No prompts/completions.
   Engine/tunnel log tails are redacted of the internal key / tunnel token. Retention via `semdok-edge purge` (30 days default for request log).
7. **Filesystem**: all workspace I/O through `storage.rs` (component allowlist, Windows reserved names, ADS `:` rejected, symlink/reparse-point
   ancestors rejected, canonical-prefix check, atomic temp+rename, quota & free-space reserve during write). Uploaded files are never extracted or executed.
8. **Model output is inert text** (history export is plain text). Tool calls are rejected at the gateway. `semdok-edge chat` and
   `semdok-edge history` strip terminal control characters (C0/C1, so no escape sequences) from replies, thinking, server error messages,
   model ids and conversation titles before printing them. `semdok-edge chat` sends its key only to plain-http loopback URLs
   (`localhost` is pinned to 127.0.0.1/::1, no proxy, redirects are never followed); keys and the tunnel token typed at a terminal are not echoed.

## Threat model (honest limits)
* Protects against: remote clients with leaked/revoked/expired/no keys; clients probing admin/upstream routes; oversized/malformed
  input; queue flooding (bounded, 429); path traversal in the storage API; accidental exposure of the raw engine.
* Connection limits keep clients without a key from holding the gateway's connection places (a connection that has not sent a
  request gives way to a new one; every connection carries one request), but they cannot stop a local process, or a LAN peer when
  `allow_non_loopback` is set, that floods new connections at hundreds of thousands per second. Internet exposure relies on
  Cloudflare's edge for flood protection.
* Does **not** protect against: a malicious or compromised Windows administrator or owner account (DPAPI is user-scoped, same user can unwrap);
  malware in the user session; a malicious model/engine build (weights are data, but engine native code is trusted);
  the tunnel provider, which terminates TLS and can read plaintext prompts/outputs; SSD secure erase (deletes are not secure wipes).
* Integrity checks of downloads show bytes match a published hash, not publisher authenticity.
* Tauri hardening (capabilities, CSP) applies to the future desktop shell; it is **not implemented yet**. Upstream's stub CSP allows
  `connect-src http://127.0.0.1:*` and must be tightened when Semdok Edge shell is added.
* Unverified: Job Object behaviour and DPAPI on a real Windows host.
