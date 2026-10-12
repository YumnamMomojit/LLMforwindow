//! One SQLite file (WAL) for keys, request metadata, models, workspaces, history.
//! Schema is versioned with `PRAGMA user_version`; each migration runs in a transaction.
use rusqlite::Connection;
use std::path::Path;

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

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let mut c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&mut c)?;
    Ok(c)
}

pub fn schema_version(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0)
}

fn migrate(c: &mut Connection) -> anyhow::Result<()> {
    let cur = schema_version(c) as usize;
    if cur > MIGRATIONS.len() {
        anyhow::bail!(
            "database schema v{cur} is newer than this application (v{})",
            MIGRATIONS.len()
        );
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(cur) {
        let tx = c.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
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
