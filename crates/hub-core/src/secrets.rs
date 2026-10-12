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
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&blob)?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, &blob)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
    fn delete(&self, name: &str) -> anyhow::Result<()> {
        check_name(name)?;
        let _ = std::fs::remove_file(self.path(name));
        Ok(())
    }
    fn protection(&self) -> &'static str {
        if cfg!(windows) {
            "Windows DPAPI (current user)"
        } else {
            "file mode 0600 (NOT OS-protected; dev only)"
        }
    }
}

/// Get or create the random credential between the gateway and the raw engine server.
pub fn internal_key(store: &dyn SecretStore) -> anyhow::Result<String> {
    if let Some(v) = store.get("internal_engine_key")? {
        return Ok(String::from_utf8(v)?);
    }
    let k = hex::encode(crate::random_bytes::<32>());
    store.set("internal_engine_key", k.as_bytes())?;
    Ok(k)
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
