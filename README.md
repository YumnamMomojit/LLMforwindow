# Semdok Edge

Run supported LLMs locally on Windows through the [Colibri](https://github.com/JustVugg/colibri) engine, expose them through a
**restricted, key-authenticated API**, and keep files/history in local workspaces. Independent project — not an official Colibri release
(see `NOTICE`, `docs/UPSTREAM.md`).

## Status: checkpoint, not a finished product
Working and tested (Linux, real Colibri engine on its random-weight fixture + protocol tests): restricted gateway, API keys, limits, SSE,
engine supervision, model validation, storage, history. Implemented but **never run**: the Cloudflare tunnel connector. **Not done:** the
desktop UI, installer, Windows runs, real-model runs, public-internet test. See `docs/VERIFICATION.md` — read it before relying on anything here.

## Windows download
`packaging/make-bundle.sh` (run on Linux; CI job `bundle` runs it too) builds `dist/SemdokEdge-<ver>-windows-x64.zip`: a cross-compiled,
unsigned `semdok-edge.exe`, the `semdok.cmd` launcher and `README-FIRST.txt`, the step-by-step guide for end users. Python (a full python.org install)
and the official Colibri v2.0.0 Windows release are added by the user; nothing is bundled from upstream. Details: `docs/WINDOWS.md`.

## Try it (Linux/macOS/Git-Bash; Windows equivalents in `docs/WINDOWS.md`)
```sh
git clone --branch v2.0.0 https://github.com/JustVugg/colibri third_party/colibri
cargo build --release -p semdok-edge
export SEMDOK_DATA_DIR=$HOME/.semdok-edge; H=target/release/semdok-edge
$H init
$H model add /path/to/model --id my-model --colibri-dir third_party/colibri     # validated, referenced in place
$H key create "my-platform" --expires-days 90                                    # secret shown once
$H serve --model my-model --colibri-dir third_party/colibri                      # local only: http://127.0.0.1:8787/v1
```
Replies: at most 4096 tokens (`serve --max-output-tokens N`, 1..32768); a request without `max_tokens` gets 1024. The API closes each
connection after its response (no HTTP keep-alive). Semdok Edge turns no GPU on: the engine runs on the CPU unless you set Colibri's own
variable before `serve`, e.g. `COLI_VULKAN=1` for the Vulkan engines of the release archives (`docs/WINDOWS.md`, GPU).
Connect a client: base URL `http://127.0.0.1:8787/v1`, model `my-model`, header `Authorization: Bearer sem_...` → `examples/`.
Or chat in a second terminal: `$H chat` (asks for the key without echoing it, or reads `SEMDOK_API_KEY`; `--max-tokens N` for longer replies; Ctrl-C stops a reply, at the prompt it ends the chat).
Disable sharing: it is off unless you start with `--share … --i-authorize-public-sharing`. Ctrl-C (also SIGTERM/SIGHUP; on Windows
Ctrl-Break or closing the console) stops tunnel, gateway and engine, also while the model is still loading. Started with
`nohup $H serve … &`, semdok-edge keeps the ignored SIGHUP ignored, so serving survives logout; stop it with SIGINT or SIGTERM.
Tests: `cargo test --workspace`; real engine: `COLIBRI_DIR=third_party/colibri bash scripts/e2e_real_fixture.sh`.

Docs: `AUDIT` · `ARCHITECTURE` · `SECURITY` · `API` · `SHARING` · `STORAGE` · `WINDOWS` · `CONFIG` · `CONTAINERS` · `VERIFICATION` (all in `docs/`).
