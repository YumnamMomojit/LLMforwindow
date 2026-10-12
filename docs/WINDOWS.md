# Windows setup, packaging, upgrade

Target: Windows 11 x64. Windows 10, ARM64: **untested**. No WSL2 is involved.

## What exists today (checkpoint)
`hubd.exe` (CLI/service), the gateway, key/storage/history/model modules, and the engine supervisor. **No installer and no desktop UI yet**;
the Windows-specific code (Job Object, DPAPI, reparse-point checks, RAM probe) compiles for `x86_64-pc-windows-gnu`
(`cargo check --target`), and its tests are wired in CI, but **nothing has been run on a Windows host by me**.

## Developer prerequisites
Rust ≥ 1.85 (MSVC toolchain), Python 3.10+, Git. For a source build of the UI shell later: Node 20+ and `cargo install tauri-cli --version "^2"`.
```powershell
git clone --branch v2.0.0 https://github.com/JustVugg/colibri third_party\colibri   # verify SHA from docs/UPSTREAM.md
cargo test --workspace
cargo build --release -p hubd
```
## End-user runtime (what an installer must carry — not yet packaged)
`hubd.exe`; Python 3.10+ (embeddable distribution or system); the upstream **Windows release archive** for v2.0.0 (engines `.exe`, `shaders\`, `c\openai_server.py`
and friends, with `LICENSE`/`NOTICE`/`THIRD_PARTY_NOTICES.md`) — **verify its SHA-256 yourself against a value you trust before bundling**; model weights are separate.
WebView2 is needed only for the future Tauri shell. `packaging/build-portable.ps1` assembles a portable folder from those pieces (UNTESTED):
it always rebuilds `hubd.exe`, refuses to package after a failed build, empties its own output folder first, and writes `hub.cmd`, which runs
`hubd.exe` with the bundled Python and Colibri. Unverified: the embeddable Python's `python3XX._pth` may stop `openai_server.py` from
importing its own folder; if `serve` fails with `ModuleNotFoundError`, try a full Python install.

## First run, models
With the portable folder (`C:\hub`), `hub.cmd` supplies `--python` and `--colibri-dir`:
```powershell
C:\hub\hub.cmd init
C:\hub\hub.cmd model validate D:\models\my-model
C:\hub\hub.cmd model add      D:\models\my-model --id my-model
C:\hub\hub.cmd key create "my-platform" --expires-days 90      # copy the secret now
C:\hub\hub.cmd serve --model my-model
```
With a source build, pass `--python C:\path\python.exe --colibri-dir C:\path\colibri` to `model validate`, `model add` and `serve`, or set
`HUB_PYTHON` and `HUB_COLIBRI_DIR` once. Model paths are stored in plain form (`D:\models\my-model`, never `\\?\D:\...`, which the engine
cannot open); a folder that has no plain form (260+ characters, reserved names such as `con`) is refused at `model add`.
Supported = whatever pinned upstream `family_registry` resolves to a text chat family (see AUDIT.md); image and decision families are
rejected at model validation. GGUF is not supported. Smallest candidates per
upstream README: Qwen3-Coder-30B-A3B (19 GB download, RAM 8/18 GB), OLMoE-7B (needs upstream conversion). Downloads are not wired into the Hub
(use upstream `coli setup`, which asks before downloading); the Hub only warns about RAM/disk with labelled estimates.

## Lifecycle
Serving lasts while `hubd serve` runs. Ctrl-C, Ctrl-Break or closing the console window (SIGINT/SIGTERM/SIGHUP on Linux/macOS) stops
tunnel → gateway → engine tree, also during the model load; if Windows ends `hubd` before that finishes (console close allows only a few
seconds), the Job Object still ends the engine tree. Verified on Linux only; the Windows console events and the Job Object are untested.
No Windows service, no autostart, no tray yet. Upgrade = replace `hubd.exe`; `hub.db` migrates forward in a transaction; data and models are left alone.
Uninstall must keep `%LOCALAPPDATA%\LocalAIHub` by default. Signing: none; installers/binaries here are **unsigned** — SmartScreen will warn. Signing needs
a code-signing certificate (OV/EV or Azure Trusted Signing); do not bypass SmartScreen/AV. Offline: after weights + runtime are on disk, local serving needs no network.
Manual updates only; no auto-update in v1.
