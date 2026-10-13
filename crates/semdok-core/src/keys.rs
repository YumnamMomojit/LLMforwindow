//! Integration API keys. Format: `sem_<8 hex id>_<43 char base64url secret>`.
//! Only SHA-256(secret) is stored (secrets are 256-bit random, so a fast hash is adequate);
//! the secret is shown once at creation. Keys carry scopes; the only scope is `inference`.
//! No key can reach administration, files, history, model downloads or process control:
//! those surfaces do not exist on the public gateway at all.
use crate::{now, random_bytes};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const SCOPE_INFERENCE: &str = "inference";
const PREFIX: &str = "sem_";
/// Keys created before the rename to Semdok Edge ("Local AI Hub") start with this; they keep working.
const LEGACY_PREFIX: &str = "lah_";

#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyRecord {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    /// May lie in the future: a rotated key keeps working until then (its grace period).
    /// For an explicitly revoked key it is informational only; `revoked` decides.
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
    /// Explicitly revoked (`revoke`): permanent, whatever the system clock says.
    pub revoked: bool,
}

impl KeyRecord {
    /// Revoked at unix time `t`: explicitly, or because its rotation grace period has ended.
    pub fn is_revoked_at(&self, t: i64) -> bool {
        self.revoked || self.revoked_at.is_some_and(|r| r <= t)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Missing,
    Invalid,
    Revoked,
    Expired,
    Forbidden,
}

fn verifier(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Returns the record and the full token (the only time it exists in plaintext).
pub fn create(
    c: &Connection,
    name: &str,
    expires_at: Option<i64>,
) -> anyhow::Result<(KeyRecord, String)> {
    let name = name.trim();
    if name.is_empty() || name.len() > 80 || name.chars().any(|ch| ch.is_control()) {
        anyhow::bail!("key name must be 1-80 printable characters");
    }
    if expires_at.is_some_and(|e| e <= now()) {
        anyhow::bail!("expiry must be in the future");
    }
    let id = hex::encode(random_bytes::<4>());
    let secret = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let created = now();
    c.execute(
        "INSERT INTO api_keys(id,name,verifier,scopes,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, name, verifier(&secret), SCOPE_INFERENCE, created, expires_at],
    )?;
    let rec = get(c, &id)?.expect("just inserted");
    Ok((rec, format!("{PREFIX}{id}_{secret}")))
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<KeyRecord> {
    let scopes: String = r.get(2)?;
    Ok(KeyRecord {
        id: r.get(0)?,
        name: r.get(1)?,
        scopes: scopes.split(',').map(str::to_string).collect(),
        created_at: r.get(3)?,
        expires_at: r.get(4)?,
        revoked_at: r.get(5)?,
        last_used_at: r.get(6)?,
        revoked: r.get(7)?,
    })
}
const COLS: &str = "id,name,scopes,created_at,expires_at,revoked_at,last_used_at,revoked";

pub fn get(c: &Connection, id: &str) -> anyhow::Result<Option<KeyRecord>> {
    Ok(c.query_row(
        &format!("SELECT {COLS} FROM api_keys WHERE id=?1"),
        [id],
        row,
    )
    .optional()?)
}

pub fn list(c: &Connection) -> anyhow::Result<Vec<KeyRecord>> {
    let mut s = c.prepare(&format!("SELECT {COLS} FROM api_keys ORDER BY created_at"))?;
    let v = s.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

/// Revoke now; this also ends the grace period of a rotated key. Revocation is a flag, not a
/// time, so moving the system clock back later cannot revive the key. `false` if there is no
/// such key or it was already revoked with this function.
pub fn revoke(c: &Connection, id: &str) -> anyhow::Result<bool> {
    Ok(c.execute(
        "UPDATE api_keys SET revoked=1, revoked_at=MIN(COALESCE(revoked_at, ?2), ?2)
         WHERE id=?1 AND revoked=0",
        params![id, now()],
    )? > 0)
}

/// Issue a replacement with the same name; the old key keeps working for `grace_secs`
/// (or until its own expiry, if that comes first).
///
/// The replacement keeps the old key's expiry, i.e. the remaining validity measured from now
/// (none if the old key had none): rotation replaces the secret, it does not renew the key.
/// The end of the grace period is recorded as a future `revoked_at` (without the `revoked`
/// flag), so the old key's expiry is never shortened and cannot leak into a later replacement.
/// Expired, revoked and already-rotated keys are refused: create a new key, or rotate the
/// replacement.
pub fn rotate(c: &Connection, id: &str, grace_secs: i64) -> anyhow::Result<(KeyRecord, String)> {
    if grace_secs < 0 {
        anyhow::bail!("grace period must not be negative");
    }
    let old = get(c, id)?.ok_or_else(|| anyhow::anyhow!("no such key"))?;
    let t = now();
    if old.is_revoked_at(t) {
        anyhow::bail!("key is already revoked");
    }
    if let Some(e) = old.expires_at.filter(|&e| e <= t) {
        anyhow::bail!(
            "key expired at {e} (unix time); an expired key cannot be rotated, create a new key"
        );
    }
    if let Some(r) = old.revoked_at {
        anyhow::bail!(
            "key was already rotated and stops working at {} (unix time); \
             rotate its replacement (same name) instead",
            old.expires_at.map_or(r, |e| e.min(r))
        );
    }
    let (rec, tok) = create(c, &old.name, old.expires_at)?;
    c.execute(
        "UPDATE api_keys SET revoked_at=?2 WHERE id=?1 AND revoked=0",
        params![id, t.saturating_add(grace_secs)],
    )?;
    Ok((rec, tok))
}

/// Verify an `Authorization` header value. `required_scope` is checked after authentication.
pub fn verify(
    c: &Connection,
    header: Option<&str>,
    required_scope: &str,
) -> Result<KeyRecord, AuthError> {
    let token = header
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::trim)
        .ok_or(AuthError::Missing)?;
    let rest = token
        .strip_prefix(PREFIX)
        .or_else(|| token.strip_prefix(LEGACY_PREFIX))
        .ok_or(AuthError::Invalid)?;
    if rest.len() < 10 || rest.as_bytes()[8] != b'_' || !rest.is_char_boundary(8) {
        return Err(AuthError::Invalid);
    }
    let (id, secret) = (&rest[..8], &rest[9..]);
    let stored: Option<String> = c
        .query_row("SELECT verifier FROM api_keys WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|_| AuthError::Invalid)?;
    // Always hash, so unknown ids and wrong secrets cost the same.
    let got = verifier(secret);
    let want = stored.clone().unwrap_or_else(|| "0".repeat(64));
    let ok: bool = got.as_bytes().ct_eq(want.as_bytes()).into();
    if stored.is_none() || !ok {
        return Err(AuthError::Invalid);
    }
    let rec = get(c, id).ok().flatten().ok_or(AuthError::Invalid)?;
    if rec.is_revoked_at(now()) {
        return Err(AuthError::Revoked);
    }
    if rec.expires_at.is_some_and(|e| e <= now()) {
        return Err(AuthError::Expired);
    }
    if !rec.scopes.iter().any(|s| s == required_scope) {
        return Err(AuthError::Forbidden);
    }
    let _ = c.execute(
        "UPDATE api_keys SET last_used_at=?2 WHERE id=?1",
        params![id, now()],
    );
    Ok(rec)
}
