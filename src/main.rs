mod config;
mod llm;
mod telegram;

use crate::config::{RewriteConfig, load_config};
use crate::llm::OllamaClient;
use crate::telegram::TelegramBot;
use anyhow::Result;
use grammers_client::Update;
use grammers_client::types::update::Message as UpdateMessage;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG_PATH: &str = "config.toml";
const TELEGRAM_MESSAGE_MAX_CHARS: usize = 4096;
const DEDUPE_TTL_SECONDS: u64 = 300;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = load_config(&config_path)?;
    let monitored_chats: HashSet<i64> = config.rewrite.chats.iter().copied().collect();

    let mut bot = TelegramBot::connect_and_authorize(&config.telegram, monitored_chats).await?;
    let llm = OllamaClient::new(
        config.ollama.url.clone(),
        config.ollama.model.clone(),
        Duration::from_secs(config.ollama.timeout_seconds),
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
                            &config.rewrite,
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
