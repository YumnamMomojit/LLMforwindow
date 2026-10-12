//! hubd - headless Local AI Hub service/CLI. The desktop shell calls the same hub-core APIs.
use anyhow::{bail, Context, Result};
use hub_core::engine::{ColibriAdapter, ColibriConfig, EngineState, InferenceAdapter};
use hub_core::gateway::{Gateway, GatewayConfig};
use hub_core::secrets::{self, FileSecrets, SecretStore};
use hub_core::tunnel::{CloudflaredTunnel, PublicSharingConsent};
use hub_core::{db, history, keys, models};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "hubd - Local AI Hub

USAGE: hubd [--data-dir DIR] <command>

  init                                   create data folders and database
  model validate <dir> --colibri-dir C [--python P]
  model add <dir> --id ID --colibri-dir C [--python P]    (references in place; no copy)
  model list | model remove <id>        (catalog entry only; files are never deleted)
  key create <name> [--expires-days N]  (secret shown ONCE)
  key list | key revoke <id> | key rotate <id> [--grace-hours N]
  secret set tunnel_token               (reads the value from stdin)
  serve --model ID --colibri-dir C [--python P] [--port N] [--max-active 1]
        [--share --cloudflared EXE --hostname H --i-authorize-public-sharing]
  purge                                  apply history/log retention

Local only by default. Sharing starts only with --share AND --i-authorize-public-sharing.";

fn data_dir(args: &mut Vec<String>) -> PathBuf {
    if let Some(i) = args.iter().position(|a| a == "--data-dir") {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        args.drain(i..(i + 2).min(args.len()));
        return PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("HUB_DATA_DIR") {
        return PathBuf::from(v);
    }
    if cfg!(windows) {
        PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()))
            .join("LocalAIHub")
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".local/share/local-ai-hub")
    }
}

fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    let v = args.get(i + 1).cloned();
    args.drain(i..(i + 2).min(args.len()));
    v
}
fn switch(args: &mut Vec<String>, name: &str) -> bool {
    args.iter()
        .position(|a| a == name)
        .map(|i| {
            args.remove(i);
        })
        .is_some()
}

fn default_python() -> PathBuf {
    PathBuf::from(if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    })
}

fn resolve_exe(p: PathBuf) -> PathBuf {
    if p.components().count() > 1 || p.is_absolute() {
        return p;
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(&p))
                .find(|c| c.is_file())
        })
        .unwrap_or(p)
}

fn fmt_time(t: Option<i64>) -> String {
    t.map_or("-".into(), |v| v.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dir = data_dir(&mut args);
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    let cmd = args.remove(0);
    let db_path = dir.join("hub.db");
    let helper = std::env::current_exe()?
        .ancestors()
        .find_map(|a| {
            let p = a.join("scripts/validate_model.py");
            p.is_file().then_some(p)
        })
        .or_else(|| {
            let p = PathBuf::from("scripts/validate_model.py");
            p.is_file().then_some(p)
        });
    match cmd.as_str() {
        "init" => {
            for d in ["", "workspaces", "exports", "cache", "logs", "secrets"] {
                std::fs::create_dir_all(dir.join(d))?;
            }
            let c = db::open(&db_path)?;
            let store = FileSecrets::new(&dir.join("secrets"))?;
            secrets::internal_key(&store)?;
            println!(
                "initialised {} (schema v{}, secrets: {})",
                dir.display(),
                db::schema_version(&c),
                store.protection()
            );
        }
        "model" => {
            let sub = args.first().cloned().unwrap_or_default();
            args.drain(..args.len().min(1));
            let c = db::open(&db_path)?;
            match sub.as_str() {
                "list" => {
                    for m in models::list(&c)? {
                        println!(
                            "{}\t{}\t{}\t{} bytes",
                            m.id,
                            m.model_type,
                            m.path.display(),
                            m.weights_bytes
                        )
                    }
                }
                "remove" => {
                    println!(
                        "{}",
                        if models::unregister(&c, &args[0])? {
                            "removed from catalog (files untouched)"
                        } else {
                            "not found"
                        }
                    )
                }
                "validate" | "add" => {
                    let colibri = PathBuf::from(
                        flag(&mut args, "--colibri-dir").context("--colibri-dir required")?,
                    );
                    let python = resolve_exe(
                        flag(&mut args, "--python")
                            .map(PathBuf::from)
                            .unwrap_or_else(default_python),
                    );
                    let id = flag(&mut args, "--id");
                    let mdir = PathBuf::from(args.first().context("model directory required")?);
                    let helper = helper
                        .clone()
                        .context("scripts/validate_model.py not found")?;
                    match models::validate(&mdir, &python, &colibri, &helper).await {
                        Ok(info) => {
                            println!(
                                "supported: {} ({}), weights {} bytes",
                                info.display_name, info.family, info.weights_bytes
                            );
                            let pf = models::preflight(info.weights_bytes, &mdir, 0);
                            println!(
                                "{} - RAM {:?} bytes, free disk {:?} bytes",
                                pf.label, pf.ram_total_bytes, pf.free_disk_bytes
                            );
                            if sub == "add" {
                                models::register(
                                    &c,
                                    &id.context("--id required")?,
                                    &mdir.canonicalize()?,
                                    &info,
                                )?;
                                println!("added (referenced in place)");
                            }
                        }
                        Err(issues) => {
                            for i in &issues {
                                eprintln!("- {i}");
                            }
                            bail!("model not usable");
                        }
                    }
                }
                _ => bail!("{USAGE}"),
            }
        }
        "key" => {
            let sub = args.first().cloned().unwrap_or_default();
            args.drain(..args.len().min(1));
            let c = db::open(&db_path)?;
            match sub.as_str() {
                "create" => {
                    let days = flag(&mut args, "--expires-days")
                        .map(|d| d.parse::<i64>())
                        .transpose()?;
                    let exp = days.map(|d| hub_core_now() + d * 86400);
                    let (rec, tok) = keys::create(&c, args.first().context("name required")?, exp)?;
                    println!("id:      {}\nname:    {}\nscopes:  {}\nsecret (shown once, store it now):\n{tok}", rec.id, rec.name, rec.scopes.join(","));
                }
                "list" => {
                    for k in keys::list(&c)? {
                        println!(
                            "{}\t{}\tscopes={}\tcreated={}\texpires={}\trevoked={}\tlast_used={}",
                            k.id,
                            k.name,
                            k.scopes.join(","),
                            k.created_at,
                            fmt_time(k.expires_at),
                            fmt_time(k.revoked_at),
                            fmt_time(k.last_used_at)
                        )
                    }
                }
                "revoke" => println!(
                    "{}",
                    if keys::revoke(&c, &args[0])? {
                        "revoked"
                    } else {
                        "no active key with that id"
                    }
                ),
                "rotate" => {
                    let hrs = flag(&mut args, "--grace-hours")
                        .map(|h| h.parse::<i64>())
                        .transpose()?
                        .unwrap_or(24);
                    let (rec, tok) = keys::rotate(&c, &args[0], hrs * 3600)?;
                    println!("new id: {}\nsecret (shown once):\n{tok}\nold key {} stays valid for {hrs} h", rec.id, args[0]);
                }
                _ => bail!("{USAGE}"),
            }
        }
        "secret" => {
            if args.first().map(String::as_str) != Some("set")
                || args.get(1).map(String::as_str) != Some("tunnel_token")
            {
                bail!("{USAGE}");
            }
            let mut v = String::new();
            std::io::stdin().read_line(&mut v)?;
            FileSecrets::new(&dir.join("secrets"))?.set("tunnel_token", v.trim().as_bytes())?;
            println!("stored");
        }
        "purge" => {
            let c = db::open(&db_path)?;
            println!(
                "conversations removed: {}, log rows removed: {}",
                history::purge_expired(&c)?,
                history::purge_request_log(&c, 30)?
            );
        }
        "serve" => serve(&dir, &db_path, args).await?,
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn hub_core_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn serve(dir: &Path, db_path: &Path, mut args: Vec<String>) -> Result<()> {
    let model_id = flag(&mut args, "--model").context("--model ID required")?;
    let colibri =
        PathBuf::from(flag(&mut args, "--colibri-dir").context("--colibri-dir required")?);
    let python = resolve_exe(
        flag(&mut args, "--python")
            .map(PathBuf::from)
            .unwrap_or_else(default_python),
    );
    let port: u16 = flag(&mut args, "--port")
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(8787);
    let max_active: usize = flag(&mut args, "--max-active")
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(1);
    let share = switch(&mut args, "--share");
    let consent = switch(&mut args, "--i-authorize-public-sharing");
    let cloudflared = flag(&mut args, "--cloudflared");
    let hostname = flag(&mut args, "--hostname");
    if share && !(consent && cloudflared.is_some() && hostname.is_some()) {
        bail!("--share needs --cloudflared EXE, --hostname H and --i-authorize-public-sharing (explicit consent to publish)");
    }
    let c = db::open(db_path)?;
    let rec = models::get(&c, &model_id)?
        .with_context(|| format!("model '{model_id}' is not in the catalog (hubd model add)"))?;
    if keys::list(&c)?.iter().all(|k| k.revoked_at.is_some()) {
        eprintln!("note: no active API keys; create one with `hubd key create <name>`");
    }
    let store = FileSecrets::new(&dir.join("secrets"))?;
    let internal = secrets::internal_key(&store)?;
    let mut engine = ColibriAdapter::new(ColibriConfig {
        python,
        colibri_dir: colibri,
        engine_exe: None,
        model_dir: rec.path.clone(),
        model_id: model_id.clone(),
        port: 0,
        internal_key: internal.clone(),
        startup_timeout: Duration::from_secs(900),
        max_tokens: 1024,
    });
    println!("starting engine for {} ...", rec.path.display());
    if let Err(e) = engine.start().await {
        bail!("engine failed: {e}");
    }
    let cfg = GatewayConfig {
        listen: format!("127.0.0.1:{port}").parse()?,
        upstream: engine.upstream_url().unwrap(),
        model_id: model_id.clone(),
        db_path: db_path.to_path_buf(),
        max_active,
        ..Default::default()
    };
    let gw = Gateway::start(cfg, internal).await?;
    println!(
        "local API ready:  http://{}/v1   model id: {model_id}   (auth: Bearer <key>)",
        gw.addr
    );
    println!("sharing: LOCAL ONLY");
    let mut tunnel = None;
    if share {
        let token = store
            .get("tunnel_token")?
            .context("no tunnel token stored (hubd secret set tunnel_token)")?;
        let mut t = CloudflaredTunnel::new(
            resolve_exe(PathBuf::from(cloudflared.unwrap())),
            hostname.unwrap(),
        );
        t.start(
            std::str::from_utf8(&token)?.trim(),
            &PublicSharingConsent::granted_by_user(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
        println!("tunnel connecting; public URL (once connected, UNVERIFIED until tested from outside): https://{}/v1", t.public_hostname);
        tunnel = Some(t);
    }
    println!("Ctrl-C to stop. Serving ends when this process ends.");
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => {
                if let EngineState::Failed(m) = engine.state() { eprintln!("engine failed: {m}"); break; }
            }
        }
    }
    println!("stopping ...");
    if let Some(mut t) = tunnel {
        t.stop().await;
    }
    gw.shutdown().await;
    engine.stop().await;
    Ok(())
}
