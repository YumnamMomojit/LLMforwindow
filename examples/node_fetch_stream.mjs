// Node 18+. Env: HUB_BASE_URL, HUB_API_KEY, HUB_MODEL
const { HUB_BASE_URL, HUB_API_KEY, HUB_MODEL } = process.env;
const res = await fetch(`${HUB_BASE_URL.replace(/\/$/, "")}/chat/completions`, {
  method: "POST",
  headers: { Authorization: `Bearer ${HUB_API_KEY}`, "Content-Type": "application/json" },
  body: JSON.stringify({ model: HUB_MODEL, stream: true, max_tokens: 64,
    messages: [{ role: "user", content: "Say hello." }] }),
  signal: AbortSignal.timeout(600_000), // explicit timeout; abort() also cancels generation
});
if (!res.ok) { console.error(res.status, await res.text(), res.headers.get("retry-after")); process.exit(1); }
const dec = new TextDecoder(); let buf = "";
for await (const chunk of res.body) {
  buf += dec.decode(chunk, { stream: true });
  let i;
  while ((i = buf.indexOf("\n\n")) >= 0) {
    const ev = buf.slice(0, i); buf = buf.slice(i + 2);
    if (ev.startsWith(":") || !ev.startsWith("data: ")) continue; // keepalive comment
    if (ev === "data: [DONE]") process.exit(0);
    const j = JSON.parse(ev.slice(6));
    if (j.error) { console.error(j.error); process.exit(1); }
    process.stdout.write(j.choices?.[0]?.delta?.content ?? "");
  }
}
