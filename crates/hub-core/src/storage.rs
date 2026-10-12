//! Workspace storage confined to an approved root.
//! Files live on disk; metadata lives in SQLite. Every path is validated component by
//! component and every existing ancestor is checked for symlinks / Windows reparse points
//! (junctions) before use. Quotas and the free-space reserve are enforced WHILE writing.
use crate::now;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    check_win32_name(c)?;
    // Compared the way NTFS compares names, so `.TMP-uploads` cannot reach the temp area.
    let key = path_key(c);
    if key.starts_with(".TMP-") || key == ".TMP" {
        return bad("reserved internal name");
    }
    if looks_like_short_name(c) {
        return bad("8.3 short-name alias");
    }
    Ok(())
}

/// Names Windows would silently change (it drops a trailing dot or space) or open as a device.
fn check_win32_name(c: &str) -> R<()> {
    let bad = |m: &str| Err(StorageError::InvalidPath(format!("{m}: {c:?}")));
    if c.ends_with('.') || c.ends_with(' ') {
        return bad("trailing dot or space");
    }
    let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return bad("reserved device name");
    }
    Ok(())
}

/// Path identity as NTFS sees it: every UTF-16 unit goes through the volume's `$UpCase`
/// table, so `a.txt` and `A.TXT` are one file there and must be one row here. That table is
/// not Unicode's uppercase: `ı`/`I`, `ſ`/`S`, `µ`/`Μ`, `ς`/`Σ`, `ǅ`/`Ǆ` and Georgian
/// Mkhedruli/Mtavruli are distinct names on NTFS, `ᾳ`/`ᾼ` are one, and characters outside
/// the BMP (surrogate pairs) are compared exactly. Maps character to character (length is kept).
fn path_key(rel: &str) -> String {
    rel.chars()
        .map(|ch| match u16::try_from(u32::from(ch)) {
            Ok(u) => char::from_u32(upcase(u).into()).unwrap_or(ch),
            Err(_) => ch,
        })
        .collect()
}

/// One UTF-16 unit through [`UPCASE`].
fn upcase(u: u16) -> u16 {
    let i = UPCASE.partition_point(|r| r.0 <= u);
    match i.checked_sub(1).map(|i| UPCASE[i]) {
        Some((first, last, to, step)) if u <= last && (step == 1 || (u - first) & 1 == 0) => {
            to + (u - first)
        }
        _ => u,
    }
}

/// The `$UpCase` table Windows 7 and later write when formatting NTFS (Windows 8's file has
/// MD5 7ff498a44e45e77374cc7c962b1b92f2, as published by ntfs-3g; a unit test checks this
/// table against it). Runs of `(first, last, first's uppercase, step)`, step 1 or 2 (every
/// other unit: alternating capital/small pairs); every other unit maps to itself. Volumes formatted by XP or Vista keep their older table, which differs for 343
/// characters (among them `ș`, `ț` and final `ς`). Those are not refused: that would refuse
/// ordinary Romanian and Greek names on every volume to cover drives formatted before 2009.
#[rustfmt::skip]
const UPCASE: [(u16, u16, u16, u16); 134] = [
    (0x0061, 0x007A, 0x0041, 1), (0x00E0, 0x00F6, 0x00C0, 1), (0x00F8, 0x00FE, 0x00D8, 1), (0x00FF, 0x00FF, 0x0178, 1),
    (0x0101, 0x012F, 0x0100, 2), (0x0133, 0x0137, 0x0132, 2), (0x013A, 0x0148, 0x0139, 2), (0x014B, 0x0177, 0x014A, 2),
    (0x017A, 0x017E, 0x0179, 2), (0x0180, 0x0180, 0x0243, 1), (0x0183, 0x0185, 0x0182, 2), (0x0188, 0x0188, 0x0187, 1),
    (0x018C, 0x018C, 0x018B, 1), (0x0192, 0x0192, 0x0191, 1), (0x0195, 0x0195, 0x01F6, 1), (0x0199, 0x0199, 0x0198, 1),
    (0x019A, 0x019A, 0x023D, 1), (0x019E, 0x019E, 0x0220, 1), (0x01A1, 0x01A5, 0x01A0, 2), (0x01A8, 0x01A8, 0x01A7, 1),
    (0x01AD, 0x01AD, 0x01AC, 1), (0x01B0, 0x01B0, 0x01AF, 1), (0x01B4, 0x01B6, 0x01B3, 2), (0x01B9, 0x01B9, 0x01B8, 1),
    (0x01BD, 0x01BD, 0x01BC, 1), (0x01BF, 0x01BF, 0x01F7, 1), (0x01C6, 0x01C6, 0x01C4, 1), (0x01C9, 0x01C9, 0x01C7, 1),
    (0x01CC, 0x01CC, 0x01CA, 1), (0x01CE, 0x01DC, 0x01CD, 2), (0x01DD, 0x01DD, 0x018E, 1), (0x01DF, 0x01EF, 0x01DE, 2),
    (0x01F3, 0x01F3, 0x01F1, 1), (0x01F5, 0x01F5, 0x01F4, 1), (0x01F9, 0x021F, 0x01F8, 2), (0x0223, 0x0233, 0x0222, 2),
    (0x023C, 0x023C, 0x023B, 1), (0x0242, 0x0242, 0x0241, 1), (0x0247, 0x024F, 0x0246, 2), (0x0250, 0x0250, 0x2C6F, 1),
    (0x0251, 0x0251, 0x2C6D, 1), (0x0253, 0x0253, 0x0181, 1), (0x0254, 0x0254, 0x0186, 1), (0x0256, 0x0257, 0x0189, 1),
    (0x0259, 0x0259, 0x018F, 1), (0x025B, 0x025B, 0x0190, 1), (0x0260, 0x0260, 0x0193, 1), (0x0263, 0x0263, 0x0194, 1),
    (0x0268, 0x0268, 0x0197, 1), (0x0269, 0x0269, 0x0196, 1), (0x026B, 0x026B, 0x2C62, 1), (0x026F, 0x026F, 0x019C, 1),
    (0x0271, 0x0271, 0x2C6E, 1), (0x0272, 0x0272, 0x019D, 1), (0x0275, 0x0275, 0x019F, 1), (0x027D, 0x027D, 0x2C64, 1),
    (0x0280, 0x0280, 0x01A6, 1), (0x0283, 0x0283, 0x01A9, 1), (0x0288, 0x0288, 0x01AE, 1), (0x0289, 0x0289, 0x0244, 1),
    (0x028A, 0x028B, 0x01B1, 1), (0x028C, 0x028C, 0x0245, 1), (0x0292, 0x0292, 0x01B7, 1), (0x0371, 0x0373, 0x0370, 2),
    (0x0377, 0x0377, 0x0376, 1), (0x037B, 0x037D, 0x03FD, 1), (0x03AC, 0x03AC, 0x0386, 1), (0x03AD, 0x03AF, 0x0388, 1),
    (0x03B1, 0x03C1, 0x0391, 1), (0x03C3, 0x03CB, 0x03A3, 1), (0x03CC, 0x03CC, 0x038C, 1), (0x03CD, 0x03CE, 0x038E, 1),
    (0x03D7, 0x03D7, 0x03CF, 1), (0x03D9, 0x03EF, 0x03D8, 2), (0x03F2, 0x03F2, 0x03F9, 1), (0x03F8, 0x03F8, 0x03F7, 1),
    (0x03FB, 0x03FB, 0x03FA, 1), (0x0430, 0x044F, 0x0410, 1), (0x0450, 0x045F, 0x0400, 1), (0x0461, 0x0481, 0x0460, 2),
    (0x048B, 0x04BF, 0x048A, 2), (0x04C2, 0x04CE, 0x04C1, 2), (0x04CF, 0x04CF, 0x04C0, 1), (0x04D1, 0x0523, 0x04D0, 2),
    (0x0561, 0x0586, 0x0531, 1), (0x1D79, 0x1D79, 0xA77D, 1), (0x1D7D, 0x1D7D, 0x2C63, 1), (0x1E01, 0x1E95, 0x1E00, 2),
    (0x1EA1, 0x1EFF, 0x1EA0, 2), (0x1F00, 0x1F07, 0x1F08, 1), (0x1F10, 0x1F15, 0x1F18, 1), (0x1F20, 0x1F27, 0x1F28, 1),
    (0x1F30, 0x1F37, 0x1F38, 1), (0x1F40, 0x1F45, 0x1F48, 1), (0x1F51, 0x1F57, 0x1F59, 2), (0x1F60, 0x1F67, 0x1F68, 1),
    (0x1F70, 0x1F71, 0x1FBA, 1), (0x1F72, 0x1F75, 0x1FC8, 1), (0x1F76, 0x1F77, 0x1FDA, 1), (0x1F78, 0x1F79, 0x1FF8, 1),
    (0x1F7A, 0x1F7B, 0x1FEA, 1), (0x1F7C, 0x1F7D, 0x1FFA, 1), (0x1F80, 0x1F87, 0x1F88, 1), (0x1F90, 0x1F97, 0x1F98, 1),
    (0x1FA0, 0x1FA7, 0x1FA8, 1), (0x1FB0, 0x1FB1, 0x1FB8, 1), (0x1FB3, 0x1FB3, 0x1FBC, 1), (0x1FC3, 0x1FC3, 0x1FCC, 1),
    (0x1FD0, 0x1FD1, 0x1FD8, 1), (0x1FE0, 0x1FE1, 0x1FE8, 1), (0x1FE5, 0x1FE5, 0x1FEC, 1), (0x1FF3, 0x1FF3, 0x1FFC, 1),
    (0x214E, 0x214E, 0x2132, 1), (0x2170, 0x217F, 0x2160, 1), (0x2184, 0x2184, 0x2183, 1), (0x24D0, 0x24E9, 0x24B6, 1),
    (0x2C30, 0x2C5E, 0x2C00, 1), (0x2C61, 0x2C61, 0x2C60, 1), (0x2C65, 0x2C65, 0x023A, 1), (0x2C66, 0x2C66, 0x023E, 1),
    (0x2C68, 0x2C6C, 0x2C67, 2), (0x2C73, 0x2C73, 0x2C72, 1), (0x2C76, 0x2C76, 0x2C75, 1), (0x2C81, 0x2CE3, 0x2C80, 2),
    (0x2D00, 0x2D25, 0x10A0, 1), (0xA641, 0xA65F, 0xA640, 2), (0xA663, 0xA66D, 0xA662, 2), (0xA681, 0xA697, 0xA680, 2),
    (0xA723, 0xA72F, 0xA722, 2), (0xA733, 0xA76F, 0xA732, 2), (0xA77A, 0xA77C, 0xA779, 2), (0xA77F, 0xA787, 0xA77E, 2),
    (0xA78C, 0xA78C, 0xA78B, 1), (0xFF41, 0xFF5A, 0xFF21, 1),
];

/// `NAME~N.EXT` within 8.3 characters is how NTFS spells the short alias of a long name
/// (`PROGRA~1`, `LONGFI~1.TXT`); it would reach that other file behind the database's back.
fn looks_like_short_name(c: &str) -> bool {
    let (base, ext) = c.split_once('.').unwrap_or((c, ""));
    !ext.contains('.')
        && base.chars().count() <= 8
        && ext.chars().count() <= 3
        && base
            .split('~')
            .skip(1)
            .any(|t| t.starts_with(|ch: char| ch.is_ascii_digit()))
}

fn remove_if_exists(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// zip64 records only where needed, for the widest reader support. Deflate can grow
/// incompressible data slightly, so switch well below the 4 GiB limit.
const ZIP64_FROM: u64 = 1 << 31;

/// A sibling name to build `dest` under, so `dest` only ever holds a finished file.
fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(format!(".{}.part", hex::encode(crate::random_bytes::<4>())));
    PathBuf::from(p)
}

/// Rename a fully written `part` over `dest`, or remove it if writing failed.
fn put_in_place(written: R<()>, part: &Path, dest: &Path) -> R<()> {
    let res = written.and_then(|()| Ok(std::fs::rename(part, dest)?));
    if res.is_err() {
        let _ = std::fs::remove_file(part);
    }
    res
}

/// Give a fully written `part` the name `dest`, never replacing a file there: a hard link
/// fails if `dest` exists, however late it appeared. `part` is gone afterwards on success.
fn place_new(part: &Path, dest: &Path) -> R<()> {
    match std::fs::hard_link(part, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(StorageError::Exists(dest.display().to_string()))
        }
        // No hard links on this volume (FAT, exFAT, some network shares).
        Err(_) => claim_then_rename(part, dest),
    }
}

/// [`place_new`] without hard links: an empty placeholder created only if the name is free
/// claims `dest`, then `part` replaces that placeholder.
fn claim_then_rename(part: &Path, dest: &Path) -> R<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| match e.kind() {
            ErrorKind::AlreadyExists => StorageError::Exists(dest.display().to_string()),
            _ => e.into(),
        })?;
    std::fs::rename(part, dest).map_err(|e| {
        let _ = std::fs::remove_file(dest);
        e.into()
    })
}

/// Full (unredacted) backup snapshots are built beside the database under this prefix.
const SNAPSHOT_PREFIX: &str = ".backup-snapshot-";
/// A running backup keeps writing its snapshot; one untouched this long was abandoned.
const ABANDONED_SNAPSHOT: Duration = Duration::from_secs(3600);

/// A replacement in flight. Written to `<ws>/.tmp-uploads/<id>.txn` before anything is
/// renamed and removed once disk and database agree again; `<id>.new` is the upload and
/// `<id>.old` the file it replaces, kept until the row is committed.
#[derive(Serialize, Deserialize)]
struct Pending {
    rel_path: String,
    sha256: String,
    size: u64,
    had_old: bool,
}

/// Make the disk agree with the database for the write `txn` describes (a crash, or a COMMIT
/// that failed, between its renames and its COMMIT): if its row was committed the new file
/// stays, otherwise what was there before is put back. Idempotent. Runs with the write lock
/// held, so no write is half-way through meanwhile.
fn settle(conn: &Connection, ws: &str, wdir: &Path, txn: &Path) -> R<()> {
    let p: Pending = match std::fs::read(txn) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(p) => p,
            // Torn while being written, so nothing was renamed yet.
            Err(_) => return Ok(remove_if_exists(txn)?),
        },
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let new = txn.with_extension("new");
    let old = txn.with_extension("old");
    let target = resolve_in(wdir, &sanitize_rel(&p.rel_path)?)?;
    let committed = conn
        .query_row(
            "SELECT sha256,size FROM files WHERE ws_id=?1 AND rel_path=?2",
            params![ws, p.rel_path],
            |r| Ok(r.get::<_, String>(0)? == p.sha256 && r.get::<_, i64>(1)? as u64 == p.size),
        )
        .optional()?
        .unwrap_or(false);
    let present = |f: &Path| std::fs::symlink_metadata(f).is_ok();
    if committed {
        if present(&new) {
            std::fs::rename(&new, &target)?;
        }
        remove_if_exists(&old)?;
    } else if p.had_old {
        if present(&old) {
            std::fs::rename(&old, &target)?;
        }
    } else if !present(&new) {
        // Nothing stood there before and the new file had been renamed in: remove it.
        remove_if_exists(&target)?;
    }
    remove_if_exists(txn)?;
    remove_if_exists(&new)?;
    Ok(())
}

/// [`settle`] every write a workspace's temp area records.
fn settle_all(conn: &Connection, ws: &str, wdir: &Path, tmpdir: &Path) -> R<()> {
    let entries = match std::fs::read_dir(tmpdir) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for e in entries {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "txn") {
            settle(conn, ws, wdir, &p)?;
        }
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
        let mut ws = Self {
            conn,
            root: root.canonicalize()?,
            reserve_free_bytes,
        };
        ws.recover()?;
        Ok(ws)
    }

    /// Interrupted writes only ever leave files in `<ws>/.tmp-uploads`. At startup, settle the
    /// replacements a crash cut short (see [`Pending`]), then remove the rest; also remove full
    /// snapshots a killed backup left beside the database.
    fn recover(&mut self) -> R<()> {
        let mut leftovers = Vec::new();
        for e in std::fs::read_dir(&self.root)?.flatten() {
            let t = e.path().join(".tmp-uploads");
            // A workspace deleted meanwhile is simply skipped.
            if matches!(is_link(&e.path()), Ok(false))
                && matches!(is_link(&t), Ok(false))
                && t.is_dir()
                && std::fs::read_dir(&t).is_ok_and(|mut d| d.next().is_some())
            {
                leftovers.push((e.file_name().to_string_lossy().into_owned(), e.path(), t));
            }
        }
        if !leftovers.is_empty() {
            // Under the write lock, so no write in another process is half-way through.
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (ws, wdir, t) in &leftovers {
                // One that cannot be settled now keeps its files (including the one it would
                // put back); the next write to that workspace retries and reports why.
                if settle_all(&tx, ws, wdir, t).is_ok() {
                    let _ = std::fs::remove_dir_all(t);
                }
            }
            tx.commit()?;
        }
        for e in std::fs::read_dir(self.snapshot_dir())
            .into_iter()
            .flatten()
            .flatten()
        {
            let abandoned = e.file_name().to_string_lossy().starts_with(SNAPSHOT_PREFIX)
                && e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > ABANDONED_SNAPSHOT);
            if abandoned {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(())
    }

    /// Where a backup builds its full snapshot: beside the database, which holds the same
    /// data anyway, rather than in the destination's folder (which may be synced or shared).
    fn snapshot_dir(&self) -> PathBuf {
        self.conn
            .path()
            .and_then(|p| Path::new(p).parent())
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| self.root.clone(), Path::to_path_buf)
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

    /// The tracked file `rel_norm` names, as `(id, stored rel_path, size)`. Matched by
    /// [`path_key`], so every spelling NTFS treats as that file finds its one row.
    fn find_file(&self, ws: &str, rel_norm: &str) -> R<Option<(i64, String, i64)>> {
        let key = path_key(rel_norm);
        // path_key keeps the length, so only rows of the same length can match; exact first.
        let mut st = self.conn.prepare(
            "SELECT id,rel_path,size FROM files WHERE ws_id=?1 AND length(rel_path)=?2 ORDER BY rel_path=?3 DESC",
        )?;
        let rows = st.query_map(
            params![ws, rel_norm.chars().count() as i64, rel_norm],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        for row in rows {
            let row: (i64, String, i64) = row?;
            if path_key(&row.1) == key {
                return Ok(Some(row));
            }
        }
        Ok(None)
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
        let mut comps = sanitize_rel(rel)?;
        let mut rel_norm = comps.join("/");
        let mut existing_size = 0;
        if let Some((_, stored, size)) = self.find_file(ws, &rel_norm)? {
            if !overwrite {
                return Err(StorageError::Exists(stored));
            }
            // The same file under another spelling: keep the stored one, so row and disk stay 1:1.
            comps = sanitize_rel(&stored)?;
            rel_norm = stored;
            existing_size = size;
        }
        let final_path = resolve_in(&wdir, &comps)?;
        // Never replace a file the database does not know about (a leftover, or an alias
        // the key cannot see) unless asked to.
        if !overwrite && std::fs::symlink_metadata(&final_path).is_ok() {
            return Err(StorageError::Exists(rel_norm));
        }
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
            // re-verify after creation (a racing link would be caught here)
            resolve_in(&wdir, &comps)?;
        }
        let tmpdir = wdir.join(".tmp-uploads");
        std::fs::create_dir_all(&tmpdir)?;
        let id = hex::encode(crate::random_bytes::<8>());
        let tmp = tmpdir.join(format!("{id}.new"));
        let base_used = info.used_bytes.saturating_sub(existing_size as u64);
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
        let created = now();
        let txn = tmpdir.join(format!("{id}.txn"));
        let old = tmpdir.join(format!("{id}.old"));
        // Under the write lock: journal, row, renames, COMMIT. The replaced file is kept as
        // `old` until the row is committed, so a failed COMMIT or a crash (see `settle`)
        // never leaves the disk holding a file the row does not describe.
        let committed = (|| -> R<()> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // A write a crash cut short is settled before this one touches the workspace.
            settle_all(&tx, ws, &wdir, &tmpdir)?;
            // Checked again under the lock: another writer may have created it meanwhile.
            let had_old = match std::fs::symlink_metadata(&final_path) {
                Ok(m) if overwrite && m.is_file() => true,
                Ok(_) => return Err(StorageError::Exists(rel_norm.clone())),
                Err(e) if e.kind() == ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            };
            let pending = Pending {
                rel_path: rel_norm.clone(),
                sha256: sha.clone(),
                size,
                had_old,
            };
            let mut j = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&txn)?;
            j.write_all(&serde_json::to_vec(&pending).map_err(std::io::Error::other)?)?;
            j.sync_all()?;
            tx.execute(
                "INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(ws_id,rel_path) DO UPDATE SET size=?3,sha256=?4,kind=?5,created_at=?6",
                params![ws, rel_norm, size as i64, sha, kind, created])?;
            if had_old {
                std::fs::rename(&final_path, &old)?;
            }
            std::fs::rename(&tmp, &final_path)?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(e) = committed {
            // A COMMIT that reported an error may still have landed, so settle against what
            // the database now holds. If even that fails, the next write or start-up does it.
            if !txn.exists() || self.settle_now(ws, &wdir, &txn).is_ok() {
                let _ = std::fs::remove_file(&tmp);
            }
            return Err(e);
        }
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(&txn);
        Ok(FileRecord {
            rel_path: rel_norm,
            size,
            sha256: sha,
            kind: kind.into(),
            created_at: created,
        })
    }

    /// [`settle`] one write, taking the write lock for it.
    fn settle_now(&mut self, ws: &str, wdir: &Path, txn: &Path) -> R<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        settle(&tx, ws, wdir, txn)?;
        tx.commit()?;
        Ok(())
    }

    pub fn read_file(&self, ws: &str, rel: &str) -> R<std::fs::File> {
        let wdir = self.ws_dir(ws)?;
        let comps = sanitize_rel(rel)?;
        let (_, stored, _) = self
            .find_file(ws, &comps.join("/"))?
            .ok_or_else(|| StorageError::NotFound(rel.into()))?;
        Ok(std::fs::File::open(resolve_in(
            &wdir,
            &sanitize_rel(&stored)?,
        )?)?)
    }

    pub fn delete_file(&self, ws: &str, rel: &str, confirmed: bool) -> R<()> {
        if !confirmed {
            return Err(StorageError::NotConfirmed);
        }
        let wdir = self.ws_dir(ws)?;
        let comps = sanitize_rel(rel)?;
        let (id, stored, _) = self
            .find_file(ws, &comps.join("/"))?
            .ok_or_else(|| StorageError::NotFound(rel.into()))?;
        let p = resolve_in(&wdir, &sanitize_rel(&stored)?)?;
        // Disk first: if the removal fails, the row (and its quota use) stays and a retry works.
        remove_if_exists(&p)?;
        self.conn.execute("DELETE FROM files WHERE id=?1", [id])?;
        Ok(())
    }

    /// Deletes the workspace folder and rows. Not a secure erase (SSD wear levelling).
    /// Disk before rows: if a removal fails part-way (a file held open on Windows, a permission
    /// error) the workspace stays, listing exactly the files still on disk, and can be deleted again.
    pub fn delete_workspace(&self, ws: &str, confirmed: bool) -> R<()> {
        if !confirmed {
            return Err(StorageError::NotConfirmed);
        }
        self.info(ws)?;
        let d = self.ws_dir(ws)?;
        let mut removed = Vec::new();
        let res = (|| -> R<()> {
            for f in self.list_files(ws)? {
                // A stored path that no longer validates or crosses a link is left to
                // remove_dir_all, which removes links without following them.
                if let Ok(p) = sanitize_rel(&f.rel_path).and_then(|c| resolve_in(&d, &c)) {
                    remove_if_exists(&p)?;
                    removed.push(f.rel_path);
                }
            }
            if d.exists() {
                std::fs::remove_dir_all(&d)?;
            }
            Ok(())
        })();
        let tx = self.conn.unchecked_transaction()?;
        if res.is_ok() {
            tx.execute("DELETE FROM workspaces WHERE id=?1", [ws])?;
        } else {
            for r in &removed {
                tx.execute(
                    "DELETE FROM files WHERE ws_id=?1 AND rel_path=?2",
                    params![ws, r],
                )?;
            }
        }
        tx.commit()?;
        res
    }

    /// Export one workspace (files + metadata.json) to a zip. Never includes models or secrets.
    /// Built under a temporary name and renamed into place, so a failed export leaves `dest` as it was.
    pub fn export_workspace(&self, ws: &str, dest: &Path) -> R<()> {
        let info = self.info(ws)?;
        let wdir = self.ws_dir(ws)?;
        let name = dest
            .file_name()
            .ok_or_else(|| StorageError::InvalidPath(format!("export destination {dest:?}")))?;
        check_win32_name(&name.to_string_lossy())?;
        // Compare real locations, not spellings (`..`, links, relative paths, `\\?\` on
        // Windows): nothing may be written under the workspaces root. The file itself is made
        // through the caller's own path: a canonical `\\?\` one would skip Windows' name rules
        // and could create a name other programs cannot open.
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if parent.canonicalize()?.join(name).starts_with(&self.root) {
            return Err(StorageError::InvalidPath(
                "export destination is inside the workspaces folder".into(),
            ));
        }
        let files = self.list_files(ws)?;
        let part = part_path(dest);
        let written = (|| -> R<()> {
            let out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)?;
            let mut z = zip::ZipWriter::new(out);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            z.start_file("metadata.json", opts)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            z.write_all(
                serde_json::to_string_pretty(
                    &serde_json::json!({"workspace": info, "files": files}),
                )
                .unwrap()
                .as_bytes(),
            )?;
            for f in &files {
                let comps = sanitize_rel(&f.rel_path)?;
                let mut src = std::fs::File::open(resolve_in(&wdir, &comps)?)?;
                let large = src.metadata()?.len() >= ZIP64_FROM;
                z.start_file(format!("files/{}", f.rel_path), opts.large_file(large))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                std::io::copy(&mut src, &mut z)?;
            }
            z.finish()
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .sync_all()?;
            Ok(())
        })();
        put_in_place(written, &part, dest)
    }

    /// Consistent database snapshot (VACUUM INTO, not a file copy). API-key verifiers excluded.
    /// The full snapshot is built and redacted beside the database, never in `dest`'s folder
    /// (a synced folder would upload it). Only the redacted copy is written there, under a
    /// temporary name, and it never replaces a file at `dest`, even one that appeared meanwhile.
    pub fn backup_database(&self, dest: &Path) -> R<()> {
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(StorageError::Exists(dest.display().to_string()));
        }
        let dir = self.snapshot_dir();
        let snap = dir.join(format!(
            "{SNAPSHOT_PREFIX}{}",
            hex::encode(crate::random_bytes::<8>())
        ));
        let redacted = (|| -> R<()> {
            // The snapshot, then VACUUM's rebuild of it, on the database's volume.
            let size: i64 = self.conn.query_row(
                "SELECT page_count*page_size FROM pragma_page_count, pragma_page_size",
                [],
                |r| r.get(0),
            )?;
            let need = (size.max(0) as u64).saturating_mul(2);
            if fs4::available_space(&dir)
                .map(|a| a < self.reserve_free_bytes.saturating_add(need))
                .unwrap_or(false)
            {
                return Err(StorageError::DiskReserve(self.reserve_free_bytes));
            }
            self.conn
                .execute("VACUUM INTO ?1", [snap.to_string_lossy().as_ref()])?;
            let b = Connection::open(&snap)?;
            b.execute("DELETE FROM api_keys", [])?;
            b.execute("DELETE FROM request_log", [])?;
            b.execute_batch("VACUUM")?;
            // Closing checkpoints any -wal into the file before it is copied.
            b.close().map_err(|(_, e)| e)?;
            Ok(())
        })();
        let res = redacted.and_then(|()| {
            let part = part_path(dest);
            let copied = (|| -> R<()> {
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&part)?;
                std::io::copy(&mut std::fs::File::open(&snap)?, &mut out)?;
                out.sync_all()?;
                Ok(())
            })();
            let res = copied.and_then(|()| place_new(&part, dest));
            if res.is_err() {
                let _ = std::fs::remove_file(&part);
            }
            res
        });
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut s = snap.as_os_str().to_owned();
            s.push(suffix);
            let _ = std::fs::remove_file(s);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// The runs rebuild Windows 8's `$UpCase` file exactly: this is the SHA-256 of the 128 KiB
    /// file whose MD5 ntfs-3g publishes (7ff498a44e45e77374cc7c962b1b92f2).
    #[test]
    fn upcase_is_the_ntfs_table() {
        assert!(
            UPCASE.windows(2).all(|r| r[0].1 < r[1].0),
            "sorted, disjoint"
        );
        let mut h = Sha256::new();
        for u in 0..=u16::MAX {
            h.update(upcase(u).to_le_bytes());
        }
        assert_eq!(
            hex::encode(h.finalize()),
            "41c26bc7a12bdaeb26025c93118697c7e3ef81ee048b00fe5cce2a472e0e0742"
        );
    }

    fn open(d: &Path) -> Workspaces {
        Workspaces::new(
            crate::db::open(&d.join("hub.db")).unwrap(),
            &d.join("workspaces"),
            0,
        )
        .unwrap()
    }

    /// A workspace holding `f.txt` = "old".
    fn setup() -> (tempfile::TempDir, Workspaces, String, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let mut w = open(d.path());
        let id = w.create("a", None).unwrap().id;
        let wdir = w.root.join(&id);
        w.write_file(
            &id,
            "f.txt",
            &mut Cursor::new(b"old".to_vec()),
            "import",
            false,
        )
        .unwrap();
        (d, w, id, wdir)
    }

    /// What a write of `content` to `rel` leaves if the process dies after `renamed` of its
    /// renames (0: none, 1: the old file moved aside, 2: the new one in place too), with its
    /// row `committed` or not.
    fn crash(w: &Workspaces, ws: &str, rel: &str, content: &[u8], renamed: u8, committed: bool) {
        let wdir = w.root.join(ws);
        let tmp = wdir.join(".tmp-uploads");
        std::fs::create_dir_all(&tmp).unwrap();
        let target = wdir.join(rel);
        let sha = hex::encode(Sha256::digest(content));
        let p = Pending {
            rel_path: rel.into(),
            sha256: sha.clone(),
            size: content.len() as u64,
            had_old: target.is_file(),
        };
        std::fs::write(tmp.join("c.txn"), serde_json::to_vec(&p).unwrap()).unwrap();
        std::fs::write(tmp.join("c.new"), content).unwrap();
        if renamed >= 1 && p.had_old {
            std::fs::rename(&target, tmp.join("c.old")).unwrap();
        }
        if renamed >= 2 {
            std::fs::rename(tmp.join("c.new"), &target).unwrap();
        }
        if committed {
            w.conn
                .execute(
                    "INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,?2,?3,?4,'output',0)
                     ON CONFLICT(ws_id,rel_path) DO UPDATE SET size=?3,sha256=?4",
                    params![ws, rel, content.len() as i64, sha],
                )
                .unwrap();
        }
    }

    #[test]
    fn startup_puts_back_what_an_uncommitted_write_replaced() {
        for renamed in 0..=2 {
            let (d, w, id, wdir) = setup();
            crash(&w, &id, "f.txt", b"brand new", renamed, false);
            drop(w);
            let w = open(d.path());
            assert_eq!(
                std::fs::read(wdir.join("f.txt")).unwrap(),
                b"old",
                "{renamed}"
            );
            let i = w.info(&id).unwrap();
            assert_eq!((i.files, i.used_bytes), (1, 3));
            assert!(!wdir.join(".tmp-uploads").exists());
        }
    }

    #[test]
    fn startup_keeps_what_a_committed_write_put_in_place() {
        // renamed = 1: the COMMIT reached the disk but the second rename did not
        for renamed in 1..=2 {
            let (d, w, id, wdir) = setup();
            crash(&w, &id, "f.txt", b"brand new", renamed, true);
            drop(w);
            let w = open(d.path());
            assert_eq!(std::fs::read(wdir.join("f.txt")).unwrap(), b"brand new");
            assert_eq!(w.info(&id).unwrap().used_bytes, 9);
            assert!(!wdir.join(".tmp-uploads").exists());
        }
    }

    #[test]
    fn startup_removes_a_new_file_whose_row_never_committed() {
        let (d, w, id, wdir) = setup();
        crash(&w, &id, "n.txt", b"x", 2, false);
        drop(w);
        let w = open(d.path());
        assert!(!wdir.join("n.txt").exists());
        assert_eq!(w.info(&id).unwrap().files, 1);
    }

    #[test]
    fn a_torn_journal_means_nothing_was_renamed() {
        let (d, w, id, wdir) = setup();
        let tmp = wdir.join(".tmp-uploads");
        std::fs::write(tmp.join("c.txn"), "{\"rel_pa").unwrap();
        std::fs::write(tmp.join("c.new"), "partial").unwrap();
        drop(w);
        let w = open(d.path());
        assert_eq!(std::fs::read(wdir.join("f.txt")).unwrap(), b"old");
        assert_eq!(w.info(&id).unwrap().files, 1);
        assert!(!tmp.exists());
    }

    /// A long-lived instance never restarts: its next write settles first.
    #[test]
    fn the_next_write_settles_a_crashed_one_first() {
        let (_d, mut w, id, wdir) = setup();
        crash(&w, &id, "f.txt", b"brand new", 2, false);
        w.write_file(
            &id,
            "g.txt",
            &mut Cursor::new(b"g".to_vec()),
            "import",
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(wdir.join("f.txt")).unwrap(), b"old");
        assert_eq!(
            std::fs::read_dir(wdir.join(".tmp-uploads"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn placing_a_backup_never_replaces_a_file() {
        let d = tempfile::tempdir().unwrap();
        let (part, dest) = (d.path().join("p"), d.path().join("dest"));
        for place in [place_new as fn(&Path, &Path) -> R<()>, claim_then_rename] {
            std::fs::write(&part, "NEW").unwrap();
            std::fs::write(&dest, "USER DATA").unwrap();
            assert!(matches!(place(&part, &dest), Err(StorageError::Exists(_))));
            assert_eq!(std::fs::read(&dest).unwrap(), b"USER DATA");
            std::fs::remove_file(&dest).unwrap();
            place(&part, &dest).unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), b"NEW");
            assert!(!part.exists());
            std::fs::remove_file(&dest).unwrap();
        }
    }

    #[test]
    fn startup_removes_abandoned_snapshots_only() {
        let d = tempfile::tempdir().unwrap();
        drop(open(d.path()));
        let hours_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
        for name in [".backup-snapshot-0a", ".backup-snapshot-0a-wal", "other"] {
            let f = std::fs::File::create(d.path().join(name)).unwrap();
            f.set_modified(hours_ago).unwrap();
        }
        std::fs::write(d.path().join(".backup-snapshot-0b"), "running").unwrap();
        drop(open(d.path()));
        let mut left: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with("hub.db") && n != "workspaces")
            .collect();
        left.sort();
        assert_eq!(left, [".backup-snapshot-0b", "other"]);
    }
}
