mod delegation;
mod inference_client;
mod options;
mod presentation;
mod prompt;
mod service;
mod session;
mod supervised_chat;
mod supervisor_driver;
mod terminal;
mod tools;
mod turn_runtime;

pub use inference_client::{ChatInferenceJob, InferenceClient, InferenceClientHandle};
pub use options::{ChatOptions, DEFAULT_MAX_OUTPUT_TOKENS, InferenceOptions, ToolAccessMode};
pub use presentation::{
    ModelAttemptOutcome, NoopTurnPresentationSink, PresentationDelivery, ToolActionOutcome,
    TurnPresentationEvent, TurnPresentationOutcome, TurnPresentationSink,
};
pub use service::{
    ChatService, ChatSessionSummary, ChatTurnExecution, ChatTurnOutput, ChatTurnStatus,
    chat_workspace_root, replay_turn_messages, stopped_turn_context_message,
};
pub use session::InferenceSession;
pub use supervised_chat::SupervisedChat;
pub use supervisor_driver::{ChatRunInput, ChatSupervisorFactory};
pub use terminal::assistant_text_for_terminal;
pub use turn_runtime::{ChatTurnRuntime, shared_process_handle, shared_terminal_registry};
