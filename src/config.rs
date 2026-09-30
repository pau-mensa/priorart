//! Settings, read once from `PRIORART_*` environment variables.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServerMode {
    #[default]
    Local,
    Authenticated,
    Hosted,
}

/// The operator token for `/v1/admin`: 32–256 visible ASCII characters. Debug
/// output never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct AdminToken(String);

impl AdminToken {
    pub fn new(token: impl Into<String>) -> Result<Self, ConfigError> {
        let token = token.into();
        if !(32..=256).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ConfigError(
                "PRIORART_ADMIN_TOKEN must be 32-256 visible ASCII characters".into(),
            ));
        }
        Ok(Self(token))
    }

    pub fn matches(&self, candidate: &str) -> bool {
        Sha256::digest(&self.0)
            .ct_eq(&Sha256::digest(candidate))
            .into()
    }
}

impl fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminToken([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mode: ServerMode,
    /// Enables the operator endpoints; without it they do not exist.
    pub admin_token: Option<AdminToken>,
    pub data_dir: PathBuf,
    pub max_loaded_indexes: usize,
    pub max_tokens: usize,
    pub host: String,
    pub port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: ServerMode::Local,
            admin_token: None,
            data_dir: PathBuf::from("data"),
            max_loaded_indexes: 8,
            max_tokens: 8192,
            host: "127.0.0.1".to_owned(),
            port: 8000,
        }
    }
}

impl Settings {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(std::env::vars().collect())
    }

    pub fn from_vars(vars: HashMap<String, String>) -> Result<Self, ConfigError> {
        let get = |name: &str| vars.get(&format!("PRIORART_{name}")).cloned();
        let mut settings = Self::default();
        if let Some(value) = get("MODE") {
            settings.mode = match value.as_str() {
                "local" => ServerMode::Local,
                "authenticated" => ServerMode::Authenticated,
                "hosted" => ServerMode::Hosted,
                _ => return Err(ConfigError("unknown server mode".into())),
            };
        }
        if let Some(value) = get("ADMIN_TOKEN") {
            settings.admin_token = Some(AdminToken::new(value)?);
        }
        if let Some(value) = get("DATA_DIR") {
            settings.data_dir = PathBuf::from(value);
        }
        if let Some(value) = get("MAX_LOADED_INDEXES") {
            settings.max_loaded_indexes = parse("PRIORART_MAX_LOADED_INDEXES", &value)?;
        }
        if let Some(value) = get("MAX_TOKENS") {
            settings.max_tokens = parse("PRIORART_MAX_TOKENS", &value)?;
        }
        if let Some(value) = get("HOST") {
            settings.host = value;
        }
        if let Some(value) = get("PORT") {
            settings.port = parse("PRIORART_PORT", &value)?;
        }
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError(message.to_owned()));
        if self.mode == ServerMode::Hosted {
            return invalid("hosted mode is not available");
        }
        if self.host != "localhost"
            && !self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        {
            return invalid("only loopback hosts are supported until hosted mode is available");
        }
        if self.max_loaded_indexes == 0 {
            return invalid("max_loaded_indexes must be positive");
        }
        if self.max_tokens == 0 {
            return invalid("max_tokens must be positive");
        }
        if self.port == 0 {
            return invalid("port must be in 1..65535");
        }
        Ok(())
    }

    /// The coarse HTTP body limit: generous enough that text is truncated, not rejected.
    pub fn max_body_bytes(&self) -> usize {
        self.max_tokens.saturating_mul(256).saturating_add(65_536)
    }
}

fn parse<T: std::str::FromStr>(name: &str, value: &str) -> Result<T, ConfigError> {
    value
        .parse()
        .map_err(|_| ConfigError(format!("{name} must be a positive integer, got {value:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn incomplete_hosted_mode_and_non_loopback_binds_are_rejected() {
        for mode in ["local", "authenticated"] {
            for host in ["127.0.0.1", "::1", "localhost"] {
                assert!(Settings::from_vars(vars(&[
                    ("PRIORART_MODE", mode),
                    ("PRIORART_HOST", host)
                ]))
                .is_ok());
            }
            for host in ["0.0.0.0", "::", "192.168.1.2", "example.com"] {
                assert!(Settings::from_vars(vars(&[
                    ("PRIORART_MODE", mode),
                    ("PRIORART_HOST", host)
                ]))
                .is_err());
            }
        }
        assert!(Settings::from_vars(vars(&[("PRIORART_MODE", "hosted")])).is_err());
        assert!(Settings::from_vars(vars(&[("PRIORART_MODE", "unknown")])).is_err());
        let directory = tempfile::tempdir().unwrap();
        let settings = Settings {
            mode: ServerMode::Hosted,
            data_dir: directory.path().join("unopened"),
            ..Settings::default()
        };
        assert!(matches!(
            crate::service::Service::open(settings),
            Err(crate::service::OpenError::Config(_))
        ));
        assert!(!directory.path().join("unopened").exists());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (key, value) in [
            ("PRIORART_MAX_LOADED_INDEXES", "0"),
            ("PRIORART_MAX_TOKENS", "0"),
            ("PRIORART_PORT", "0"),
            ("PRIORART_PORT", "70000"),
            ("PRIORART_ADMIN_TOKEN", "too-short"),
            ("PRIORART_ADMIN_TOKEN", &format!("{} x", "a".repeat(40))),
        ] {
            assert!(
                Settings::from_vars(vars(&[(key, value)])).is_err(),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn the_admin_token_is_matched_exactly_and_never_printed() {
        let secret = "s".repeat(40);
        let settings = Settings::from_vars(vars(&[("PRIORART_ADMIN_TOKEN", &secret)])).unwrap();
        let token = settings.admin_token.as_ref().unwrap();
        assert!(token.matches(&secret));
        assert!(!token.matches(&"s".repeat(39)));
        assert!(!format!("{settings:?}").contains(&secret));
    }
}
