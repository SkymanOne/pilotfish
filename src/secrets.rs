//! The TypeSafe API key.
//!
//! Resolution is `$TYPESAFE_API_KEY` first — what CI, headless boxes and a
//! shell profile's `export` use — and then `[routing] api_key` in the user
//! config, `~/.pilotfish/config.toml`, which the console writes when the
//! human pastes a key into it. That file is written readable by its owner
//! only (see [`crate::paths::set_routing`]).
//!
//! There is deliberately no credential store any more: the operating
//! system's Keychain put up a password dialog for every rebuilt binary, and
//! an unsigned `cargo install` is a rebuilt binary every time.

use std::path::PathBuf;

use crate::paths::RoutingConfig;

/// The environment variable that holds a key. It wins over the config file.
pub const KEY_VAR: &str = "TYPESAFE_API_KEY";

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

impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|key| Self::new(&key))
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
    /// [`KEY_VAR`], which wins over the config file.
    Env,
    /// `[routing] api_key` in the user config at this path.
    Config(PathBuf),
}

/// The key in force: `env`'s value when it holds one, else the config's.
/// Injected values keep tests off the ambient environment.
#[must_use]
pub fn typesafe_key_with(
    env: Option<&str>,
    config: &RoutingConfig,
    config_path: Option<PathBuf>,
) -> Option<(Secret, KeySource)> {
    if let Some(key) = env.map(Secret::new).filter(|key| !key.is_empty()) {
        return Some((key, KeySource::Env));
    }
    let key = config.api_key.clone().filter(|key| !key.is_empty())?;
    Some((key, KeySource::Config(config_path.unwrap_or_default())))
}

/// [`typesafe_key_with`] against the real environment; `user_dir` is where
/// `config` was read from, for saying where the key lives.
#[must_use]
pub fn typesafe_key(
    config: &RoutingConfig,
    user_dir: Option<&std::path::Path>,
) -> Option<(Secret, KeySource)> {
    typesafe_key_with(
        std::env::var(KEY_VAR).ok().as_deref(),
        config,
        user_dir.map(|dir| dir.join("config.toml")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ts_live_0123456789abcdef";

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
    fn the_environment_wins_over_the_config_file() {
        let config = RoutingConfig {
            api_key: Some(Secret::new("configured-key-00000000")),
            ..RoutingConfig::default()
        };
        let path = PathBuf::from("/home/me/.pilotfish/config.toml");
        let (key, source) = typesafe_key_with(Some(KEY), &config, Some(path.clone())).unwrap();
        assert_eq!(key.expose(), KEY);
        assert_eq!(source, KeySource::Env);

        // a blank variable is no variable
        let (key, source) = typesafe_key_with(Some("  "), &config, Some(path.clone())).unwrap();
        assert_eq!(source, KeySource::Config(path));
        assert_eq!(key.expose(), "configured-key-00000000");

        assert_eq!(
            typesafe_key_with(None, &RoutingConfig::default(), None),
            None,
            "neither set is no key"
        );
    }

    #[test]
    fn a_configured_key_never_prints_with_its_config() {
        let config: RoutingConfig = toml::from_str(&format!("api_key = \"  {KEY} \"")).unwrap();
        assert_eq!(
            config.api_key.as_ref().map(Secret::expose),
            Some(KEY),
            "trimmed"
        );
        let printed = format!("{config:?}");
        assert!(!printed.contains("0123456789"), "{printed}");
    }
}
