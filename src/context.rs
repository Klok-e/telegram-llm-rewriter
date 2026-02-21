#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    pub sender_name: String,
    pub text: String,
}

impl ContextMessage {
    pub fn as_llm_user_content(&self) -> String {
        format!("{}: {}", self.sender_name, self.text)
    }
}

pub fn resolve_sender_name(outgoing: bool, peer_name: Option<&str>) -> String {
    if outgoing {
        "Me".to_owned()
    } else {
        peer_name
            .filter(|name| !name.trim().is_empty())
            .map(|name| name.to_owned())
            .unwrap_or_else(|| "Unknown".to_owned())
    }
}
