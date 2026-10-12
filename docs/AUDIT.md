# Repository audit (upstream Colibri v2.0.0, `bf24429`)

Everything below was read from the checked-out source or exercised, not taken from marketing text.
Status legend: **Impl** = merged and used as-is; **Reuse** = usable with a wrapper; **Missing**;
**Unverified** = not exercised by me.

## Findings that shaped the design

* The checkout of *this* repository was empty; upstream was cloned read-only for the audit.
* `desktop/` is an **11-line Tauri v2 stub** (`lib.rs`+`main.rs`), packaging `web/` in a window. Its own README says it
  does not start the engine, manage processes, download models, or have filesystem/process permissions. No capability
  beyond the default window exists. `web/` is a real React 18 / TypeScript / Vite / Tailwind app.
* The serving stack is **Python 3.10+ (stdlib only)** `c/openai_server.py` (8 325 lines) driving native C engines over a
  stdin/stdout protocol (`docs/serve_protocol.md`). The launcher `c/coli` (3 700 lines) is Python too. So **a Python
  runtime is a runtime dependency**, not only the C engine. Windows release archives ship `.exe` engines (Vulkan,
  shaders in `shaders\`), `coli.cmd`, and need Python on PATH.
* The upstream server already has: bearer key (`COLI_API_KEY`, constant-time compare), Host-header (DNS-rebinding) guard,
  4 MiB body cap, single-`Content-Length` framing, bounded queue (`--max-queue`, `--queue-timeout`), cancellation on
  client disconnect (CANCEL sent to engine), SSE, usage counts, refuses non-loopback bind without a key. It is **one
  shared key, no scopes, no per-key limits, and ~dozens of routes** (UI, systemone, completions, Anthropic-style, ...), and
  CORS defaults/`*` are configurable. It must not be exposed directly → the Hub's restricted gateway.
* Layout: the release archives (release.yml "Package" step + `c/tools/pack_python.py`) put `openai_server.py`, its import
  closure (incl. `family_registry.py`) and the engines **flat at the archive root**; a source checkout keeps them in `c/`. The Hub
  accepts both (`engine::server_script`, `scripts/validate_model.py`); the first build only accepted `c/` and failed on the
  release layout with `ModuleNotFoundError: family_registry` (found while packaging, fixed, tested with a pack_python.py-built folder).
* The server serialises generation by default (`--kv-slots 1`; 2.0.0 adds up to 16 slots). The Hub keeps 1 active generation.

## Capability matrix

| Area | Status | Notes |
|---|---|---|
| OpenAI-style `GET /v1/models`, `POST /v1/chat/completions`, SSE, usage | **Impl** (upstream) | verified live against fixture (see VERIFICATION) |
| Engine families | **Impl** | `family_registry`: glm53, glm, inkling, kimi, olmoe, qwen36 (+Qwen3-Coder, Qwen3.8-27B), qwen38, deepseek_v4, deepseek_v41, mimo, qwen_image (image), laya, gliner_decide (decision). Hub chat supports **text chat families only**; image/decision families are rejected at model validation (`hubd model validate`/`add`) by design. |
| Model format | **Impl** | HF-style dir: `config.json`, `tokenizer.json`, safetensors shards; some families need an upstream conversion/prep step ("By hand" table). **No GGUF serving** — not assumed. |
| CPU backend | **Impl** | pure C; no GPU needed |
| Vulkan backend | **Impl, Unverified here** | in Windows release; runtime-loaded; falls back to CPU without Vulkan |
| CUDA | **Reuse, Unverified** | separate `-cuda.zip` for RTX 30/40/50 (CC ≥ 8.0) |
| AMD XDNA NPU | Opt-in, Unverified | not used by Hub |
| Windows launcher | **Reuse** | `START-HERE.bat`/`coli setup`: interactive, may offer winget Python install, builds with MSYS2 if present. The Hub does **not** run it; it launches `openai_server.py` directly with an argv array. |
| Model discovery/download | **Reuse, not wired** | `coli setup`/`setup_catalog.py` (HF resume). Hub v0 imports existing dirs; download UI deferred. |
| Smallest chat model | — | per upstream README: Qwen3-Coder-30B-A3B 19 GB download (RAM 8/18 GB); OLMoE 7B is "by hand", 14 GB → 7 GB. Not tested here (no weights, no HF access). |
| Auth / scopes / per-key limits | **Missing upstream** | implemented in Hub gateway |
| Tauri shell + web UI | **Impl (stub shell)** | no native commands; Hub screens (Dashboard/Models/Chat/Sharing/Storage/Settings) **not built** |
| Job-object process ownership, DPAPI, tunnels, storage, history | **Missing upstream** | implemented in `hub-core` (Windows parts compile-checked only) |
| Release workflow | **Impl** | `.github/workflows/release.yml` builds engines with MSYS2/UCRT64 on `windows-latest` |
| Open issues/PRs | not reviewed | no GitHub API access to the upstream repo from this session; only merged code at the pinned commit was used |

## Dependencies for end users (Windows)
Python 3.10+ (bundle an embeddable distribution or require it), the upstream Windows release engines + `shaders\`,
WebView2 (for the future Tauri shell; ships with Windows 11, Evergreen installer otherwise), a Vulkan-capable driver for GPU (optional).
End users need no Git, Rust, Node or compiler.
