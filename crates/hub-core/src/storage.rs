//! Workspace storage confined to an approved root.
//! Files live on disk; metadata lives in SQLite. Every path is validated component by
//! component and every existing ancestor is checked for symlinks / Windows reparse points
//! (junctions) before use. Quotas and the free-space reserve are enforced WHILE writing.
use crate::now;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("path escapes the workspace (link or reparse point): {0}")]
    Escape(String),
    #[error("workspace quota exceeded ({0} bytes)")]
    QuotaExceeded(u64),
    #[error("not enough free disk space (reserve {0} bytes)")]
    DiskReserve(u64),
    #[error("{0} already exists")]
    Exists(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("destructive operation requires explicit confirmation")]
    NotConfirmed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}
type R<T> = Result<T, StorageError>;

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn check_component(c: &str) -> R<()> {
    let bad = |m: &str| Err(StorageError::InvalidPath(format!("{m}: {c:?}")));
    if c.is_empty() || c == "." || c == ".." {
        return bad("empty or dot component");
    }
    if c.len() > 255 {
        return bad("component too long");
    }
    if c.chars()
        .any(|ch| ch.is_control() || "<>:\"|?*\\/".contains(ch))
    {
        return bad("illegal character");
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return bad("trailing dot or space");
    }
    let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return bad("reserved device name");
    }
    if c.starts_with(".tmp-") || c == ".tmp" {
        return bad("reserved internal name");
    }
    Ok(())
}

/// Split a user-supplied relative path ("a/b.txt" or "a\\b.txt") into safe components.
pub fn sanitize_rel(rel: &str) -> R<Vec<String>> {
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        return Err(StorageError::InvalidPath(format!(
            "must be relative: {rel:?}"
        )));
    }
    let comps: Vec<String> = rel.split(['/', '\\']).map(str::to_string).collect();
    for c in &comps {
        check_component(c)?;
    }
    Ok(comps)
}

fn is_link(p: &Path) -> std::io::Result<bool> {
    let m = std::fs::symlink_metadata(p)?;
    if m.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return Ok(true); // FILE_ATTRIBUTE_REPARSE_POINT: junctions, mount points
        }
    }
    Ok(false)
}

/// Join components under `root`, refusing links in any existing ancestor (incl. the root's children).
pub fn resolve_in(root: &Path, comps: &[String]) -> R<PathBuf> {
    let mut cur = root.to_path_buf();
    for c in comps {
        cur.push(c);
        match is_link(&cur) {
            Ok(true) => return Err(StorageError::Escape(cur.display().to_string())),
            Ok(false) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    // Belt and braces: the deepest existing ancestor must canonicalise inside the root.
    let canon_root = root.canonicalize()?;
    let mut probe = cur.clone();
    while !probe.exists() {
        if !probe.pop() {
            break;
        }
    }
    if !probe.canonicalize()?.starts_with(&canon_root) {
        return Err(StorageError::Escape(cur.display().to_string()));
    }
    Ok(cur)
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRecord {
    pub rel_path: String,
    pub size: u64,
    pub sha256: String,
    pub kind: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub quota_bytes: Option<u64>,
    pub used_bytes: u64,
    pub files: u64,
}

pub struct Workspaces {
    pub conn: Connection,
    root: PathBuf,
    /// Writes fail rather than leave less than this free on the volume.
    pub reserve_free_bytes: u64,
}

impl Workspaces {
    pub fn new(conn: Connection, root: &Path, reserve_free_bytes: u64) -> R<Self> {
        std::fs::create_dir_all(root)?;
        let ws = Self {
            conn,
            root: root.canonicalize()?,
            reserve_free_bytes,
        };
        ws.recover()?;
        Ok(ws)
    }

    /// Interrupted writes only ever leave files in `<ws>/.tmp-*`; remove them at startup.
    fn recover(&self) -> R<()> {
        for e in std::fs::read_dir(&self.root)?.flatten() {
            let t = e.path().join(".tmp-uploads");
            if t.is_dir() && !is_link(&t)? {
                let _ = std::fs::remove_dir_all(&t);
            }
        }
        Ok(())
    }

    fn ws_dir(&self, id: &str) -> R<PathBuf> {
        if id.len() != 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(StorageError::InvalidPath("workspace id".into()));
        }
        let d = self.root.join(id);
        if d.exists() && is_link(&d)? {
            return Err(StorageError::Escape(d.display().to_string()));
        }
        Ok(d)
    }

    pub fn create(&self, name: &str, quota: Option<u64>) -> R<WorkspaceInfo> {
        let name = name.trim();
        if name.is_empty() || name.len() > 80 || name.chars().any(|c| c.is_control()) {
            return Err(StorageError::InvalidPath("workspace name".into()));
        }
        let id = hex::encode(crate::random_bytes::<8>());
        std::fs::create_dir_all(self.root.join(&id))?;
        self.conn.execute("INSERT INTO workspaces(id,name,quota_bytes,created_at) VALUES(?1,?2,?3,?4)",
            params![id, name, quota.map(|q| q as i64), now()]).map_err(|e| {
            let _ = std::fs::remove_dir(self.root.join(&id));
            if matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation) {
                StorageError::Exists(name.to_string())
            } else { e.into() }
        })?;
        self.info(&id)
    }

    pub fn info(&self, id: &str) -> R<WorkspaceInfo> {
        self.conn.query_row(
            "SELECT w.id,w.name,w.quota_bytes,COALESCE(SUM(f.size),0),COUNT(f.id) FROM workspaces w LEFT JOIN files f ON f.ws_id=w.id WHERE w.id=?1 GROUP BY w.id",
            [id], |r| Ok(WorkspaceInfo { id: r.get(0)?, name: r.get(1)?, quota_bytes: r.get::<_, Option<i64>>(2)?.map(|q| q as u64),
                used_bytes: r.get::<_, i64>(3)? as u64, files: r.get::<_, i64>(4)? as u64 }))
            .optional()?.ok_or_else(|| StorageError::NotFound(id.into()))
    }

    pub fn list_workspaces(&self) -> R<Vec<WorkspaceInfo>> {
        let ids: Vec<String> = self
            .conn
            .prepare("SELECT id FROM workspaces ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        ids.iter().map(|i| self.info(i)).collect()
    }

    pub fn list_files(&self, ws: &str) -> R<Vec<FileRecord>> {
        self.info(ws)?;
        let v = self.conn.prepare("SELECT rel_path,size,sha256,kind,created_at FROM files WHERE ws_id=?1 ORDER BY rel_path")?
            .query_map([ws], |r| Ok(FileRecord { rel_path: r.get(0)?, size: r.get::<_, i64>(1)? as u64, sha256: r.get(2)?, kind: r.get(3)?, created_at: r.get(4)? }))?
            .collect::<Result<_, _>>()?;
        Ok(v)
    }

    /// Stream `data` into `<workspace>/<rel>`. `kind` is "import" or "output".
    pub fn write_file(
        &mut self,
        ws: &str,
        rel: &str,
        data: &mut dyn Read,
        kind: &str,
        overwrite: bool,
    ) -> R<FileRecord> {
        let info = self.info(ws)?;
        let wdir = self.ws_dir(ws)?;
        let comps = sanitize_rel(rel)?;
        let rel_norm = comps.join("/");
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT size FROM files WHERE ws_id=?1 AND rel_path=?2",
                params![ws, rel_norm],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_some() && !overwrite {
            return Err(StorageError::Exists(rel_norm));
        }
        let final_path = resolve_in(&wdir, &comps)?;
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
            // re-verify after creation (a racing link would be caught here)
            resolve_in(&wdir, &comps)?;
        }
        let tmpdir = wdir.join(".tmp-uploads");
        std::fs::create_dir_all(&tmpdir)?;
        let tmp = tmpdir.join(hex::encode(crate::random_bytes::<8>()));
        let base_used = info.used_bytes.saturating_sub(existing.unwrap_or(0) as u64);
        let quota = info.quota_bytes;
        let result = (|| -> R<(u64, String)> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            let (mut buf, mut total, mut h) = (vec![0u8; 64 * 1024], 0u64, Sha256::new());
            loop {
                let n = data.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if let Some(q) = quota {
                    if base_used + total > q {
                        return Err(StorageError::QuotaExceeded(q));
                    }
                }
                if (total % (1 << 20) < n as u64 || total == n as u64)
                    && fs4::available_space(&self.root)
                        .map(|a| a < self.reserve_free_bytes + n as u64)
                        .unwrap_or(false)
                {
                    return Err(StorageError::DiskReserve(self.reserve_free_bytes));
                }
                h.update(&buf[..n]);
                f.write_all(&buf[..n])?;
            }
            f.sync_all()?;
            Ok((total, hex::encode(h.finalize())))
        })();
        let (size, sha) = match result {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        std::fs::rename(&tmp, &final_path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        let created = now();
        self.conn.execute(
            "INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(ws_id,rel_path) DO UPDATE SET size=?3,sha256=?4,kind=?5,created_at=?6",
            params![ws, rel_norm, size as i64, sha, kind, created])?;
        Ok(FileRecord {
            rel_path: rel_norm,
            size,
            sha256: sha,
            kind: kind.into(),
            created_at: created,
        })
    }

    pub fn read_file(&self, ws: &str, rel: &str) -> R<std::fs::File> {
        let wdir = self.ws_dir(ws)?;
        let comps = sanitize_rel(rel)?;
        let known: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM files WHERE ws_id=?1 AND rel_path=?2",
                params![ws, comps.join("/")],
                |r| r.get(0),
            )
            .optional()?;
        known.ok_or_else(|| StorageError::NotFound(rel.into()))?;
        Ok(std::fs::File::open(resolve_in(&wdir, &comps)?)?)
    }

    pub fn delete_file(&self, ws: &str, rel: &str, confirmed: bool) -> R<()> {
        if !confirmed {
            return Err(StorageError::NotConfirmed);
        }
        let wdir = self.ws_dir(ws)?;
        let comps = sanitize_rel(rel)?;
        let p = resolve_in(&wdir, &comps)?;
        let n = self.conn.execute(
            "DELETE FROM files WHERE ws_id=?1 AND rel_path=?2",
            params![ws, comps.join("/")],
        )?;
        if n == 0 {
            return Err(StorageError::NotFound(rel.into()));
        }
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Deletes the workspace folder and rows. Not a secure erase (SSD wear levelling).
    pub fn delete_workspace(&self, ws: &str, confirmed: bool) -> R<()> {
        if !confirmed {
            return Err(StorageError::NotConfirmed);
        }
        self.info(ws)?;
        let d = self.ws_dir(ws)?;
        self.conn
            .execute("DELETE FROM workspaces WHERE id=?1", [ws])?;
        if d.exists() {
            std::fs::remove_dir_all(d)?;
        }
        Ok(())
    }

    /// Export one workspace (files + metadata.json) to a zip. Never includes models or secrets.
    pub fn export_workspace(&self, ws: &str, dest: &Path) -> R<()> {
        let info = self.info(ws)?;
        let wdir = self.ws_dir(ws)?;
        if dest.starts_with(&wdir) {
            return Err(StorageError::InvalidPath(
                "export destination is inside the workspace".into(),
            ));
        }
        let files = self.list_files(ws)?;
        let out = std::fs::File::create(dest)?;
        let mut z = zip::ZipWriter::new(out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        z.start_file("metadata.json", opts)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        z.write_all(
            serde_json::to_string_pretty(&serde_json::json!({"workspace": info, "files": files}))
                .unwrap()
                .as_bytes(),
        )?;
        for f in &files {
            let comps = sanitize_rel(&f.rel_path)?;
            let p = resolve_in(&wdir, &comps)?;
            z.start_file(format!("files/{}", f.rel_path), opts)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            std::io::copy(&mut std::fs::File::open(p)?, &mut z)?;
        }
        z.finish()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Consistent database snapshot (VACUUM INTO, not a file copy). API-key verifiers excluded.
    pub fn backup_database(&self, dest: &Path) -> R<()> {
        if dest.exists() {
            return Err(StorageError::Exists(dest.display().to_string()));
        }
        self.conn
            .execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
        let b = Connection::open(dest)?;
        b.execute("DELETE FROM api_keys", [])?;
        b.execute("DELETE FROM request_log", [])?;
        b.execute_batch("VACUUM")?;
        Ok(())
    }
}
