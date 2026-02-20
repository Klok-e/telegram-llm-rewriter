use crate::config::TelegramConfig;
use anyhow::{Context, Result, bail};
use grammers_client::client::updates::{UpdateStream, UpdatesLike};
use grammers_client::types::update::Message as UpdateMessage;
use grammers_client::{Client, SignInError, Update, UpdatesConfiguration};
use grammers_mtsender::{SenderPool, SenderPoolHandle};
use grammers_session::storages::SqliteSession;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tracing::info;

pub struct TelegramBot {
    client: Client,
    updates: Option<UpdateStream>,
    monitored_chats: HashSet<i64>,
    pool_handle: SenderPoolHandle,
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
    pool_handle: SenderPoolHandle,
    pool_task: JoinHandle<()>,
}

impl TelegramBot {
    pub async fn connect_for_rewrite(
        config: &TelegramConfig,
        monitored_chats: HashSet<i64>,
    ) -> Result<Self> {
        let ConnectionParts {
            client,
            updates_rx,
            pool_handle,
            pool_task,
        } = connect_and_auth(config).await?;

        let updates = client.stream_updates(
            updates_rx,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
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
        let mut chats = Vec::new();

        while let Some(dialog) = dialogs
            .next()
            .await
            .context("failed while iterating Telegram dialogs")?
        {
            let peer = dialog.peer();
            let name = peer.name().unwrap_or_default().trim().to_owned();
            let matches = query
                .as_ref()
                .is_none_or(|q| name.to_lowercase().contains(q));
            if matches {
                chats.push(ChatListItem {
                    id: peer.id().bot_api_dialog_id(),
                    name,
                });
            }
        }

        chats.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then(left.id.cmp(&right.id))
        });
        Ok(chats)
    }

    pub fn update_monitored_chats(&mut self, chats: HashSet<i64>) {
        self.monitored_chats = chats;
    }

    pub fn is_monitored_chat(&self, chat_id: i64) -> bool {
        self.monitored_chats.contains(&chat_id)
    }

    pub fn chat_id_for_message(&self, message: &UpdateMessage) -> i64 {
        message.peer_id().bot_api_dialog_id()
    }

    pub async fn edit_message(&self, message: &UpdateMessage, new_text: &str) -> Result<()> {
        let message_id = message.id();
        let peer = match message.peer() {
            Ok(peer) => peer.clone(),
            Err(peer_ref) => self
                .client
                .resolve_peer(peer_ref)
                .await
                .context("failed to resolve peer for Telegram message edit")?,
        };

        self.client
            .edit_message(peer, message_id, new_text)
            .await
            .context("failed to edit Telegram message")?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(updates) = self.updates.as_mut() {
            updates.sync_update_state();
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

async fn connect_and_auth(config: &TelegramConfig) -> Result<ConnectionParts> {
    let session = Arc::new(SqliteSession::open(&config.session_file).with_context(|| {
        format!(
            "failed to open session db: {}",
            config.session_file.display()
        )
    })?);

    let pool = SenderPool::new(Arc::clone(&session), config.api_id);
    let client = Client::new(&pool);
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
            info!(user_id = user.bare_id(), "Telegram sign-in successful");
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
        Err(SignInError::SignUpRequired { .. }) => {
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
