use semdok_core::storage::{sanitize_rel, StorageError, Workspaces};
use semdok_core::{db, history};
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
        // the temp area under any spelling NTFS resolves to it
        ".TMP-uploads/x",
        ".Tmp-Uploads/x",
        "TMP-UP~1/x",
        // 8.3 short-name aliases of other files
        "PROGRA~1",
        "a/LONGFI~1.TXT",
        "report~2.txt",
    ] {
        assert!(sanitize_rel(bad).is_err(), "should reject {bad:?}");
    }
    for good in [
        "a.txt",
        "dir/sub/file.md",
        "naïve/файл 名前.txt",
        "a b/c",
        "draft~v2.txt",
        "my long report~1.txt",
        ".tmpfile",
    ] {
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

/// NTFS resolves case variants to one file, so the DB must too (checked on any OS).
#[test]
fn case_variants_are_one_file() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let wdir = d.path().join("workspaces").join(&a.id);
    w.write_file(
        &a.id,
        "Dir/ünï.txt",
        &mut Cursor::new(b"one".to_vec()),
        "import",
        false,
    )
    .unwrap();
    for alias in ["dir/ÜNÏ.TXT", "DIR/Ünï.txt"] {
        let e = w
            .write_file(
                &a.id,
                alias,
                &mut Cursor::new(b"two!".to_vec()),
                "import",
                false,
            )
            .unwrap_err();
        assert!(matches!(e, StorageError::Exists(_)), "{alias}: {e}");
    }
    assert_eq!(std::fs::read(wdir.join("Dir/ünï.txt")).unwrap(), b"one");
    // overwrite replaces that one file and row, under the stored spelling
    let f = w
        .write_file(
            &a.id,
            "DIR/ÜNÏ.TXT",
            &mut Cursor::new(b"two!".to_vec()),
            "output",
            true,
        )
        .unwrap();
    assert_eq!(f.rel_path, "Dir/ünï.txt");
    let i = w.info(&a.id).unwrap();
    assert_eq!((i.files, i.used_bytes), (1, 4));
    assert_eq!(std::fs::read(wdir.join("Dir/ünï.txt")).unwrap(), b"two!");
    let mut s = String::new();
    std::io::Read::read_to_string(&mut w.read_file(&a.id, "dir/ünï.TXT").unwrap(), &mut s).unwrap();
    assert_eq!(s, "two!");
    w.delete_file(&a.id, "DIR/ÜNÏ.txt", true).unwrap();
    assert_eq!(w.info(&a.id).unwrap().files, 0);
    assert!(!wdir.join("Dir/ünï.txt").exists());
}

#[test]
fn untracked_file_on_disk_is_not_clobbered_without_overwrite() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let stray = d.path().join("workspaces").join(&a.id).join("stray.txt");
    std::fs::write(&stray, "keep").unwrap();
    let e = w
        .write_file(
            &a.id,
            "stray.txt",
            &mut Cursor::new(b"new".to_vec()),
            "import",
            false,
        )
        .unwrap_err();
    assert!(matches!(e, StorageError::Exists(_)), "{e}");
    assert_eq!(std::fs::read(&stray).unwrap(), b"keep");
    w.write_file(
        &a.id,
        "stray.txt",
        &mut Cursor::new(b"new".to_vec()),
        "import",
        true,
    )
    .unwrap();
    assert_eq!(w.info(&a.id).unwrap().used_bytes, 3);
}

#[test]
fn failed_row_update_never_replaces_the_file() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let wdir = d.path().join("workspaces").join(&a.id);
    w.write_file(
        &a.id,
        "f.txt",
        &mut Cursor::new(b"old".to_vec()),
        "import",
        false,
    )
    .unwrap();
    w.conn
        .execute_batch(
            "CREATE TEMP TRIGGER no_insert BEFORE INSERT ON files BEGIN SELECT RAISE(ABORT,'injected'); END;
             CREATE TEMP TRIGGER no_update BEFORE UPDATE ON files BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .unwrap();
    for (rel, overwrite) in [("new.txt", false), ("f.txt", true)] {
        assert!(w
            .write_file(
                &a.id,
                rel,
                &mut Cursor::new(b"new content".to_vec()),
                "output",
                overwrite
            )
            .is_err());
    }
    assert!(!wdir.join("new.txt").exists());
    assert_eq!(std::fs::read(wdir.join("f.txt")).unwrap(), b"old");
    assert_eq!(
        std::fs::read_dir(wdir.join(".tmp-uploads"))
            .unwrap()
            .count(),
        0
    );
}

/// A directory standing where a tracked file was makes its removal fail on every OS, root included.
#[test]
fn failed_deletes_keep_usage_right_and_can_be_retried() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let wdir = d.path().join("workspaces").join(&a.id);
    for (rel, data) in [("a.txt", "hello"), ("b.txt", "abc")] {
        w.write_file(
            &a.id,
            rel,
            &mut Cursor::new(data.as_bytes().to_vec()),
            "import",
            false,
        )
        .unwrap();
    }
    std::fs::remove_file(wdir.join("b.txt")).unwrap();
    std::fs::create_dir(wdir.join("b.txt")).unwrap();
    std::fs::write(wdir.join("b.txt").join("in-use"), "x").unwrap();

    assert!(w.delete_file(&a.id, "b.txt", true).is_err());
    let i = w.info(&a.id).unwrap();
    assert_eq!((i.files, i.used_bytes), (2, 8));

    assert!(w.delete_workspace(&a.id, true).is_err());
    // still listed (so the delete can be retried), minus the file that did go
    let i = w.info(&a.id).unwrap();
    assert_eq!((i.files, i.used_bytes), (1, 3));
    assert!(!wdir.join("a.txt").exists());

    std::fs::remove_dir_all(wdir.join("b.txt")).unwrap();
    w.delete_workspace(&a.id, true).unwrap();
    assert!(matches!(w.info(&a.id), Err(StorageError::NotFound(_))));
    assert!(!wdir.exists());
}

#[test]
fn export_is_atomic_and_never_lands_under_the_workspaces_root() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let b = w.create("b", None).unwrap();
    let root = d.path().join("workspaces");
    w.write_file(
        &a.id,
        "x.txt",
        &mut Cursor::new(b"hello".to_vec()),
        "output",
        false,
    )
    .unwrap();
    let out = d.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let dest = out.join("e.zip");
    std::fs::write(&dest, "PREVIOUS GOOD EXPORT").unwrap();
    // a tracked file missing from disk makes the export fail part-way
    w.conn.execute("INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,'gone.txt',1,'x','import',0)", [&a.id]).unwrap();
    assert!(w.export_workspace(&a.id, &dest).is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"PREVIOUS GOOD EXPORT");
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        1,
        "temp file left"
    );
    w.conn
        .execute("DELETE FROM files WHERE rel_path='gone.txt'", [])
        .unwrap();
    w.export_workspace(&a.id, &dest).unwrap();
    let mut z = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
    assert_eq!(z.by_name("files/x.txt").unwrap().size(), 5);
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        1,
        "temp file left"
    );

    // any spelling of a place under the workspaces root is refused, before anything is written
    let dotted = root.join("..").join("workspaces").join(&a.id);
    for bad in [
        dotted.join("e.zip"),
        dotted.join("x.txt"),
        root.join(&b.id).join("e.zip"),
        root.join("e.zip"),
    ] {
        let e = w.export_workspace(&a.id, &bad).unwrap_err();
        assert!(matches!(e, StorageError::InvalidPath(_)), "{bad:?}: {e}");
    }
    assert_eq!(
        std::fs::read(root.join(&a.id).join("x.txt")).unwrap(),
        b"hello"
    );
    assert!(!root.join(&a.id).join("e.zip").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join(&a.id), d.path().join("ln")).unwrap();
        let e = w
            .export_workspace(&a.id, &d.path().join("ln").join("e.zip"))
            .unwrap_err();
        assert!(matches!(e, StorageError::InvalidPath(_)), "{e}");
    }
}

/// zip64 is needed past 4 GiB. Slow (deflates 4 GiB), so opt-in:
/// `cargo test -p semdok-core --release --test storage -- --ignored`.
#[test]
#[ignore = "deflates 4 GiB; run with --release -- --ignored"]
fn export_handles_files_over_4_gib() {
    let d = tempfile::tempdir().unwrap();
    let w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let big = (1u64 << 32) + 1;
    // sparse, and registered directly: streaming 4 GiB through write_file adds nothing here
    std::fs::File::create(d.path().join("workspaces").join(&a.id).join("big.bin"))
        .unwrap()
        .set_len(big)
        .unwrap();
    w.conn.execute("INSERT INTO files(ws_id,rel_path,size,sha256,kind,created_at) VALUES(?1,'big.bin',?2,'x','import',0)", rusqlite::params![a.id, big as i64]).unwrap();
    let dest = d.path().join("big.zip");
    w.export_workspace(&a.id, &dest).unwrap();
    let mut z = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
    assert_eq!(z.by_name("files/big.bin").unwrap().size(), big);
}

#[test]
fn failed_backup_leaves_nothing_at_dest_and_can_be_retried() {
    let d = tempfile::tempdir().unwrap();
    let w = ws(d.path());
    semdok_core::keys::create(&w.conn, "k", None).unwrap();
    // the snapshot carries this trigger, so redacting it fails after VACUUM INTO
    w.conn
        .execute_batch("CREATE TRIGGER keep_keys BEFORE DELETE ON api_keys BEGIN SELECT RAISE(ABORT,'injected'); END;")
        .unwrap();
    let out = d.path().join("bk");
    std::fs::create_dir(&out).unwrap();
    let bk = out.join("backup.db");
    assert!(w.backup_database(&bk).is_err());
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "unredacted copy left"
    );
    w.conn.execute_batch("DROP TRIGGER keep_keys").unwrap();
    w.backup_database(&bk).unwrap();
    let names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["backup.db"]);
    let b = rusqlite::Connection::open(&bk).unwrap();
    assert_eq!(
        b.query_row::<i64, _, _>("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
            .unwrap(),
        0
    );
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
    semdok_core::keys::create(&w.conn, "k", None).unwrap();
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
    assert_eq!(db::schema_version(&c), 3);
    drop(c);
    assert_eq!(db::schema_version(&db::open(&p).unwrap()), 3);
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
