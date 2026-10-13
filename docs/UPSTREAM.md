# Upstream pin

| Item | Value |
|---|---|
| Repository | https://github.com/JustVugg/colibri (Apache-2.0, © 2026 Vincenzo Fornaro) |
| Release tag | `v2.0.0` (CHANGELOG dated 2026-10-06) |
| Commit examined | `bf2442915d6e3dd4cdfd2eb9c2a3d2aa44a25850` ("Merge pull request #1966 from JustVugg/dev", 2026-10-06T22:02:24+02:00) |
| Examined on | 2026-10-12, shallow clone (`--depth 1`) |

Semdok Edge is an independent application. It is **not** an official Colibri release and uses a
different name and identifier. The upstream engine is used unmodified; its `LICENSE`, `NOTICE`
and `THIRD_PARTY_NOTICES.md` must ship with any redistributed engine files. The engine licence
(Apache-2.0) is separate from each model's licence (e.g. upstream states Qwen-Image-2.1 is
non-commercial only); Semdok Edge never implies a model licence from the engine licence.

Never build against a moving branch: `git clone --branch v2.0.0 https://github.com/JustVugg/colibri`
then verify `git rev-parse HEAD` equals the SHA above. Upgrading the pin means re-running
`scripts/e2e_real_fixture.sh` and updating this file and `docs/AUDIT.md`.
