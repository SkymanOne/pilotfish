//! The TypeSafe API key, kept out of every file `pilotfish` writes.
//!
//! Resolution is the environment first — `$PILOTFISH_TYPESAFE_API_KEY`, then
//! `$TYPESAFE_API_KEY`, which is what CI and headless boxes use — and then
//! the operating system's credential store: the macOS Keychain, the Windows
//! Credential Manager, or the Secret Service on Linux. There is deliberately
//! no file fallback. A machine without a credential store gets the
//! environment variable or nothing, because a plaintext key in `~/.pilotfish` is
//! the one thing this module exists to avoid.
//!
//! Every call into the store blocks, and on macOS the first read by a newly
//! built binary can put up a system dialog asking to allow it. Callers on an
//! async path run these on a blocking thread.

use crate::paths::env_var;

/// The credential store entry: service and account.
const SERVICE: &str = "pilotfish";
const ACCOUNT: &str = "typesafe-api-key";

/// A secret that never prints. `Debug` masks it, so an effect or a state
/// that carries one can be logged, asserted on and diffed without the key
/// ever reaching a terminal or a file.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a key, trimmed: a key pasted from a web page often carries a
    /// trailing newline or space, and neither is ever part of it.
    #[must_use]
    pub fn new(key: &str) -> Self {
        Self(key.trim().to_string())
    }

    /// The key itself, for the one place that needs it: the request header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many characters have been typed, for a masked field to draw one
    /// dot per character without ever holding the text itself.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    /// Append one typed character.
    pub fn push(&mut self, ch: char) {
        self.0.push(ch);
    }

    /// Append pasted text; newlines and other controls are never part of a key.
    pub fn push_str(&mut self, text: &str) {
        self.0.extend(text.chars().filter(|c| !c.is_control()));
    }

    /// Remove the last character, as backspace does.
    pub fn pop(&mut self) {
        self.0.pop();
    }

    /// The finished key, trimmed the way [`Secret::new`] trims.
    #[must_use]
    pub fn finished(&self) -> Self {
        Self::new(&self.0)
    }

    /// Enough of the key to tell two keys apart and nothing more: its last
    /// four characters, and only when there are enough others to hide.
    #[must_use]
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() < 12 {
            return "••••".to_string();
        }
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("••••{tail}")
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({})", self.masked())
    }
}

/// Where the key came from, so the console can say which one is in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// An environment variable, named. It wins over the credential store.
    Env(String),
    /// The operating system's credential store.
    Store,
}

/// What went wrong talking to the credential store.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// There is no credential store here, or it refused access: a headless
    /// Linux box without a Secret Service, or a denied Keychain dialog.
    #[error("no credential store is reachable here ({0}) — set ${var} instead", var = env_var("TYPESAFE_API_KEY"))]
    Unavailable(String),
    /// The store is there and the call failed anyway.
    #[error("the credential store failed: {0}")]
    Failed(String),
}

/// A place a secret can be kept. The operating system's store is the only
/// real one; the trait exists so tests never touch the developer's Keychain.
pub trait SecretStore {
    /// The stored key, or `None` when nothing is stored.
    ///
    /// # Errors
    ///
    /// [`SecretError`] when the store cannot be reached or the read fails.
    fn get(&self) -> Result<Option<Secret>, SecretError>;
    /// Store `key`, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// [`SecretError`] when the store cannot be reached or the write fails.
    fn set(&self, key: &Secret) -> Result<(), SecretError>;
    /// Remove the key. Removing nothing is not an error.
    ///
    /// # Errors
    ///
    /// [`SecretError`] when the store cannot be reached or the delete fails.
    fn delete(&self) -> Result<(), SecretError>;
}

/// The operating system's credential store.
#[derive(Debug, Clone, Copy, Default)]
pub struct Keychain;

impl Keychain {
    fn entry() -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(SERVICE, ACCOUNT).map_err(classify)
    }
}

impl SecretStore for Keychain {
    fn get(&self) -> Result<Option<Secret>, SecretError> {
        match Self::entry()?.get_password() {
            Ok(key) => Ok(Some(Secret::new(&key)).filter(|k| !k.is_empty())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(classify(err)),
        }
    }

    fn set(&self, key: &Secret) -> Result<(), SecretError> {
        Self::entry()?.set_password(key.expose()).map_err(classify)
    }

    fn delete(&self) -> Result<(), SecretError> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(classify(err)),
        }
    }
}

/// Sort a store error into "there is no store" and "the store failed", which
/// is the distinction the console has to explain.
fn classify(err: keyring::Error) -> SecretError {
    match err {
        keyring::Error::NoStorageAccess(_) | keyring::Error::NoDefaultStore => {
            SecretError::Unavailable(err.to_string())
        }
        other => SecretError::Failed(other.to_string()),
    }
}

/// The credential store's name as the console should say it.
#[must_use]
pub const fn store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else if cfg!(target_os = "windows") {
        "the Windows Credential Manager"
    } else {
        "the Secret Service keyring"
    }
}

/// The environment variables that hold a key, most specific first.
fn env_vars() -> [String; 2] {
    [env_var("TYPESAFE_API_KEY"), "TYPESAFE_API_KEY".to_string()]
}

/// The key from the environment, if one is set, with the variable it came
/// from. Injected values keep tests off the ambient environment.
#[must_use]
pub fn key_from_env(values: [Option<&str>; 2]) -> Option<(Secret, KeySource)> {
    env_vars()
        .into_iter()
        .zip(values)
        .find_map(|(name, value)| {
            let key = Secret::new(value?);
            (!key.is_empty()).then_some((key, KeySource::Env(name)))
        })
}

/// The ambient environment's values for [`key_from_env`].
fn ambient_env() -> [Option<String>; 2] {
    env_vars().map(|name| std::env::var(name).ok())
}

/// The key in force: the environment's, else the store's.
///
/// # Errors
///
/// [`SecretError`] only when no environment variable is set *and* the store
/// cannot be read — a store that simply holds nothing is `Ok(None)`.
pub fn typesafe_key_with(
    store: &dyn SecretStore,
    env: [Option<&str>; 2],
) -> Result<Option<(Secret, KeySource)>, SecretError> {
    if let Some(found) = key_from_env(env) {
        return Ok(Some(found));
    }
    Ok(store.get()?.map(|key| (key, KeySource::Store)))
}

/// [`typesafe_key_with`] against the real environment and the real store.
///
/// # Errors
///
/// As [`typesafe_key_with`].
pub fn typesafe_key() -> Result<Option<(Secret, KeySource)>, SecretError> {
    let env = ambient_env();
    typesafe_key_with(&Keychain, [env[0].as_deref(), env[1].as_deref()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A store in memory, so no test ever reads or writes the real Keychain.
    #[derive(Default)]
    struct Memory {
        key: Mutex<Option<Secret>>,
        unavailable: bool,
    }

    impl SecretStore for Memory {
        fn get(&self) -> Result<Option<Secret>, SecretError> {
            if self.unavailable {
                return Err(SecretError::Unavailable("no store".into()));
            }
            Ok(self.key.lock().unwrap().clone())
        }
        fn set(&self, key: &Secret) -> Result<(), SecretError> {
            *self.key.lock().unwrap() = Some(key.clone());
            Ok(())
        }
        fn delete(&self) -> Result<(), SecretError> {
            *self.key.lock().unwrap() = None;
            Ok(())
        }
    }

    const KEY: &str = "ts_live_0123456789abcdef";

    /// The real credential store, round-tripped under a throwaway entry that
    /// is deleted again. Ignored by default: it touches the machine's
    /// Keychain, which a hermetic suite must not. Run it by hand once per
    /// platform: `cargo test --lib secrets -- --ignored`.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn the_operating_system_store_round_trips_a_throwaway_entry() {
        let entry = keyring::Entry::new(SERVICE, "roundtrip-test").unwrap();
        entry.set_password("throwaway-value").unwrap();
        assert_eq!(entry.get_password().unwrap(), "throwaway-value");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }

    #[test]
    fn a_secret_never_prints_and_shows_only_its_last_four() {
        let key = Secret::new(KEY);
        let printed = format!("{key:?}");
        assert!(!printed.contains("0123456789"), "{printed}");
        assert_eq!(key.masked(), "••••cdef");
        assert_eq!(printed, "Secret(••••cdef)");
        // a short key shows none of itself
        assert_eq!(Secret::new("abc123").masked(), "••••");
    }

    #[test]
    fn a_key_typed_into_a_masked_field_never_shows_either() {
        let mut typed = Secret::default();
        for ch in "ts_live_".chars() {
            typed.push(ch);
        }
        typed.push_str("0123456789abcdef\n");
        typed.pop();
        typed.push('f');
        assert_eq!(
            typed.len(),
            24,
            "one dot per character is all it gives away"
        );
        assert_eq!(typed.finished().expose(), "ts_live_0123456789abcdef");
        assert!(!format!("{typed:?}").contains("live"));
    }

    #[test]
    fn a_pasted_key_loses_its_surrounding_whitespace() {
        assert_eq!(Secret::new(&format!("  {KEY}\n")).expose(), KEY);
        assert!(Secret::new(" \n ").is_empty());
    }

    #[test]
    fn the_environment_wins_over_the_store_and_names_its_variable() {
        let store = Memory::default();
        store.set(&Secret::new("stored-key-0000000000")).unwrap();

        let (key, source) = typesafe_key_with(&store, [Some(KEY), None])
            .unwrap()
            .unwrap();
        assert_eq!(key.expose(), KEY);
        assert_eq!(source, KeySource::Env("PILOTFISH_TYPESAFE_API_KEY".into()));

        let (_, source) = typesafe_key_with(&store, [None, Some(KEY)])
            .unwrap()
            .unwrap();
        assert_eq!(source, KeySource::Env("TYPESAFE_API_KEY".into()));

        // a blank variable is no variable
        let (key, source) = typesafe_key_with(&store, [Some("  "), None])
            .unwrap()
            .unwrap();
        assert_eq!(source, KeySource::Store);
        assert_eq!(key.expose(), "stored-key-0000000000");
    }

    #[test]
    fn an_empty_store_is_no_key_and_an_absent_store_is_an_error() {
        assert_eq!(
            typesafe_key_with(&Memory::default(), [None, None]).unwrap(),
            None
        );
        let absent = Memory {
            unavailable: true,
            ..Memory::default()
        };
        let err = typesafe_key_with(&absent, [None, None]).unwrap_err();
        assert!(matches!(err, SecretError::Unavailable(_)));
        assert!(
            err.to_string().contains("PILOTFISH_TYPESAFE_API_KEY"),
            "the error says what to do instead: {err}"
        );
        // …unless the environment already answered
        assert!(typesafe_key_with(&absent, [Some(KEY), None]).is_ok());
    }

    #[test]
    fn a_stored_key_round_trips_and_deleting_nothing_is_fine() {
        let store = Memory::default();
        store.set(&Secret::new(KEY)).unwrap();
        assert_eq!(store.get().unwrap().unwrap().expose(), KEY);
        store.delete().unwrap();
        store.delete().unwrap();
        assert_eq!(store.get().unwrap(), None);
    }
}
