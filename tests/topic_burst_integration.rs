use anyhow::{Context, Result, bail};
use brainrot_tg_llm_rewrite::app::{
    RewriteEvent, RewriteHooks, RewriteRuntimeOptions, run_rewrite_mode_with_shutdown_and_hooks,
};
use brainrot_tg_llm_rewrite::config::{Config, ConfigMode, load_config_for_mode};
use grammers_client::Client;
use grammers_client::message::InputMessage;
use grammers_session::types::PeerRef;
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

const CONFIG_PATH: &str = "config.toml";
const MESSAGES_PER_TOPIC: usize = 20;
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const TEST_REWRITE_TEXT: &str = "[it-edited]";

#[derive(Debug, Clone)]
struct SentMessage {
    id: i32,
    topic_label: &'static str,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real Telegram/Ollama with configured [integration_test] in config.toml"]
async fn topic_burst_messages_are_all_processed() -> Result<()> {
    let config_path = std::path::PathBuf::from(CONFIG_PATH);
    let base_config = load_config_for_mode(&config_path, ConfigMode::Rewrite)
        .with_context(|| format!("failed to load config at {}", config_path.display()))?;
    let integration = base_config
        .integration_test
        .as_ref()
        .context("missing [integration_test] section in config.toml")?
        .clone();

    let runtime_config = ensure_chat_monitored(&base_config, integration.chat_id)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RewriteEvent>();
    let (client_tx, client_rx) = oneshot::channel::<Client>();
    let hooks = RewriteHooks::with_event_handler(move |event| {
        let _ = event_tx.send(event);
    })
    .with_client_channel(client_tx);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let runtime_config_path = config_path.clone();
    let runtime_task = tokio::spawn(async move {
        run_rewrite_mode_with_shutdown_and_hooks(
            &runtime_config,
            &runtime_config_path,
            async move {
                let _ = shutdown_rx.await;
            },
            hooks,
            RewriteRuntimeOptions {
                catch_up_enabled: true,
                skip_historical_catch_up_messages: false,
                rewrite_override: Some(TEST_REWRITE_TEXT.to_owned()),
            },
        )
        .await
    });

    let test_result = async {
        let runtime_client = wait_for_runtime_ready(client_rx).await?;
        eprintln!(
            "[it] rewriter started in-process; chat_id={} topic_a_root_id={} topic_b_root_id={}",
            integration.chat_id, integration.topic_a_root_id, integration.topic_b_root_id
        );

        let run_id = unique_run_id();
        eprintln!("[it] run_id={run_id}");

        let peer_ref = resolve_dialog_peer_ref_by_chat_id(&runtime_client, integration.chat_id)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve dialog peer for chat {}",
                    integration.chat_id
                )
            })?;

        let mut sent = Vec::with_capacity(MESSAGES_PER_TOPIC * 2 + 1);

        sent.extend(
            send_topic_burst(
                &runtime_client,
                peer_ref,
                topic_root_from_config(integration.topic_a_root_id),
                "topic_a",
                &run_id,
            )
            .await?,
        );
        sent.extend(
            send_topic_burst(
                &runtime_client,
                peer_ref,
                topic_root_from_config(integration.topic_b_root_id),
                "topic_b",
                &run_id,
            )
            .await?,
        );

        let trigger = send_marker_message(
            &runtime_client,
            peer_ref,
            topic_root_from_config(integration.topic_a_root_id),
            &format!("[it:{run_id}] post-burst trigger"),
            "trigger",
        )
        .await
        .context("failed to send post-burst trigger message")?;
        eprintln!(
            "[it] sent post-burst trigger; message_id={} root_id={:?}",
            trigger.id,
            topic_root_from_config(integration.topic_a_root_id)
        );
        sent.push(trigger);

        let (pending, recent_events) = wait_until_all_edited_events(&mut event_rx, &sent).await;
        if pending.is_empty() {
            return Ok(());
        }

        let mut pending_topic_a = Vec::new();
        let mut pending_topic_b = Vec::new();
        let mut pending_other = Vec::new();
        for message in pending {
            if message.topic_label == "topic_a" {
                pending_topic_a.push(message.id);
            } else if message.topic_label == "topic_b" {
                pending_topic_b.push(message.id);
            } else {
                pending_other.push(message.id);
            }
        }
        pending_topic_a.sort_unstable();
        pending_topic_b.sort_unstable();
        pending_other.sort_unstable();
        bail!(
            "timed out waiting for rewrites; pending topic_a ids: {:?}; pending topic_b ids: {:?}; pending other ids: {:?}\n\nrecent runtime events:\n{}",
            pending_topic_a,
            pending_topic_b,
            pending_other,
            recent_events.join("\n"),
        );
    }
    .await;

    let _ = shutdown_tx.send(());
    let shutdown_result = tokio::time::timeout(Duration::from_secs(10), runtime_task)
        .await
        .context("timed out waiting for in-process rewriter shutdown")?
        .context("in-process rewriter task panicked")?;

    if let Err(test_err) = test_result {
        if let Err(runtime_err) = shutdown_result {
            bail!("{test_err}\n\nrewriter task error during shutdown: {runtime_err}");
        }
        bail!("{test_err}");
    }

    shutdown_result.context("in-process rewriter returned error")?;

    Ok(())
}

fn ensure_chat_monitored(config: &Config, chat_id: i64) -> Result<Config> {
    let mut runtime_config = config.clone();
    let rewrite = runtime_config
        .rewrite
        .as_mut()
        .context("missing required [rewrite] section for rewrite mode")?;
    if !rewrite.chats.contains(&chat_id) {
        rewrite.chats.push(chat_id);
    }
    Ok(runtime_config)
}

async fn wait_for_runtime_ready(client_rx: oneshot::Receiver<Client>) -> Result<Client> {
    match tokio::time::timeout(STARTUP_TIMEOUT, client_rx).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(_)) => bail!("client channel closed before runtime sent the client"),
        Err(_) => bail!(
            "timed out waiting for in-process runtime-ready client after {} seconds",
            STARTUP_TIMEOUT.as_secs()
        ),
    }
}

async fn resolve_dialog_peer_ref_by_chat_id(client: &Client, chat_id: i64) -> Result<PeerRef> {
    let mut dialogs = client.iter_dialogs();
    while let Some(dialog) = dialogs
        .next()
        .await
        .context("failed while iterating dialogs to resolve target chat")?
    {
        if dialog.peer_id().bot_api_dialog_id() == chat_id {
            return Ok(dialog.peer_ref());
        }
    }
    bail!("chat_id {chat_id} was not found in available dialogs")
}

async fn send_topic_burst(
    client: &Client,
    peer_ref: PeerRef,
    topic_root_id: Option<i32>,
    topic_label: &'static str,
    run_id: &str,
) -> Result<Vec<SentMessage>> {
    let mut sent = Vec::with_capacity(MESSAGES_PER_TOPIC);
    for index in 1..=MESSAGES_PER_TOPIC {
        let text = format!("[it:{run_id}] {topic_label} message {index:02}");
        let input = InputMessage::new().text(text).reply_to(topic_root_id);
        let sent_message = client
            .send_message(peer_ref, input)
            .await
            .with_context(|| {
                format!(
                    "failed to send message {index} to topic {topic_label} (root_id={topic_root_id:?})"
                )
            })?;
        eprintln!(
            "[it] sent topic message; topic={} index={} message_id={} root_id={topic_root_id:?}",
            topic_label,
            index,
            sent_message.id()
        );
        sent.push(SentMessage {
            id: sent_message.id(),
            topic_label,
        });
    }
    Ok(sent)
}

async fn send_marker_message(
    client: &Client,
    peer_ref: PeerRef,
    topic_root_id: Option<i32>,
    text: &str,
    topic_label: &'static str,
) -> Result<SentMessage> {
    let input = InputMessage::new().text(text).reply_to(topic_root_id);
    let message = client
        .send_message(peer_ref, input)
        .await
        .context("failed to send marker message")?;
    Ok(SentMessage {
        id: message.id(),
        topic_label,
    })
}

fn topic_root_from_config(value: i32) -> Option<i32> {
    if value == 0 { None } else { Some(value) }
}

async fn wait_until_all_edited_events(
    event_rx: &mut mpsc::UnboundedReceiver<RewriteEvent>,
    sent: &[SentMessage],
) -> (Vec<SentMessage>, Vec<String>) {
    let mut pending: HashSet<i32> = sent.iter().map(|message| message.id).collect();
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    let mut last_report = tokio::time::Instant::now();
    let mut last_pending_count = pending.len();
    let mut recent_events: VecDeque<String> = VecDeque::with_capacity(500);

    eprintln!(
        "[it] waiting for edit confirmations from in-process events; expected={} timeout_seconds={}",
        pending.len(),
        POLL_TIMEOUT.as_secs()
    );

    while !pending.is_empty() && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let poll_for = remaining.min(POLL_INTERVAL);
        let recv_result = tokio::time::timeout(poll_for, event_rx.recv()).await;

        if let Ok(Some(event)) = recv_result {
            if recent_events.len() >= 500 {
                recent_events.pop_front();
            }
            recent_events.push_back(format!("{event:?}"));

            if let RewriteEvent::MessageEdited { message_id, .. } = event {
                pending.remove(&message_id);
            }
        }

        if pending.len() != last_pending_count {
            eprintln!(
                "[it] edit progress; pending={} confirmed={}",
                pending.len(),
                sent.len().saturating_sub(pending.len())
            );
            last_pending_count = pending.len();
        }

        if tokio::time::Instant::now().saturating_duration_since(last_report)
            >= Duration::from_secs(5)
        {
            let mut sample: Vec<i32> = pending.iter().copied().take(10).collect();
            sample.sort_unstable();
            eprintln!(
                "[it] still waiting; pending_count={} sample_pending_ids={:?}",
                pending.len(),
                sample
            );
            last_report = tokio::time::Instant::now();
        }
    }

    let mut still_pending = Vec::new();
    for message in sent {
        if pending.contains(&message.id) {
            still_pending.push(message.clone());
        }
    }

    (still_pending, recent_events.into_iter().collect())
}

fn unique_run_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("topic_burst_{ts}")
}

#[test]
fn ensure_chat_monitored_adds_target_chat_when_missing() {
    let source = r#"
[telegram]
api_id = 1
api_hash = "hash"
session_file = "session.sqlite3"

[ollama]
url = "http://localhost:11434"
model = "x"

[rewrite]
chats = [-1001]
system_prompt = "rewrite"
"#;

    let config: Config = toml::from_str(source).expect("fixture TOML should deserialize as Config");

    let adjusted = ensure_chat_monitored(&config, -1002).expect("chat should be injected");
    let chats = adjusted.rewrite.expect("rewrite section must exist").chats;
    assert!(chats.contains(&-1001));
    assert!(chats.contains(&-1002));
}
