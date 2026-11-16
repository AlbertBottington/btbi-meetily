//! Refresh-token storage. Windows uses user-scoped DPAPI, never a password-sized
//! Credential Manager slot. Other platforms retain the OS keyring.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct TokenStore {
    legacy: keyring::Entry,
    #[cfg(windows)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    binding: Vec<u8>,
}

fn identity(tenant: &str, client: &str, account: &str) -> String {
    // Length-delimited serialization avoids ambiguous identity concatenations.
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(tenant, client, account)).unwrap())
    )
}

impl TokenStore {
    pub fn new(root: &Path, tenant: &str, client: &str, account: &str) -> Result<Self> {
        ensure!(
            !tenant.is_empty() && !client.is_empty() && !account.is_empty(),
            "Microsoft account identity missing"
        );
        let id = identity(tenant, client, account);
        let legacy = keyring::Entry::new("BTBI Meetily", &format!("{tenant}:{client}:{account}"))?;
        #[cfg(not(windows))]
        let _ = (root, id);
        Ok(Self {
            legacy,
            #[cfg(windows)]
            path: root.join("tokens").join(format!("{id}.dpapi")),
            #[cfg(windows)]
            binding: id.into_bytes(),
        })
    }

    pub fn set(&self, token: &str) -> Result<()> {
        ensure!(
            !token.is_empty() && token.len() <= 1024 * 1024,
            "Invalid Microsoft refresh token size"
        );
        #[cfg(windows)]
        {
            use std::io::Write;
            let encrypted = windows::protect(token.as_bytes(), &self.binding)?;
            let parent = self.path.parent().context("Token directory unavailable")?;
            std::fs::create_dir_all(parent)
                .context("Could not create protected token directory")?;
            // Only ciphertext touches disk. Same-directory replacement preserves the
            // previous credential if encryption, writing or persistence fails.
            let mut temp = tempfile::NamedTempFile::new_in(parent)
                .context("Could not prepare protected token file")?;
            temp.write_all(&encrypted)
                .context("Could not write protected token")?;
            temp.as_file()
                .sync_all()
                .context("Could not flush protected token")?;
            temp.persist(&self.path)
                .map_err(|_| anyhow::anyhow!("Could not replace protected token"))?;
            Ok(())
        }
        #[cfg(not(windows))]
        self.legacy
            .set_password(token)
            .context("Could not save Microsoft token in OS keyring")
    }

    pub fn get(&self) -> Result<String> {
        #[cfg(windows)]
        {
            use std::io::Read;
            match std::fs::File::open(&self.path) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(2 * 1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .context("Could not read protected token")?;
                    ensure!(
                        bytes.len() <= 2 * 1024 * 1024,
                        "Protected token file is too large"
                    );
                    let token = String::from_utf8(windows::unprotect(&bytes, &self.binding)?)
                        .map_err(|_| {
                            anyhow::anyhow!("Protected token is invalid; sign in again")
                        })?;
                    ensure!(
                        !token.is_empty() && token.len() <= 1024 * 1024,
                        "Protected token is invalid; sign in again"
                    );
                    return Ok(token);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => anyhow::bail!("Could not open protected token; check app storage access"),
            }
            // Upgrade a smaller credential from an earlier installation only after
            // the protected file has been safely written. Never hide corrupt files
            // by falling back to an older credential.
            let token = self
                .legacy
                .get_password()
                .context("Microsoft sign-in required")?;
            self.set(&token)?;
            self.delete_legacy()?;
            Ok(token)
        }
        #[cfg(not(windows))]
        self.legacy
            .get_password()
            .context("Microsoft sign-in required")
    }

    fn delete_legacy(&self) -> Result<()> {
        match self.legacy.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => anyhow::bail!("Could not remove previous Microsoft credential"),
        }
    }

    pub fn delete(&self) -> Result<()> {
        // Remove legacy first so it cannot be resurrected on the next read.
        self.delete_legacy()?;
        #[cfg(windows)]
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("Could not remove protected Microsoft token"),
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    fn transform(input: &[u8], entropy: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: input.len().try_into()?,
            pbData: input.as_ptr() as *mut u8,
        };
        let entropy = CRYPT_INTEGER_BLOB {
            cbData: entropy.len().try_into()?,
            pbData: entropy.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // No LOCAL_MACHINE flag: DPAPI protection belongs to the signed-in Windows
        // user. Buffers remain live for the synchronous call; DPAPI owns output.
        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    &entropy,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        ensure!(ok != 0, "Windows could not protect or unlock Microsoft sign-in; sign in again under the same Windows account");
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        // Best-effort erase of DPAPI's allocated plaintext before releasing it.
        unsafe {
            for offset in 0..output.cbData as usize {
                std::ptr::write_volatile(output.pbData.add(offset), 0);
            }
            LocalFree(output.pbData as _);
        }
        Ok(bytes)
    }
    pub fn protect(input: &[u8], binding: &[u8]) -> Result<Vec<u8>> {
        transform(input, binding, true)
    }
    pub fn unprotect(input: &[u8], binding: &[u8]) -> Result<Vec<u8>> {
        transform(input, binding, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_is_scoped_and_path_safe() {
        let id = identity("tenant", "client", "../account");
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, identity("other", "client", "../account"));
        assert_ne!(id, identity("tenant", "other", "../account"));
        assert_ne!(identity("a:b", "c", "d"), identity("a", "b:c", "d"));
    }
    #[cfg(windows)]
    fn store(root: &Path, account: &str) -> TokenStore {
        TokenStore::new(root, "test-tenant", "test-client", account).unwrap()
    }
    #[cfg(windows)]
    #[test]
    fn oversized_rotation_restart_and_signout() {
        let root = tempfile::tempdir().unwrap();
        let account = uuid::Uuid::new_v4().to_string();
        let s = store(root.path(), &account);
        let token = "synthetic-refresh-token-".repeat(2048);
        s.set(&token).unwrap();
        assert_eq!(s.get().unwrap(), token);
        let bytes = std::fs::read(&s.path).unwrap();
        assert!(!bytes
            .windows(b"synthetic-refresh-token-".len())
            .any(|w| w == b"synthetic-refresh-token-"));
        // Fresh OS process, not just another struct, must decrypt the saved token.
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tokens::tests::restart_read", "--ignored"])
            .env("BTBI_TEST_TOKEN_ROOT", root.path())
            .env("BTBI_TEST_TOKEN_ACCOUNT", &account)
            .status()
            .unwrap();
        assert!(child.success());
        s.set(&"rotated-".repeat(4096)).unwrap();
        assert_eq!(
            store(root.path(), &account).get().unwrap(),
            "rotated-".repeat(4096)
        );
        assert!(s.set("").is_err());
        assert_eq!(s.get().unwrap(), "rotated-".repeat(4096));
        s.delete().unwrap();
        assert!(!s.path.exists());
        assert!(s.get().is_err());
        s.delete().unwrap();
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "invoked by parent in fresh process"]
    fn restart_read() {
        let root = std::env::var("BTBI_TEST_TOKEN_ROOT").unwrap();
        let account = std::env::var("BTBI_TEST_TOKEN_ACCOUNT").unwrap();
        assert_eq!(
            store(Path::new(&root), &account).get().unwrap(),
            "synthetic-refresh-token-".repeat(2048)
        );
    }
    #[cfg(windows)]
    #[test]
    fn corrupt_and_cross_account_files_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path(), &uuid::Uuid::new_v4().to_string());
        let other = store(root.path(), &uuid::Uuid::new_v4().to_string());
        s.set("synthetic").unwrap();
        std::fs::copy(&s.path, &other.path).unwrap();
        assert!(other.get().is_err());
        let mut bytes = std::fs::read(&s.path).unwrap();
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        std::fs::write(&s.path, bytes).unwrap();
        assert!(s.get().is_err());
    }
    #[cfg(windows)]
    #[test]
    fn failed_replace_retains_previous_credential() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path(), &uuid::Uuid::new_v4().to_string());
        s.set("previous").unwrap();
        // Windows denies replacement while a handle disallows delete sharing.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&s.path)
            .unwrap();
        assert!(s.set("replacement").is_err());
        drop(held);
        assert_eq!(s.get().unwrap(), "previous");
    }
    #[cfg(windows)]
    #[test]
    fn migrates_legacy_and_removes_both_on_signout() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path(), &uuid::Uuid::new_v4().to_string());
        s.legacy.set_password("old-synthetic").unwrap();
        assert_eq!(s.get().unwrap(), "old-synthetic");
        assert!(s.path.exists());
        assert!(matches!(
            s.legacy.get_password(),
            Err(keyring::Error::NoEntry)
        ));
        s.legacy.set_password("stale-synthetic").unwrap();
        s.delete().unwrap();
        assert!(s.get().is_err());
    }
}
