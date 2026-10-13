# Windows setup, packaging, upgrade

Target: Windows 11 x64. Windows 10, ARM64: **untested**. No WSL2 is involved.

## What exists today (checkpoint)
`hubd.exe` (CLI/service), the gateway, key/storage/history/model modules, and the engine supervisor, shipped as a zip bundle with a
`hub.cmd` launcher. **No installer and no desktop UI yet**; the Windows-specific code (Job Object, DPAPI, reparse-point checks, RAM probe)
compiles for `x86_64-pc-windows-gnu`, and its tests are wired in CI, but **nothing has been run on a Windows host by me**. `hub.cmd` was run
under Wine's `cmd` only.

## Developer prerequisites
Rust ≥ 1.85 (MSVC toolchain), Python 3.10+, Git. For a source build of the UI shell later: Node 20+ and `cargo install tauri-cli --version "^2"`.
```powershell
git clone --branch v2.0.0 https://github.com/JustVugg/colibri third_party\colibri   # verify SHA from docs/UPSTREAM.md
cargo test --workspace
cargo build --release -p hubd
```

## The download bundle (`packaging/make-bundle.sh`)
The bundle is built on Linux, not on Windows: `packaging/make-bundle.sh` (needs `rustup target add x86_64-pc-windows-gnu`,
`x86_64-w64-mingw32-gcc`, `cargo install cargo-about --locked --features cli`, and a clean, committed tree) writes
`dist/LocalAIHub-<ver>-windows-x64.zip`, `dist/LocalAIHub-<ver>-linux-x64.tar.gz` and their `SHA256SUMS.txt`. CI builds the same zip
(job `bundle`) and keeps it as a workflow artifact. The Windows zip holds one folder with the cross-compiled `hubd.exe`, `hub.cmd`,
`README-FIRST.txt` (the end-user guide), `docs\`, `examples\`, `scripts\`, `LICENSE`, `NOTICE`, `THIRD-PARTY-LICENSES.txt` (cargo-about),
`BUILDINFO.txt` (commit, toolchain, target) and `SHA256SUMS.txt`, plus an empty `colibri\` folder. Everything is **unsigned**.

* **Not bundled**: Python and the Colibri engine. The user installs Python 3.10+ from python.org and unzips the official Colibri v2.0.0
  Windows release (engines `.exe`, `shaders\`, `openai_server.py` and the Python it imports, flat at the archive root, with its own
  `LICENSE`/`NOTICE`/`THIRD_PARTY_NOTICES.md`) into `colibri\`, after checking its SHA-256 against the release page. Model weights are separate.
* **The embeddable Python does not work**: its `python3XX._pth` file keeps the script's own folder off `sys.path`, so `openai_server.py`
  cannot import the modules next to it and `serve` fails with `ModuleNotFoundError` (reproduced on Linux with a `._pth` file; not run on
  Windows). `hub.cmd` therefore skips a `python\` folder next to it that holds a `._pth` file, and `hub.cmd check` fails for any Python
  that does not put a script's folder on `sys.path` (a `._pth` file, or `PYTHONSAFEPATH`).
* **The binary**: the script always relinks `hubd` and copies the executable cargo reports for that build, wherever the target directory
  is (`CARGO_TARGET_DIR`, `build.target-dir`); it stops if that file is missing or older than the build, so a leftover binary is never shipped.
* **Reproducible** for the same commit and toolchain (rustc, mingw-w64): dates come from the commit, the checkout, target, cargo and
  rustup paths are remapped, the Windows linker writes no timestamp, the Windows symbol table is stripped (it held the target path and
  rustc temp-dir names from the `windows-sys`/`getrandom`/`parking_lot_core` import libraries), and archive entries are ordered and stamped.
  Checked on one Linux host (rustc 1.97.0, mingw-w64 gcc 13): two clones at different paths, one with its own `CARGO_TARGET_DIR` and
  umask 002, and a rebuild after `cargo clean -p getrandom -p parking_lot_core -p windows-sys`, gave byte-identical zips and tarballs.
  Another host or toolchain version has not been compared.

`hub.cmd` only runs `hubd.exe` with your arguments (delayed expansion off, so a `!` in an argument survives). `hubd` itself finds
Python, in this order: `--python`/`HUB_PYTHON`, `python\python.exe` next to hubd.exe, the `py -3` launcher, `python` on PATH. A
candidate is used only if it is 3.10 or newer and imports a script's own folder (the embeddable package's `._pth` file prevents
that, and the engine would not start). Python is run with `-X utf8` and answers in UTF-8, so a profile folder like
`C:\Users\José` comes back intact (an earlier `for /f` in hub.cmd decoded it with the console code page and garbled it).
Colibri: `--colibri-dir`/`HUB_COLIBRI_DIR`, else `colibri\` next to hubd.exe (flat release layout or `c\`), or one folder level
below it. `hub.cmd check` (= `hubd check`) shows what was found and why anything was skipped; it exits 1 until both are there.
Tested on Linux (non-ASCII folder, stand-ins for old and embeddable Pythons) and under Wine's `cmd` from a folder with spaces,
`&`, `(x86)` and non-ASCII characters; not on Windows.

## First run, models
From the unzipped bundle folder, in a Command Prompt (README-FIRST.txt has the full walk-through):
```bat
hub.cmd check
colibri\coli.cmd setup --model qwen3-coder-30b --dir "%USERPROFILE%\colibri-models" --no-start
hub.cmd init
hub.cmd model validate "%USERPROFILE%\colibri-models\qwen3-coder-30b"
hub.cmd model add      "%USERPROFILE%\colibri-models\qwen3-coder-30b" --id my-model
hub.cmd key create "my-platform" --expires-days 90
hub.cmd serve --model my-model
```
`key create` shows the secret once: copy it then. With a source build, pass `--python C:\path\python.exe --colibri-dir C:\path\colibri` to `model validate`, `model add` and `serve`, or set
`HUB_PYTHON` and `HUB_COLIBRI_DIR` once. Model paths are stored in plain form (`C:\Users\me\colibri-models\m`, never `\\?\C:\...`, which
the engine cannot open); a folder that has no plain form (260+ characters, reserved names such as `con`) is refused at `model add`.
Supported = whatever pinned upstream `family_registry` resolves to a text chat family (see AUDIT.md); image and decision families are
rejected at model validation. GGUF is not supported. Smallest candidates per
upstream README: Qwen3-Coder-30B-A3B (19 GB download, RAM 8/18 GB), OLMoE-7B (needs upstream conversion). Downloads are not wired into the Hub;
use upstream `coli setup`. `coli setup --list` only lists the catalog with sizes and what fits; `coli setup --model ID` checks the PC and then
starts the download **without asking** (Ctrl-C interrupts, the same command resumes), and `--no-start` stops it from starting Colibri's own
server afterwards. The Hub itself only warns about RAM/disk with labelled estimates.

Reply length: `serve --max-output-tokens N` (1..32768, default 4096) is both the engine's `--max-tokens` and the gateway's cap on a
request's `max_tokens`; a request that sets none gets 1024 (or N, if lower). `hub.cmd chat --max-tokens N` asks for more than 1024.

## GPU
The Colibri Windows release builds every engine with its Vulkan backend, but an engine uses it only when `COLI_VULKAN=1` is in its
environment (`colibri.c`, `qwen36.c` and the other engines read it at start; shaders are found in `shaders\` next to the engine).
`coli start` sets it after `coli setup` picked Vulkan; the Hub does not run that launcher and does not set it. hubd passes its own
environment on to `openai_server.py`, which passes it to the engine, so this turns the GPU on (from the source; **not tested** with a GPU):
```bat
set COLI_VULKAN=1
hub.cmd serve --model my-model
```
Without a usable Vulkan device most engines print a `[VK]` line and continue on the CPU; the GLM engine (`colibri.exe`) exits instead, and
`serve` then fails with that line in the engine log: serve again without the variable. Without it, everything runs on the CPU.
CUDA (the separate `-cuda.zip`) is **not supported** by the Hub in this version: upstream's launcher sets `COLI_CUDA`/`COLI_GPUS` and sizes the
VRAM tier from its resource planner, which the Hub does not run.

## Lifecycle
Serving lasts while `hubd serve` runs. Ctrl-C, Ctrl-Break or closing the console window (SIGINT/SIGTERM/SIGHUP on Linux/macOS; a SIGHUP
already ignored at start, as under `nohup`, stays ignored) stops tunnel → gateway → engine tree, also during the model load; if Windows ends `hubd` before that finishes (console close allows only a few
seconds), the Job Object still ends the engine tree. Verified on Linux only; the Windows console events and the Job Object are untested.
No Windows service, no autostart, no tray yet. Upgrade = unzip the new bundle and move `colibri\` over (or replace `hubd.exe`); `hub.db`
migrates forward in a transaction (see STORAGE.md for the one clock assumption); data and models are left alone.
Uninstall must keep `%LOCALAPPDATA%\LocalAIHub` by default. Signing: none; installers/binaries here are **unsigned** — SmartScreen will warn. Signing needs
a code-signing certificate (OV/EV or Azure Trusted Signing); do not bypass SmartScreen/AV. Offline: after weights + runtime are on disk, local serving needs no network.
Manual updates only; no auto-update in v1.
