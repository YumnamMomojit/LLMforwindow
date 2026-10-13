//! Local chat history with an explicit retention policy and a no-history mode.
//! Messages are stored as inert text; rendering is the UI's job and must never execute them.
use crate::{db, now};
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

pub fn enabled(c: &Connection) -> bool {
    db::get_setting(c, "history.enabled")
        .map(|v| v != "0")
        .unwrap_or(true)
}
pub fn set_enabled(c: &Connection, on: bool) -> anyhow::Result<()> {
    db::set_setting(c, "history.enabled", if on { "1" } else { "0" })
}
/// None = keep until deleted.
pub fn retention_days(c: &Connection) -> Option<i64> {
    db::get_setting(c, "history.retention_days")
        .and_then(|v| v.parse().ok())
        .filter(|d| *d > 0)
}
pub fn set_retention_days(c: &Connection, days: Option<i64>) -> anyhow::Result<()> {
    db::set_setting(
        c,
        "history.retention_days",
        &days.map(|d| d.to_string()).unwrap_or_default(),
    )
}

/// Returns None (and stores nothing) when history is disabled.
pub fn create_conversation(
    c: &Connection,
    title: &str,
    system_prompt: &str,
    model: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if !enabled(c) {
        return Ok(None);
    }
    let id = hex::encode(crate::random_bytes::<8>());
    c.execute("INSERT INTO conversations(id,title,system_prompt,model,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5)",
        params![id, title, system_prompt, model, now()])?;
    Ok(Some(id))
}

pub fn append(c: &mut Connection, conv: &str, role: &str, content: &str) -> anyhow::Result<()> {
    if !enabled(c) {
        return Ok(());
    }
    if !["system", "user", "assistant"].contains(&role) {
        anyhow::bail!("bad role");
    }
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO messages(conv_id,role,content,created_at) VALUES(?1,?2,?3,?4)",
        params![conv, role, content, now()],
    )?;
    tx.execute(
        "UPDATE conversations SET updated_at=?2 WHERE id=?1",
        params![conv, now()],
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub model: Option<String>,
    pub updated_at: i64,
    pub messages: i64,
}

/// Newest first.
pub fn list_conversations(c: &Connection) -> anyhow::Result<Vec<Conversation>> {
    let v = c
        .prepare(
            "SELECT c.id,c.title,c.model,c.updated_at,COUNT(m.id) FROM conversations c
             LEFT JOIN messages m ON m.conv_id=c.id GROUP BY c.id ORDER BY c.updated_at DESC, c.id",
        )?
        .query_map([], |r| {
            Ok(Conversation {
                id: r.get(0)?,
                title: r.get(1)?,
                model: r.get(2)?,
                updated_at: r.get(3)?,
                messages: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

pub fn messages(c: &Connection, conv: &str) -> anyhow::Result<Vec<Message>> {
    let v = c
        .prepare("SELECT role,content,created_at FROM messages WHERE conv_id=?1 ORDER BY id")?
        .query_map([conv], |r| {
            Ok(Message {
                role: r.get(0)?,
                content: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

pub fn delete_conversation(c: &Connection, conv: &str) -> anyhow::Result<bool> {
    Ok(c.execute("DELETE FROM conversations WHERE id=?1", [conv])? > 0)
}

/// Apply the retention policy; returns conversations removed.
pub fn purge_expired(c: &Connection) -> anyhow::Result<usize> {
    let Some(d) = retention_days(c) else {
        return Ok(0);
    };
    Ok(c.execute(
        "DELETE FROM conversations WHERE updated_at < ?1",
        [now() - d * 86400],
    )?)
}

pub fn purge_request_log(c: &Connection, days: i64) -> anyhow::Result<usize> {
    Ok(c.execute(
        "DELETE FROM request_log WHERE ts < ?1",
        [now() - days * 86400],
    )?)
}

/// Plain-text export (no HTML), safe to open in any editor.
pub fn export_text(c: &Connection, conv: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    for m in messages(c, conv)? {
        out.push_str(&format!("[{}]\n{}\n\n", m.role, m.content));
    }
    Ok(out)
}
