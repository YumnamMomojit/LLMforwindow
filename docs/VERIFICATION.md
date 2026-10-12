# Verification table

Environment of every "tested" claim: Linux x86_64 container, 4 cores, 15 GB RAM, no GPU, no Windows, no internet beyond package/git proxies,
no model weights, no Cloudflare account/domain. Date 2026-10-12. Upstream pinned at `bf2442915d6e…` (v2.0.0).

| Requirement | Status | Evidence / command |
|---|---|---|
| Upstream audit, pin | **Done** | `docs/AUDIT.md`, `docs/UPSTREAM.md` |
| Native engine proof (A) | **Partial** | `make mimo mimo-tiny-generate` (upstream) then `openai_server.py` served `/v1/models`, JSON and SSE chat on the **random-weight MiMo fixture**. Output is noise: proves plumbing, **not** model quality or performance. Linux only. No real model, no Windows, no GPU. |
| Gateway protocol: auth matrix, revoked/expired, internal key rejected, route allowlist, 405s | **Tested** (mock upstream = *test double*) | `cargo test -p hub-core --test gateway` → 12 passed |
| Request sanitising, clamp, 413, 415, upstream error mapping | **Tested** (mock) | same |
| Incremental SSE, `[DONE]`, keepalive comments | **Tested** (mock) and **real engine** (48 events, `verify_remote.py` dry run) | same + `scripts/e2e_real_fixture.sh` |
| Client disconnect frees slot & drops upstream stream | **Tested** (mock). **Real engine: not proven** — fixture finished before the 1 s cut | gateway test `client_disconnect_cancels_upstream_and_frees_slot` |
| Bounded queue, no output crossover, per-key rate limit | **Tested** (mock); queue/429/serialisation also observed with 6 parallel curl clients on the real engine (1×429, 5×200) | gateway tests; manual run |
| Verified usage logged, prompts not logged | **Tested** | `usage_and_metadata_logging_without_prompts` |
| Real clients: curl, Python stdlib, OpenAI SDK 3.29.0, Node, verify_remote | **Tested** against real engine+fixture | `COLIBRI_DIR=... scripts/e2e_real_fixture.sh` → all steps ran, `PASS: no orphans` |
| Engine start/readiness/stop, actionable errors, process-tree cleanup | **Tested on Linux** (no orphans after SIGINT). Windows Job Object: **compiles, not run** | e2e script; `cargo check --target x86_64-pc-windows-gnu` OK |
| Model validation, bad-folder rejection | **Tested** (fixture dir vs empty dir) | `hubd model validate` |
| Storage: traversal, reserved names, quota-in-write, reserve, overwrite accounting, symlink escape, export/backup, interrupted-write cleanup, migrations | **Tested** Linux | `cargo test -p hub-core --test storage` → 8 passed. Junction test written for Windows CI, **not run** |
| History: no-history, retention, inert text | **Tested** | storage tests |
| DPAPI secrets | **Compiles for Windows; not run**; Linux uses 0600 file (dev) | |
| Cloudflare tunnel | **Implemented, never run** (no account, no cloudflared). cloudflared CLI flags unverified | |
| Public HTTPS reachability, separate-network test | **Blocked / unverified** | needs domain+tunnel; `scripts/verify_remote.py` ready |
| Windows paths with spaces/Unicode, fresh Windows install, upgrade, uninstall, installer | **Not done** | no Windows host; no installer exists |
| Desktop UI (Dashboard/Models/Chat/Sharing/Storage/Settings), tray, autostart, Tauri capabilities/CSP | **Not built** (deferred) | upstream stub only |
| Model catalog/downloads/resume/hash check | **Deferred** | use upstream `coli setup` |
| GPU fallback tests, VRAM preflight | **Deferred / untested** | |
| Stop-Sharing-while-chat-continues | Design supports it (`CloudflaredTunnel::stop`); **not exercised** | |
| Containers | **Roadmap doc only** (`docs/CONTAINERS.md`) | |

## Smallest remaining validation
1. Windows 11 box: `cargo test --workspace` (runs the junction test) and `scripts\e2e` equivalent with the Windows release engines + a fixture.
2. Load one real small model (e.g. Qwen3-Coder-30B-A3B, 19 GB) and run a real client conversation; then cancel a long generation mid-stream and confirm engine CANCEL in the log.
3. Create the named tunnel, run `verify_remote.py --from-separate-network` from a phone hotspot; check long-prefill streaming past 100 s.
4. Build the Tauri shell screens and installer, then clean-VM install/upgrade/uninstall.
