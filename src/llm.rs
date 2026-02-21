use crate::context::ContextMessage;
use anyhow::{Context, Result, bail};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, InputItem, InputParam, MessageType,
    OutputItem, OutputMessageContent, Role,
};
use std::time::Duration;
use tracing::debug;

pub struct OpenAiClient {
    model: String,
    client: Client<OpenAIConfig>,
}

impl OpenAiClient {
    pub fn new(api_key: String, model: String, timeout: Duration) -> Result<Self> {
        dbg!(&api_key);
        let api_key = api_key.trim().to_owned();
        if api_key.is_empty() {
            bail!("openai api key must not be empty");
        }

        let model = model.trim().to_owned();
        if model.is_empty() {
            bail!("openai model must not be empty");
        }

        let config = OpenAIConfig::new().with_api_key(api_key);
        let http_client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client for OpenAI")?;
        let client = Client::with_config(config).with_http_client(http_client);

        debug!(
            timeout_seconds = timeout.as_secs(),
            model = %model,
            "built openai HTTP client"
        );

        Ok(Self { model, client })
    }

    pub async fn rewrite(
        &self,
        system_prompt: &str,
        context: &[ContextMessage],
        input: &str,
    ) -> Result<String> {
        let request = build_response_request(&self.model, system_prompt, context, input);

        debug!(
            model = %self.model,
            "sending rewrite request to openai responses api"
        );

        let response = self
            .client
            .responses()
            .create(request)
            .await
            .context("failed to send request to OpenAI")?;

        if let Some(err) = response.error {
            bail!(
                "openai responses api returned error {}: {}",
                err.code,
                err.message
            );
        }

        let text = extract_response_text(&response.output);
        if text.trim().is_empty() {
            bail!("openai response missing assistant text content");
        }

        Ok(text.trim().to_owned())
    }
}

fn build_response_request(
    model: &str,
    system_prompt: &str,
    context: &[ContextMessage],
    input: &str,
) -> CreateResponse {
    let mut items = Vec::with_capacity(context.len() + 2);
    items.push(input_item(Role::System, system_prompt.to_owned()));
    items.extend(
        context
            .iter()
            .map(|context_message| input_item(Role::User, context_message.as_llm_user_content())),
    );
    items.push(input_item(Role::User, input.to_owned()));

    CreateResponse {
        model: Some(model.to_owned()),
        input: InputParam::Items(items),
        ..Default::default()
    }
}

fn input_item(role: Role, text: String) -> InputItem {
    InputItem::EasyMessage(EasyInputMessage {
        r#type: MessageType::Message,
        role,
        content: EasyInputContent::Text(text),
    })
}

fn extract_response_text(output: &[OutputItem]) -> String {
    output
        .iter()
        .filter_map(|item| {
            if let OutputItem::Message(message) = item {
                let text = message
                    .content
                    .iter()
                    .filter_map(|content| {
                        if let OutputMessageContent::OutputText(output_text) = content {
                            let value = output_text.text.trim();
                            if value.is_empty() {
                                None
                            } else {
                                Some(value.to_owned())
                            }
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() { None } else { Some(text) }
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{build_response_request, extract_response_text};
    use crate::context::ContextMessage;
    use async_openai::types::responses::{
        AssistantRole, EasyInputContent, InputItem, InputParam, MessageType, OutputItem,
        OutputMessage, OutputMessageContent, OutputStatus, OutputTextContent, Role,
    };

    #[test]
    fn build_response_request_includes_context_in_expected_order() {
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

        let request = build_response_request("gpt-4.1-mini", "Rewrite politely", &context, "ok");

        assert_eq!(request.model.as_deref(), Some("gpt-4.1-mini"));
        let items = match request.input {
            InputParam::Items(items) => items,
            InputParam::Text(_) => panic!("expected structured input items"),
        };
        assert_eq!(items.len(), 4);

        assert_message_text(&items[0], Role::System, "Rewrite politely");
        assert_message_text(&items[1], Role::User, "Alice: Hey there");
        assert_message_text(&items[2], Role::User, "Me: Hi!");
        assert_message_text(&items[3], Role::User, "ok");
    }

    fn assert_message_text(item: &InputItem, expected_role: Role, expected_text: &str) {
        let message = match item {
            InputItem::EasyMessage(message) => message,
            _ => panic!("expected easy message item"),
        };
        assert_eq!(message.r#type, MessageType::Message);
        assert_eq!(message.role, expected_role);
        let text = match &message.content {
            EasyInputContent::Text(text) => text,
            EasyInputContent::ContentList(_) => panic!("expected text input"),
        };
        assert_eq!(text, expected_text);
    }

    #[test]
    fn extract_response_text_keeps_message_boundaries() {
        let output = vec![
            OutputItem::Message(OutputMessage {
                content: vec![OutputMessageContent::OutputText(OutputTextContent {
                    annotations: vec![],
                    logprobs: None,
                    text: "first".to_owned(),
                })],
                id: "msg-1".to_owned(),
                role: AssistantRole::Assistant,
                status: OutputStatus::Completed,
            }),
            OutputItem::Message(OutputMessage {
                content: vec![OutputMessageContent::OutputText(OutputTextContent {
                    annotations: vec![],
                    logprobs: None,
                    text: "second".to_owned(),
                })],
                id: "msg-2".to_owned(),
                role: AssistantRole::Assistant,
                status: OutputStatus::Completed,
            }),
        ];

        assert_eq!(extract_response_text(&output), "first\nsecond");
    }
}
