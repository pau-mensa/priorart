//! Settings, read once from `PRIORART_*` environment variables.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

use ipnet::IpNet;
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
    /// Peers whose `X-Forwarded-Proto` is believed; they must report `https`.
    pub trusted_proxies: Vec<IpNet>,
    /// Accepts plain HTTP from any peer. For private networks only.
    pub allow_insecure_http: bool,
    pub key_requests_per_minute: Option<u32>,
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
            trusted_proxies: Vec::new(),
            allow_insecure_http: false,
            key_requests_per_minute: None,
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
        if let Some(value) = get("TRUSTED_PROXIES") {
            settings.trusted_proxies = value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    entry
                        .parse()
                        .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                        .map_err(|_| {
                            ConfigError(format!(
                                "PRIORART_TRUSTED_PROXIES entries must be IPs or CIDRs, got {entry:?}"
                            ))
                        })
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(value) = get("ALLOW_INSECURE_HTTP") {
            settings.allow_insecure_http = match value.as_str() {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(ConfigError(
                        "PRIORART_ALLOW_INSECURE_HTTP must be true or false".into(),
                    ))
                }
            };
        }
        if let Some(value) = get("KEY_REQUESTS_PER_MINUTE") {
            settings.key_requests_per_minute =
                Some(parse("PRIORART_KEY_REQUESTS_PER_MINUTE", &value)?);
        }
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError(message.to_owned()));
        let remote = self.accepts_remote_peers();
        if self.mode == ServerMode::Local && remote {
            return invalid("local mode serves loopback clients only; use authenticated mode");
        }
        if !remote
            && self.host != "localhost"
            && !self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        {
            return invalid(
                "a non-loopback host needs PRIORART_TRUSTED_PROXIES or PRIORART_ALLOW_INSECURE_HTTP",
            );
        }
        if self.key_requests_per_minute == Some(0) {
            return invalid("key_requests_per_minute must be positive");
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

    /// Whether anything but a loopback peer can be served.
    pub fn accepts_remote_peers(&self) -> bool {
        !self.trusted_proxies.is_empty() || self.allow_insecure_http
    }

    pub fn trusts_proxy(&self, peer: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|net| net.contains(&peer))
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
    fn non_loopback_binds_need_authenticated_mode_and_a_transport_decision() {
        let settings = |pairs: &[(&str, &str)]| Settings::from_vars(vars(pairs));
        for mode in ["local", "authenticated"] {
            for host in ["127.0.0.1", "::1", "localhost"] {
                assert!(settings(&[("PRIORART_MODE", mode), ("PRIORART_HOST", host)]).is_ok());
            }
            for host in ["0.0.0.0", "::", "192.168.1.2", "example.com"] {
                assert!(settings(&[("PRIORART_MODE", mode), ("PRIORART_HOST", host)]).is_err());
            }
            for opt_in in [
                ("PRIORART_TRUSTED_PROXIES", "10.0.0.0/8"),
                ("PRIORART_ALLOW_INSECURE_HTTP", "true"),
            ] {
                let result = settings(&[
                    ("PRIORART_MODE", mode),
                    ("PRIORART_HOST", "0.0.0.0"),
                    opt_in,
                ]);
                assert_eq!(result.is_ok(), mode == "authenticated", "{mode} {opt_in:?}");
            }
        }
        let proxied = settings(&[
            ("PRIORART_MODE", "authenticated"),
            ("PRIORART_TRUSTED_PROXIES", " 10.0.0.0/8, ::1 ,192.168.1.7"),
        ])
        .unwrap();
        for (peer, trusted) in [
            ("10.2.3.4", true),
            ("::1", true),
            ("192.168.1.7", true),
            ("192.168.1.8", false),
            ("127.0.0.1", false),
        ] {
            assert_eq!(
                proxied.trusts_proxy(peer.parse().unwrap()),
                trusted,
                "{peer}"
            );
        }
        assert!(settings(&[("PRIORART_MODE", "unknown")]).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (key, value) in [
            ("PRIORART_MAX_LOADED_INDEXES", "0"),
            ("PRIORART_MAX_TOKENS", "0"),
            ("PRIORART_PORT", "0"),
            ("PRIORART_PORT", "70000"),
            ("PRIORART_ADMIN_TOKEN", "too-short"),
            ("PRIORART_KEY_REQUESTS_PER_MINUTE", "0"),
            ("PRIORART_TRUSTED_PROXIES", "10.0.0.0/33"),
            ("PRIORART_TRUSTED_PROXIES", "proxy.internal"),
            ("PRIORART_ALLOW_INSECURE_HTTP", "yes"),
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
