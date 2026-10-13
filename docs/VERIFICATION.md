# Verification table

Every "tested" claim below was made in a Linux x86_64 container (4 cores, 15 GB RAM, no GPU, no Windows, internet only through
package/git proxies, no model weights, no Cloudflare account). Windows behaviour was exercised only by cross-compiling
(`x86_64-pc-windows-gnu`) and running the binary under **Wine 9.0**, which is not Windows. Upstream pinned at `bf2442915d6e…` (v2.0.0).
Dates 2026-10-12/13.

## Test suites
| Suite | What | Result |
|---|---|---|
| `cargo test --workspace` | 168 tests: gateway protocol (mock upstream = *test double*), keys, storage, history, engine lifecycle (fake launchers), CLI arguments, `hubd serve` lifecycle and signals, chat client | 168 passed, 0 failed, 1 ignored, three consecutive runs |
| ignored test `export_handles_files_over_4_gib` | zip64 export of a 4 GiB + 1 file | passed when run in release by a verifier (14 s) |
| `cargo clippy` (Linux and `--target x86_64-pc-windows-gnu`) | all targets | 0 warnings |
| `scripts/e2e_real_fixture.sh` | **real** Colibri engine + upstream random-weight MiMo fixture through hubd and the gateway; curl, Python stdlib, OpenAI SDK 3.29.0, Node fetch, `verify_remote.py`; SIGINT/SIGTERM shutdown; then the same from a folder laid out like the official release zip (built with upstream's `pack_python.py`) | pass; no engine process survives |
| Wine smoke tests | `hubd.exe` init (DPAPI), key create/list/rotate/revoke, strict argument errors, hidden secret prompt; `hub.cmd check`/passthrough from `C:\Program Files (x86)\Local AI Hub` and from paths with `&` and `!`; storage tests for NTFS case identity and backups | pass under Wine (Wine's `cmd` and case folding differ from Windows in places) |

The fixture's weights are random: its replies are noise. This proves plumbing, not model quality or speed.

## Requirements
| Requirement | Status | Evidence |
|---|---|---|
| Upstream audit and pin | **Done** | `docs/AUDIT.md`, `docs/UPSTREAM.md` |
| Real inference through the Hub | **Partial**: real engine on a random-weight fixture, Linux only. **No real model run**, no Windows run, no GPU | e2e script |
| Official release-zip layout (flat, no `c\`) | **Tested** (pack_python.py-built folder) | e2e script, `engine_layout` tests |
| Model validation (upstream `family_registry`; text-chat families only; GGUF and image/decision models rejected) | **Tested** | `engine_models`, manual |
| Engine start/readiness/stop, tree cleanup, startup timeout, death detection, actionable errors | **Tested on Linux** (process groups, PR_SET_PDEATHSIG). Windows Job Object: **compiles, not run on Windows** | `engine_*`, `cli_serve`, e2e |
| Gateway: route allowlist, auth (missing/invalid/revoked/expired, internal key refused), request re-serialisation, limits (body, rate, queue, deadlines, first-byte, idle, keepalive comments), truncation and stalled-client handling, header-read timeout, connection cap with eviction of request-less connections, no keep-alive | **Tested** (mock upstream) | `gateway` tests (raw-TCP probes included) |
| Incremental SSE, `[DONE]`, verified usage, no prompt logging | **Tested** (mock and real engine) | `gateway`, e2e |
| Client disconnect cancels generation | **Tested with the mock** (upstream connection dropped, slot freed). **Real engine: not proven** (the fixture answers before a client can leave) | `client_disconnect_cancels_upstream_and_frees_slot` |
| Simultaneous clients, no crossover | **Tested** (mock); queue/429 observed with 6 parallel clients on the real engine | `bounded_queue_serialises_without_crossover` |
| API keys: CSPRNG, verifier only, scopes, expiry, rotation grace, clock-independent revocation (schema v3) | **Tested** | `keys_regressions`, `gateway` |
| Secrets: DPAPI on Windows, 0600 file elsewhere | DPAPI **ran under Wine**, not on Windows | Wine smoke test |
| Storage: confinement, traversal, reserved/8.3/ADS names, NTFS `$UpCase` identity (checked byte-for-byte against ntfs-3g's Win7+ table), quota during writes, free-space reserve, atomic crash-safe overwrite (kill -9 tested 250x by a verifier), disk-first deletes, atomic export (zip64) and redacted backup | **Tested on Linux**; junction escape test is Windows-only, **not run** | `storage*` tests |
| History: on/off, retention, inert text | **Tested** | `storage`, CLI |
| CLI: strict arguments, help, workspace/history/backup/chat commands, nohup-safe, Ctrl-C everywhere in chat, hidden secret prompts | **Tested** (Linux; prompts also under Wine) | `cli_args`, `cli_serve`, `cli2_*`, `cli3_*`, `packaging3_*` |
| Windows bundle (`make-bundle.sh`): hubd.exe imports only system DLLs; SHA256SUMS; BUILDINFO; third-party licences; reproducible across checkout paths | **Built and checked on one Linux host**; CI bundle job written, **not run on GitHub** | bundle inspection |
| `hub.cmd` Python detection via `py`/`python` | **Not tested** (no Windows Python available here) | — |
| GPU (`COLI_VULKAN=1`) | Documented from upstream source, **not tested** (no GPU); CUDA zip **not supported** by the Hub | `docs/WINDOWS.md` |
| Cloudflare tunnel, public HTTPS, separate-network test | **Implemented, never run** (no account/domain); `verify_remote.py` ready | `docs/SHARING.md` |
| Windows install/upgrade/uninstall, signing | **Not done**: zip only, unsigned, no installer | — |
| Desktop UI, tray, autostart | **Not built** | — |
| Model downloads in the Hub | **Deferred**: use upstream `coli setup` | README-FIRST |
| Containers | **Roadmap only** | `docs/CONTAINERS.md` |

## Review rounds
Independent multi-agent reviews, each claim checked by two skeptical verifiers before acting on it:
1. Initial code: 39 claims → 22 confirmed, 9 disputed, 8 refuted. All fixed with regression tests (5 file groups).
2. Check of those fixes: 25/31 fully fixed, 6 partial; 14 new defects introduced by the fixes. Fixed, plus a first review of the new
   CLI/packaging code (about 20 more issues).
3. Check of round 2: 43/45 confirmed fixed; 22 new findings (user-flow and Windows hunters), including a high-severity setup bug
   (embeddable Python breaks the engine) and a pooled-connection race through cloudflared. Fixed; the untested PowerShell
   packaging script was removed.
4. Release gate on round 3: see the end of this file.

Found outside the reviews while packaging: the Hub originally rejected the official release zip's flat layout (would have failed for
every user of the official download); fixed and covered by tests and the e2e script.

Known, accepted limits: a client that reads one byte at a time keeps its socket until the request deadline; connection floods at
hundreds of thousands per second from the local machine/LAN are out of scope (Cloudflare's edge for internet exposure); exFAT data
directories follow NTFS case rules (rare Greek/Cyrillic edge cases differ); the v3 migration trusts the clock at upgrade time.

## Smallest remaining validation
1. On a Windows 11 PC: unzip the bundle, follow README-FIRST.txt, run `hub.cmd check`, a real small model, `hub.cmd chat`, the local API
   with `examples\powershell.ps1`, and `cargo test --workspace` from source (runs the junction test).
2. Cancel a long real generation mid-stream and confirm the engine logs the CANCEL.
3. Create the named tunnel and run `verify_remote.py --from-separate-network` from a phone hotspot, with a prompt long enough to prefill > 100 s.
4. Desktop UI and a signed installer.
