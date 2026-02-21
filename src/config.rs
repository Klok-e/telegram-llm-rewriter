use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_CONTEXT_MESSAGES: usize = 10;

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

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OllamaConfig {
    pub url: String,
    pub model: String,
    #[serde(default = "default_ollama_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RewriteConfig {
    pub chats: Vec<i64>,
    pub system_prompt: String,
    #[serde(default = "default_context_messages")]
    pub context_messages: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotConfig {
    pub ollama_url: String,
    pub ollama_model: String,
    pub rewrite: RewriteConfig,
}

fn default_ollama_timeout_seconds() -> u64 {
    DEFAULT_OLLAMA_TIMEOUT_SECONDS
}

fn default_context_messages() -> usize {
    DEFAULT_CONTEXT_MESSAGES
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
    let url = config.url.trim();
    if url.is_empty() {
        bail!("ollama.url must not be empty");
    }
    Url::parse(url).context("ollama.url must be a valid URL string")?;
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

pub fn extract_hot_config(config: &Config) -> Result<HotConfig> {
    let ollama = config.ollama_required()?;
    let rewrite = config.rewrite_required()?;
    Ok(HotConfig {
        ollama_url: ollama.url.clone(),
        ollama_model: ollama.model.clone(),
        rewrite: rewrite.clone(),
    })
}

pub fn load_hot_config(path: &Path) -> Result<HotConfig> {
    let config = load_config_for_mode(path, ConfigMode::Rewrite)?;
    extract_hot_config(&config)
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
        let rewrite = config.rewrite.expect("rewrite section should exist");
        assert_eq!(rewrite.chats, vec![-1001234567890]);
        assert_eq!(rewrite.context_messages, 10);
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

    #[test]
    fn extract_hot_config_from_valid_config() {
        let config = parse_and_validate_config(VALID_FULL_CONFIG, ConfigMode::Rewrite)
            .expect("config should parse");
        let hot = super::extract_hot_config(&config).expect("should extract hot config");
        assert_eq!(hot.ollama_url, "http://localhost:11434");
        assert_eq!(hot.ollama_model, "llama3");
        assert_eq!(hot.rewrite.chats, vec![-1001234567890]);
        assert_eq!(hot.rewrite.system_prompt, "rewrite this");
    }

    #[test]
    fn load_hot_config_round_trip() {
        let dir = std::env::temp_dir().join("brainrot_test_hot_config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, VALID_FULL_CONFIG).unwrap();

        let hot = super::load_hot_config(&path).expect("should load hot config");
        assert_eq!(hot.ollama_url, "http://localhost:11434");
        assert_eq!(hot.ollama_model, "llama3");
        assert_eq!(hot.rewrite.system_prompt, "rewrite this");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_hot_config_rejects_missing_model() {
        let dir = std::env::temp_dir().join("brainrot_test_hot_missing_model");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
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
        std::fs::write(&path, invalid).unwrap();

        let err = super::load_hot_config(&path).expect_err("should fail");
        assert!(err.to_string().contains("TOML") || err.to_string().contains("model"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_hot_config_rejects_empty_system_prompt() {
        let dir = std::env::temp_dir().join("brainrot_test_hot_empty_prompt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let invalid = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[ollama]
url = "http://localhost:11434"
model = "llama3"

[rewrite]
chats = [-1001234567890]
system_prompt = "   "
"#;
        std::fs::write(&path, invalid).unwrap();

        let err = super::load_hot_config(&path).expect_err("should fail");
        assert!(err.to_string().contains("system_prompt"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_hot_config_rejects_invalid_url() {
        let dir = std::env::temp_dir().join("brainrot_test_hot_invalid_url");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let invalid = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[ollama]
url = "not-a-url"
model = "llama3"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"
"#;
        std::fs::write(&path, invalid).unwrap();

        let err = super::load_hot_config(&path).expect_err("should fail");
        assert!(err.to_string().contains("valid URL"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hot_config_partial_eq() {
        let a = super::HotConfig {
            ollama_url: "http://localhost:11434".into(),
            ollama_model: "llama3".into(),
            rewrite: super::RewriteConfig {
                chats: vec![1],
                system_prompt: "test".into(),
                context_messages: 10,
            },
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = super::HotConfig {
            ollama_model: "gemma".into(),
            ..a.clone()
        };
        assert_ne!(a, c);
    }
}
