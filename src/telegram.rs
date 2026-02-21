use crate::config::TelegramConfig;
use crate::context::{ContextMessage, resolve_sender_name};
use anyhow::{Context, Result, bail};
use grammers_client::client::{UpdateStream, UpdatesConfiguration};
use grammers_client::message::Message as TelegramMessage;
use grammers_client::update::{Message as UpdateMessage, Update};
use grammers_client::{Client, SignInError, tl};
use grammers_mtsender::{SenderPool, SenderPoolFatHandle};
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerRef;
use grammers_session::updates::UpdatesLike;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tracing::info;

const CONTEXT_SCAN_FACTOR: usize = 20;
const CONTEXT_SCAN_MIN_MESSAGES: usize = 200;
const UPDATE_QUEUE_LIMIT: usize = 10_000;

pub struct TelegramBot {
    client: Client,
    updates: Option<UpdateStream>,
    monitored_chats: HashSet<i64>,
    pool_handle: SenderPoolFatHandle,
    pool_task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct ChatListItem {
    pub id: i64,
    pub name: String,
}

struct ConnectionParts {
    client: Client,
    updates_rx: UnboundedReceiver<UpdatesLike>,
    pool_handle: SenderPoolFatHandle,
    pool_task: JoinHandle<()>,
}

impl TelegramBot {
    pub async fn connect_for_rewrite(
        config: &TelegramConfig,
        monitored_chats: HashSet<i64>,
        catch_up: bool,
    ) -> Result<Self> {
        let ConnectionParts {
            client,
            updates_rx,
            pool_handle,
            pool_task,
        } = connect_and_auth(config).await?;
        preflight_monitored_chats(&client, &monitored_chats).await?;

        let updates = client
            .stream_updates(
                updates_rx,
                UpdatesConfiguration {
                    catch_up,
                    update_queue_limit: Some(UPDATE_QUEUE_LIMIT),
                },
            )
            .await;

        info!(
            catch_up,
            update_queue_limit = UPDATE_QUEUE_LIMIT,
            "configured telegram update stream"
        );

        Ok(Self {
            client,
            updates: Some(updates),
            monitored_chats,
            pool_handle,
            pool_task: Some(pool_task),
        })
    }

    pub async fn connect_for_listing(config: &TelegramConfig) -> Result<Self> {
        let ConnectionParts {
            client,
            pool_handle,
            pool_task,
            ..
        } = connect_and_auth(config).await?;

        Ok(Self {
            client,
            updates: None,
            monitored_chats: HashSet::new(),
            pool_handle,
            pool_task: Some(pool_task),
        })
    }

    pub async fn next_update(&mut self) -> Result<Update> {
        let updates = self
            .updates
            .as_mut()
            .context("telegram bot is not connected for update streaming")?;
        updates
            .next()
            .await
            .context("failed to fetch Telegram update")
    }

    pub async fn list_chats(&self, query: Option<&str>) -> Result<Vec<ChatListItem>> {
        let query = query.map(|value| value.to_lowercase());
        let mut dialogs = self.client.iter_dialogs();
        let mut chats: Vec<(String, ChatListItem)> = Vec::new();

        while let Some(dialog) = dialogs
            .next()
            .await
            .context("failed while iterating Telegram dialogs")?
        {
            let peer = dialog.peer();
            let name = peer.name().unwrap_or_default().trim().to_owned();
            let name_lower = name.to_lowercase();
            let matches = query.as_ref().is_none_or(|q| name_lower.contains(q));
            if matches {
                chats.push((
                    name_lower,
                    ChatListItem {
                        id: peer.id().bot_api_dialog_id(),
                        name,
                    },
                ));
            }
        }

        chats.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.id.cmp(&right.1.id)));
        Ok(chats.into_iter().map(|(_, item)| item).collect())
    }

    pub fn update_monitored_chats(&mut self, chats: HashSet<i64>) {
        self.monitored_chats = chats;
    }

    pub fn is_monitored_chat(&self, chat_id: i64) -> bool {
        self.monitored_chats.contains(&chat_id)
    }

    pub(crate) fn client_clone(&self) -> Client {
        self.client.clone()
    }

    pub async fn edit_message(&self, message: &UpdateMessage, new_text: &str) -> Result<()> {
        let message_id = message.id();
        let peer = message
            .peer_ref()
            .await
            .context("failed to resolve peer for Telegram message edit")?;

        self.client
            .edit_message(peer, message_id, new_text)
            .await
            .context("failed to edit Telegram message")?;
        Ok(())
    }

    pub async fn fetch_context(
        &self,
        message: &UpdateMessage,
        count: usize,
        target_topic_root_id: Option<i32>,
    ) -> Result<Vec<ContextMessage>> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let peer_ref: PeerRef = message
            .peer_ref()
            .await
            .context("failed to resolve peer for fetching context")?;

        let message_id = message.id();
        let mut iter = self.client.iter_messages(peer_ref);
        let mut messages = Vec::new();
        let max_scan = context_scan_limit(count);
        let mut scanned = 0;

        while let Some(msg) = iter
            .next()
            .await
            .context("failed while iterating messages for context")?
        {
            scanned += 1;
            if scanned > max_scan {
                break;
            }

            if msg.id() == message_id {
                continue;
            }
            if message_topic_root_id(&msg) != target_topic_root_id {
                continue;
            }

            let text = msg.text().trim().to_owned();
            if text.is_empty() {
                continue;
            }

            let peer_name = msg.sender().and_then(|p| p.name().map(str::to_owned));
            let sender_name = resolve_sender_name(msg.outgoing(), peer_name.as_deref());
            messages.push(ContextMessage { sender_name, text });

            if messages.len() >= count {
                break;
            }
        }

        if scanned >= max_scan && messages.len() < count {
            info!(
                message_id,
                target_topic_root_id = ?target_topic_root_id,
                requested_context_messages = count,
                scanned_messages = scanned,
                scan_limit = max_scan,
                fetched_context_messages = messages.len(),
                "stopped context fetch after scan limit"
            );
        }

        messages.reverse();
        Ok(messages)
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(updates) = self.updates.as_ref() {
            updates.sync_update_state().await;
        }
        self.pool_handle.quit();
        if let Some(pool_task) = self.pool_task.take() {
            pool_task
                .await
                .context("failed waiting for Telegram sender pool task")?;
        }
        Ok(())
    }
}

async fn preflight_monitored_chats(client: &Client, monitored_chats: &HashSet<i64>) -> Result<()> {
    let known_chat_ids = prime_dialog_chat_ids(client).await?;
    let unresolved_chat_ids = unresolved_monitored_chats(monitored_chats, &known_chat_ids);
    if !unresolved_chat_ids.is_empty() {
        bail!(
            "monitored chat ids are not present in Telegram dialogs for this session: {:?}",
            unresolved_chat_ids
        );
    }

    info!(
        monitored_chat_count = monitored_chats.len(),
        known_dialog_chat_count = known_chat_ids.len(),
        "primed telegram peer cache for monitored chats"
    );

    Ok(())
}

async fn prime_dialog_chat_ids(client: &Client) -> Result<HashSet<i64>> {
    let mut dialogs = client.iter_dialogs();
    let mut chat_ids = HashSet::new();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("failed while iterating dialogs for monitored chat preflight")?
    {
        chat_ids.insert(dialog.peer_id().bot_api_dialog_id());
    }
    Ok(chat_ids)
}

fn unresolved_monitored_chats(
    monitored_chats: &HashSet<i64>,
    known_chat_ids: &HashSet<i64>,
) -> Vec<i64> {
    let mut unresolved: Vec<i64> = monitored_chats
        .iter()
        .filter(|chat_id| !known_chat_ids.contains(chat_id))
        .copied()
        .collect();
    unresolved.sort_unstable();
    unresolved
}

pub fn message_topic_root_id(message: &TelegramMessage) -> Option<i32> {
    if let Some(reply_header) = message_reply_header(message) {
        if let Some(top_id) = reply_header.reply_to_top_id {
            return Some(top_id);
        }
        if reply_header.forum_topic {
            // Some forum-topic replies may not include reply_to_top_id.
            if let Some(reply_to_id) = reply_header.reply_to_msg_id {
                return Some(reply_to_id);
            }
        }
    }

    if matches!(
        message.action(),
        Some(tl::enums::MessageAction::TopicCreate(_))
    ) {
        return Some(message.id());
    }

    None
}

fn message_reply_header(message: &TelegramMessage) -> Option<&tl::types::MessageReplyHeader> {
    let reply_to = match &message.raw {
        tl::enums::Message::Message(raw) => raw.reply_to.as_ref(),
        tl::enums::Message::Service(raw) => raw.reply_to.as_ref(),
        tl::enums::Message::Empty(_) => None,
    }?;

    match reply_to {
        tl::enums::MessageReplyHeader::Header(header) => Some(header),
        tl::enums::MessageReplyHeader::MessageReplyStoryHeader(_) => None,
    }
}

fn context_scan_limit(count: usize) -> usize {
    count
        .saturating_mul(CONTEXT_SCAN_FACTOR)
        .max(CONTEXT_SCAN_MIN_MESSAGES)
}

async fn connect_and_auth(config: &TelegramConfig) -> Result<ConnectionParts> {
    let session = Arc::new(
        SqliteSession::open(&config.session_file)
            .await
            .with_context(|| {
                format!(
                    "failed to open session db: {}",
                    config.session_file.display()
                )
            })?,
    );

    let pool = SenderPool::new(Arc::clone(&session), config.api_id);
    let client = Client::new(pool.handle.clone());
    let SenderPool {
        runner,
        updates,
        handle,
    } = pool;
    let pool_task = tokio::spawn(runner.run());

    if !client
        .is_authorized()
        .await
        .context("failed to check Telegram authorization")?
    {
        info!("session not authorized; starting interactive Telegram login");
        sign_in_interactively(&client, &config.api_hash).await?;
    }

    Ok(ConnectionParts {
        client,
        updates_rx: updates,
        pool_handle: handle,
        pool_task,
    })
}

async fn sign_in_interactively(client: &Client, api_hash: &str) -> Result<()> {
    let phone = prompt("Telegram phone number (with country code): ")?;
    let login_token = client
        .request_login_code(phone.trim(), api_hash)
        .await
        .context("failed to request login code from Telegram")?;
    let code = prompt("Telegram login code: ")?;

    match client.sign_in(&login_token, code.trim()).await {
        Ok(user) => {
            info!(user_id = user.id().bare_id(), "Telegram sign-in successful");
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let password = prompt("Telegram 2FA password: ")?;
            client
                .check_password(password_token, password.trim())
                .await
                .context("failed to validate Telegram 2FA password")?;
            Ok(())
        }
        Err(SignInError::SignUpRequired) => {
            bail!("this Telegram account must be registered in an official client first")
        }
        Err(err) => Err(err).context("Telegram sign-in failed"),
    }
}

fn prompt(prompt: &str) -> Result<String> {
    {
        let mut out = io::stdout().lock();
        out.write_all(prompt.as_bytes())
            .context("failed to write prompt to stdout")?;
        out.flush().context("failed to flush stdout")?;
    }

    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read from stdin")?;
    Ok(line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{context_scan_limit, unresolved_monitored_chats};
    use std::collections::HashSet;

    #[test]
    fn context_scan_limit_uses_minimum_window() {
        assert_eq!(context_scan_limit(1), 200);
    }

    #[test]
    fn context_scan_limit_scales_with_requested_context() {
        assert_eq!(context_scan_limit(20), 400);
    }

    #[test]
    fn unresolved_monitored_chats_returns_sorted_missing_chat_ids() {
        let monitored = HashSet::from([-1003, -1001, -1002]);
        let known = HashSet::from([-1001]);
        assert_eq!(
            unresolved_monitored_chats(&monitored, &known),
            vec![-1003, -1002]
        );
    }
}
