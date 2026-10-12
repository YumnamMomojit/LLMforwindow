# Configuration schema

`hubd` takes its settings from flags today; there is **no config-file loader yet**. The schema below is the `GatewayConfig` struct
(`crates/hub-core/src/gateway.rs`, serde-ready JSON) that a future settings screen/file will persist.

| Field | Default | Meaning |
|---|---|---|
| listen | 127.0.0.1:8787 | non-loopback needs allow_non_loopback |
| upstream | http://127.0.0.1:18080 | must be http + loopback (set automatically by `serve`) |
| model_id | — | only id served/accepted |
| max_body_bytes / max_messages | 1 MiB / 256 | |
| max_active / max_queue | 1 / 4 | simultaneous generations / waiters |
| queue_timeout_secs / request_deadline_secs | 30 / 900 | |
| first_byte_timeout_secs / stream_idle_timeout_secs / keepalive_secs | 600 / 120 / 15 | |
| max_output_tokens / default_output_tokens | 1024 / 256 | |
| rate_limit_per_min | 30 | per key; 0 = off |
| allowed_hosts | [] | Host allowlist; empty = any |

`hubd serve` flags: `--port` (8787), `--max-active` (1..16, default 1), `--startup-timeout` (seconds to wait for the engine to become ready,
1..86400, default 900), `--share` group. Data dir: `--data-dir` / `HUB_DATA_DIR`. Engine runtime: `--python` / `HUB_PYTHON` (else `python3`,
`python.exe` on Windows, from PATH) and `--colibri-dir` / `HUB_COLIBRI_DIR`; a flag wins over the variable. Unknown flags, extra arguments
and flags without a value are errors. Settings in DB: `history.enabled`, `history.retention_days`.
