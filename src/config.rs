use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_TIMEOUT_SECONDS: u64 = 20;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub ollama: OllamaConfig,
    pub rewrite: RewriteConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub session_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaConfig {
    pub url: String,
    pub model: String,
    #[serde(default = "default_ollama_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RewriteConfig {
    pub chats: Vec<i64>,
    pub system_prompt: String,
}

fn default_ollama_timeout_seconds() -> u64 {
    DEFAULT_OLLAMA_TIMEOUT_SECONDS
}

pub fn load_config(path: &Path) -> Result<Config> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_config(&raw)
}

fn parse_config(raw: &str) -> Result<Config> {
    let config: Config = toml::from_str(raw).context("failed to parse config.toml as TOML")?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    if config.telegram.api_id <= 0 {
        bail!("telegram.api_id must be positive");
    }
    if config.telegram.api_hash.trim().is_empty() {
        bail!("telegram.api_hash must not be empty");
    }
    if config.telegram.session_file.as_os_str().is_empty() {
        bail!("telegram.session_file must not be empty");
    }
    if config.ollama.url.trim().is_empty() {
        bail!("ollama.url must not be empty");
    }
    if config.ollama.model.trim().is_empty() {
        bail!("ollama.model must not be empty");
    }
    if config.rewrite.system_prompt.trim().is_empty() {
        bail!("rewrite.system_prompt must not be empty");
    }
    if config.rewrite.chats.is_empty() {
        bail!("rewrite.chats must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_config;

    const VALID_CONFIG: &str = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[ollama]
url = "http://localhost:11434"
model = "llama3"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"
"#;

    #[test]
    fn valid_config_parses() {
        let config = parse_config(VALID_CONFIG).expect("config should parse");
        assert_eq!(config.telegram.api_id, 12345);
        assert_eq!(config.rewrite.chats, vec![-1001234567890]);
        assert_eq!(config.ollama.timeout_seconds, 20);
    }

    #[test]
    fn missing_required_fields_fail() {
        let invalid = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[ollama]
url = "http://localhost:11434"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"
"#;

        assert!(parse_config(invalid).is_err());
    }

    #[test]
    fn empty_chat_list_fails() {
        let invalid = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[ollama]
url = "http://localhost:11434"
model = "llama3"

[rewrite]
chats = []
system_prompt = "rewrite this"
"#;

        let err = parse_config(invalid).expect_err("expected validation to fail");
        assert!(err.to_string().contains("rewrite.chats"));
    }
}
