use crate::config::TelegramConfig;
use anyhow::{Context, Result, bail};
use grammers_client::client::updates::UpdateStream;
use grammers_client::types::update::Message as UpdateMessage;
use grammers_client::{Client, SignInError, Update, UpdatesConfiguration};
use grammers_mtsender::{SenderPool, SenderPoolHandle};
use grammers_session::storages::SqliteSession;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::info;

pub struct TelegramBot {
    updates: UpdateStream,
    monitored_chats: HashSet<i64>,
    pool_handle: SenderPoolHandle,
    pool_task: Option<JoinHandle<()>>,
}

impl TelegramBot {
    pub async fn connect_and_authorize(
        config: &TelegramConfig,
        monitored_chats: HashSet<i64>,
    ) -> Result<Self> {
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

        let updates = client.stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
        );

        Ok(Self {
            updates,
            monitored_chats,
            pool_handle: handle,
            pool_task: Some(pool_task),
        })
    }

    pub async fn next_update(&mut self) -> Result<Update> {
        self.updates
            .next()
            .await
            .context("failed to fetch Telegram update")
    }

    pub fn is_monitored_chat(&self, chat_id: i64) -> bool {
        self.monitored_chats.contains(&chat_id)
    }

    pub fn chat_id_for_message(&self, message: &UpdateMessage) -> i64 {
        message.peer_id().bot_api_dialog_id()
    }

    pub async fn edit_message(&self, message: &UpdateMessage, new_text: &str) -> Result<()> {
        message
            .edit(new_text)
            .await
            .context("failed to edit Telegram message")?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.updates.sync_update_state();
        self.pool_handle.quit();
        if let Some(pool_task) = self.pool_task.take() {
            pool_task
                .await
                .context("failed waiting for Telegram sender pool task")?;
        }
        Ok(())
    }
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
