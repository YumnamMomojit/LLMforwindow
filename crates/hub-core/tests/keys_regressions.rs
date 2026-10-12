//! Regression tests for key rotation, the internal engine key store and schema migrations.
use hub_core::secrets::{self, FileSecrets, SecretStore};
use hub_core::{db, keys};
use rusqlite::Connection;
use std::sync::{Arc, Barrier};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn auth(c: &Connection, tok: &str) -> Result<keys::KeyRecord, keys::AuthError> {
    keys::verify(c, Some(&format!("Bearer {tok}")), keys::SCOPE_INFERENCE)
}

fn key_db() -> (tempfile::TempDir, Connection) {
    let d = tempfile::tempdir().unwrap();
    let c = db::open(&d.path().join("k.db")).unwrap();
    (d, c)
}

#[test]
fn rotating_an_expired_key_is_refused_and_issues_nothing() {
    let (_d, c) = key_db();
    let (rec, _) = keys::create(&c, "ci", Some(now() + 3600)).unwrap();
    c.execute(
        "UPDATE api_keys SET expires_at=?2 WHERE id=?1",
        rusqlite::params![rec.id, now() - 60],
    )
    .unwrap();
    let e = keys::rotate(&c, &rec.id, 24 * 3600).unwrap_err();
    assert!(e.to_string().contains("expired"), "{e}");
    assert_eq!(keys::list(&c).unwrap().len(), 1, "no dead replacement");
}

#[test]
fn rotation_keeps_the_original_expiry_and_cannot_chain_off_a_grace_period() {
    let (_d, c) = key_db();
    let exp = now() + 30 * 86400;
    let (a, a_tok) = keys::create(&c, "ci", Some(exp)).unwrap();
    let (b, b_tok) = keys::rotate(&c, &a.id, 3600).unwrap();
    assert_eq!(b.expires_at, Some(exp));
    // The old key's own expiry is untouched; its grace period ends via `revoked_at`.
    let a2 = keys::get(&c, &a.id).unwrap().unwrap();
    assert_eq!(a2.expires_at, Some(exp));
    assert!(a2
        .revoked_at
        .is_some_and(|r| r > now() && r <= now() + 3600));
    assert!(auth(&c, &a_tok).is_ok(), "grace period");
    assert!(auth(&c, &b_tok).is_ok());
    // Rotating A again (e.g. B's secret was lost) would chain off A's grace period: refused.
    let e = keys::rotate(&c, &a.id, 3600).unwrap_err();
    assert!(e.to_string().contains("already rotated"), "{e}");
    assert_eq!(keys::list(&c).unwrap().len(), 2);
    // Rotating the replacement keeps the original expiry, not A's grace end.
    let (c2, c_tok) = keys::rotate(&c, &b.id, 0).unwrap();
    assert_eq!(c2.expires_at, Some(exp));
    assert_eq!(auth(&c, &b_tok).unwrap_err(), keys::AuthError::Revoked);
    assert!(auth(&c, &c_tok).is_ok());
}

#[test]
fn rotation_of_a_non_expiring_key_never_gains_an_expiry() {
    let (_d, c) = key_db();
    let (a, _) = keys::create(&c, "svc", None).unwrap();
    let (b, _) = keys::rotate(&c, &a.id, 3600).unwrap();
    assert_eq!(b.expires_at, None);
    assert_eq!(keys::get(&c, &a.id).unwrap().unwrap().expires_at, None);
    assert!(keys::rotate(&c, &a.id, 3600).is_err());
    let (c2, _) = keys::rotate(&c, &b.id, 3600).unwrap();
    assert_eq!(c2.expires_at, None);
}

#[test]
fn rotation_rejects_negative_grace_and_does_not_extend_the_old_key() {
    let (_d, c) = key_db();
    let soon = now() + 60;
    let (a, _) = keys::create(&c, "ci", Some(soon)).unwrap();
    assert!(keys::rotate(&c, &a.id, -5 * 3600).is_err());
    assert_eq!(keys::list(&c).unwrap().len(), 1);
    assert_eq!(keys::get(&c, &a.id).unwrap().unwrap().revoked_at, None);
    // A grace period longer than the remaining validity does not extend the old key.
    let (b, _) = keys::rotate(&c, &a.id, 24 * 3600).unwrap();
    assert_eq!(b.expires_at, Some(soon));
    assert_eq!(
        keys::get(&c, &a.id).unwrap().unwrap().expires_at,
        Some(soon)
    );
}

#[test]
fn revoke_ends_a_rotation_grace_period() {
    let (_d, c) = key_db();
    let (a, a_tok) = keys::create(&c, "ci", None).unwrap();
    let (_, b_tok) = keys::rotate(&c, &a.id, 3600).unwrap();
    assert!(auth(&c, &a_tok).is_ok());
    assert!(keys::revoke(&c, &a.id).unwrap());
    assert_eq!(auth(&c, &a_tok).unwrap_err(), keys::AuthError::Revoked);
    assert!(!keys::revoke(&c, &a.id).unwrap(), "already revoked");
    assert!(auth(&c, &b_tok).is_ok());
}

/// Moves a key's `revoked_at` into the future: the stand-in for the system clock being
/// stepped back after the revocation (e.g. a fast clock corrected by time sync).
fn clock_steps_back(c: &Connection, id: &str) {
    c.execute(
        "UPDATE api_keys SET revoked_at=?2 WHERE id=?1",
        rusqlite::params![id, now() + 3600],
    )
    .unwrap();
}

#[test]
fn explicit_revocation_survives_the_clock_moving_back() {
    let (_d, c) = key_db();
    let (a, a_tok) = keys::create(&c, "leaked", None).unwrap();
    assert!(keys::revoke(&c, &a.id).unwrap());
    clock_steps_back(&c, &a.id);
    assert_eq!(auth(&c, &a_tok).unwrap_err(), keys::AuthError::Revoked);
    let rec = keys::get(&c, &a.id).unwrap().unwrap();
    assert!(rec.revoked && rec.is_revoked_at(now()));
    let e = keys::rotate(&c, &a.id, 3600).unwrap_err();
    assert!(e.to_string().contains("already revoked"), "{e}");
    assert!(!keys::revoke(&c, &a.id).unwrap(), "already revoked");
    assert_eq!(keys::list(&c).unwrap().len(), 1);

    // Revoking a rotated key during its grace period is just as permanent.
    let (b, b_tok) = keys::create(&c, "ci", None).unwrap();
    let (_, b2_tok) = keys::rotate(&c, &b.id, 3600).unwrap();
    assert!(keys::revoke(&c, &b.id).unwrap());
    clock_steps_back(&c, &b.id);
    assert_eq!(auth(&c, &b_tok).unwrap_err(), keys::AuthError::Revoked);
    assert!(auth(&c, &b2_tok).is_ok());
}

#[test]
fn rotation_grace_is_not_a_revocation() {
    let (_d, c) = key_db();
    let (a, a_tok) = keys::create(&c, "ci", None).unwrap();
    let (_, b_tok) = keys::rotate(&c, &a.id, 3600).unwrap();
    let a2 = keys::get(&c, &a.id).unwrap().unwrap();
    assert!(!a2.revoked && !a2.is_revoked_at(now()));
    assert!(a2.revoked_at.is_some_and(|r| r > now()));
    assert!(auth(&c, &a_tok).is_ok(), "grace period");
    // The grace period ends by the clock.
    c.execute(
        "UPDATE api_keys SET revoked_at=?2 WHERE id=?1",
        rusqlite::params![a.id, now() - 1],
    )
    .unwrap();
    assert_eq!(auth(&c, &a_tok).unwrap_err(), keys::AuthError::Revoked);
    assert!(auth(&c, &b_tok).is_ok());
}

/// Turns a freshly created database back into the given older schema version.
fn downgrade(c: &Connection, to: i64) {
    if to < 3 {
        c.execute_batch("ALTER TABLE api_keys DROP COLUMN revoked;")
            .unwrap();
    }
    if to < 2 {
        c.execute_batch(
            "DROP TABLE messages; DROP TABLE conversations; DROP TABLE files;
             DROP TABLE workspaces;",
        )
        .unwrap();
    }
    c.pragma_update(None, "user_version", to).unwrap();
}

#[test]
fn migration_from_v2_keeps_revoked_keys_revoked_and_grace_periods_running() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("v2.db");
    let (revoked, rotated, grace_over, active);
    {
        let c = db::open(&p).unwrap();
        revoked = keys::create(&c, "revoked", None).unwrap();
        rotated = keys::create(&c, "rotated", None).unwrap();
        grace_over = keys::create(&c, "grace-over", None).unwrap();
        active = keys::create(&c, "active", None).unwrap();
        downgrade(&c, 2);
        // v2 kept both an explicit revocation and a rotation grace end in `revoked_at`.
        for (id, at) in [
            (&revoked.0.id, now() - 60),
            (&rotated.0.id, now() + 3600),
            (&grace_over.0.id, now() - 1),
        ] {
            c.execute(
                "UPDATE api_keys SET revoked_at=?2 WHERE id=?1",
                rusqlite::params![id, at],
            )
            .unwrap();
        }
    }
    let c = db::open(&p).unwrap();
    assert_eq!(db::schema_version(&c), 3);
    let get = |id: &str| keys::get(&c, id).unwrap().unwrap();
    assert!(get(&revoked.0.id).revoked);
    assert!(get(&grace_over.0.id).revoked);
    assert!(!get(&rotated.0.id).revoked);
    assert!(!get(&active.0.id).revoked);
    // Revoked before the upgrade: still revoked, now independently of the clock.
    clock_steps_back(&c, &revoked.0.id);
    assert_eq!(auth(&c, &revoked.1).unwrap_err(), keys::AuthError::Revoked);
    clock_steps_back(&c, &grace_over.0.id);
    assert_eq!(
        auth(&c, &grace_over.1).unwrap_err(),
        keys::AuthError::Revoked
    );
    // A grace period running at the upgrade keeps running, and the key is still "rotated".
    assert!(auth(&c, &rotated.1).is_ok());
    let e = keys::rotate(&c, &rotated.0.id, 3600).unwrap_err();
    assert!(e.to_string().contains("already rotated"), "{e}");
    assert!(auth(&c, &active.1).is_ok());
    assert!(keys::revoke(&c, &rotated.0.id).unwrap());
    assert_eq!(auth(&c, &rotated.1).unwrap_err(), keys::AuthError::Revoked);
}

/// The only file in the secrets directory (its extension differs per platform).
fn only_file(dir: &std::path::Path) -> std::path::PathBuf {
    let v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(v.len(), 1, "no temp file left behind: {v:?}");
    v.into_iter().next().unwrap()
}

#[test]
fn internal_key_is_stable_and_a_corrupt_file_is_reported_not_replaced() {
    let d = tempfile::tempdir().unwrap();
    let store = FileSecrets::new(d.path()).unwrap();
    let k = secrets::internal_key(&store).unwrap();
    assert_eq!(k.len(), 64);
    assert!(k.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(secrets::internal_key(&store).unwrap(), k);
    let f = only_file(d.path());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Truncated by a crash (empty), or garbage (on Windows: an undecryptable DPAPI blob).
    for bad in [&b""[..], &b"not a key"[..]] {
        std::fs::write(&f, bad).unwrap();
        let e = format!("{:#}", secrets::internal_key(&store).unwrap_err());
        assert!(e.contains(&f.display().to_string()), "{e}");
        assert!(e.contains("delete that file"), "{e}");
        assert_eq!(std::fs::read(&f).unwrap(), bad, "must not be replaced");
    }
    // The documented reset: delete the file and a fresh key is generated.
    store.delete("internal_engine_key").unwrap();
    let k2 = secrets::internal_key(&store).unwrap();
    assert_ne!(k2, k);
    assert_eq!(secrets::internal_key(&store).unwrap(), k2);
}

/// Several processes (here: threads with their own connections) opening the same database at
/// the same moment, fresh or one schema version behind, must all succeed instead of failing
/// with "table ... already exists" or "database is locked".
#[test]
fn concurrent_opens_migrate_once() {
    for round in 0..40 {
        let d = tempfile::tempdir().unwrap();
        let p = Arc::new(d.path().join("race.db"));
        if round % 2 == 1 {
            // A v1 database, as left by an older build.
            downgrade(&db::open(&p).unwrap(), 1);
        }
        let n = 4;
        let gate = Arc::new(Barrier::new(n));
        let hs: Vec<_> = (0..n)
            .map(|_| {
                let (p, gate) = (p.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    db::open(&p).map(|c| db::schema_version(&c))
                })
            })
            .collect();
        for h in hs {
            let v = h.join().unwrap();
            assert_eq!(v.as_ref().ok(), Some(&3), "round {round}: {v:?}");
        }
    }
}
