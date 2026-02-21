use crate::context::ContextMessage;
use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

pub struct OllamaClient {
    base_url: Url,
    model: String,
    http: Client,
}

impl OllamaClient {
    pub fn new(base_url: String, model: String, timeout: Duration) -> Result<Self> {
        let base_url =
            Url::parse(base_url.trim()).context("ollama.url must be a valid URL string")?;
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client for Ollama")?;

        debug!(
            timeout_seconds = timeout.as_secs(),
            "built ollama HTTP client"
        );

        Ok(Self {
            base_url,
            model: model.trim().to_owned(),
            http,
        })
    }

    pub async fn rewrite(
        &self,
        system_prompt: &str,
        context: &[ContextMessage],
        input: &str,
    ) -> Result<String> {
        let endpoint = self
            .base_url
            .join("api/chat")
            .context("failed to build Ollama /api/chat endpoint URL")?;

        let request = build_chat_request(&self.model, system_prompt, context, input);

        debug!(model = %self.model, "sending rewrite request to ollama");

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

fn build_chat_request(
    model: &str,
    system_prompt: &str,
    context: &[ContextMessage],
    input: &str,
) -> ChatRequest {
    let mut messages = Vec::with_capacity(context.len() + 2);
    messages.push(ChatMessage {
        role: "system".to_owned(),
        content: system_prompt.to_owned(),
    });
    messages.extend(context.iter().map(|context_message| ChatMessage {
        role: "user".to_owned(),
        content: context_message.as_llm_user_content(),
    }));
    messages.push(ChatMessage {
        role: "user".to_owned(),
        content: input.to_owned(),
    });

    ChatRequest {
        model: model.to_owned(),
        messages,
        stream: false,
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Option<AssistantMessage>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::build_chat_request;
    use crate::context::ContextMessage;

    #[test]
    fn build_chat_request_includes_context_in_expected_order() {
        let context = vec![
            ContextMessage {
                sender_name: "Alice".to_owned(),
                text: "Hey there".to_owned(),
            },
            ContextMessage {
                sender_name: "Me".to_owned(),
                text: "Hi!".to_owned(),
            },
        ];

        let request = build_chat_request("llama3", "Rewrite politely", &context, "ok");

        assert_eq!(request.model, "llama3");
        assert!(!request.stream);
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(request.messages[0].content, "Rewrite politely");
        assert_eq!(request.messages[1].role, "user");
        assert_eq!(request.messages[1].content, "Alice: Hey there");
        assert_eq!(request.messages[2].role, "user");
        assert_eq!(request.messages[2].content, "Me: Hi!");
        assert_eq!(request.messages[3].role, "user");
        assert_eq!(request.messages[3].content, "ok");
    }
}
