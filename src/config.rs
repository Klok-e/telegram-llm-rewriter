use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_TIMEOUT_SECONDS: u64 = 20;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub ollama: Option<OllamaConfig>,
    pub rewrite: Option<RewriteConfig>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigMode {
    Rewrite,
    ListChats,
}

pub fn load_config_for_mode(path: &Path, mode: ConfigMode) -> Result<Config> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_and_validate_config(&raw, mode)
}

fn parse_and_validate_config(raw: &str, mode: ConfigMode) -> Result<Config> {
    let config: Config = toml::from_str(raw).context("failed to parse config.toml as TOML")?;
    validate_config_for_mode(&config, mode)?;
    Ok(config)
}

fn validate_telegram_config(config: &TelegramConfig) -> Result<()> {
    if config.api_id <= 0 {
        bail!("telegram.api_id must be positive");
    }
    if config.api_hash.trim().is_empty() {
        bail!("telegram.api_hash must not be empty");
    }
    if config.session_file.as_os_str().is_empty() {
        bail!("telegram.session_file must not be empty");
    }
    Ok(())
}

fn validate_ollama_config(config: &OllamaConfig) -> Result<()> {
    if config.url.trim().is_empty() {
        bail!("ollama.url must not be empty");
    }
    if config.model.trim().is_empty() {
        bail!("ollama.model must not be empty");
    }
    Ok(())
}

fn validate_rewrite_config(config: &RewriteConfig) -> Result<()> {
    if config.system_prompt.trim().is_empty() {
        bail!("rewrite.system_prompt must not be empty");
    }
    if config.chats.is_empty() {
        bail!("rewrite.chats must not be empty");
    }
    Ok(())
}

fn validate_config_for_mode(config: &Config, mode: ConfigMode) -> Result<()> {
    validate_telegram_config(&config.telegram)?;

    if mode == ConfigMode::Rewrite {
        let ollama = config
            .ollama
            .as_ref()
            .context("missing required [ollama] section for rewrite mode")?;
        validate_ollama_config(ollama)?;

        let rewrite = config
            .rewrite
            .as_ref()
            .context("missing required [rewrite] section for rewrite mode")?;
        validate_rewrite_config(rewrite)?;
    }

    Ok(())
}

impl Config {
    pub fn ollama_required(&self) -> Result<&OllamaConfig> {
        self.ollama
            .as_ref()
            .context("missing required [ollama] section")
    }

    pub fn rewrite_required(&self) -> Result<&RewriteConfig> {
        self.rewrite
            .as_ref()
            .context("missing required [rewrite] section")
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigMode, parse_and_validate_config};

    const VALID_FULL_CONFIG: &str = r#"
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
    fn valid_full_config_parses_for_rewrite_mode() {
        let config = parse_and_validate_config(VALID_FULL_CONFIG, ConfigMode::Rewrite)
            .expect("config should parse");
        assert_eq!(config.telegram.api_id, 12345);
        assert_eq!(
            config.rewrite.expect("rewrite section should exist").chats,
            vec![-1001234567890]
        );
        assert_eq!(
            config
                .ollama
                .expect("ollama section should exist")
                .timeout_seconds,
            20
        );
    }

    #[test]
    fn missing_required_fields_fail_in_rewrite_mode() {
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

        assert!(parse_and_validate_config(invalid, ConfigMode::Rewrite).is_err());
    }

    #[test]
    fn empty_chat_list_fails_in_rewrite_mode() {
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

        let err = parse_and_validate_config(invalid, ConfigMode::Rewrite)
            .expect_err("expected validation to fail");
        assert!(err.to_string().contains("rewrite.chats"));
    }

    #[test]
    fn list_mode_allows_telegram_only_config() {
        let telegram_only = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"
"#;

        parse_and_validate_config(telegram_only, ConfigMode::ListChats)
            .expect("telegram-only config should parse for list mode");
    }

    #[test]
    fn list_mode_accepts_full_config() {
        parse_and_validate_config(VALID_FULL_CONFIG, ConfigMode::ListChats)
            .expect("full config should parse for list mode");
    }

    #[test]
    fn rewrite_mode_requires_ollama_and_rewrite_sections() {
        let telegram_only = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"
"#;

        let err = parse_and_validate_config(telegram_only, ConfigMode::Rewrite)
            .expect_err("rewrite mode should require more than telegram section");
        assert!(err.to_string().contains("[ollama]"));
    }
}
