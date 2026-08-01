use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Result;

/// The service name credentials are filed under in the OS keychain. Entries show
/// up as `alkyon / <source id>` in Credential Manager, Keychain Access, or
/// seahorse.
const SERVICE: &str = "alkyon";

/// Where source credentials live.
///
/// `Keyring` is the real one: Windows Credential Manager, macOS Keychain, or
/// libsecret. `Memory` is for tests, and for containers where there is no
/// keychain and secrets arrive through `ALKYON_SOURCES` — there they are not
/// meant to outlive the process.
pub enum Vault {
    Keyring,
    Memory(Mutex<HashMap<String, String>>),
}

impl Vault {
    pub fn memory() -> Self {
        Vault::Memory(Mutex::new(HashMap::new()))
    }

    /// `ALKYON_VAULT=memory` opts out of the keychain. Anything else, including
    /// unset, uses it.
    pub fn from_env() -> Self {
        match std::env::var("ALKYON_VAULT").as_deref() {
            Ok("memory") => {
                tracing::warn!("ALKYON_VAULT=memory: credentials will not be persisted");
                Vault::memory()
            }
            _ => Vault::Keyring,
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Vault::Keyring => "os keychain",
            Vault::Memory(_) => "memory",
        }
    }

    pub fn store(&self, id: &str, secret: &str) -> Result<()> {
        match self {
            Vault::Keyring => Ok(keyring::Entry::new(SERVICE, id)?.set_password(secret)?),
            Vault::Memory(map) => {
                map.lock().unwrap().insert(id.to_owned(), secret.to_owned());
                Ok(())
            }
        }
    }

    /// `None` means there is no entry, which is not an error — an `integrated`
    /// source legitimately has none.
    pub fn load(&self, id: &str) -> Result<Option<String>> {
        match self {
            Vault::Keyring => match keyring::Entry::new(SERVICE, id)?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.into()),
            },
            Vault::Memory(map) => Ok(map.lock().unwrap().get(id).cloned()),
        }
    }

    /// Deleting an entry that is not there succeeds, so removing a source is
    /// idempotent.
    pub fn delete(&self, id: &str) -> Result<()> {
        match self {
            Vault::Keyring => match keyring::Entry::new(SERVICE, id)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            },
            Vault::Memory(map) => {
                map.lock().unwrap().remove(id);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_vault_round_trips() {
        let vault = Vault::memory();
        assert_eq!(vault.load("pg").unwrap(), None);

        vault.store("pg", "hunter2").unwrap();
        assert_eq!(vault.load("pg").unwrap().as_deref(), Some("hunter2"));

        vault.store("pg", "hunter3").unwrap();
        assert_eq!(vault.load("pg").unwrap().as_deref(), Some("hunter3"));

        vault.delete("pg").unwrap();
        assert_eq!(vault.load("pg").unwrap(), None);
        vault.delete("pg").expect("deleting twice is fine");
    }
}
