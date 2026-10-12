//! One SQLite file (WAL) for keys, request metadata, models, workspaces, history.
//! Schema is versioned with `PRAGMA user_version`; each migration runs in a transaction.
use rusqlite::{Connection, ErrorCode, TransactionBehavior};
use std::path::Path;
use std::time::{Duration, Instant};

const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE api_keys(
        id TEXT PRIMARY KEY, name TEXT NOT NULL, verifier TEXT NOT NULL,
        scopes TEXT NOT NULL, created_at INTEGER NOT NULL,
        expires_at INTEGER, revoked_at INTEGER, last_used_at INTEGER);
     CREATE TABLE request_log(
        id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, key_id TEXT, model TEXT,
        status INTEGER NOT NULL, outcome TEXT NOT NULL, stream INTEGER NOT NULL,
        queue_ms INTEGER, first_byte_ms INTEGER, total_ms INTEGER,
        prompt_tokens INTEGER, completion_tokens INTEGER);
     CREATE INDEX request_log_ts ON request_log(ts);
     CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE models(
        id TEXT PRIMARY KEY, path TEXT NOT NULL, model_type TEXT NOT NULL,
        weights_bytes INTEGER NOT NULL, added_at INTEGER NOT NULL);",
    // v2
    "CREATE TABLE workspaces(
        id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, quota_bytes INTEGER, created_at INTEGER NOT NULL);
     CREATE TABLE files(
        id INTEGER PRIMARY KEY, ws_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        rel_path TEXT NOT NULL, size INTEGER NOT NULL, sha256 TEXT NOT NULL,
        kind TEXT NOT NULL, created_at INTEGER NOT NULL, UNIQUE(ws_id, rel_path));
     CREATE TABLE conversations(
        id TEXT PRIMARY KEY, title TEXT NOT NULL, system_prompt TEXT NOT NULL DEFAULT '',
        model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
     CREATE TABLE messages(
        id INTEGER PRIMARY KEY, conv_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        role TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL);",
];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let mut c = Connection::open(path)?;
    c.busy_timeout(BUSY_TIMEOUT)?;
    set_wal(&c)?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&mut c)?;
    Ok(c)
}

/// Switching a new file to WAL needs an exclusive lock, and SQLite reports BUSY without
/// waiting (deadlock avoidance) when several processes open it at the same moment: retry.
fn set_wal(c: &Connection) -> rusqlite::Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match c.pragma_update(None, "journal_mode", "WAL") {
            Err(e)
                if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            r => return r,
        }
    }
}

pub fn schema_version(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Current `user_version`, refusing a schema newer than this build. Unlike `schema_version`,
/// a read error is reported rather than taken for v0 (which would re-run applied migrations).
fn checked_version(c: &Connection) -> anyhow::Result<usize> {
    let cur = c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if cur > MIGRATIONS.len() {
        anyhow::bail!(
            "database schema v{cur} is newer than this application (v{})",
            MIGRATIONS.len()
        );
    }
    Ok(cur)
}

fn migrate(c: &mut Connection) -> anyhow::Result<()> {
    // Usual case: already current, no write lock needed.
    if checked_version(c)? == MIGRATIONS.len() {
        return Ok(());
    }
    // Take the write lock *before* reading the version (BEGIN IMMEDIATE) and re-read it for
    // every step: another process opening the same database at the same time (first start, or
    // `serve` plus a CLI command after an upgrade) may have applied the migration meanwhile.
    loop {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cur = checked_version(&tx)?;
        let Some(sql) = MIGRATIONS.get(cur) else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (cur + 1) as i64)?;
        tx.commit()?;
    }
}

pub fn get_setting(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .ok()
}

pub fn set_setting(c: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        [key, value],
    )?;
    Ok(())
}
