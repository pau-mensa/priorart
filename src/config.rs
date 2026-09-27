//! Settings, read once from `PRIORART_*` environment variables.

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub encoder: String,
    pub encoder_file: String,
    pub encoder_revision: String,
    pub encoder_threads: Option<usize>,
    pub gather_limit: usize,
    pub max_loaded_indexes: usize,
    pub max_text_bytes: usize,
    pub host: String,
    pub port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            encoder: "none".to_owned(),
            encoder_file: "model_int8.onnx".to_owned(),
            encoder_revision: "main".to_owned(),
            encoder_threads: None,
            gather_limit: 500,
            max_loaded_indexes: 8,
            max_text_bytes: 262_144,
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
        if let Some(value) = get("DATA_DIR") {
            settings.data_dir = PathBuf::from(value);
        }
        if let Some(value) = get("ENCODER") {
            settings.encoder = value;
        }
        if let Some(value) = get("ENCODER_FILE") {
            settings.encoder_file = value;
        }
        if let Some(value) = get("ENCODER_REVISION") {
            settings.encoder_revision = value;
        }
        if let Some(value) = get("ENCODER_THREADS") {
            settings.encoder_threads = match value.as_str() {
                "" => None,
                _ => Some(parse("PRIORART_ENCODER_THREADS", &value)?),
            };
        }
        if let Some(value) = get("MAX_LOADED_INDEXES") {
            settings.max_loaded_indexes = parse("PRIORART_MAX_LOADED_INDEXES", &value)?;
        }
        if let Some(value) = get("GATHER_LIMIT") {
            settings.gather_limit = parse("PRIORART_GATHER_LIMIT", &value)?;
        }
        if let Some(value) = get("MAX_TEXT_BYTES") {
            settings.max_text_bytes = parse("PRIORART_MAX_TEXT_BYTES", &value)?;
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
        if self.max_loaded_indexes == 0 {
            return invalid("max_loaded_indexes must be positive");
        }
        if self.gather_limit == 0 {
            return invalid("gather_limit must be positive");
        }
        if self.max_text_bytes == 0 {
            return invalid("max_text_bytes must be positive");
        }
        if self.encoder_threads == Some(0) {
            return invalid("encoder_threads must be positive");
        }
        if self.port == 0 {
            return invalid("port must be in 1..65535");
        }
        Ok(())
    }

    pub fn lexical_only(&self) -> bool {
        self.encoder.eq_ignore_ascii_case("none")
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
    fn encoder_fields_come_from_the_environment() {
        let settings = Settings::from_vars(vars(&[
            ("PRIORART_ENCODER", "lightonai/LateOn-Code"),
            ("PRIORART_ENCODER_FILE", "model.onnx"),
            ("PRIORART_ENCODER_THREADS", "4"),
        ]))
        .unwrap();
        assert_eq!(settings.encoder, "lightonai/LateOn-Code");
        assert_eq!(settings.encoder_file, "model.onnx");
        assert_eq!(settings.encoder_threads, Some(4));
        assert!(!settings.lexical_only());
        let blank = Settings::from_vars(vars(&[("PRIORART_ENCODER_THREADS", "")])).unwrap();
        assert_eq!(blank.encoder_threads, None);
        assert!(blank.lexical_only());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (key, value) in [
            ("PRIORART_ENCODER_THREADS", "0"),
            ("PRIORART_GATHER_LIMIT", "0"),
            ("PRIORART_MAX_LOADED_INDEXES", "0"),
            ("PRIORART_MAX_TEXT_BYTES", "-1"),
            ("PRIORART_PORT", "0"),
            ("PRIORART_PORT", "70000"),
        ] {
            assert!(
                Settings::from_vars(vars(&[(key, value)])).is_err(),
                "{key}={value}"
            );
        }
    }
}
