use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

pub struct OllamaClient {
    base_url: Url,
    model: String,
    timeout: Duration,
    http: Client,
}

impl OllamaClient {
    pub fn new(base_url: String, model: String, timeout: Duration) -> Result<Self> {
        if model.trim().is_empty() {
            bail!("ollama.model must not be empty");
        }

        let base_url =
            Url::parse(base_url.trim()).context("ollama.url must be a valid URL string")?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client for Ollama")?;

        Ok(Self {
            base_url,
            model: model.trim().to_owned(),
            timeout,
            http,
        })
    }

    pub async fn rewrite(&self, system_prompt: &str, input: &str) -> Result<String> {
        let endpoint = self
            .base_url
            .join("api/chat")
            .context("failed to build Ollama /api/chat endpoint URL")?;

        let request = ChatRequest {
            model: &self.model,
            stream: false,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system_prompt,
                },
                ChatMessage {
                    role: "user",
                    content: input,
                },
            ],
        };

        debug!(
            timeout_seconds = self.timeout.as_secs(),
            model = %self.model,
            "sending rewrite request to ollama"
        );

        let response = self
            .http
            .post(endpoint)
            .json(&request)
            .send()
            .await
            .context("failed to send request to Ollama")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("ollama request failed with status {status}: {body}");
        }

        let parsed: ChatResponse = response
            .json()
            .await
            .context("failed to parse Ollama /api/chat response JSON")?;
        let message = parsed
            .message
            .context("ollama response missing assistant message")?;

        Ok(message.content.trim().to_owned())
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Option<AssistantMessage>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String,
}
