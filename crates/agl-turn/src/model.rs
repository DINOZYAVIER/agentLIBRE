#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub turn_id: String,
    pub request_index: usize,
    pub messages: Vec<ModelMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelResponse {
    pub content: String,
}
