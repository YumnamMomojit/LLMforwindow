//! Local-owner commands: workspaces, chat history settings, database backup, and a terminal
//! chat client for the local API. None of this is reachable through the gateway.
use crate::{usage_error, Args};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use hub_core::storage::{WorkspaceInfo, Workspaces};
use hub_core::{db, history};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Writes stop rather than leave less than this free on the workspace volume.
const RESERVE_FREE_BYTES: u64 = 512 << 20;
const MAX_QUOTA_MB: i64 = 16 << 20; // 16 TiB

fn workspaces(dir: &Path) -> Result<Workspaces> {
    let c = db::open(&dir.join("hub.db"))?;
    Ok(Workspaces::new(
        c,
        &dir.join("workspaces"),
        RESERVE_FREE_BYTES,
    )?)
}

/// A workspace by exact id, else by exact name.
fn find(w: &Workspaces, which: &str) -> Result<WorkspaceInfo> {
    let all = w.list_workspaces()?;
    all.iter()
        .find(|i| i.id == which)
        .or_else(|| all.iter().find(|i| i.name == which))
        .cloned()
        .with_context(|| format!("no workspace named or with id '{which}' (hubd workspace list)"))
}

fn mb(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KB", b as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", b as f64 / 1_073_741_824.0),
    }
}

pub fn workspace(dir: &Path, mut args: Args) -> Result<()> {
    let sub =
        args.word("workspace subcommand (list, create, files, put, get, rm, delete, export)")?;
    match sub.as_str() {
        "list" => {
            args.finish()?;
            let w = workspaces(dir)?;
            let all = w.list_workspaces()?;
            if all.is_empty() {
                println!("no workspaces yet: hubd workspace create <name>");
            }
            for i in all {
                let quota = i.quota_bytes.map_or("no quota".into(), mb);
                println!(
                    "{}\t{}\t{} files\t{} used\t{}",
                    i.id,
                    i.name,
                    i.files,
                    mb(i.used_bytes),
                    quota
                );
            }
        }
        "create" => {
            let quota = args
                .value("--quota-mb")?
                .map(|v| crate::bounded("--quota-mb", &v, 1, MAX_QUOTA_MB))
                .transpose()?;
            let name = args.word("workspace name")?;
            args.finish()?;
            let i = workspaces(dir)?.create(&name, quota.map(|q| q as u64 * (1 << 20)))?;
            println!("created workspace {} ({})", i.name, i.id);
        }
        "files" => {
            let which = args.word("workspace")?;
            args.finish()?;
            let w = workspaces(dir)?;
            let i = find(&w, &which)?;
            for f in w.list_files(&i.id)? {
                println!(
                    "{}\t{}\t{}\tsha256:{}",
                    f.rel_path,
                    mb(f.size),
                    f.kind,
                    f.sha256
                );
            }
        }
        "put" => {
            let as_rel = args.value("--as")?;
            let overwrite = args.switch("--overwrite");
            let which = args.word("workspace")?;
            let src = PathBuf::from(args.word("file to import")?);
            args.finish()?;
            let rel = match as_rel {
                Some(r) => r,
                None => src
                    .file_name()
                    .context("the source path has no file name; pass --as NAME")?
                    .to_string_lossy()
                    .into_owned(),
            };
            let mut f = std::fs::File::open(&src)
                .with_context(|| format!("cannot open {}", src.display()))?;
            if !f.metadata()?.is_file() {
                bail!("{} is not a regular file", src.display());
            }
            let mut w = workspaces(dir)?;
            let i = find(&w, &which)?;
            // Stored as data: never extracted, never executed.
            let r = w.write_file(&i.id, &rel, &mut f, "import", overwrite)?;
            println!(
                "stored {} in {} ({}, sha256:{})",
                r.rel_path,
                i.name,
                mb(r.size),
                r.sha256
            );
        }
        "get" => {
            let overwrite = args.switch("--overwrite");
            let which = args.word("workspace")?;
            let rel = args.word("file in the workspace")?;
            let dest = PathBuf::from(args.word("destination file")?);
            args.finish()?;
            let w = workspaces(dir)?;
            let i = find(&w, &which)?;
            let mut src = w.read_file(&i.id, &rel)?;
            let mut out = std::fs::OpenOptions::new();
            out.write(true);
            if overwrite {
                out.create(true).truncate(true);
            } else {
                out.create_new(true);
            }
            let mut out = out.open(&dest).with_context(|| {
                format!("cannot write {} (exists? use --overwrite)", dest.display())
            })?;
            let n = std::io::copy(&mut src, &mut out)?;
            out.sync_all()?;
            println!("wrote {} ({})", dest.display(), mb(n));
        }
        "rm" => {
            let yes = args.switch("--yes");
            let which = args.word("workspace")?;
            let rel = args.word("file in the workspace")?;
            args.finish()?;
            if !yes {
                bail!("deleting '{rel}' cannot be undone; repeat with --yes");
            }
            let w = workspaces(dir)?;
            let i = find(&w, &which)?;
            w.delete_file(&i.id, &rel, true)?;
            println!("deleted {rel} from {}", i.name);
        }
        "delete" => {
            let yes = args.switch("--yes");
            let which = args.word("workspace")?;
            args.finish()?;
            let w = workspaces(dir)?;
            let i = find(&w, &which)?;
            if !yes {
                bail!(
                    "deleting workspace '{}' removes its {} files ({}) and cannot be undone; repeat with --yes",
                    i.name, i.files, mb(i.used_bytes)
                );
            }
            w.delete_workspace(&i.id, true)?;
            println!("deleted workspace {} (not a secure erase)", i.name);
        }
        "export" => {
            let which = args.word("workspace")?;
            let dest = PathBuf::from(args.word("destination .zip")?);
            args.finish()?;
            let w = workspaces(dir)?;
            let i = find(&w, &which)?;
            w.export_workspace(&i.id, &dest)?;
            println!(
                "exported {} ({} files) to {}",
                i.name,
                i.files,
                dest.display()
            );
        }
        _ => return Err(usage_error(format!("unknown workspace subcommand '{sub}'"))),
    }
    Ok(())
}

pub fn backup(dir: &Path, mut args: Args) -> Result<()> {
    let dest = PathBuf::from(args.word("destination file")?);
    args.finish()?;
    workspaces(dir)?.backup_database(&dest)?;
    println!(
        "database snapshot written to {} (API keys and request log removed; workspace files, \
         models and secrets are not included: use `workspace export`)",
        dest.display()
    );
    Ok(())
}

pub fn history_cmd(dir: &Path, mut args: Args) -> Result<()> {
    let sub = args.word("history subcommand (status, on, off, retention, list, show, delete)")?;
    let c = db::open(&dir.join("hub.db"))?;
    match sub.as_str() {
        "status" => {
            args.finish()?;
            println!(
                "history: {}; retention: {}",
                if history::enabled(&c) {
                    "on"
                } else {
                    "off (nothing new is stored)"
                },
                history::retention_days(&c)
                    .map_or("keep until deleted".into(), |d| format!("{d} days"))
            );
        }
        "on" | "off" => {
            args.finish()?;
            history::set_enabled(&c, sub == "on")?;
            println!("history {sub}");
        }
        "retention" => {
            let v = args.word("days, or 'none'")?;
            args.finish()?;
            let days = if v == "none" {
                None
            } else {
                Some(crate::bounded("retention", &v, 1, 36_500)?)
            };
            history::set_retention_days(&c, days)?;
            println!(
                "retention set; removed now: {} conversations",
                history::purge_expired(&c)?
            );
        }
        "list" => {
            args.finish()?;
            for v in history::list_conversations(&c)? {
                println!(
                    "{}\t{}\t{} messages\tupdated {}\t{}",
                    v.id,
                    v.model.unwrap_or_default(),
                    v.messages,
                    v.updated_at,
                    v.title
                );
            }
        }
        "show" => {
            let id = args.word("conversation id")?;
            args.finish()?;
            print!("{}", clean(&history::export_text(&c, &id)?));
        }
        "delete" => {
            let yes = args.switch("--yes");
            let id = args.word("conversation id")?;
            args.finish()?;
            if !yes {
                bail!("deleting conversation {id} cannot be undone; repeat with --yes");
            }
            println!(
                "{}",
                if history::delete_conversation(&c, &id)? {
                    "deleted"
                } else {
                    "no such conversation"
                }
            );
        }
        _ => return Err(usage_error(format!("unknown history subcommand '{sub}'"))),
    }
    Ok(())
}

/// Model output is inert text: drop terminal control sequences (ESC, other C0/C1 controls)
/// so a reply cannot move the cursor, retitle the window or hide text. Keeps \n and \t.
pub(crate) fn clean(s: &str) -> String {
    s.chars()
        .filter(|&ch| ch == '\n' || ch == '\t' || !(ch.is_control()))
        .collect()
}

struct ChatOpts {
    url: String,
    model: Option<String>,
    system: String,
    max_tokens: Option<i64>,
    no_history: bool,
}

/// Parsed, not pattern-matched: `http://127.0.0.1:80@evil.com/` has host evil.com.
fn loopback_http(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    u.scheme() == "http"
        && u.username().is_empty()
        && u.password().is_none()
        && match u.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        }
}

pub async fn chat(dir: &Path, mut args: Args) -> Result<()> {
    let o = ChatOpts {
        url: args
            .value("--url")?
            .unwrap_or_else(|| "http://127.0.0.1:8787/v1".into()),
        model: args.value("--model")?,
        system: args.value("--system")?.unwrap_or_default(),
        max_tokens: args
            .value("--max-tokens")?
            .map(|v| crate::bounded("--max-tokens", &v, 1, 1_000_000))
            .transpose()?,
        no_history: args.switch("--no-history"),
    };
    args.finish()?;
    // Local chat only: a key must not travel over plain HTTP to another machine.
    if !loopback_http(&o.url) {
        bail!("hubd chat talks to the local API only (http://127.0.0.1:<port>/v1); for a remote endpoint use the examples/");
    }
    let base = o.url.trim_end_matches('/').to_string();
    let key = match std::env::var("HUB_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        Some(k) => k.trim().to_string(),
        None => {
            eprint!("API key (lah_..., input is visible; set HUB_API_KEY to skip): ");
            std::io::stderr().flush()?;
            let mut k = String::new();
            std::io::stdin().lock().read_line(&mut k)?;
            k.trim().to_string()
        }
    };
    if !key.starts_with("lah_") {
        bail!("that is not a Local AI Hub key (they start with lah_); create one with `hubd key create <name>`");
    }
    let http = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let model = match o.model.clone() {
        Some(m) => m,
        None => {
            let r = http
                .get(format!("{base}/models"))
                .bearer_auth(&key)
                .send()
                .await
                .with_context(|| format!("cannot reach {base}: is `hubd serve` running?"))?;
            let st = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            if !st.is_success() {
                bail!(
                    "{st}: {}",
                    v["error"]["message"].as_str().unwrap_or("request failed")
                );
            }
            v["data"][0]["id"]
                .as_str()
                .context("the server lists no model")?
                .to_string()
        }
    };
    let mut db_conn = db::open(&dir.join("hub.db"))?;
    let mut conv: Option<String> = None;
    let mut msgs: Vec<Value> = vec![];
    let mut system = o.system.clone();
    let reset = |system: &str, msgs: &mut Vec<Value>| {
        msgs.clear();
        if !system.is_empty() {
            msgs.push(json!({"role": "system", "content": system}));
        }
    };
    reset(&system, &mut msgs);
    let saving = !o.no_history && history::enabled(&db_conn);
    println!(
        "chatting with {model} at {base}. History: {}. Commands: /new, /system TEXT, /exit. Ctrl-C stops a reply.",
        if saving { "saved locally (hubd history off to stop)" } else { "not saved" }
    );
    let stdin = std::io::stdin();
    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        match line.trim() {
            "" => continue,
            "/exit" | "/quit" => break,
            "/new" => {
                conv = None;
                reset(&system, &mut msgs);
                println!("(new conversation)");
                continue;
            }
            t if t.starts_with("/system") => {
                system = t["/system".len()..].trim().to_string();
                conv = None;
                reset(&system, &mut msgs);
                println!("(system prompt set; new conversation)");
                continue;
            }
            t if t.starts_with('/') => {
                println!("commands: /new, /system TEXT, /exit");
                continue;
            }
            _ => {}
        }
        if saving && conv.is_none() {
            let title: String = line.chars().take(60).collect();
            conv = history::create_conversation(&db_conn, &title, &system, Some(&model))?;
            if let Some(c) = &conv {
                if !system.is_empty() {
                    history::append(&mut db_conn, c, "system", &system)?;
                }
            }
        }
        msgs.push(json!({"role": "user", "content": line}));
        let mut body = json!({"model": model, "messages": msgs, "stream": true});
        if let Some(m) = o.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let reply = tokio::select! {
            r = stream_reply(&http, &base, &key, &body) => r,
            _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("stopped")),
        };
        match reply {
            Ok(text) => {
                if let Some(c) = &conv {
                    history::append(&mut db_conn, c, "user", &line)?;
                    history::append(&mut db_conn, c, "assistant", &text)?;
                }
                msgs.push(json!({"role": "assistant", "content": text}));
            }
            Err(e) => {
                msgs.pop(); // the unanswered turn is not part of the conversation
                println!("\n[{e:#}]");
                if format!("{e:#}").contains("context_length_exceeded") {
                    println!(
                        "[the conversation no longer fits the model's context: /new starts over]"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Streams one reply to stdout and returns its text. Thinking (`reasoning_content`) is shown
/// on stderr and not kept in the conversation.
async fn stream_reply(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
) -> Result<String> {
    let r = http
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(body)
        .timeout(Duration::from_secs(3600))
        .send()
        .await
        .context("cannot reach the local API: is `hubd serve` running?")?;
    let st = r.status();
    if !st.is_success() {
        let v: Value = r.json().await.unwrap_or(Value::Null);
        bail!(
            "{st}: {} ({})",
            v["error"]["message"].as_str().unwrap_or("request failed"),
            v["error"]["code"].as_str().unwrap_or("-")
        );
    }
    let (mut buf, mut text, mut out) = (Vec::<u8>::new(), String::new(), std::io::stdout());
    let mut s = r.bytes_stream();
    while let Some(chunk) = s.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
            let ev: Vec<u8> = buf.drain(..i + 2).collect();
            let ev = String::from_utf8_lossy(&ev);
            for line in ev.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                }; // ": keepalive" etc.
                if data == "[DONE]" {
                    println!();
                    return Ok(text);
                }
                let v: Value = serde_json::from_str(data).context("malformed stream event")?;
                if let Some(e) = v.get("error") {
                    bail!(
                        "{} ({})",
                        e["message"].as_str().unwrap_or("error"),
                        e["code"].as_str().unwrap_or("-")
                    );
                }
                let d = &v["choices"][0]["delta"];
                if let Some(t) = d["reasoning_content"].as_str() {
                    eprint!("{}", clean(t));
                }
                if let Some(t) = d["content"].as_str() {
                    text.push_str(t);
                    write!(out, "{}", clean(t))?;
                    out.flush()?;
                }
            }
        }
    }
    bail!("the stream ended without [DONE]; the reply may be incomplete")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_sequences_are_removed() {
        assert_eq!(
            clean("a\x1b]0;pwn\x07b\x1b[2Jc\u{9b}d\r\ne\tf"),
            "a]0;pwnb[2Jcd\ne\tf"
        );
    }

    #[test]
    fn chat_only_talks_to_loopback_http() {
        for ok in [
            "http://127.0.0.1:8787/v1",
            "http://localhost:1/v1",
            "http://[::1]:8787/v1",
        ] {
            assert!(loopback_http(ok), "{ok}");
        }
        for bad in [
            "https://api.example.com/v1",
            "http://192.168.1.5:8787/v1",
            "http://127.0.0.1.evil.com/v1",
            "http://localhost@evil.com/v1",
            "http://127.0.0.1:80@evil.com/v1",
            "http://user:pw@127.0.0.1/v1",
            "ftp://127.0.0.1/",
            "not a url",
        ] {
            assert!(!loopback_http(bad), "{bad}");
        }
    }
}
