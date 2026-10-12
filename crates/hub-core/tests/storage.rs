use hub_core::storage::{sanitize_rel, StorageError, Workspaces};
use hub_core::{db, history};
use std::io::Cursor;

fn ws(dir: &std::path::Path) -> Workspaces {
    Workspaces::new(
        db::open(&dir.join("hub.db")).unwrap(),
        &dir.join("workspaces"),
        0,
    )
    .unwrap()
}

#[test]
fn traversal_and_unsafe_names_rejected() {
    for bad in [
        "../x",
        "a/../../x",
        "/etc/passwd",
        "\\windows\\x",
        "C:\\x",
        "a/b:stream",
        "CON",
        "nul.txt",
        "a/com1.log",
        "trail.",
        "sp ",
        "a//b",
        "",
        ".",
        "a/./b",
        "x<y",
        "q?",
        ".tmp-1/x",
        "a\u{0}b",
    ] {
        assert!(sanitize_rel(bad).is_err(), "should reject {bad:?}");
    }
    for good in ["a.txt", "dir/sub/file.md", "naïve/файл 名前.txt", "a b/c"] {
        assert!(sanitize_rel(good).is_ok(), "should accept {good:?}");
    }
}

#[test]
fn write_read_quota_overwrite_and_unicode() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("Проект 名前", Some(100)).unwrap();
    assert!(matches!(
        w.create("Проект 名前", None),
        Err(StorageError::Exists(_))
    ));
    let f = w
        .write_file(
            &a.id,
            "dir/ünï.txt",
            &mut Cursor::new(vec![b'x'; 60]),
            "import",
            false,
        )
        .unwrap();
    assert_eq!(f.size, 60);
    assert!(matches!(
        w.write_file(
            &a.id,
            "dir/ünï.txt",
            &mut Cursor::new(b"y".to_vec()),
            "import",
            false
        ),
        Err(StorageError::Exists(_))
    ));
    // quota enforced DURING the write, and no partial file remains
    let e = w
        .write_file(
            &a.id,
            "big.bin",
            &mut Cursor::new(vec![0u8; 50]),
            "import",
            false,
        )
        .unwrap_err();
    assert!(matches!(e, StorageError::QuotaExceeded(100)), "{e}");
    assert!(w
        .list_files(&a.id)
        .unwrap()
        .iter()
        .all(|f| f.rel_path != "big.bin"));
    // overwrite replaces usage accounting rather than adding
    w.write_file(
        &a.id,
        "dir/ünï.txt",
        &mut Cursor::new(vec![b'z'; 90]),
        "output",
        true,
    )
    .unwrap();
    assert_eq!(w.info(&a.id).unwrap().used_bytes, 90);
    let mut s = String::new();
    std::io::Read::read_to_string(&mut w.read_file(&a.id, "dir/ünï.txt").unwrap(), &mut s).unwrap();
    assert_eq!(s.len(), 90);
}

#[test]
fn free_space_reserve_blocks_writes() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Workspaces::new(
        db::open(&d.path().join("h.db")).unwrap(),
        &d.path().join("w"),
        u64::MAX / 2,
    )
    .unwrap();
    let a = w.create("a", None).unwrap();
    let e = w
        .write_file(&a.id, "f", &mut Cursor::new(vec![0u8; 10]), "import", false)
        .unwrap_err();
    assert!(matches!(e, StorageError::DiskReserve(_)), "{e}");
}

#[cfg(unix)]
#[test]
fn symlink_escape_blocked() {
    let d = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let wdir = d.path().join("workspaces").join(&a.id);
    std::os::unix::fs::symlink(outside.path(), wdir.join("link")).unwrap();
    let e = w
        .write_file(
            &a.id,
            "link/pwn.txt",
            &mut Cursor::new(b"x".to_vec()),
            "import",
            false,
        )
        .unwrap_err();
    assert!(matches!(e, StorageError::Escape(_)), "{e}");
    assert!(!outside.path().join("pwn.txt").exists());
    // symlinked file reads are refused too
    std::fs::write(outside.path().join("secret"), "s").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), wdir.join("f")).unwrap();
    w.conn.execute("INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,'f',1,'x','import',0)", [&a.id]).unwrap();
    assert!(matches!(
        w.read_file(&a.id, "f"),
        Err(StorageError::Escape(_))
    ));
}

#[test]
fn destructive_ops_need_confirmation_and_export_backup_exclude_secrets() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    w.write_file(
        &a.id,
        "x/y.txt",
        &mut Cursor::new(b"hello".to_vec()),
        "output",
        false,
    )
    .unwrap();
    assert!(matches!(
        w.delete_file(&a.id, "x/y.txt", false),
        Err(StorageError::NotConfirmed)
    ));
    assert!(matches!(
        w.delete_workspace(&a.id, false),
        Err(StorageError::NotConfirmed)
    ));
    let zip_path = d.path().join("out.zip");
    w.export_workspace(&a.id, &zip_path).unwrap();
    let mut z = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
    let names: Vec<_> = (0..z.len())
        .map(|i| z.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(
        names.contains(&"files/x/y.txt".to_string())
            && names.contains(&"metadata.json".to_string()),
        "{names:?}"
    );
    // export into the workspace itself is refused
    assert!(w
        .export_workspace(
            &a.id,
            &d.path().join("workspaces").join(&a.id).join("e.zip")
        )
        .is_err());
    // key verifiers are not in a backup
    hub_core::keys::create(&w.conn, "k", None).unwrap();
    let bk = d.path().join("backup.db");
    w.backup_database(&bk).unwrap();
    let b = rusqlite::Connection::open(&bk).unwrap();
    assert_eq!(
        b.query_row::<i64, _, _>("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        b.query_row::<i64, _, _>("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))
            .unwrap(),
        1
    );
    w.delete_file(&a.id, "x/y.txt", true).unwrap();
    w.delete_workspace(&a.id, true).unwrap();
    assert!(!d.path().join("workspaces").join(&a.id).exists());
}

#[test]
fn interrupted_write_leftovers_cleaned_on_startup() {
    let d = tempfile::tempdir().unwrap();
    let id;
    {
        let w = ws(d.path());
        id = w.create("a", None).unwrap().id;
        let t = d.path().join("workspaces").join(&id).join(".tmp-uploads");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("partial"), "half").unwrap();
    }
    let _w = ws(d.path());
    assert!(!d
        .path()
        .join("workspaces")
        .join(&id)
        .join(".tmp-uploads")
        .exists());
}

#[test]
fn history_modes_and_retention() {
    let d = tempfile::tempdir().unwrap();
    let mut c = db::open(&d.path().join("h.db")).unwrap();
    let id = history::create_conversation(&c, "t", "be brief", Some("m"))
        .unwrap()
        .unwrap();
    history::append(&mut c, &id, "user", "<script>alert(1)</script>").unwrap();
    assert_eq!(
        history::export_text(&c, &id).unwrap(),
        "[user]\n<script>alert(1)</script>\n\n"
    );
    history::set_enabled(&c, false).unwrap();
    assert!(history::create_conversation(&c, "t2", "", None)
        .unwrap()
        .is_none());
    history::append(&mut c, &id, "user", "not stored").unwrap();
    assert_eq!(history::messages(&c, &id).unwrap().len(), 1);
    history::set_enabled(&c, true).unwrap();
    history::set_retention_days(&c, Some(1)).unwrap();
    c.execute(
        "UPDATE conversations SET updated_at = updated_at - 200000",
        [],
    )
    .unwrap();
    assert_eq!(history::purge_expired(&c).unwrap(), 1);
    assert!(history::messages(&c, &id).unwrap().is_empty());
}

#[test]
fn migrations_idempotent_and_future_schema_refused() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("m.db");
    let c = db::open(&p).unwrap();
    assert_eq!(db::schema_version(&c), 2);
    drop(c);
    assert_eq!(db::schema_version(&db::open(&p).unwrap()), 2);
    rusqlite::Connection::open(&p)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    assert!(db::open(&p).is_err());
}

/// Runs on Windows CI only: an NTFS junction inside a workspace must not be followed.
#[cfg(windows)]
#[test]
fn junction_escape_blocked() {
    let d = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let link = d.path().join("workspaces").join(&a.id).join("junc");
    let st = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .status()
        .unwrap();
    assert!(st.success());
    let e = w
        .write_file(
            &a.id,
            "junc/pwn.txt",
            &mut Cursor::new(b"x".to_vec()),
            "import",
            false,
        )
        .unwrap_err();
    assert!(matches!(e, StorageError::Escape(_)), "{e}");
    assert!(!outside.path().join("pwn.txt").exists());
}
