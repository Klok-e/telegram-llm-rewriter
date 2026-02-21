use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OPENAI_TIMEOUT_SECONDS: u64 = 20;
const DEFAULT_CONTEXT_MESSAGES: usize = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub openai: Option<OpenAiConfig>,
    pub rewrite: Option<RewriteConfig>,
    pub integration_test: Option<IntegrationTestConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub session_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_openai_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RewriteConfig {
    pub chats: Vec<i64>,
    pub system_prompt: String,
    #[serde(default = "default_context_messages")]
    pub context_messages: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IntegrationTestConfig {
    pub chat_id: i64,
    pub topic_a_root_id: i32,
    pub topic_b_root_id: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotConfig {
    pub openai_api_key: String,
    pub openai_model: String,
    pub rewrite: RewriteConfig,
}

fn default_openai_timeout_seconds() -> u64 {
    DEFAULT_OPENAI_TIMEOUT_SECONDS
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

fn validate_openai_config(config: &OpenAiConfig) -> Result<()> {
    if config.api_key.trim().is_empty() {
        bail!("openai.api_key must not be empty");
    }
    if config.model.trim().is_empty() {
        bail!("openai.model must not be empty");
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

fn validate_integration_test_config(config: &IntegrationTestConfig) -> Result<()> {
    if config.chat_id == 0 {
        bail!("integration_test.chat_id must not be zero");
    }
    if config.topic_a_root_id < 0 {
        bail!("integration_test.topic_a_root_id must be non-negative");
    }
    if config.topic_b_root_id < 0 {
        bail!("integration_test.topic_b_root_id must be non-negative");
    }
    if config.topic_a_root_id == config.topic_b_root_id {
        bail!("integration_test topic ids must be different");
    }
    Ok(())
}

fn validate_config_for_mode(config: &Config, mode: ConfigMode) -> Result<()> {
    validate_telegram_config(&config.telegram)?;
    if let Some(integration_test) = config.integration_test.as_ref() {
        validate_integration_test_config(integration_test)?;
    }

    if mode == ConfigMode::Rewrite {
        let openai = config
            .openai
            .as_ref()
            .context("missing required [openai] section for rewrite mode")?;
        validate_openai_config(openai)?;

        let rewrite = config
            .rewrite
            .as_ref()
            .context("missing required [rewrite] section for rewrite mode")?;
        validate_rewrite_config(rewrite)?;
    }

    Ok(())
}

impl Config {
    pub fn openai_required(&self) -> Result<&OpenAiConfig> {
        self.openai
            .as_ref()
            .context("missing required [openai] section")
    }

    pub fn rewrite_required(&self) -> Result<&RewriteConfig> {
        self.rewrite
            .as_ref()
            .context("missing required [rewrite] section")
    }
}

pub fn extract_hot_config(config: &Config) -> Result<HotConfig> {
    let openai = config.openai_required()?;
    let rewrite = config.rewrite_required()?;
    Ok(HotConfig {
        openai_api_key: openai.api_key.clone(),
        openai_model: openai.model.clone(),
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

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

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
                .openai
                .expect("openai section should exist")
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

[openai]
api_key = "sk-test"

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

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

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
    fn rewrite_mode_requires_openai_and_rewrite_sections() {
        let telegram_only = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"
"#;

        let err = parse_and_validate_config(telegram_only, ConfigMode::Rewrite)
            .expect_err("rewrite mode should require more than telegram section");
        assert!(err.to_string().contains("[openai]"));
    }

    #[test]
    fn extract_hot_config_from_valid_config() {
        let config = parse_and_validate_config(VALID_FULL_CONFIG, ConfigMode::Rewrite)
            .expect("config should parse");
        let hot = super::extract_hot_config(&config).expect("should extract hot config");
        assert_eq!(hot.openai_api_key, "sk-test");
        assert_eq!(hot.openai_model, "gpt-4.1-mini");
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
        assert_eq!(hot.openai_api_key, "sk-test");
        assert_eq!(hot.openai_model, "gpt-4.1-mini");
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

[openai]
api_key = "sk-test"

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

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

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
    fn load_hot_config_rejects_empty_api_key() {
        let dir = std::env::temp_dir().join("brainrot_test_hot_empty_api_key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let invalid = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "   "
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"
"#;
        std::fs::write(&path, invalid).unwrap();

        let err = super::load_hot_config(&path).expect_err("should fail");
        assert!(err.to_string().contains("openai.api_key"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hot_config_partial_eq() {
        let a = super::HotConfig {
            openai_api_key: "sk-test".into(),
            openai_model: "gpt-4.1-mini".into(),
            rewrite: super::RewriteConfig {
                chats: vec![1],
                system_prompt: "test".into(),
                context_messages: 10,
            },
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = super::HotConfig {
            openai_model: "gpt-4.1".into(),
            ..a.clone()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn integration_test_config_parses_when_present() {
        let with_integration = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"

[integration_test]
chat_id = -1001234567890
topic_a_root_id = 101
topic_b_root_id = 202
"#;
        let config = parse_and_validate_config(with_integration, ConfigMode::Rewrite)
            .expect("config with integration_test should parse");
        let integration = config
            .integration_test
            .expect("integration_test section should exist");
        assert_eq!(integration.chat_id, -1001234567890);
        assert_eq!(integration.topic_a_root_id, 101);
        assert_eq!(integration.topic_b_root_id, 202);
    }

    #[test]
    fn integration_test_config_allows_general_topic_marker_zero() {
        let with_integration = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"

[integration_test]
chat_id = -1001234567890
topic_a_root_id = 0
topic_b_root_id = 202
"#;
        parse_and_validate_config(with_integration, ConfigMode::Rewrite)
            .expect("topic_*_root_id = 0 should be accepted as general topic marker");
    }

    #[test]
    fn integration_test_config_rejects_zero_chat_id() {
        let with_integration = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"

[integration_test]
chat_id = 0
topic_a_root_id = 101
topic_b_root_id = 202
"#;
        let err = parse_and_validate_config(with_integration, ConfigMode::Rewrite)
            .expect_err("config should reject zero integration_test.chat_id");
        assert!(err.to_string().contains("integration_test.chat_id"));
    }

    #[test]
    fn integration_test_config_rejects_identical_topic_ids() {
        let with_integration = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"

[integration_test]
chat_id = -1001234567890
topic_a_root_id = 101
topic_b_root_id = 101
"#;
        let err = parse_and_validate_config(with_integration, ConfigMode::Rewrite)
            .expect_err("config should reject identical integration_test topic ids");
        assert!(err.to_string().contains("topic ids"));
    }

    #[test]
    fn integration_test_config_rejects_negative_topic_id() {
        let with_integration = r#"
[telegram]
api_id = 12345
api_hash = "hash"
session_file = "session.bin"

[openai]
api_key = "sk-test"
model = "gpt-4.1-mini"

[rewrite]
chats = [-1001234567890]
system_prompt = "rewrite this"

[integration_test]
chat_id = -1001234567890
topic_a_root_id = -1
topic_b_root_id = 101
"#;
        let err = parse_and_validate_config(with_integration, ConfigMode::Rewrite)
            .expect_err("negative integration_test topic id should fail");
        assert!(err.to_string().contains("non-negative"));
    }
}
