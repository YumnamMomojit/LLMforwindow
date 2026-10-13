//! Model import/validation. Imported models are referenced in place (never copied or
//! deleted). Which architectures are servable is decided by the pinned upstream
//! `family_registry` (via scripts/validate_model.py), not by a list duplicated here.
use crate::now;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub family: String,
    pub display_name: String,
    pub model_type: String,
    /// The upstream family's modality: "text" (a chat engine), "image" or "decision".
    /// Only "text" passes validation.
    pub modality: String,
    pub weights_bytes: u64,
    pub has_tokenizer: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRecord {
    pub id: String,
    pub path: PathBuf,
    pub model_type: String,
    pub weights_bytes: u64,
}

/// 1-64 chars of [A-Za-z0-9._-], not starting with '.' or '-' (the id is an argv value for
/// the launcher, and a leading '-' reads as an option there).
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && !id.starts_with(['.', '-'])
}

/// Pure-Rust structural checks: config, tokenizer, shard files named by the index, sizes.
pub fn structural_check(dir: &Path) -> (u64, bool, Vec<String>) {
    let mut issues = vec![];
    if !dir.is_dir() {
        return (
            0,
            false,
            vec![format!("not a directory: {}", dir.display())],
        );
    }
    if !dir.join("config.json").is_file() {
        issues.push("config.json is missing".into());
    }
    let has_tok = ["tokenizer.json", "tokenizer.model", "tokenizer"]
        .iter()
        .any(|n| dir.join(n).exists());
    if !has_tok {
        issues.push("no tokenizer found (tokenizer.json)".into());
    }
    let mut total = 0u64;
    let mut shards = std::collections::BTreeSet::new();
    if let Ok(idx) = std::fs::read_to_string(dir.join("model.safetensors.index.json")) {
        match serde_json::from_str::<serde_json::Value>(&idx) {
            Ok(v) => {
                if let Some(m) = v["weight_map"].as_object() {
                    for f in m.values().filter_map(|x| x.as_str()) {
                        shards.insert(f.to_string());
                    }
                }
            }
            Err(_) => issues.push("model.safetensors.index.json is not valid JSON".into()),
        }
    } else if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".safetensors") {
                shards.insert(n);
            }
        }
    }
    if shards.is_empty() {
        issues.push("no .safetensors weight files found".into());
    }
    for s in &shards {
        // The index is untrusted: refuse names that could point outside the model folder.
        if s.contains('/') || s.contains('\\') || s.contains("..") {
            issues.push(format!("suspicious shard name in index: {s}"));
            continue;
        }
        match std::fs::metadata(dir.join(s)) {
            Ok(m) => total += m.len(),
            Err(_) => issues.push(format!("missing weight file: {s}")),
        }
    }
    (total, has_tok, issues)
}

/// Full validation: structure + the pinned upstream's own family resolution.
pub async fn validate(
    dir: &Path,
    python: &Path,
    colibri_dir: &Path,
    helper: &Path,
) -> Result<ModelInfo, Vec<String>> {
    let (bytes, has_tok, mut issues) = structural_check(dir);
    let out = tokio::process::Command::new(python)
        .args(["-I"])
        .arg(helper)
        .arg(colibri_dir)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let (mut family, mut display, mut mtype) = (String::new(), String::new(), String::new());
    let mut modality = String::new();
    match out {
        Ok(o) => match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            Ok(v) if v["ok"] == true => {
                family = v["family"].as_str().unwrap_or("").into();
                display = v["display_name"].as_str().unwrap_or("").into();
                mtype = v["model_type"].as_str().unwrap_or("").into();
                modality = v["modality"].as_str().unwrap_or("").into();
                // Semdok Edge serves /v1/chat/completions only. Upstream's decision engines
                // (Laya, GLiNER2.5-Decide: POST /v1/systemone) and its image engine
                // (Qwen-Image) resolve to a family too, but can never answer a chat.
                match modality.as_str() {
                    "text" => {}
                    "" => issues.push(
                        "the validator did not report the model's modality; check that \
                         --colibri-dir is the pinned Colibri release"
                            .into(),
                    ),
                    m => issues.push(format!(
                        "{display} ({family}) is not a chat model (upstream modality \"{m}\"); \
                         Semdok Edge serves text chat models only"
                    )),
                }
            }
            Ok(v) => issues.push(format!(
                "not supported by the Colibri engine: {}",
                v["error"].as_str().unwrap_or("unknown")
            )),
            Err(_) => issues.push("could not interpret the validator output".into()),
        },
        Err(e) => issues.push(format!("could not run Python ({}): {e}", python.display())),
    }
    if family.is_empty() || !issues.is_empty() {
        return Err(issues);
    }
    Ok(ModelInfo {
        family,
        display_name: display,
        model_type: mtype,
        modality,
        weights_bytes: bytes,
        has_tokenizer: has_tok,
        issues,
    })
}

pub fn register(c: &Connection, id: &str, path: &Path, info: &ModelInfo) -> anyhow::Result<()> {
    if !valid_model_id(id) {
        anyhow::bail!(
            "model id must be 1-64 chars of [A-Za-z0-9._-], not starting with '.' or '-'"
        );
    }
    c.execute(
        "INSERT INTO models(id,path,model_type,weights_bytes,added_at) VALUES(?1,?2,?3,?4,?5)",
        params![
            id,
            path.to_string_lossy(),
            info.family,
            info.weights_bytes as i64,
            now()
        ],
    )?;
    Ok(())
}

pub fn list(c: &Connection) -> anyhow::Result<Vec<ModelRecord>> {
    let mut s =
        c.prepare("SELECT id,path,model_type,weights_bytes FROM models ORDER BY added_at")?;
    let v = s
        .query_map([], |r| {
            Ok(ModelRecord {
                id: r.get(0)?,
                path: PathBuf::from(r.get::<_, String>(1)?),
                model_type: r.get(2)?,
                weights_bytes: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

pub fn get(c: &Connection, id: &str) -> anyhow::Result<Option<ModelRecord>> {
    Ok(list(c)?.into_iter().find(|m| m.id == id))
}

/// Removes the catalog entry only. The user's files are never touched.
pub fn unregister(c: &Connection, id: &str) -> anyhow::Result<bool> {
    Ok(c.execute("DELETE FROM models WHERE id=?1", [id])? > 0)
}

#[derive(Debug, Serialize)]
pub struct Preflight {
    pub label: &'static str,
    pub weights_bytes: u64,
    pub ram_total_bytes: Option<u64>,
    pub free_disk_bytes: Option<u64>,
    pub warnings: Vec<String>,
}

/// Rough, explicitly-labelled estimate. Colibri streams MoE experts from disk, so RAM need is
/// not simply the weight size; upstream's resource planner is authoritative at load time.
pub fn preflight(weights_bytes: u64, dest: &Path, need_download_bytes: u64) -> Preflight {
    let ram = ram_total_bytes();
    let free = fs4::available_space(dest).ok();
    let mut w = vec![];
    if let Some(f) = free {
        if need_download_bytes > 0 && f < need_download_bytes + (2 << 30) {
            w.push(format!(
                "Not enough free disk space: need ~{} GB plus a 2 GB reserve, have {} GB.",
                need_download_bytes >> 30,
                f >> 30
            ));
        }
    }
    if let Some(r) = ram {
        if r < (8u64 << 30) {
            w.push("Less than 8 GB RAM detected; upstream lists 8 GB as the minimum for its smallest model.".into());
        }
    }
    Preflight {
        label: "ESTIMATE ONLY - the engine's own resource plan decides at load time",
        weights_bytes,
        ram_total_bytes: ram,
        free_disk_bytes: free,
        warnings: w,
    }
}

fn ram_total_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = s
            .lines()
            .find(|l| l.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        return Some(kb * 1024);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        unsafe {
            let mut m: MEMORYSTATUSEX = std::mem::zeroed();
            m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut m) != 0 {
                return Some(m.ullTotalPhys);
            }
        }
        return None;
    }
    #[allow(unreachable_code)]
    None
}
