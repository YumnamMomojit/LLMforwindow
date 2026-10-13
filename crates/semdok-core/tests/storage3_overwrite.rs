//! Storage round 3: an overwrite replaces the file in one step, and a volume that merges
//! names the key keeps apart never gets one file tracked under two rows.
use semdok_core::db;
use semdok_core::storage::{StorageError, Workspaces};
use std::io::{Cursor, ErrorKind, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

fn ws(dir: &Path) -> Workspaces {
    Workspaces::new(
        db::open(&dir.join("hub.db")).unwrap(),
        &dir.join("workspaces"),
        0,
    )
    .unwrap()
}

/// Version `i` of the file: large enough that a torn read would show.
fn version(i: u32) -> Vec<u8> {
    format!("version {i:05}\n").repeat(512).into_bytes()
}

/// A whole version, never a mix or a part of one.
fn is_a_version(b: &[u8]) -> bool {
    std::str::from_utf8(&b[..b.len().min(14)])
        .ok()
        .and_then(|s| s.strip_prefix("version "))
        .and_then(|n| n.trim_end().parse().ok())
        .is_some_and(|i| b == version(i))
}

/// Readers that open the file while it is overwritten (another program through its path, and
/// another `Workspaces` on the same database) always find a whole file: the replacement is
/// one rename over the name, with the old file kept as a second link until the row commits.
#[test]
fn an_overwrite_never_takes_the_name_away() {
    let d = tempfile::tempdir().unwrap();
    let probe = d.path().join("probe");
    std::fs::write(&probe, "x").unwrap();
    if std::fs::hard_link(&probe, d.path().join("probe2")).is_err() {
        // Such a volume moves the old file aside instead; the name is briefly missing there.
        eprintln!("skipped: no hard links on this volume");
        return;
    }
    let mut w = ws(d.path());
    let id = w.create("a", None).unwrap().id;
    let path = d.path().join("workspaces").join(&id).join("f.txt");
    w.write_file(&id, "f.txt", &mut Cursor::new(version(0)), "import", false)
        .unwrap();
    let other = ws(d.path());

    let stop = Arc::new(AtomicBool::new(false));
    let (opens, missing) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let raw = std::thread::spawn({
        let (stop, opens, missing) = (stop.clone(), opens.clone(), missing.clone());
        move || {
            while !stop.load(Ordering::Relaxed) {
                let mut b = Vec::new();
                match std::fs::File::open(&path) {
                    Ok(mut f) => {
                        f.read_to_end(&mut b).unwrap();
                        assert!(is_a_version(&b), "torn read of {} bytes", b.len());
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => panic!("{e}"),
                }
                opens.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    let lib = std::thread::spawn({
        let (stop, id) = (stop.clone(), id.clone());
        move || {
            let (mut n, mut failed) = (0u64, Vec::new());
            while !stop.load(Ordering::Relaxed) {
                let mut b = Vec::new();
                match other.read_file(&id, "f.txt") {
                    Ok(mut f) => {
                        f.read_to_end(&mut b).unwrap();
                        assert!(is_a_version(&b), "torn read of {} bytes", b.len());
                    }
                    Err(e) => failed.push(e.to_string()),
                }
                n += 1;
            }
            (n, failed)
        }
    });
    for i in 1..=300 {
        w.write_file(&id, "f.txt", &mut Cursor::new(version(i)), "output", true)
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    raw.join().unwrap();
    let (lib_reads, lib_failed) = lib.join().unwrap();
    let opens = opens.load(Ordering::Relaxed);
    assert!(opens > 0 && lib_reads > 0);
    assert_eq!(
        missing.load(Ordering::Relaxed),
        0,
        "the name was missing during an overwrite ({opens} opens)"
    );
    assert!(
        lib_failed.is_empty(),
        "{} of {lib_reads} reads failed: {:?}",
        lib_failed.len(),
        lib_failed.first()
    );
    let i = w.info(&id).unwrap();
    assert_eq!((i.files, i.used_bytes), (1, version(300).len() as u64));
    let tmp = d.path().join("workspaces").join(&id).join(".tmp-uploads");
    assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);
}

/// Whatever the volume merges, rows and files stay one to one. On a volume that keeps
/// `λογος` and `ΛΟΓΟΣ` apart they are two files; on one that merges them (exFAT, which folds
/// final `ς` to `Σ` where NTFS does not) the second spelling is refused, as a file name and
/// as a folder name. Run with TMPDIR on such a volume to see the second case.
#[test]
fn one_file_on_disk_is_never_two_rows() {
    let d = tempfile::tempdir().unwrap();
    let mut w = ws(d.path());
    let id = w.create("a", None).unwrap().id;
    let wdir = d.path().join("workspaces").join(&id);
    let mut put = |rel: &str, data: &str| {
        w.write_file(
            &id,
            rel,
            &mut Cursor::new(data.as_bytes().to_vec()),
            "import",
            true,
        )
    };
    put("λογος.txt", "first").unwrap();
    put("λογος/a.txt", "in the folder").unwrap();
    let merged = wdir.join("ΛΟΓΟΣ.txt").exists();
    assert_eq!(merged, wdir.join("ΛΟΓΟΣ").exists());
    for (rel, check) in [("ΛΟΓΟΣ.txt", "ΛΟΓΟΣ.txt"), ("ΛΟΓΟΣ/b.txt", "ΛΟΓΟΣ")] {
        let r = put(rel, "second");
        if merged {
            match r {
                Err(StorageError::Exists(m)) => assert!(m.contains(check), "{m}"),
                other => panic!("{rel}: {other:?}"),
            }
        } else {
            assert_eq!(r.unwrap().rel_path, rel);
        }
    }
    assert_eq!(std::fs::read(wdir.join("λογος.txt")).unwrap(), b"first");
    let files = w.list_files(&id).unwrap();
    let on_disk = |p: &str| std::fs::read(wdir.join(p)).ok();
    for f in &files {
        assert_eq!(on_disk(&f.rel_path).map(|b| b.len() as u64), Some(f.size));
    }
    assert_eq!(files.len(), if merged { 2 } else { 4 });
}
