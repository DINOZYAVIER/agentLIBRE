//! Process-local runtime used only by explicit one-shot and cron commands.
//!
//! The interactive surface is daemon-backed and must not import this owner.

pub(crate) type OneShotSession = agl_chat::SupervisedChat;
