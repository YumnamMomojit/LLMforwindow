//! Recoverable secrets (internal engine key, tunnel token).
//! Windows: DPAPI, scoped to the current user (CryptProtectData), file `<name>.dpapi`.
//! Elsewhere: 0600 file, explicitly NOT OS-protected (development/test hosts only).
//! API keys are not stored here; only their verifiers live in SQLite.
use std::path::{Path, PathBuf};

pub trait SecretStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>>;
    fn set(&self, name: &str, value: &[u8]) -> anyhow::Result<()>;
    fn delete(&self, name: &str) -> anyhow::Result<()>;
    fn protection(&self) -> &'static str;
    /// Where a secret is kept, for error messages.
    fn location(&self, name: &str) -> String {
        format!("secret '{name}'")
    }
}

fn check_name(n: &str) -> anyhow::Result<()> {
    if n.is_empty()
        || n.len() > 64
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("invalid secret name");
    }
    Ok(())
}

pub struct FileSecrets {
    dir: PathBuf,
}

impl FileSecrets {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }
    fn path(&self, n: &str) -> PathBuf {
        self.dir.join(format!(
            "{n}.{}",
            if cfg!(windows) { "dpapi" } else { "secret" }
        ))
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        check_name(name)?;
        match std::fs::read(self.path(name)) {
            Ok(b) => Ok(Some(unprotect(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, name: &str, value: &[u8]) -> anyhow::Result<()> {
        check_name(name)?;
        let blob = protect(value)?;
        let p = self.path(name);
        let tmp = p.with_extension("tmp");
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        // On disk before the rename, so a crash cannot leave an empty or partial file under
        // the real name (which would then fail to decrypt, or read back as an empty key).
        std::io::Write::write_all(&mut f, &blob)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(tmp, p)?;
        // Persist the rename itself (the directory entry); best effort, not every
        // filesystem supports syncing a directory.
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
    fn delete(&self, name: &str) -> anyhow::Result<()> {
        check_name(name)?;
        let _ = std::fs::remove_file(self.path(name));
        Ok(())
    }
    fn location(&self, name: &str) -> String {
        self.path(name).display().to_string()
    }
    fn protection(&self) -> &'static str {
        if cfg!(windows) {
            "Windows DPAPI (current user)"
        } else {
            "file mode 0600 (NOT OS-protected; dev only)"
        }
    }
}

const INTERNAL_KEY: &str = "internal_engine_key";

/// Get or create the random credential between the gateway and the raw engine server.
///
/// A stored key that cannot be read or is not 64 hex characters (a file truncated by a crash,
/// a DPAPI blob from another user or machine) is an error naming the file, not silently
/// replaced: an engine started earlier may still be using it. The key only links the gateway
/// to its own engine, so deleting the file is a safe reset; a new key is then generated.
pub fn internal_key(store: &dyn SecretStore) -> anyhow::Result<String> {
    let unusable = |why: String| {
        anyhow::anyhow!(
            "internal engine key {} is unusable ({why}); stop hubd, delete that file and \
             retry: a new key is generated (it only links the gateway to its own engine)",
            store.location(INTERNAL_KEY)
        )
    };
    match store.get(INTERNAL_KEY) {
        Ok(Some(v)) if v.len() == 64 && v.iter().all(u8::is_ascii_hexdigit) => {
            Ok(String::from_utf8(v)?)
        }
        Ok(Some(_)) => Err(unusable("not a 64-character hex key".into())),
        Err(e) => Err(unusable(format!("{e:#}"))),
        Ok(None) => {
            let k = hex::encode(crate::random_bytes::<32>());
            store.set(INTERNAL_KEY, k.as_bytes())?;
            Ok(k)
        }
    }
}

#[cfg(not(windows))]
fn protect(v: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(v.to_vec())
}
#[cfg(not(windows))]
fn unprotect(v: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(v.to_vec())
}

#[cfg(windows)]
fn protect(v: &[u8]) -> anyhow::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::*;
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: v.len() as u32,
            pbData: v.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        ) == 0
        {
            anyhow::bail!("DPAPI protect failed: {}", std::io::Error::last_os_error());
        }
        let r = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Ok(r)
    }
}

#[cfg(windows)]
fn unprotect(v: &[u8]) -> anyhow::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::*;
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: v.len() as u32,
            pbData: v.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        ) == 0
        {
            anyhow::bail!(
                "DPAPI unprotect failed (different user or machine?): {}",
                std::io::Error::last_os_error()
            );
        }
        let r = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Ok(r)
    }
}
