mod config;
mod context;
mod llm;
mod telegram;

use crate::config::{
    Config, ConfigMode, HotConfig, RewriteConfig, extract_hot_config, load_config_for_mode,
    load_hot_config,
};
use crate::context::{ContextMessage, resolve_sender_name};
use crate::llm::OllamaClient;
use crate::telegram::TelegramBot;
use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser};
use grammers_client::update::{Message as UpdateMessage, Update};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{CreateKind, ModifyKind, RemoveKind},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
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

struct ActiveRewriteState {
    hot_config: HotConfig,
    monitored_chats: HashSet<i64>,
    llm: OllamaClient,
}

impl ActiveRewriteState {
    fn from_hot_config(hot_config: HotConfig, timeout: Duration) -> Result<Self> {
        let monitored_chats: HashSet<i64> = hot_config.rewrite.chats.iter().copied().collect();
        let llm = OllamaClient::new(
            hot_config.ollama_url.clone(),
            hot_config.ollama_model.clone(),
            timeout,
        )?;

        Ok(Self {
            hot_config,
            monitored_chats,
            llm,
        })
    }
}

fn is_relevant_config_event_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
            | EventKind::Create(CreateKind::File | CreateKind::Any)
            | EventKind::Remove(RemoveKind::File | RemoveKind::Any)
            | EventKind::Any
    )
}

fn path_targets_watched_config(candidate: &Path, watched_path: &Path) -> bool {
    if candidate == watched_path {
        return true;
    }
    candidate
        .canonicalize()
        .map(|canonical| canonical == watched_path)
        .unwrap_or(false)
}

fn event_targets_watched_config(event: &Event, watched_path: &Path) -> bool {
    event
        .paths
        .iter()
        .any(|path| path_targets_watched_config(path, watched_path))
}

fn spawn_config_watcher(
    config_path: &Path,
    hot_tx: watch::Sender<HotConfig>,
) -> Result<RecommendedWatcher> {
    let canonical = config_path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize config path: {}",
            config_path.display()
        )
    })?;
    let parent = canonical
        .parent()
        .context("config path has no parent directory")?
        .to_owned();

    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<()>();

    let watched_path = canonical.clone();
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let event = match res {
            Ok(ev) => ev,
            Err(err) => {
                warn!(error = %err, "filesystem watcher error");
                return;
            }
        };

        if !is_relevant_config_event_kind(&event.kind) {
            return;
        }

        if !event_targets_watched_config(&event, &watched_path) {
            return;
        }

        let _ = notify_tx.send(());
    })
    .context("failed to create filesystem watcher")?;

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch directory: {}", parent.display()))?;

    let reload_path = canonical;
    tokio::spawn(async move {
        while notify_rx.recv().await.is_some() {
            // Coalesce rapid events
            while notify_rx.try_recv().is_ok() {}

            // Small delay for atomic renames to settle
            tokio::time::sleep(Duration::from_millis(50)).await;
            while notify_rx.try_recv().is_ok() {}

            match load_hot_config(&reload_path) {
                Ok(new_cfg) => {
                    hot_tx.send_if_modified(|current| {
                        if *current != new_cfg {
                            *current = new_cfg;
                            true
                        } else {
                            false
                        }
                    });
                }
                Err(err) => {
                    warn!(error = %err, "config reload failed; keeping previous config");
                }
            }
        }
    });

    Ok(watcher)
}

async fn run_rewrite_mode(config: &Config, config_path: &Path) -> Result<()> {
    let timeout = Duration::from_secs(config.ollama_required()?.timeout_seconds);
    let mut active = ActiveRewriteState::from_hot_config(extract_hot_config(config)?, timeout)?;

    let mut bot =
        TelegramBot::connect_for_rewrite(&config.telegram, active.monitored_chats.clone()).await?;
    let mut dedupe_cache = DedupeCache::new(Duration::from_secs(DEDUPE_TTL_SECONDS));
    let mut context_cache = ContextCache::new(active.hot_config.rewrite.context_messages);

    let (hot_tx, mut hot_rx) = watch::channel(active.hot_config.clone());
    let _watcher = spawn_config_watcher(config_path, hot_tx)?;

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
                        let chat_id = message.peer_id().bot_api_dialog_id();
                        if bot.is_monitored_chat(chat_id) {
                            info!(
                                chat_id,
                                message_id = message.id(),
                                outgoing = message.outgoing(),
                                "received new message in monitored chat"
                            );
                            context_cache.observe_update_message(chat_id, &message);
                            if let Err(err) = process_message(
                                &bot,
                                &active.llm,
                                &active.hot_config.rewrite,
                                message,
                                chat_id,
                                &mut dedupe_cache,
                                &mut context_cache,
                            )
                            .await
                            {
                                error!(error = %err, "failed to process message");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(err) => warn!(error = %err, "telegram update stream error"),
                }
            }
            Ok(()) = hot_rx.changed() => {
                let new_hot = hot_rx.borrow_and_update().clone();
                match ActiveRewriteState::from_hot_config(new_hot, timeout) {
                    Ok(new_active) => {
                        bot.update_monitored_chats(new_active.monitored_chats.clone());
                        context_cache.retain_chats(&new_active.monitored_chats);
                        context_cache.set_per_chat_limit(new_active.hot_config.rewrite.context_messages);
                        info!(
                            model = %new_active.hot_config.ollama_model,
                            url = %new_active.hot_config.ollama_url,
                            chats = ?new_active.hot_config.rewrite.chats,
                            "config reloaded"
                        );
                        active = new_active;
                    }
                    Err(err) => {
                        warn!(error = %err, "ignoring config reload; keeping previous active config");
                    }
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
    chat_id: i64,
    dedupe_cache: &mut DedupeCache,
    context_cache: &mut ContextCache,
) -> Result<()> {
    if !message.outgoing() {
        return Ok(());
    }

    let message_id = message.id();
    if dedupe_cache.contains(chat_id, message_id) {
        info!(chat_id, message_id, "skipping deduped message");
        return Ok(());
    }

    let original = message.text().trim().to_owned();
    if original.is_empty() {
        info!(chat_id, message_id, "skipping non-text or empty message");
        return Ok(());
    }

    let mut context = context_cache.recent_before(chat_id, message_id, rewrite.context_messages);
    if context_cache.should_backfill(chat_id, rewrite.context_messages, context.len()) {
        info!(
            chat_id,
            message_id,
            requested_context_messages = rewrite.context_messages,
            cached_context_messages = context.len(),
            "fetching context messages from telegram"
        );
        context_cache.mark_hydrated(chat_id);
        match bot.fetch_context(&message, rewrite.context_messages).await {
            Ok(fetched) => {
                info!(
                    chat_id,
                    message_id,
                    fetched_context_messages = fetched.len(),
                    "fetched context messages from telegram"
                );
                context = fetched;
            }
            Err(err) => {
                warn!(
                    chat_id,
                    message_id,
                    requested_context_messages = rewrite.context_messages,
                    error = %err,
                    "failed to fetch context messages; using cached context only"
                );
            }
        }
    }

    let rewritten = match llm
        .rewrite(&rewrite.system_prompt, &context, &original)
        .await
    {
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
        info!(chat_id, message_id, "skipping empty rewrite result");
        return Ok(());
    }
    if rewritten == original {
        info!(chat_id, message_id, "skipping unchanged rewrite result");
        return Ok(());
    }

    match bot.edit_message(&message, rewritten).await {
        Ok(()) => {
            dedupe_cache.insert(chat_id, message_id);
            info!(chat_id, message_id, "rewrote and edited message");
        }
        Err(err) => {
            warn!(
                chat_id,
                message_id,
                original_text = %original,
                rewritten_text = %rewritten,
                error = %err,
                "failed to edit message; continuing"
            );
        }
    }

    Ok(())
}

#[derive(Clone)]
struct CachedContextMessage {
    message_id: i32,
    message: ContextMessage,
}

struct ContextCache {
    per_chat_limit: usize,
    entries: HashMap<i64, VecDeque<CachedContextMessage>>,
    hydrated_chats: HashSet<i64>,
}

impl ContextCache {
    fn new(per_chat_limit: usize) -> Self {
        Self {
            per_chat_limit,
            entries: HashMap::new(),
            hydrated_chats: HashSet::new(),
        }
    }

    fn set_per_chat_limit(&mut self, per_chat_limit: usize) {
        self.per_chat_limit = per_chat_limit;
        for messages in self.entries.values_mut() {
            while messages.len() > self.per_chat_limit {
                messages.pop_front();
            }
        }
    }

    fn retain_chats(&mut self, chats: &HashSet<i64>) {
        self.entries.retain(|chat_id, _| chats.contains(chat_id));
        self.hydrated_chats
            .retain(|chat_id| chats.contains(chat_id));
    }

    fn observe_update_message(&mut self, chat_id: i64, message: &UpdateMessage) {
        let text = message.text().trim().to_owned();
        if text.is_empty() {
            return;
        }

        let peer_name = message.sender().and_then(|p| p.name().map(str::to_owned));
        let sender_name = resolve_sender_name(message.outgoing(), peer_name.as_deref());
        self.record_message(chat_id, message.id(), ContextMessage { sender_name, text });
    }

    fn record_message(&mut self, chat_id: i64, message_id: i32, message: ContextMessage) {
        let chat_messages = self.entries.entry(chat_id).or_default();
        if chat_messages
            .back()
            .is_some_and(|cached| cached.message_id == message_id)
        {
            return;
        }
        chat_messages.push_back(CachedContextMessage {
            message_id,
            message,
        });
        while chat_messages.len() > self.per_chat_limit {
            chat_messages.pop_front();
        }
    }

    fn recent_before(&self, chat_id: i64, message_id: i32, count: usize) -> Vec<ContextMessage> {
        if count == 0 {
            return Vec::new();
        }

        let mut recent = Vec::with_capacity(count);
        if let Some(messages) = self.entries.get(&chat_id) {
            for cached in messages.iter().rev() {
                if cached.message_id == message_id {
                    continue;
                }
                recent.push(cached.message.clone());
                if recent.len() >= count {
                    break;
                }
            }
        }
        recent.reverse();
        recent
    }

    fn should_backfill(&self, chat_id: i64, count: usize, cached_count: usize) -> bool {
        count > 0 && cached_count < count && !self.hydrated_chats.contains(&chat_id)
    }

    fn mark_hydrated(&mut self, chat_id: i64) {
        self.hydrated_chats.insert(chat_id);
    }
}

fn truncate_to_telegram_limit(input: &str, max_chars: usize) -> &str {
    match input.char_indices().nth(max_chars) {
        Some((byte_offset, _)) => &input[..byte_offset],
        None => input,
    }
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
    entries: HashMap<(i64, i32), Instant>,
    ttl: Duration,
}

impl DedupeCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    fn contains(&mut self, chat_id: i64, message_id: i32) -> bool {
        self.evict_expired();
        self.entries.contains_key(&(chat_id, message_id))
    }

    fn insert(&mut self, chat_id: i64, message_id: i32) {
        self.entries.insert((chat_id, message_id), Instant::now());
    }

    fn evict_expired(&mut self) {
        let ttl = self.ttl;
        self.entries.retain(|_, seen_at| seen_at.elapsed() <= ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveRewriteState, AppMode, ContextCache, DedupeCache, event_targets_watched_config,
        is_relevant_config_event_kind, parse_args_from,
    };
    use crate::config::{HotConfig, RewriteConfig};
    use crate::context::ContextMessage;
    use notify::{
        Event, EventKind,
        event::{AccessKind, CreateKind, ModifyKind, RemoveKind},
    };
    use std::path::PathBuf;
    use std::time::Duration;

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

    #[test]
    fn relevant_config_event_kinds_are_detected() {
        assert!(is_relevant_config_event_kind(&EventKind::Modify(
            ModifyKind::Any
        )));
        assert!(is_relevant_config_event_kind(&EventKind::Create(
            CreateKind::Any
        )));
        assert!(is_relevant_config_event_kind(&EventKind::Remove(
            RemoveKind::Any
        )));
        assert!(is_relevant_config_event_kind(&EventKind::Any));
        assert!(!is_relevant_config_event_kind(&EventKind::Access(
            AccessKind::Any
        )));
    }

    #[test]
    fn event_targets_watched_config_by_exact_path() {
        let watched_parent = std::env::temp_dir().join("brainrot_watcher_exact_match");
        std::fs::create_dir_all(&watched_parent).expect("parent should exist");
        let watched_path = watched_parent.join("config.toml");
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![watched_path.clone()],
            attrs: Default::default(),
        };
        assert!(event_targets_watched_config(&event, &watched_path));
        std::fs::remove_dir_all(&watched_parent).ok();
    }

    #[test]
    fn event_targets_watched_config_by_normalized_parent_path() {
        let watched_parent = std::env::temp_dir().join("brainrot_watcher_normalized_parent");
        std::fs::create_dir_all(&watched_parent).expect("parent should exist");
        let watched_path = watched_parent.join("config.toml");
        let path_with_dot = watched_parent.join(".").join("config.toml");
        let event = Event {
            kind: EventKind::Create(CreateKind::Any),
            paths: vec![path_with_dot],
            attrs: Default::default(),
        };
        assert!(event_targets_watched_config(&event, &watched_path));
        std::fs::remove_dir_all(&watched_parent).ok();
    }

    #[test]
    fn event_does_not_target_other_files() {
        let watched_parent = std::env::temp_dir().join("brainrot_watcher_other_files");
        std::fs::create_dir_all(&watched_parent).expect("parent should exist");
        let watched_path = watched_parent.join("config.toml");
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![watched_parent.join("other.toml")],
            attrs: Default::default(),
        };
        assert!(!event_targets_watched_config(&event, &watched_path));
        std::fs::remove_dir_all(&watched_parent).ok();
    }

    #[test]
    fn active_rewrite_state_rejects_invalid_ollama_url() {
        let hot = HotConfig {
            ollama_url: "not-a-url".to_owned(),
            ollama_model: "llama3".to_owned(),
            rewrite: RewriteConfig {
                chats: vec![-1001234567890],
                system_prompt: "rewrite this".to_owned(),
                context_messages: 10,
            },
        };
        let result = ActiveRewriteState::from_hot_config(hot, Duration::from_secs(5));
        assert!(result.is_err(), "invalid URL should fail");
        let err = match result {
            Ok(_) => unreachable!("checked above"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("valid URL"));
    }

    #[test]
    fn dedupe_cache_scopes_entries_by_chat_id() {
        let mut cache = DedupeCache::new(Duration::from_secs(300));
        let message_id = 42;

        assert!(!cache.contains(1, message_id));
        cache.insert(1, message_id);
        assert!(cache.contains(1, message_id));
        assert!(
            !cache.contains(2, message_id),
            "same message id in another chat must not dedupe"
        );
    }

    #[test]
    fn context_cache_returns_recent_messages_in_order_excluding_current() {
        let mut cache = ContextCache::new(10);
        let chat_id = -1001234567890;
        cache.record_message(
            chat_id,
            1,
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "one".to_owned(),
            },
        );
        cache.record_message(
            chat_id,
            2,
            ContextMessage {
                sender_name: "Bob".to_owned(),
                text: "two".to_owned(),
            },
        );
        cache.record_message(
            chat_id,
            3,
            ContextMessage {
                sender_name: "Me".to_owned(),
                text: "three".to_owned(),
            },
        );

        let context = cache.recent_before(chat_id, 3, 2);
        assert_eq!(
            context,
            vec![
                ContextMessage {
                    sender_name: "Alice".to_owned(),
                    text: "one".to_owned(),
                },
                ContextMessage {
                    sender_name: "Bob".to_owned(),
                    text: "two".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn context_cache_marks_chat_hydrated_to_avoid_repeat_backfill() {
        let mut cache = ContextCache::new(10);
        let chat_id = -1001234567890;

        assert!(cache.should_backfill(chat_id, 10, 0));
        cache.mark_hydrated(chat_id);
        assert!(!cache.should_backfill(chat_id, 10, 0));
    }
}
