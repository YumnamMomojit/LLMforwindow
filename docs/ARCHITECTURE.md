# Architecture and trust boundaries

```
 Desktop UI (future Tauri + web/) ──restricted native commands──▶ semdok-core services
 semdok-core ──argv + env, Job Object──▶ Colibri openai_server.py (127.0.0.1:<ephemeral>, internal key)──▶ C engine
 External client ──HTTPS──▶ tunnel (cloudflared, outbound) ──▶ gateway 127.0.0.1:8787 ──▶ Colibri
 Local files ──▶ Workspaces (approved root only)       Future: ContainerManager (separate, disabled)
```

| Crate / file | Responsibility |
|---|---|
| `semdok-core/gateway.rs` | The only network-reachable surface. Route+method allowlist, key auth, request re-serialisation, bounded queue, rate limit, deadlines, SSE pass-through with keepalives, metadata log. |
| `keys.rs` | Key creation (CSPRNG), SHA-256 verifier, expiry, revoke, rotate, scopes. |
| `engine.rs` / `proc.rs` | `InferenceAdapter` trait; `ColibriAdapter` (validated paths, argv array, secret in env, readiness = `/v1/models` lists the model, actionable failures); Job Object / process group ownership. |
| `models.rs` | Structural checks + upstream `family_registry` via `scripts/validate_model.py` (text chat families only; image/decision families are rejected here); catalog references models in place, never copies/deletes. |
| `storage.rs`, `history.rs`, `db.rs` | Workspaces, quotas, safe paths, export/backup, history with retention; SQLite WAL, versioned migrations. |
| `secrets.rs` | DPAPI (Windows) / 0600 file (dev); internal engine key. |
| `tunnel.rs` | Cloudflare named-tunnel connector, explicit consent type, Stop Sharing. |
| `semdok-edge` | Headless CLI/service used by tests and, later, by the desktop shell. Strict argument parsing; `serve` handles stop signals itself (from before the engine starts) and stops the engine tree on every exit path. |

Single owner: Semdok Edge process owns `hub.db`; the gateway and CLI open it with WAL + busy timeout (CLI key commands are
short transactions). Revocation takes effect on the next request (keys are read per request).

Engine concurrency: `max_active=1` (Colibri serves one generation at a time by default). Requests queue (bounded) and are
never multiplexed onto separate model copies; verified with 5 simultaneous requests against the real engine.
