use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Result;

/// The service name credentials are filed under in the OS keychain. Entries show
/// up as `alkyon / <source id>` in Credential Manager, Keychain Access, or
/// seahorse.
const SERVICE: &str = "alkyon";

/// How much one keychain entry will be asked to hold, in UTF-16 code units.
///
/// Windows Credential Manager caps a credential blob at
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE`, which is 2560 **bytes** — so **1280 code
/// units**, not 2560. Its error says otherwise: *Attribute 'password encoded as
/// UTF-16' is longer than platform limit of 2560 chars*, printing the byte
/// constant next to the word chars. The check behind it is
/// `password.encode_utf16().count() * 2 > 2560`, and believing the message
/// instead of the check is worth exactly one wasted attempt.
///
/// Well under it rather than at it: the margin costs nothing, and being one unit
/// over costs a source that cannot be saved.
const CHUNK: usize = 1_000;

/// What the platform actually refuses, in bytes. Only the tests use it — to
/// check the pieces against the real limit rather than against [`CHUNK`], which
/// would only ever prove that the code agrees with itself.
#[cfg(test)]
const PLATFORM_BYTES: usize = 2_560;

/// A secret in more than one entry is announced by this, under the id itself.
///
/// A leading U+0001 is not something anyone types into a password box, which is
/// what makes it safe to read as a marker rather than as a credential.
const PARTS: &str = "\u{1}alkyon-parts:";

/// A bound, so that a header saying `parts:99999` cannot turn a load into a
/// thousand keychain calls — and, since a store sweeps up to it, so that saving a
/// short secret is not thirty-two pointless deletes either. Sixteen parts is
/// 16 000 characters, several times the longest refresh token seen.
const MAX_PARTS: usize = 16;

/// How many UTF-16 code units a string takes — what the platform limit counts.
fn units(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Split into pieces no entry will refuse, never through the middle of a
/// character.
fn split(secret: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut length = 0;
    for c in secret.chars() {
        if length + c.len_utf16() > CHUNK {
            parts.push(std::mem::take(&mut part));
            length = 0;
        }
        part.push(c);
        length += c.len_utf16();
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// Where part `n` of a long secret lives. Under the id, so Credential Manager
/// still shows them together.
fn part_key(id: &str, n: usize) -> String {
    format!("{id}#{n}")
}

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

    /// Store a secret, in as many entries as the platform will take.
    ///
    /// A secret that fits is written exactly as it always was — no marker, no
    /// parts — so entries written before any of this still load, and a password
    /// still reads as a password in Credential Manager.
    pub fn store(&self, id: &str, secret: &str) -> Result<()> {
        let parts = if units(secret) <= CHUNK {
            Vec::new()
        } else {
            split(secret)
        };

        if parts.is_empty() {
            self.put(id, secret)?;
        } else {
            // The parts before the header: a load that arrives in between finds
            // the old secret whole rather than a header pointing at nothing.
            for (n, part) in parts.iter().enumerate() {
                self.put(&part_key(id, n + 1), part)?;
            }
            self.put(id, &format!("{PARTS}{}", parts.len()))?;
        }

        // Whatever the last secret needed and this one does not. Overwriting a
        // long secret with a short one would otherwise leave parts behind, and
        // the next long one would read some of them back.
        self.sweep(id, parts.len())
    }

    /// `None` means there is no entry, which is not an error — an `integrated`
    /// source legitimately has none.
    pub fn load(&self, id: &str) -> Result<Option<String>> {
        let Some(head) = self.get(id)? else {
            return Ok(None);
        };
        let Some(count) = head.strip_prefix(PARTS) else {
            return Ok(Some(head));
        };

        let count: usize = count
            .trim()
            .parse()
            .ok()
            .filter(|n| (1..=MAX_PARTS).contains(n))
            .ok_or_else(|| {
                crate::error::Error::BadRequest(format!(
                    "the credential for `{id}` says it is in {count} parts, which it cannot be"
                ))
            })?;

        let mut secret = String::new();
        for n in 1..=count {
            // A missing part is an error rather than a shorter secret: half a
            // token looks like a wrong password, and that is a bad afternoon.
            let part = self.get(&part_key(id, n))?.ok_or_else(|| {
                crate::error::Error::MissingSecret(format!("{id} (part {n} of {count})"))
            })?;
            secret.push_str(&part);
        }
        Ok(Some(secret))
    }

    /// Deleting an entry that is not there succeeds, so removing a source is
    /// idempotent.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.forget(id)?;
        self.sweep(id, 0)
    }

    /// Drop every part past `keep`. Bounded, and a missing one is the ordinary
    /// case rather than a failure.
    fn sweep(&self, id: &str, keep: usize) -> Result<()> {
        for n in keep + 1..=MAX_PARTS {
            self.forget(&part_key(id, n))?;
        }
        Ok(())
    }

    // ------------------------------------------------------- one entry at a time

    fn put(&self, key: &str, secret: &str) -> Result<()> {
        match self {
            Vault::Keyring => Ok(keyring::Entry::new(SERVICE, key)?.set_password(secret)?),
            Vault::Memory(map) => {
                map.lock().unwrap().insert(key.to_owned(), secret.to_owned());
                Ok(())
            }
        }
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Vault::Keyring => match keyring::Entry::new(SERVICE, key)?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.into()),
            },
            Vault::Memory(map) => Ok(map.lock().unwrap().get(key).cloned()),
        }
    }

    fn forget(&self, key: &str) -> Result<()> {
        match self {
            Vault::Keyring => match keyring::Entry::new(SERVICE, key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            },
            Vault::Memory(map) => {
                map.lock().unwrap().remove(key);
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

    fn held(vault: &Vault) -> HashMap<String, String> {
        match vault {
            Vault::Memory(map) => map.lock().unwrap().clone(),
            Vault::Keyring => unreachable!("the tests use the memory vault"),
        }
    }

    /// An Entra refresh token is longer than Windows Credential Manager will
    /// take in one entry — *longer than platform limit of 2560 chars* — which is
    /// a source that tests fine and cannot be saved.
    #[test]
    fn a_secret_too_long_for_one_entry_is_split_and_rejoined() {
        let vault = Vault::memory();
        let long: String = std::iter::repeat_n("0.AXkAus", 700).collect();
        assert!(units(&long) > CHUNK, "the fixture has to be over the limit");

        vault.store("azure", &long).unwrap();
        assert_eq!(vault.load("azure").unwrap().as_deref(), Some(long.as_str()));

        // Split, not squeezed — and measured against what Windows actually
        // refuses, in bytes, rather than against our own constant.
        let entries = held(&vault);
        assert!(entries.len() > 1, "{:?}", entries.keys());
        for (key, value) in &entries {
            let bytes = units(value) * 2;
            assert!(
                bytes <= PLATFORM_BYTES,
                "{key} is {bytes} bytes, past the {PLATFORM_BYTES} the platform takes"
            );
        }
        // And the id itself holds the header, so nothing reads it as a password.
        assert!(entries["azure"].starts_with(PARTS), "{:?}", entries["azure"]);
    }

    #[test]
    fn a_secret_that_fits_is_stored_exactly_as_before() {
        let vault = Vault::memory();
        vault.store("pg", "hunter2").unwrap();
        // One entry, the password itself: an entry written before any of this
        // still loads, and Credential Manager still shows something readable.
        assert_eq!(held(&vault), HashMap::from([("pg".to_owned(), "hunter2".to_owned())]));
    }

    /// The bug this guards: a long secret replaced by a short one leaves parts
    /// behind, and the next long one reads a mixture of the two.
    #[test]
    fn parts_do_not_outlive_the_secret_that_needed_them() {
        let vault = Vault::memory();
        let long: String = std::iter::repeat_n('x', CHUNK * 3).collect();

        vault.store("azure", &long).unwrap();
        vault.store("azure", "short").unwrap();
        assert_eq!(vault.load("azure").unwrap().as_deref(), Some("short"));
        assert_eq!(held(&vault).len(), 1, "{:?}", held(&vault).keys());

        vault.store("azure", &long).unwrap();
        vault.delete("azure").unwrap();
        assert_eq!(vault.load("azure").unwrap(), None);
        assert!(held(&vault).is_empty(), "{:?}", held(&vault).keys());
    }

    /// Half a token looks exactly like a wrong password, so a part that is not
    /// there is an error rather than a shorter secret.
    #[test]
    fn a_missing_part_is_not_a_shorter_secret() {
        let vault = Vault::memory();
        let long: String = std::iter::repeat_n('x', CHUNK * 2).collect();
        vault.store("azure", &long).unwrap();

        vault.forget(&part_key("azure", 2)).unwrap();
        assert!(vault.load("azure").is_err());
    }

    #[test]
    fn nothing_is_split_through_the_middle_of_a_character() {
        // Two UTF-16 units each, so a split that counted bytes or chars would
        // land inside one.
        let emoji: String = std::iter::repeat_n('🦆', CHUNK).collect();
        let parts = split(&emoji);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(units(part) <= CHUNK);
        }
        assert_eq!(parts.concat(), emoji);
    }
}
