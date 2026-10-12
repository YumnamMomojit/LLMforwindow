# Local AI Hub (working name)

Run supported LLMs locally on Windows through the [Colibri](https://github.com/JustVugg/colibri) engine, expose them through a
**restricted, key-authenticated API**, and keep files/history in local workspaces. Independent project — not an official Colibri release
(see `NOTICE`, `docs/UPSTREAM.md`).

## Status: checkpoint, not a finished product
Working and tested (Linux, real Colibri engine on its random-weight fixture + protocol tests): restricted gateway, API keys, limits, SSE,
engine supervision, model validation, storage, history. Implemented but **never run**: the Cloudflare tunnel connector. **Not done:** the
desktop UI, installer, Windows runs, real-model runs, public-internet test. See `docs/VERIFICATION.md` — read it before relying on anything here.

## Try it (Linux/macOS/Git-Bash; Windows equivalents in `docs/WINDOWS.md`)
```sh
git clone --branch v2.0.0 https://github.com/JustVugg/colibri third_party/colibri
cargo build --release -p hubd
export HUB_DATA_DIR=$HOME/.hub; H=target/release/hubd
$H init
$H model add /path/to/model --id my-model --colibri-dir third_party/colibri     # validated, referenced in place
$H key create "my-platform" --expires-days 90                                    # secret shown once
$H serve --model my-model --colibri-dir third_party/colibri                      # local only: http://127.0.0.1:8787/v1
```
Connect a client: base URL `http://127.0.0.1:8787/v1`, model `my-model`, header `Authorization: Bearer lah_...` → `examples/`.
Disable sharing: it is off unless you start with `--share … --i-authorize-public-sharing`. Ctrl-C (also SIGTERM/SIGHUP; on Windows
Ctrl-Break or closing the console) stops tunnel, gateway and engine, also while the model is still loading.
Tests: `cargo test --workspace`; real engine: `COLIBRI_DIR=third_party/colibri scripts/e2e_real_fixture.sh`.

Docs: `AUDIT` · `ARCHITECTURE` · `SECURITY` · `API` · `SHARING` · `STORAGE` · `WINDOWS` · `CONFIG` · `CONTAINERS` · `VERIFICATION` (all in `docs/`).
