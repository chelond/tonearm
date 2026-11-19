//! Secret values and the system secret store (keyring).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

/// A string that must never be printed: `Debug` and `Display` are redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Explicit access to the secret value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Errors from the secret store. Messages never contain secret data.
#[derive(Debug, thiserror::Error)]
pub enum SecretStoreError {
    #[error("{0}")]
    Unavailable(String),
}

/// Storage for secrets. Implementations may block (D-Bus, WinAPI): call them from blocking threads.
pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretStoreError>;
    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretStoreError>;
    fn delete(&self, key: &str) -> Result<(), SecretStoreError>;
}

/// System keyring: Secret Service on Linux, Credential Manager on Windows.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(&self.service, key).map_err(map_keyring_err)
    }
}

/// Only the error category is kept: some keyring errors carry raw secret bytes.
fn map_keyring_err(err: keyring::Error) -> SecretStoreError {
    let reason = match err {
        keyring::Error::NoStorageAccess(_) => "нет доступа к хранилищу",
        keyring::Error::PlatformFailure(_) => "ошибка платформы",
        keyring::Error::NoDefaultStore => "служба секретов не найдена",
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => "повреждённая запись",
        _ => "неизвестная ошибка",
    };
    SecretStoreError::Unavailable(reason.to_owned())
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretStoreError> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_err(err)),
        }
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretStoreError> {
        self.entry(key)?
            .set_password(value.expose())
            .map_err(map_keyring_err)
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_keyring_err(err)),
        }
    }
}

/// In-memory store for tests and for running without a keyring.
#[derive(Debug, Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<String, Secret>>,
    fail: bool,
}

impl MemoryStore {
    /// A store whose every call fails, to test the "no keyring" path.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }

    fn check(&self) -> Result<(), SecretStoreError> {
        if self.fail {
            Err(SecretStoreError::Unavailable("тестовый сбой".into()))
        } else {
            Ok(())
        }
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, SecretStoreError> {
        self.check()?;
        Ok(self.values.lock().expect("poisoned").get(key).cloned())
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), SecretStoreError> {
        self.check()?;
        self.values
            .lock()
            .expect("poisoned")
            .insert(key.to_owned(), value.clone());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        self.check()?;
        self.values.lock().expect("poisoned").remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new("top-secret-token");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert!(!format!("{:?}", Some(s)).contains("top-secret"));
    }

    /// Round-trip through the real system keyring. Needs a running Secret Service on Linux.
    /// Run manually: `cargo test -p tonearm-core -- --ignored keyring`.
    #[test]
    #[ignore = "needs a system keyring"]
    fn keyring_roundtrip() {
        let store = KeyringStore::new("tonearm-test");
        store.set("probe", &Secret::new("value")).unwrap();
        assert_eq!(store.get("probe").unwrap().unwrap().expose(), "value");
        store.delete("probe").unwrap();
        assert!(store.get("probe").unwrap().is_none());
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryStore::default();
        assert!(store.get("k").unwrap().is_none());
        store.set("k", &Secret::new("v")).unwrap();
        assert_eq!(store.get("k").unwrap().unwrap().expose(), "v");
        store.delete("k").unwrap();
        assert!(store.get("k").unwrap().is_none());
    }
}
