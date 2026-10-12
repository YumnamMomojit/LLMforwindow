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
| Real clients: curl, Python stdlib, OpenAI SDK 3.29.0, Node, verify_remote | **Tested** against real engine+fixture (output is noise) | `COLIBRI_DIR=... scripts/e2e_real_fixture.sh`. Asserted: every client exits 0; curl lists the model and streams a non-empty delta and `data: [DONE]`; the stdlib and Node examples exit nonzero without `[DONE]` and must print text (the fixture only emits `reasoning_content`, which they show on stderr); the SDK reports `completion_tokens` ≥ 1 for JSON and in the stream's usage chunk; verify_remote passes |
| Engine start/readiness/stop, process-tree cleanup | **Tested on Linux**. Real engine: hubd exits 0 and no engine process survives SIGINT, nor SIGTERM in a second run (e2e script). Fake launcher with a child standing in for the engine: SIGINT/SIGTERM/SIGHUP during the model load and while serving, `--startup-timeout` expiry, port/token/cloudflared errors reported before the engine starts. Windows Job Object and console events: **compile only, not run** | e2e script; `cargo test -p hubd --test cli_serve`; `cargo check --target x86_64-pc-windows-gnu` |
| Actionable engine start errors | **One case tested** (out-of-memory text from a fake launcher → actionable message, exit code 1); the other messages are never triggered by a test or a real engine failure | `cli_serve` |
| CLI arguments: help anywhere, missing/unknown/extra arguments, flags without values, `--expires-days`/`--grace-hours` ranges, empty tunnel token | **Tested** | `cargo test -p hubd --test cli_args` |
| Model validation, bad-folder rejection | **Tested** (fixture dir vs empty dir) | `hubd model validate` |
| Storage: traversal, reserved names, quota-in-write, reserve, overwrite accounting, symlink escape, export/backup, interrupted-write cleanup, migrations | **Tested** Linux | `cargo test -p hub-core --test storage` → 8 passed. Junction test written for Windows CI, **not run** |
| History: no-history, retention, inert text | **Tested** | storage tests |
| DPAPI secrets | **Compiles for Windows; not run**; Linux uses 0600 file (dev) | |
| Cloudflare tunnel | **Implemented, never run** (no account, no cloudflared). cloudflared CLI flags unverified. Only hubd's side is tested, with a fake cloudflared that logs "Registered tunnel connection": `sharing: CONNECTING` → `CONNECTED`, token never printed, connector stopped with hubd | `cli_serve` `sharing_state_is_reported_and_stops_with_hubd` |
| Public HTTPS reachability, separate-network test | **Blocked / unverified** | needs domain+tunnel; `scripts/verify_remote.py` ready |
| Windows paths with spaces/Unicode, fresh Windows install, upgrade, uninstall, installer | **Not done** | no Windows host; no installer exists |
| Desktop UI (Dashboard/Models/Chat/Sharing/Storage/Settings), tray, autostart, Tauri capabilities/CSP | **Not built** (deferred) | upstream stub only |
| Model catalog/downloads/resume/hash check | **Deferred** | use upstream `coli setup` |
| GPU fallback tests, VRAM preflight | **Deferred / untested** | |
| Stop-Sharing-while-chat-continues | Design supports it (`CloudflaredTunnel::stop`); **not exercised** | |
| Containers | **Roadmap doc only** (`docs/CONTAINERS.md`) | |

## Review round 2026-10-12
An independent review made 39 claims; two verifiers checked each: **22 confirmed, 9 disputed** (one verifier said real), **8 refuted**.
Fixed in the CLI, scripts and docs group (fixes in the core modules are listed separately):
* `hubd serve` handled only Ctrl-C, and only once serving: Ctrl-C during the model load, SIGTERM or SIGHUP left the engine running on
  Linux/macOS, and errors after the engine start (port taken, missing token) returned without stopping it. Now stop requests are handled
  from before the engine starts (Unix: SIGINT/SIGTERM/SIGHUP; Windows: Ctrl-C/Ctrl-Break/console close), every exit path stops the engine,
  port, token and cloudflared path are checked before the model load, an engine failure exits nonzero, and `--startup-timeout` exists.
* Model paths were stored as Windows verbatim paths (`\\?\D:\...`), which the C engine cannot open; now stored in plain form (string logic
  unit-tested on Linux; the Windows `canonicalize` test is written, **not run**).
* Docs said image/decision engines are rejected at the gateway, which nothing did; they now say "at model validation", where the core
  fix adds the check.
* `serve --share` printed `LOCAL ONLY` and never the tunnel state, and an empty tunnel token was accepted; the state is now printed and an
  empty token is refused.
* `key create --expires-days` without a value made a never-expiring key; 0, negative and huge values made dead keys or panicked. Now 1..3650.
* Missing arguments panicked; unknown or extra arguments were ignored (`purge --help` purged). Parsing is now strict; help never acts.
* The e2e script asserted nothing for four clients and leaked its temp dir on early failure; the rows above now state what it asserts.
* The portable build script could ship a stale `hubd.exe`, nested folders on re-run, and the bundled Python was unused (still **untested**).

## Smallest remaining validation
1. Windows 11 box: `cargo test --workspace` (runs the junction test) and `scripts\e2e` equivalent with the Windows release engines + a fixture.
2. Load one real small model (e.g. Qwen3-Coder-30B-A3B, 19 GB) and run a real client conversation; then cancel a long generation mid-stream and confirm engine CANCEL in the log.
3. Create the named tunnel, run `verify_remote.py --from-separate-network` from a phone hotspot; check long-prefill streaming past 100 s.
4. Build the Tauri shell screens and installer, then clean-VM install/upgrade/uninstall.
