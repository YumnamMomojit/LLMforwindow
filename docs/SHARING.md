# Sharing over the internet

Default: **Local only**. Nothing is published unless you pass `--share --cloudflared EXE --hostname H --i-authorize-public-sharing`.
The Hub never opens router ports, uses UPnP, or touches Windows Firewall.

| Address | Reachable from |
|---|---|
| `http://127.0.0.1:8787/v1` | this computer only |
| `http://<LAN/VPN ip>:8787/v1` | needs `allow_non_loopback` (not exposed by the CLI yet); same private network/VPN only; hosted platforms cannot reach it |
| `https://<hostname>/v1` via tunnel | the internet, with a key |

## Chosen deployment path: named Cloudflare Tunnel (remotely managed) — NOT live-tested
1. Own a domain on Cloudflare; create a named tunnel in Zero Trust → Networks → Tunnels; add a public hostname
   `api.example.com` → service `http://127.0.0.1:8787`; copy the tunnel token.
2. `echo <token> | hubd secret set tunnel_token` (an empty token is refused); install `cloudflared` yourself (the Hub does not download it).
3. `hubd serve --model ID --colibri-dir C --share --cloudflared C:\path\cloudflared.exe --hostname api.example.com --i-authorize-public-sharing`.
   The token and the cloudflared path are checked before the model loads. hubd then prints the sharing state as it changes:
   `sharing: CONNECTING`, `CONNECTED` once cloudflared logs a registered connection, or `FAILED` (nothing is published, the local API keeps
   running); a warning follows if it is still not connected after 60 s. cloudflared's own log is not shown yet.
4. From another network: `python scripts/verify_remote.py --base-url https://api.example.com/v1 --api-key lah_... --model ID --from-separate-network`
5. Stop Sharing = stop the tunnel process (Ctrl-C ends everything today; a separate Stop button comes with the UI; `CloudflaredTunnel::stop`
   is meant to leave engine+gateway running — not exercised by any test).

Facts to check before relying on it (I could not read Cloudflare's docs directly in this environment; search results indicated):
* Free/Pro/Business zones have a fixed **100 s proxy read timeout** (HTTP 524) for the first response bytes; only Enterprise can raise it.
  The gateway therefore sends response headers immediately for streams, then a `: keepalive` SSE comment every 15 s (`keepalive_secs`;
  the first one 15 s after the request) until the first token. **Non-streaming** requests with
  >100 s prefill will fail via Cloudflare → use `stream:true` for long prompts. Whether comments keep a *tunnel* connection alive was not tested.
* Upload size limits (plans differ) are far above the Hub's 1 MiB body cap.
* Quick Tunnels (`trycloudflare.com`) are random, ephemeral, and documented as not for production; the Hub does not use them.
* Cloudflare terminates TLS and can see prompts and outputs. "Local inference" does not mean data never leaves the machine when shared.
* cloudflared flags `tunnel --no-autoupdate run` + env `TUNNEL_TOKEN` were written from memory of the CLI, **unverified**; connection state is
  inferred from the log line "Registered tunnel connection".
* The host must stay powered on, awake, online and running `hubd`; sharing ends when it stops. No Windows service is installed.
* Alternative (replaceable): any reverse proxy / tunnel that forwards HTTPS to `127.0.0.1:8787`, with SSE buffering disabled.
