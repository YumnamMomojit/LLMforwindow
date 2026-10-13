# Verification table

Every "tested" claim below was made in a Linux x86_64 container (4 cores, 15 GB RAM, no GPU, no Windows, internet only through
package/git proxies, no model weights, no Cloudflare account). Windows behaviour was exercised only by cross-compiling
(`x86_64-pc-windows-gnu`) and running the binary under **Wine 9.0**, which is not Windows. Upstream pinned at `bf2442915d6e…` (v2.0.0).
Dates 2026-10-12/13.

## Test suites
| Suite | What | Result |
|---|---|---|
| `cargo test --workspace` | 175 tests: gateway protocol (mock upstream = *test double*), keys, storage, history, engine lifecycle (fake launchers), CLI arguments, `semdok-edge serve` lifecycle and signals, chat client | 175 passed, 0 failed, 1 ignored, three consecutive runs |
| ignored test `export_handles_files_over_4_gib` | zip64 export of a 4 GiB + 1 file | passed when run in release by a verifier (14 s) |
| `cargo clippy` (Linux and `--target x86_64-pc-windows-gnu`) | all targets | 0 warnings |
| `scripts/e2e_real_fixture.sh` | **real** Colibri engine + upstream random-weight MiMo fixture through semdok-edge and the gateway; curl, Python stdlib, OpenAI SDK 3.29.0, Node fetch, `verify_remote.py`; SIGINT/SIGTERM shutdown; then the same from a folder laid out like the official release zip (built with upstream's `pack_python.py`) | pass; no engine process survives |
| Windows tests under Wine (`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine`, `LANG=C.UTF-8`) | `semdok.cmd` keeps `!` with delayed expansion on; `semdok.cmd check` reaches `semdok-edge check` and finds a nested Colibri from a folder with spaces, `&`, `(x86)` and non-ASCII characters | pass |
| Wine smoke tests | `semdok-edge.exe` init (DPAPI), key create/list/rotate/revoke, strict argument errors, hidden secret prompt; `semdok.cmd check`/passthrough from `C:\Program Files (x86)\Semdok Edge` and from paths with `&` and `!`; storage tests for NTFS case identity and backups | pass under Wine (Wine's `cmd` and case folding differ from Windows in places) |

The fixture's weights are random: its replies are noise. This proves plumbing, not model quality or speed.

## Requirements
| Requirement | Status | Evidence |
|---|---|---|
| Upstream audit and pin | **Done** | `docs/AUDIT.md`, `docs/UPSTREAM.md` |
| Real inference through Semdok Edge | **Partial**: real engine on a random-weight fixture, Linux only. **No real model run**, no Windows run, no GPU | e2e script |
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
| Windows bundle (`make-bundle.sh`): semdok-edge.exe imports only system DLLs; SHA256SUMS; BUILDINFO; third-party licences; reproducible across checkout paths | **Built and checked on one Linux host**; CI bundle job written, **not run on GitHub** | bundle inspection |
| Python and Colibri discovery (`semdok-edge check`): `SEMDOK_PYTHON`, `python\` next to semdok-edge, `py -3`, `python`; old and embeddable Pythons skipped with a reason; UTF-8 path handling | **Tested on Linux** (non-ASCII folder, stand-ins). The `py` launcher and a real Windows Python: **not tested** (none available here) | `gate_setup` |
| GPU (`COLI_VULKAN=1`) | Documented from upstream source, **not tested** (no GPU); CUDA zip **not supported** by Semdok Edge | `docs/WINDOWS.md` |
| Cloudflare tunnel, public HTTPS, separate-network test | **Implemented, never run** (no account/domain); `verify_remote.py` ready | `docs/SHARING.md` |
| Windows install/upgrade/uninstall, signing | **Not done**: zip only, unsigned, no installer | — |
| Desktop UI, tray, autostart | **Not built** | — |
| Model downloads in Semdok Edge | **Deferred**: use upstream `coli setup` | README-FIRST |
| Containers | **Roadmap only** | `docs/CONTAINERS.md` |

## Review rounds
Independent multi-agent reviews, each claim checked by two skeptical verifiers before acting on it:
1. Initial code: 39 claims → 22 confirmed, 9 disputed, 8 refuted. All fixed with regression tests (5 file groups).
2. Check of those fixes: 25/31 fully fixed, 6 partial; 14 new defects introduced by the fixes. Fixed, plus a first review of the new
   CLI/packaging code (about 20 more issues).
3. Check of round 2: 43/45 confirmed fixed; 22 new findings (user-flow and Windows hunters), including a high-severity setup bug
   (embeddable Python breaks the engine) and a pooled-connection race through cloudflared. Fixed; the untested PowerShell
   packaging script was removed.
4. Release gate on round 3: 5 confirmed, 1 disputed, all addressed:
   * high: semdok.cmd read Python's path through `for /f`, which decodes with the console code page, so a profile like
     `C:\Users\José` broke check/validate/serve. Python and Colibri discovery moved into semdok-edge (`-X utf8`, read as UTF-8);
     semdok.cmd is now a thin launcher; `semdok-edge check` replaces the batch-file check.
   * medium: a non-streaming reply was cut by the 600 s first-byte timeout and reported as 502 "engine unreachable"; now only
     the request deadline bounds it (504), whose default rose from 900 s to 3600 s (`serve --request-deadline`).
   * medium: `workspace get ... /dev/stdout > file` let the summary line overwrite the copy; the summary now goes to stderr.
   * low: the hidden-prompt signal handler could stop a background job (SIGTTOU); SIGTTOU is now blocked in the handler.
   * low: the cut-reply note blamed the server limit when the context window ended the reply; it now names both.
   * disputed: README-FIRST's advice for long replies ignored CPU speed and context size; rewritten.

Found outside the reviews while packaging: Semdok Edge originally rejected the official release zip's flat layout (would have failed for
every user of the official download); fixed and covered by tests and the e2e script.

Known, accepted limits: a client that reads one byte at a time keeps its socket until the request deadline; connection floods at
hundreds of thousands per second from the local machine/LAN are out of scope (Cloudflare's edge for internet exposure); exFAT data
directories follow NTFS case rules (rare Greek/Cyrillic edge cases differ); the v3 migration trusts the clock at upgrade time.

## Smallest remaining validation
1. On a Windows 11 PC: unzip the bundle, follow README-FIRST.txt, run `semdok.cmd check`, a real small model, `semdok.cmd chat`, the local API
   with `examples\powershell.ps1`, and `cargo test --workspace` from source (runs the junction test).
2. Cancel a long real generation mid-stream and confirm the engine logs the CANCEL.
3. Create the named tunnel and run `verify_remote.py --from-separate-network` from a phone hotspot, with a prompt long enough to prefill > 100 s.
4. Desktop UI and a signed installer.
