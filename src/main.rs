mod config;
mod llm;
mod telegram;

use crate::config::{Config, ConfigMode, RewriteConfig, load_config_for_mode};
use crate::llm::OllamaClient;
use crate::telegram::TelegramBot;
use anyhow::{Result, anyhow};
use clap::{ArgAction, Parser};
use grammers_client::Update;
use grammers_client::types::update::Message as UpdateMessage;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const TELEGRAM_MESSAGE_MAX_CHARS: usize = 4096;
const DEDUPE_TTL_SECONDS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppMode {
    Rewrite,
    ListChats { query: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppArgs {
    config_path: PathBuf,
    mode: AppMode,
}

#[derive(Debug, Parser)]
#[command(name = "brainrot_tg_llm_rewrite")]
#[command(about = "Telegram userbot rewriter with optional chat listing mode")]
struct Cli {
    #[arg(long, value_name = "path", default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    #[arg(long, action = ArgAction::SetTrue)]
    list_chats: bool,
    #[arg(value_name = "query", requires = "list_chats")]
    query: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args = parse_args()?;
    let config_mode = match args.mode {
        AppMode::Rewrite => ConfigMode::Rewrite,
        AppMode::ListChats { .. } => ConfigMode::ListChats,
    };
    let config = load_config_for_mode(&args.config_path, config_mode)?;

    match args.mode {
        AppMode::ListChats { query } => run_list_mode(&config, query.as_deref()).await,
        AppMode::Rewrite => run_rewrite_mode(&config, &args.config_path).await,
    }
}

async fn run_list_mode(config: &Config, query: Option<&str>) -> Result<()> {
    let mut bot = TelegramBot::connect_for_listing(&config.telegram).await?;
    let chats = bot.list_chats(query).await?;

    if chats.is_empty() {
        if let Some(query) = query {
            println!("No chats matched filter: {query}");
        } else {
            println!("No chats found.");
        }
    } else {
        for chat in chats {
            println!("{}\t{}", chat.id, chat.name);
        }
    }

    bot.shutdown().await?;
    Ok(())
}

async fn run_rewrite_mode(config: &Config, config_path: &std::path::Path) -> Result<()> {
    let ollama = config.ollama_required()?.clone();
    let rewrite = config.rewrite_required()?.clone();
    let monitored_chats: HashSet<i64> = rewrite.chats.iter().copied().collect();

    let mut bot = TelegramBot::connect_for_rewrite(&config.telegram, monitored_chats).await?;
    let llm = OllamaClient::new(
        ollama.url.clone(),
        ollama.model.clone(),
        Duration::from_secs(ollama.timeout_seconds),
    )?;
    let mut dedupe_cache = DedupeCache::new(Duration::from_secs(DEDUPE_TTL_SECONDS));

    info!(config_path = %config_path.display(), "brainrot rewriter started");

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                if let Err(err) = signal {
                    warn!(error = %err, "failed to listen for Ctrl+C");
                }
                info!("shutdown signal received");
                break;
            }
            update_result = bot.next_update() => {
                match update_result {
                    Ok(Update::NewMessage(message)) => {
                        if let Err(err) = process_message(
                            &bot,
                            &llm,
                            &rewrite,
                            message,
                            &mut dedupe_cache,
                        ).await {
                            error!(error = %err, "failed to process message");
                        }
                    }
                    Ok(_) => {}
                    Err(err) => warn!(error = %err, "telegram update stream error"),
                }
            }
        }
    }

    bot.shutdown().await?;

    Ok(())
}

fn parse_args() -> Result<AppArgs> {
    parse_args_from(std::env::args_os())
}

fn parse_args_from<I, S>(args: I) -> Result<AppArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).map_err(|error| anyhow!(error.to_string()))?;
    let mode = if cli.list_chats {
        AppMode::ListChats { query: cli.query }
    } else {
        AppMode::Rewrite
    };

    Ok(AppArgs {
        config_path: cli.config,
        mode,
    })
}

async fn process_message(
    bot: &TelegramBot,
    llm: &OllamaClient,
    rewrite: &RewriteConfig,
    message: UpdateMessage,
    dedupe_cache: &mut DedupeCache,
) -> Result<()> {
    if !message.outgoing() {
        return Ok(());
    }

    let chat_id = bot.chat_id_for_message(&message);
    if !bot.is_monitored_chat(chat_id) {
        return Ok(());
    }

    let message_id = message.id();
    if dedupe_cache.contains(message_id) {
        debug!(chat_id, message_id, "skipping deduped message");
        return Ok(());
    }

    let original = message.text().trim().to_owned();
    if original.is_empty() {
        debug!(chat_id, message_id, "skipping non-text or empty message");
        return Ok(());
    }

    let rewritten = match llm.rewrite(&rewrite.system_prompt, &original).await {
        Ok(text) => text,
        Err(err) => {
            warn!(
                chat_id,
                message_id,
                error = %err,
                "ollama rewrite failed; leaving original message unchanged"
            );
            return Ok(());
        }
    };

    let rewritten = truncate_to_telegram_limit(rewritten.trim(), TELEGRAM_MESSAGE_MAX_CHARS);
    if rewritten.is_empty() {
        debug!(chat_id, message_id, "skipping empty rewrite result");
        return Ok(());
    }
    if rewritten == original {
        debug!(chat_id, message_id, "skipping unchanged rewrite result");
        return Ok(());
    }

    match bot.edit_message(&message, &rewritten).await {
        Ok(()) => {
            dedupe_cache.insert(message_id);
            info!(chat_id, message_id, "rewrote and edited message");
        }
        Err(err) => {
            warn!(
                chat_id,
                message_id,
                error = %err,
                "failed to edit message; continuing"
            );
        }
    }

    Ok(())
}

fn truncate_to_telegram_limit(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_owned();
    }
    input.chars().take(max_chars).collect()
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();
}

struct DedupeCache {
    entries: HashMap<i32, Instant>,
    ttl: Duration,
}

impl DedupeCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    fn contains(&mut self, message_id: i32) -> bool {
        self.evict_expired();
        self.entries.contains_key(&message_id)
    }

    fn insert(&mut self, message_id: i32) {
        self.evict_expired();
        self.entries.insert(message_id, Instant::now());
    }

    fn evict_expired(&mut self) {
        let ttl = self.ttl;
        self.entries.retain(|_, seen_at| seen_at.elapsed() <= ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::{AppMode, parse_args_from};
    use std::path::PathBuf;

    #[test]
    fn parse_list_chats_without_query() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--list-chats"])
            .expect("parsing should succeed");
        assert_eq!(parsed.mode, AppMode::ListChats { query: None });
    }

    #[test]
    fn parse_list_chats_with_query() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--list-chats", "work"])
            .expect("parsing should succeed");
        assert_eq!(
            parsed.mode,
            AppMode::ListChats {
                query: Some("work".to_string()),
            }
        );
    }

    #[test]
    fn parse_config_path() {
        let parsed = parse_args_from(["brainrot_tg_llm_rewrite", "--config", "custom.toml"])
            .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("custom.toml"));
        assert_eq!(parsed.mode, AppMode::Rewrite);
    }

    #[test]
    fn parse_missing_config_path_fails() {
        let err = parse_args_from(["brainrot_tg_llm_rewrite", "--config"])
            .expect_err("parsing should fail");
        assert!(err.to_string().contains("--config"));
    }

    #[test]
    fn parse_unknown_flag_fails() {
        let err =
            parse_args_from(["brainrot_tg_llm_rewrite", "--wat"]).expect_err("parsing should fail");
        assert!(err.to_string().contains("--wat"));
    }

    #[test]
    fn parse_config_then_list_mode_with_query() {
        let parsed = parse_args_from([
            "brainrot_tg_llm_rewrite",
            "--config",
            "x.toml",
            "--list-chats",
            "team",
        ])
        .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("x.toml"));
        assert_eq!(
            parsed.mode,
            AppMode::ListChats {
                query: Some("team".to_string()),
            }
        );
    }

    #[test]
    fn parse_list_mode_then_config_without_query() {
        let parsed = parse_args_from([
            "brainrot_tg_llm_rewrite",
            "--list-chats",
            "--config",
            "x.toml",
        ])
        .expect("parsing should succeed");
        assert_eq!(parsed.config_path, PathBuf::from("x.toml"));
        assert_eq!(parsed.mode, AppMode::ListChats { query: None });
    }

    #[test]
    fn parse_query_without_list_mode_fails() {
        let err =
            parse_args_from(["brainrot_tg_llm_rewrite", "work"]).expect_err("parsing should fail");
        assert!(err.to_string().contains("--list-chats"));
    }
}
