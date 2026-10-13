//! Storage regressions: a failed COMMIT, NTFS name identity, export destinations, backups.
use semdok_core::db;
use semdok_core::storage::{StorageError, Workspaces};
use std::io::{Cursor, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn ws(dir: &Path) -> Workspaces {
    Workspaces::new(
        db::open(&dir.join("hub.db")).unwrap(),
        &dir.join("workspaces"),
        0,
    )
    .unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn failed_commit_leaves_disk_and_row_as_they_were() {
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
    // A deferred foreign-key violation makes the COMMIT itself fail, after both renames.
    w.conn
        .execute_batch(
            "CREATE TEMP TABLE p(id INTEGER PRIMARY KEY);
             CREATE TEMP TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TEMP TRIGGER fi AFTER INSERT ON main.files BEGIN INSERT INTO c VALUES(1); END;
             CREATE TEMP TRIGGER fu AFTER UPDATE ON main.files BEGIN INSERT INTO c VALUES(1); END;",
        )
        .unwrap();
    for (rel, overwrite) in [("f.txt", true), ("n.txt", false)] {
        let e = w
            .write_file(
                &a.id,
                rel,
                &mut Cursor::new(b"brand new content".to_vec()),
                "output",
                overwrite,
            )
            .unwrap_err();
        assert!(matches!(e, StorageError::Db(_)), "{rel}: {e}");
    }
    assert_eq!(std::fs::read(wdir.join("f.txt")).unwrap(), b"old");
    assert!(!wdir.join("n.txt").exists());
    let i = w.info(&a.id).unwrap();
    assert_eq!((i.files, i.used_bytes), (1, 3));
    assert_eq!(names(&wdir.join(".tmp-uploads")), Vec::<String>::new());

    w.conn
        .execute_batch("DROP TRIGGER fi; DROP TRIGGER fu;")
        .unwrap();
    w.write_file(
        &a.id,
        "f.txt",
        &mut Cursor::new(b"brand new content".to_vec()),
        "output",
        true,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(wdir.join("f.txt")).unwrap(),
        b"brand new content"
    );
    let i = w.info(&a.id).unwrap();
    assert_eq!((i.files, i.used_bytes), (1, 17));
}

/// Identity follows NTFS's `$UpCase` table, not Unicode's uppercase (checked on any OS).
#[test]
fn names_are_one_file_exactly_when_ntfs_says_so() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let id = w.create("a", None).unwrap().id;
    let wdir = d.path().join("workspaces").join(&id);
    let mut put = |rel: &str, data: &str, overwrite: bool| {
        w.write_file(
            &id,
            rel,
            &mut Cursor::new(data.as_bytes().to_vec()),
            "import",
            overwrite,
        )
    };
    // Distinct names on NTFS: the second is a new file, the first keeps its content.
    let distinct = [
        ("Kasim.txt", "Kasım.txt"),
        ("5µm.txt", "5μm.txt"),
        ("s.txt", "ſ.txt"),
        ("ǅ.txt", "Ǆ.txt"),
        ("ა.txt", "Ა.txt"),
        ("λόγος.txt", "λόγοσ.txt"),
        ("\u{10428}.txt", "\u{10400}.txt"),
        ("ß.txt", "ẞ.txt"),
    ];
    for (first, second) in distinct {
        put(first, "keep me", false).unwrap();
        assert_eq!(put(second, "other", true).unwrap().rel_path, second);
        assert_eq!(std::fs::read(wdir.join(first)).unwrap(), b"keep me");
        assert_eq!(std::fs::read(wdir.join(second)).unwrap(), b"other");
    }
    // One file on NTFS: refused without overwrite; overwrite keeps the stored spelling.
    let same = [
        ("ᾳ.txt", "ᾼ.txt"),
        ("ș.txt", "Ș.txt"),
        ("ÿ.txt", "Ÿ.txt"),
        ("ⓐ.txt", "Ⓐ.txt"),
        ("ａ.txt", "Ａ.txt"),
        ("я.txt", "Я.txt"),
        ("ⅰ.txt", "Ⅰ.txt"),
    ];
    for (first, second) in same {
        put(first, "one", false).unwrap();
        let e = put(second, "two", false).unwrap_err();
        assert!(matches!(e, StorageError::Exists(_)), "{second}: {e}");
        assert_eq!(put(second, "two", true).unwrap().rel_path, first);
        assert_eq!(std::fs::read(wdir.join(first)).unwrap(), b"two");
    }
    let i = w.info(&id).unwrap();
    assert_eq!(i.files as usize, 2 * distinct.len() + same.len());
}

#[test]
fn export_refuses_names_windows_would_alter_and_writes_where_asked() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let a = w.create("a", None).unwrap();
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
    for bad in ["e.zip ", "e.zip.", "nul", "aux.zip", "COM1.zip"] {
        let e = w.export_workspace(&a.id, &out.join(bad)).unwrap_err();
        assert!(matches!(e, StorageError::InvalidPath(_)), "{bad:?}: {e}");
    }
    assert_eq!(names(&out), Vec::<String>::new());
    // the folder is resolved for the check only; the zip goes where the caller's path says
    w.export_workspace(&a.id, &out.join("..").join("out").join("e.zip"))
        .unwrap();
    assert_eq!(names(&out), ["e.zip"]);
}

/// Windows only, not run on Linux CI: the zip is made through the caller's own (non-verbatim)
/// path, so it can be opened and deleted by that path.
#[cfg(windows)]
#[test]
fn export_on_windows_is_reachable_through_the_callers_path() {
    let d = tempfile::tempdir().unwrap();
    let w = ws(d.path());
    let a = w.create("a", None).unwrap();
    let out = d.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let dest = out.join("e.zip");
    w.export_workspace(&a.id, &dest).unwrap();
    zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
    assert_eq!(names(&out), ["e.zip"]);
    std::fs::remove_file(&dest).unwrap();
    for bad in ["e1.zip ", "e2.zip.", "nul", "aux.zip"] {
        assert!(
            w.export_workspace(&a.id, &out.join(bad)).is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(names(&out), Vec::<String>::new());
}

/// The full snapshot (keys, request log) is redacted beside the database, never in dest's folder.
#[test]
fn backup_redacts_beside_the_database_not_in_the_destination_folder() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let w = ws(&data);
    semdok_core::keys::create(&w.conn, "k", None).unwrap();
    // Copied into the snapshot, this records where the redaction ran.
    w.conn
        .execute_batch(
            "CREATE TRIGGER where_redacted BEFORE DELETE ON api_keys BEGIN
               INSERT OR REPLACE INTO settings(key,value)
                 SELECT 'redacted_in', file FROM pragma_database_list WHERE name='main';
             END;",
        )
        .unwrap();
    let out = d.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let bk = out.join("backup.db");
    w.backup_database(&bk).unwrap();
    let b = rusqlite::Connection::open(&bk).unwrap();
    let at: String = b
        .query_row(
            "SELECT value FROM settings WHERE key='redacted_in'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        Path::new(&at).parent().unwrap().canonicalize().unwrap(),
        data.canonicalize().unwrap()
    );
    assert_eq!(
        b.query_row::<i64, _, _>("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(names(&out), ["backup.db"]);
    assert!(
        names(&data)
            .iter()
            .all(|n| n.starts_with("hub.db") || n == "workspaces"),
        "{:?}",
        names(&data)
    );
}

#[test]
fn backup_never_replaces_a_file_that_appears_at_dest() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let w = ws(&data);
    // 16 MB, so the backup takes long enough for the other writer to get in
    w.conn
        .execute_batch(
            "CREATE TABLE pad(b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<64)
             INSERT INTO pad SELECT randomblob(262144) FROM n;",
        )
        .unwrap();
    let out = d.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let bk = out.join("backup.db");
    let user = std::thread::spawn({
        let (out, bk) = (out.clone(), bk.clone());
        move || {
            // As soon as the backup puts anything in the folder, a user saves a file at dest.
            let deadline = Instant::now() + Duration::from_secs(60);
            while std::fs::read_dir(&out).unwrap().next().is_none() {
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::yield_now();
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&bk)
                .and_then(|mut f| f.write_all(b"USER DATA"))
                .is_ok()
        }
    });
    let res = w.backup_database(&bk);
    if user.join().unwrap() {
        assert!(matches!(res, Err(StorageError::Exists(_))), "{res:?}");
        assert_eq!(std::fs::read(&bk).unwrap(), b"USER DATA");
    } else {
        res.unwrap();
    }
    assert_eq!(names(&out), ["backup.db"]);
}

#[test]
fn backup_keeps_the_free_space_reserve() {
    let d = tempfile::tempdir().unwrap();
    let w = Workspaces::new(
        db::open(&d.path().join("hub.db")).unwrap(),
        &d.path().join("workspaces"),
        u64::MAX / 2,
    )
    .unwrap();
    let bk = d.path().join("backup.db");
    let e = w.backup_database(&bk).unwrap_err();
    assert!(matches!(e, StorageError::DiskReserve(_)), "{e}");
    assert!(!bk.exists());
}
