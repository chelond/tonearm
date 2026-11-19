//! Encrypted secret file: the fallback on Linux when no Secret Service daemon is running.
//!
//! Threat model, stated honestly: the key is derived from `/etc/machine-id` and the owner uid,
//! so a copied file (backup, sync, another machine or user) is useless. A process running as the
//! same user on the same machine can still decrypt it, which is also true of an unlocked
//! Secret Service keyring.
//!
//! File layout: `MAGIC | salt (16) | nonce (12) | ciphertext + tag`. The AAD binds magic and key name.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hkdf::{HKDF_SHA256, Salt};
use ring::rand::{SecureRandom, SystemRandom};

use crate::secret::{Secret, SecretStore, SecretStoreError};

const MAGIC: &[u8; 4] = b"TNA1";
const SALT_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + SALT_LEN + NONCE_LEN;
const APP_CONTEXT: &[u8] = b"tonearm secret file v1";

#[derive(Debug, Clone)]
pub struct FileStore {
    dir: PathBuf,
    /// Machine- and user-bound input keying material. Not secret by itself.
    identity: Vec<u8>,
}

fn io_err(what: &str, err: &std::io::Error) -> SecretStoreError {
    SecretStoreError::Unavailable(format!("{what}: {err}"))
}

impl FileStore {
    /// Store in the platform data dir (`~/.local/share/tonearm/secrets`), bound to this machine and user.
    pub fn for_current_user() -> Result<Self, SecretStoreError> {
        let dirs = directories::ProjectDirs::from("", "", "tonearm")
            .ok_or_else(|| SecretStoreError::Unavailable("не найден домашний каталог".into()))?;
        let dir = dirs.data_local_dir().join("secrets");
        create_private_dir(&dir)?;
        let machine_id = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SecretStoreError::Unavailable("нет /etc/machine-id".into()))?;
        let mut identity = machine_id.into_bytes();
        identity.push(0);
        identity.extend_from_slice(owner_uid(&dir)?.to_string().as_bytes());
        Ok(Self::with_identity(dir, identity))
    }

    /// Explicit directory and identity (tests).
    #[must_use]
    pub fn with_identity(dir: PathBuf, identity: Vec<u8>) -> Self {
        Self { dir, identity }
    }

    fn path(&self, key: &str) -> PathBuf {
        // Keys are internal constants, but keep file names safe anyway.
        let name: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{name}.bin"))
    }

    fn cipher(&self, salt: &[u8], key: &str) -> Result<LessSafeKey, SecretStoreError> {
        let fail = |_| SecretStoreError::Unavailable("ошибка шифрования".into());
        let prk = Salt::new(HKDF_SHA256, salt).extract(&self.identity);
        let info = [APP_CONTEXT, key.as_bytes()];
        let okm = prk.expand(&info, &CHACHA20_POLY1305).map_err(fail)?;
        let mut raw = [0u8; 32];
        okm.fill(&mut raw).map_err(fail)?;
        let unbound = UnboundKey::new(&CHACHA20_POLY1305, &raw).map_err(fail)?;
        raw.fill(0);
        Ok(LessSafeKey::new(unbound))
    }

    fn aad(key: &str) -> Vec<u8> {
        [MAGIC.as_slice(), key.as_bytes()].concat()
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretStoreError> {
        let path = self.path(key);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("чтение файла секрета", &e)),
        };
        let opened = (|| {
            if data.len() < HEADER_LEN || &data[..MAGIC.len()] != MAGIC {
                return None;
            }
            let salt = &data[MAGIC.len()..MAGIC.len() + SALT_LEN];
            let nonce =
                Nonce::try_assume_unique_for_key(&data[MAGIC.len() + SALT_LEN..HEADER_LEN]).ok()?;
            let cipher = self.cipher(salt, key).ok()?;
            let mut buf = data[HEADER_LEN..].to_vec();
            let plain = cipher
                .open_in_place(nonce, Aad::from(Self::aad(key)), &mut buf)
                .ok()?;
            String::from_utf8(plain.to_vec()).ok()
        })();
        match opened {
            Some(value) => Ok(Some(Secret::new(value))),
            None => {
                // Another machine/user, or corrupted: unusable, so drop it and ask for a new login.
                tracing::warn!(path = %path.display(), "secret file cannot be decrypted, removing it");
                let _ = fs::remove_file(&path);
                Ok(None)
            }
        }
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretStoreError> {
        let rng = SystemRandom::new();
        let fail = |_| SecretStoreError::Unavailable("нет источника случайных чисел".into());
        let mut salt = [0u8; SALT_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        rng.fill(&mut salt).map_err(fail)?;
        rng.fill(&mut nonce).map_err(fail)?;

        let cipher = self.cipher(&salt, key)?;
        let mut buf = value.expose().as_bytes().to_vec();
        cipher
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(Self::aad(key)),
                &mut buf,
            )
            .map_err(|_| SecretStoreError::Unavailable("ошибка шифрования".into()))?;

        let mut out = Vec::with_capacity(HEADER_LEN + buf.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buf);
        create_private_dir(&self.dir)?;
        write_private_atomic(&self.path(key), &out)
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("удаление файла секрета", &e)),
        }
    }
}

fn create_private_dir(dir: &Path) -> Result<(), SecretStoreError> {
    fs::create_dir_all(dir).map_err(|e| io_err("создание каталога", &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("права каталога", &e))?;
    }
    Ok(())
}

/// Writes via a temp file created with 0600 from the start, then renames over the target.
fn write_private_atomic(path: &Path, data: &[u8]) -> Result<(), SecretStoreError> {
    let tmp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| io_err("запись файла секрета", &e))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_err("запись файла секрета", &e))?;
    fs::rename(&tmp, path).map_err(|e| io_err("запись файла секрета", &e))
}

#[cfg(unix)]
fn owner_uid(dir: &Path) -> Result<u32, SecretStoreError> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(dir)
        .map(|m| m.uid())
        .map_err(|e| io_err("каталог секретов", &e))
}

#[cfg(not(unix))]
fn owner_uid(_dir: &Path) -> Result<u32, SecretStoreError> {
    Ok(0)
}

/// Secret Service first; the encrypted file when the keyring is unavailable.
/// A login saved to the file moves into the keyring as soon as one appears.
pub struct FallbackStore {
    primary: Box<dyn SecretStore>,
    fallback: Box<dyn SecretStore>,
}

impl FallbackStore {
    #[must_use]
    pub fn new(primary: Box<dyn SecretStore>, fallback: Box<dyn SecretStore>) -> Self {
        Self { primary, fallback }
    }
}

impl SecretStore for FallbackStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretStoreError> {
        match self.primary.get(key) {
            Ok(Some(v)) => Ok(Some(v)),
            Ok(None) => {
                let Some(v) = self.fallback.get(key)? else {
                    return Ok(None);
                };
                // Keyring is available now: migrate and remove the file copy.
                if self.primary.set(key, &v).is_ok() {
                    tracing::info!("moved saved login from file into the system keyring");
                    let _ = self.fallback.delete(key);
                }
                Ok(Some(v))
            }
            Err(err) => {
                tracing::info!(%err, "system keyring unavailable, using encrypted file");
                self.fallback.get(key)
            }
        }
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretStoreError> {
        match self.primary.set(key, value) {
            Ok(()) => {
                let _ = self.fallback.delete(key);
                Ok(())
            }
            Err(err) => {
                tracing::info!(%err, "system keyring unavailable, saving to encrypted file");
                self.fallback.set(key, value)
            }
        }
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        // Remove both copies; fail only if neither store could be reached.
        let primary = self.primary.delete(key);
        let fallback = self.fallback.delete(key);
        match (primary, fallback) {
            (Err(e), Err(_)) => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::MemoryStore;

    fn store(dir: &Path, identity: &[u8]) -> FileStore {
        FileStore::with_identity(dir.to_path_buf(), identity.to_vec())
    }

    #[test]
    fn roundtrip_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), b"machine\0uid");
        assert!(s.get("k").unwrap().is_none());
        s.set("k", &Secret::new("reusable-credentials")).unwrap();
        assert_eq!(
            s.get("k").unwrap().unwrap().expose(),
            "reusable-credentials"
        );
        s.delete("k").unwrap();
        assert!(s.get("k").unwrap().is_none());
    }

    #[test]
    fn file_is_encrypted_and_private() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), b"machine\0uid");
        s.set("k", &Secret::new("plain-secret-value")).unwrap();
        let path = s.path("k");
        let raw = fs::read(&path).unwrap();
        assert!(!raw.windows(12).any(|w| w == b"plain-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn other_machine_cannot_read_and_file_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        store(tmp.path(), b"machine-a\0uid")
            .set("k", &Secret::new("v"))
            .unwrap();
        let other = store(tmp.path(), b"machine-b\0uid");
        assert!(other.get("k").unwrap().is_none());
        assert!(!other.path("k").exists());
    }

    #[test]
    fn same_ciphertext_under_other_key_name_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), b"id");
        s.set("a", &Secret::new("v")).unwrap();
        fs::copy(s.path("a"), s.path("b")).unwrap();
        assert!(s.get("b").unwrap().is_none());
    }

    #[test]
    fn fallback_used_when_keyring_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let fb = FallbackStore::new(
            Box::new(MemoryStore::failing()),
            Box::new(store(tmp.path(), b"id")),
        );
        fb.set("k", &Secret::new("v")).unwrap();
        assert_eq!(fb.get("k").unwrap().unwrap().expose(), "v");
        fb.delete("k").unwrap();
        assert!(fb.get("k").unwrap().is_none());
    }

    #[test]
    fn file_login_migrates_into_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let file = store(tmp.path(), b"id");
        file.set("k", &Secret::new("v")).unwrap();
        let fb = FallbackStore::new(Box::new(MemoryStore::default()), Box::new(file.clone()));
        assert_eq!(fb.get("k").unwrap().unwrap().expose(), "v");
        assert!(
            !file.path("k").exists(),
            "file copy must be removed after migration"
        );
        assert_eq!(fb.get("k").unwrap().unwrap().expose(), "v");
    }
}
