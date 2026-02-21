use crate::config::{Config, HotConfig, RewriteConfig, extract_hot_config, load_hot_config};
use crate::context::{ContextMessage, resolve_sender_name};
use crate::llm::OpenAiClient;
use crate::telegram::{TelegramBot, message_topic_root_id};
use anyhow::{Context, Result};
use grammers_client::Client;
use grammers_client::update::{Message as UpdateMessage, Update};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{CreateKind, ModifyKind, RemoveKind},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};
use tracing_log::LogTracer;
use tracing_subscriber::EnvFilter;

const TELEGRAM_MESSAGE_MAX_CHARS: usize = 4096;
const DEDUPE_TTL_SECONDS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitoredUpdateKind {
    NewMessage,
}

#[derive(Debug, Clone)]
pub enum RewriteEvent {
    RuntimeReady {
        catch_up_enabled: bool,
        skip_historical_catch_up_messages: bool,
        startup_unix: i64,
    },
    MonitoredUpdate {
        chat_id: i64,
        topic_root_id: Option<i32>,
        message_id: i32,
        outgoing: bool,
        kind: MonitoredUpdateKind,
    },
    MessageEdited {
        chat_id: i64,
        message_id: i32,
    },
    UnsupportedUpdateIgnored {
        update_kind: String,
    },
}

#[derive(Default)]
pub struct RewriteHooks {
    on_event: Option<Arc<dyn Fn(RewriteEvent) + Send + Sync>>,
    on_client_ready: Option<oneshot::Sender<Client>>,
}

impl RewriteHooks {
    pub fn with_event_handler<F>(handler: F) -> Self
    where
        F: Fn(RewriteEvent) + Send + Sync + 'static,
    {
        Self {
            on_event: Some(Arc::new(handler)),
            on_client_ready: None,
        }
    }

    pub fn with_client_channel(mut self, sender: oneshot::Sender<Client>) -> Self {
        self.on_client_ready = Some(sender);
        self
    }

    fn emit(&self, event: RewriteEvent) {
        if let Some(handler) = self.on_event.as_ref() {
            handler(event);
        }
    }

    fn send_client(&mut self, client: Client) {
        if let Some(sender) = self.on_client_ready.take() {
            let _ = sender.send(client);
        }
    }
}

#[derive(Debug, Clone)]
pub struct RewriteRuntimeOptions {
    pub catch_up_enabled: bool,
    pub skip_historical_catch_up_messages: bool,
    pub rewrite_override: Option<String>,
}

impl RewriteRuntimeOptions {
    fn from_env() -> Self {
        Self {
            catch_up_enabled: should_enable_catch_up(),
            skip_historical_catch_up_messages: should_skip_historical_catch_up_messages(),
            rewrite_override: test_rewrite_override(),
        }
    }
}

static TRACING_INIT: OnceLock<()> = OnceLock::new();

pub fn init_tracing() {
    TRACING_INIT.get_or_init(|| {
        let _ = LogTracer::init();
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(false)
            .compact()
            .try_init();
    });
}

pub async fn run_rewrite_mode(config: &Config, config_path: &Path) -> Result<()> {
    run_rewrite_mode_with_shutdown_and_hooks(
        config,
        config_path,
        async {
            if let Err(err) = tokio::signal::ctrl_c().await {
                warn!(error = %err, "failed to listen for Ctrl+C");
            }
        },
        RewriteHooks::default(),
        RewriteRuntimeOptions::from_env(),
    )
    .await
}

pub async fn run_rewrite_mode_with_shutdown_and_hooks<S>(
    config: &Config,
    config_path: &Path,
    shutdown_signal: S,
    mut hooks: RewriteHooks,
    runtime_options: RewriteRuntimeOptions,
) -> Result<()>
where
    S: Future<Output = ()> + Send,
{
    let timeout = Duration::from_secs(config.openai_required()?.timeout_seconds);
    let mut active = ActiveRewriteState::from_hot_config(extract_hot_config(config)?, timeout)?;
    let catch_up_enabled = runtime_options.catch_up_enabled;
    let skip_historical_catch_up_messages = runtime_options.skip_historical_catch_up_messages;
    let rewrite_override = normalize_rewrite_override(runtime_options.rewrite_override);

    let mut bot = TelegramBot::connect_for_rewrite(
        &config.telegram,
        active.monitored_chats.clone(),
        catch_up_enabled,
    )
    .await?;
    let mut dedupe_cache = DedupeCache::new(Duration::from_secs(DEDUPE_TTL_SECONDS));
    let mut context_cache = ContextCache::new(active.hot_config.rewrite.context_messages);
    let startup_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    hooks.send_client(bot.client_clone());
    hooks.emit(RewriteEvent::RuntimeReady {
        catch_up_enabled,
        skip_historical_catch_up_messages,
        startup_unix,
    });

    let (hot_tx, mut hot_rx) = watch::channel(active.hot_config.clone());
    let _watcher = spawn_config_watcher(config_path, hot_tx)?;

    info!(
        config_path = %config_path.display(),
        catch_up_enabled,
        skip_historical_catch_up_messages,
        startup_unix,
        "brainrot rewriter started"
    );
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            () = &mut shutdown_signal => {
                info!("shutdown signal received");
                break;
            }
            update_result = bot.next_update() => {
                match update_result {
                    Ok(Update::NewMessage(message)) => {
                        let chat_id = message.peer_id().bot_api_dialog_id();
                        if bot.is_monitored_chat(chat_id) {
                            let context_scope = ContextScope {
                                chat_id,
                                topic_root_id: message_topic_root_id(&message),
                            };
                            let message_id = message.id();
                            let message_unix = message.date().timestamp();
                            context_cache.observe_update_message(context_scope, &message);
                            if skip_historical_catch_up_messages && is_historical_catch_up_message(
                                message_unix,
                                startup_unix
                            ) {
                                info!(
                                    chat_id,
                                    message_id,
                                    message_unix,
                                    startup_unix,
                                    "skipping historical message during catch-up"
                                );
                                continue;
                            }
                            info!(
                                chat_id,
                                topic_root_id = ?context_scope.topic_root_id,
                                update_kind = "new_message",
                                message_id,
                                outgoing = message.outgoing(),
                                "received message update in monitored chat"
                            );
                            hooks.emit(RewriteEvent::MonitoredUpdate {
                                chat_id,
                                topic_root_id: context_scope.topic_root_id,
                                message_id,
                                outgoing: message.outgoing(),
                                kind: MonitoredUpdateKind::NewMessage,
                            });
                            let mut runtime = ProcessMessageRuntime {
                                dedupe_cache: &mut dedupe_cache,
                                context_cache: &mut context_cache,
                                rewrite_override: rewrite_override.as_deref(),
                                hooks: &hooks,
                            };
                            if let Err(err) = process_message(
                                &bot,
                                &active.llm,
                                &active.hot_config.rewrite,
                                message,
                                context_scope,
                                &mut runtime,
                            )
                            .await
                            {
                                error!(error = %err, "failed to process message");
                            }
                        } else {
                            debug!(
                                chat_id,
                                message_id = message.id(),
                                outgoing = message.outgoing(),
                                "ignoring new message from unmonitored chat"
                            );
                        }
                    }
                    Ok(update) => {
                        let update_kind = update_kind_name(&update);
                        debug!(
                            update_kind,
                            "ignoring unsupported telegram update type"
                        );
                        hooks.emit(RewriteEvent::UnsupportedUpdateIgnored {
                            update_kind,
                        });
                    }
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
                            model = %new_active.hot_config.openai_model,
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

struct ActiveRewriteState {
    hot_config: HotConfig,
    monitored_chats: HashSet<i64>,
    llm: OpenAiClient,
}

impl ActiveRewriteState {
    fn from_hot_config(hot_config: HotConfig, timeout: Duration) -> Result<Self> {
        let monitored_chats: HashSet<i64> = hot_config.rewrite.chats.iter().copied().collect();
        let llm = OpenAiClient::new(
            hot_config.openai_api_key.clone(),
            hot_config.openai_model.clone(),
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
            while notify_rx.try_recv().is_ok() {}

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

fn is_historical_catch_up_message(message_unix: i64, startup_unix: i64) -> bool {
    message_unix < startup_unix
}

fn should_skip_historical_catch_up_messages() -> bool {
    std::env::var_os("BRAINROT_DISABLE_HISTORICAL_SKIP").is_none()
}

fn should_enable_catch_up() -> bool {
    std::env::var_os("BRAINROT_TEST_DISABLE_CATCH_UP").is_none()
}

fn update_kind_name(update: &Update) -> String {
    match update {
        Update::NewMessage(_) => "new_message".to_owned(),
        Update::MessageEdited(_) => "message_edited".to_owned(),
        Update::MessageDeleted(_) => "message_deleted".to_owned(),
        Update::CallbackQuery(_) => "callback_query".to_owned(),
        Update::InlineQuery(_) => "inline_query".to_owned(),
        Update::InlineSend(_) => "inline_send".to_owned(),
        Update::Raw(raw) => {
            let tl_update: &grammers_client::tl::enums::Update = raw;
            let rendered = format!("{tl_update:?}");
            let tl_name = rendered
                .split_once('(')
                .map(|(name, _)| name)
                .unwrap_or(&rendered);
            format!("raw/{tl_name}")
        }
        _ => "unknown".to_owned(),
    }
}

async fn process_message(
    bot: &TelegramBot,
    llm: &OpenAiClient,
    rewrite: &RewriteConfig,
    message: UpdateMessage,
    context_scope: ContextScope,
    runtime: &mut ProcessMessageRuntime<'_>,
) -> Result<()> {
    let chat_id = context_scope.chat_id;
    let topic_root_id = context_scope.topic_root_id;
    if !message.outgoing() {
        return Ok(());
    }

    let message_id = message.id();
    if runtime.dedupe_cache.contains(chat_id, message_id) {
        info!(chat_id, message_id, "skipping deduped message");
        return Ok(());
    }

    let original = message.text().trim().to_owned();
    if original.is_empty() {
        info!(chat_id, message_id, "skipping non-text or empty message");
        return Ok(());
    }

    let mut context =
        runtime
            .context_cache
            .recent_before(context_scope, message_id, rewrite.context_messages);
    if runtime
        .context_cache
        .should_backfill(context_scope, rewrite.context_messages, context.len())
    {
        info!(
            chat_id,
            topic_root_id = ?topic_root_id,
            message_id,
            requested_context_messages = rewrite.context_messages,
            cached_context_messages = context.len(),
            "fetching context messages from telegram"
        );
        runtime.context_cache.mark_hydrated(context_scope);
        match bot
            .fetch_context(&message, rewrite.context_messages, topic_root_id)
            .await
        {
            Ok(fetched) => {
                info!(
                    chat_id,
                    topic_root_id = ?topic_root_id,
                    message_id,
                    fetched_context_messages = fetched.len(),
                    "fetched context messages from telegram"
                );
                context = fetched;
            }
            Err(err) => {
                warn!(
                    chat_id,
                    topic_root_id = ?topic_root_id,
                    message_id,
                    requested_context_messages = rewrite.context_messages,
                    error = %err,
                    "failed to fetch context messages; using cached context only"
                );
            }
        }
    }

    let llm_context: Vec<String> = context
        .iter()
        .map(ContextMessage::as_llm_user_content)
        .collect();
    let pretty_system_prompt = rewrite.system_prompt.replace('\n', "\n    ");
    let pretty_input = original.replace('\n', "\n    ");
    let pretty_context = if llm_context.is_empty() {
        "    (none)".to_owned()
    } else {
        llm_context
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let entry = entry.replace('\n', "\n         ");
                format!("    {:02}. {}", idx + 1, entry)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    info!(
        chat_id,
        topic_root_id = ?topic_root_id,
        message_id,
        context_messages = llm_context.len(),
        model_call_enabled = runtime.rewrite_override.is_none(),
        "prepared rewrite payload\n  system_prompt:\n    {}\n  context:\n{}\n  input:\n    {}",
        pretty_system_prompt,
        pretty_context,
        pretty_input
    );

    let rewritten = if let Some(override_text) = runtime.rewrite_override {
        debug!(chat_id, message_id, "using test rewrite override text");
        override_text.to_owned()
    } else {
        match llm
            .rewrite(&rewrite.system_prompt, &context, &original)
            .await
        {
            Ok(text) => text,
            Err(err) => {
                warn!(
                    chat_id,
                    message_id,
                    error = %err,
                    "openai rewrite failed; leaving original message unchanged"
                );
                return Ok(());
            }
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
            runtime.dedupe_cache.insert(chat_id, message_id);
            info!(chat_id, message_id, "rewrote and edited message");
            runtime.hooks.emit(RewriteEvent::MessageEdited {
                chat_id,
                message_id,
            });
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

struct ProcessMessageRuntime<'a> {
    dedupe_cache: &'a mut DedupeCache,
    context_cache: &'a mut ContextCache,
    rewrite_override: Option<&'a str>,
    hooks: &'a RewriteHooks,
}

fn test_rewrite_override() -> Option<String> {
    std::env::var("BRAINROT_TEST_BYPASS_REWRITE").ok()
}

fn normalize_rewrite_override(rewrite_override: Option<String>) -> Option<String> {
    rewrite_override
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Clone)]
struct CachedContextMessage {
    message_id: i32,
    message: ContextMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ContextScope {
    chat_id: i64,
    topic_root_id: Option<i32>,
}

struct ContextCache {
    per_chat_limit: usize,
    entries: HashMap<ContextScope, VecDeque<CachedContextMessage>>,
    hydrated_scopes: HashSet<ContextScope>,
}

impl ContextCache {
    fn new(per_chat_limit: usize) -> Self {
        Self {
            per_chat_limit,
            entries: HashMap::new(),
            hydrated_scopes: HashSet::new(),
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
        self.entries
            .retain(|scope, _| chats.contains(&scope.chat_id));
        self.hydrated_scopes
            .retain(|scope| chats.contains(&scope.chat_id));
    }

    fn observe_update_message(&mut self, scope: ContextScope, message: &UpdateMessage) {
        let text = message.text().trim().to_owned();
        if text.is_empty() {
            return;
        }

        let peer_name = message.sender().and_then(|p| p.name().map(str::to_owned));
        let sender_name = resolve_sender_name(message.outgoing(), peer_name.as_deref());
        self.record_message(scope, message.id(), ContextMessage { sender_name, text });
    }

    fn record_message(&mut self, scope: ContextScope, message_id: i32, message: ContextMessage) {
        let chat_messages = self.entries.entry(scope).or_default();
        if chat_messages
            .iter()
            .any(|cached| cached.message_id == message_id)
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

    fn recent_before(
        &self,
        scope: ContextScope,
        message_id: i32,
        count: usize,
    ) -> Vec<ContextMessage> {
        if count == 0 {
            return Vec::new();
        }

        let mut recent = Vec::with_capacity(count);
        if let Some(messages) = self.entries.get(&scope) {
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

    fn should_backfill(&self, scope: ContextScope, count: usize, cached_count: usize) -> bool {
        count > 0 && cached_count < count && !self.hydrated_scopes.contains(&scope)
    }

    fn mark_hydrated(&mut self, scope: ContextScope) {
        self.hydrated_scopes.insert(scope);
    }
}

fn truncate_to_telegram_limit(input: &str, max_utf16_units: usize) -> &str {
    let mut utf16_count = 0;
    for (byte_offset, ch) in input.char_indices() {
        utf16_count += ch.len_utf16();
        if utf16_count > max_utf16_units {
            return &input[..byte_offset];
        }
    }
    input
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
        ActiveRewriteState, ContextCache, ContextScope, DedupeCache, event_targets_watched_config,
        is_historical_catch_up_message, is_relevant_config_event_kind, normalize_rewrite_override,
        truncate_to_telegram_limit, update_kind_name,
    };
    use crate::config::{HotConfig, RewriteConfig};
    use crate::context::ContextMessage;
    use grammers_client::tl;
    use grammers_client::update::Update;
    use notify::{
        Event, EventKind,
        event::{AccessKind, CreateKind, ModifyKind, RemoveKind},
    };
    use std::time::Duration;

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
    fn active_rewrite_state_rejects_empty_openai_api_key() {
        let hot = HotConfig {
            openai_api_key: "   ".to_owned(),
            openai_model: "gpt-4.1-mini".to_owned(),
            rewrite: RewriteConfig {
                chats: vec![-1001234567890],
                system_prompt: "rewrite this".to_owned(),
                context_messages: 10,
            },
        };
        let result = ActiveRewriteState::from_hot_config(hot, Duration::from_secs(5));
        assert!(result.is_err(), "empty api key should fail");
        let err = match result {
            Ok(_) => unreachable!("checked above"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("api key"));
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
    fn catch_up_message_after_startup_is_not_historical() {
        assert!(!is_historical_catch_up_message(105, 100));
    }

    #[test]
    fn context_cache_returns_recent_messages_in_order_excluding_current() {
        let mut cache = ContextCache::new(10);
        let scope = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: None,
        };
        cache.record_message(
            scope,
            1,
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "one".to_owned(),
            },
        );
        cache.record_message(
            scope,
            2,
            ContextMessage {
                sender_name: "Bob".to_owned(),
                text: "two".to_owned(),
            },
        );
        cache.record_message(
            scope,
            3,
            ContextMessage {
                sender_name: "Me".to_owned(),
                text: "three".to_owned(),
            },
        );

        let context = cache.recent_before(scope, 3, 2);
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
        let scope = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: None,
        };

        assert!(cache.should_backfill(scope, 10, 0));
        cache.mark_hydrated(scope);
        assert!(!cache.should_backfill(scope, 10, 0));
    }

    #[test]
    fn context_cache_isolated_across_topics_in_same_chat() {
        let mut cache = ContextCache::new(10);
        let general_scope = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: None,
        };
        let topic_scope = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: Some(99),
        };

        cache.record_message(
            general_scope,
            1,
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "general one".to_owned(),
            },
        );
        cache.record_message(
            topic_scope,
            2,
            ContextMessage {
                sender_name: "Bob".to_owned(),
                text: "topic one".to_owned(),
            },
        );
        cache.record_message(
            topic_scope,
            3,
            ContextMessage {
                sender_name: "Me".to_owned(),
                text: "topic two".to_owned(),
            },
        );

        let topic_context = cache.recent_before(topic_scope, 3, 5);
        assert_eq!(
            topic_context,
            vec![ContextMessage {
                sender_name: "Bob".to_owned(),
                text: "topic one".to_owned(),
            }]
        );
        let general_context = cache.recent_before(general_scope, 1, 5);
        assert!(general_context.is_empty());
    }

    #[test]
    fn context_cache_hydration_isolated_across_topics_in_same_chat() {
        let mut cache = ContextCache::new(10);
        let first_topic = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: Some(10),
        };
        let second_topic = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: Some(20),
        };

        assert!(cache.should_backfill(first_topic, 10, 0));
        cache.mark_hydrated(first_topic);
        assert!(!cache.should_backfill(first_topic, 10, 0));
        assert!(
            cache.should_backfill(second_topic, 10, 0),
            "hydrating one topic must not block another topic from backfill"
        );
    }

    #[test]
    fn truncate_counts_utf16_code_units_not_scalar_values() {
        let input = "😀😀😀😀";
        let result = truncate_to_telegram_limit(input, 6);
        assert_eq!(result, "😀😀😀");
    }

    #[test]
    fn truncate_ascii_within_limit_returns_full_string() {
        let input = "hello";
        assert_eq!(truncate_to_telegram_limit(input, 10), "hello");
    }

    #[test]
    fn truncate_mixed_bmp_and_surrogate_pairs() {
        let input = "a😀a";
        let result = truncate_to_telegram_limit(input, 3);
        assert_eq!(result, "a😀");
    }

    #[test]
    fn record_message_deduplicates_non_consecutive_ids() {
        let mut cache = ContextCache::new(10);
        let scope = ContextScope {
            chat_id: -1001234567890,
            topic_root_id: None,
        };

        cache.record_message(
            scope,
            1,
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "first".to_owned(),
            },
        );
        cache.record_message(
            scope,
            2,
            ContextMessage {
                sender_name: "Bob".to_owned(),
                text: "second".to_owned(),
            },
        );
        cache.record_message(
            scope,
            1,
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "first again".to_owned(),
            },
        );

        let context = cache.recent_before(scope, 99, 10);
        assert_eq!(
            context.len(),
            2,
            "duplicate message_id=1 should not be added again"
        );
        assert_eq!(context[0].text, "first");
        assert_eq!(context[1].text, "second");
    }

    #[test]
    fn runtime_options_respect_explicit_rewrite_override() {
        assert_eq!(
            normalize_rewrite_override(Some(" [forced] ".to_owned())).as_deref(),
            Some("[forced]")
        );
        assert_eq!(normalize_rewrite_override(Some("   ".to_owned())), None);
    }

    #[test]
    fn update_kind_name_includes_tl_variant_for_raw_updates() {
        let raw_tl: tl::enums::Update = tl::types::UpdateConfig {}.into();
        let raw = grammers_client::update::Raw {
            raw: raw_tl,
            state: grammers_session::updates::State {
                date: 0,
                seq: 0,
                message_box: None,
            },
        };
        let update = Update::Raw(raw);
        assert_eq!(update_kind_name(&update), "raw/Config");
    }
}
