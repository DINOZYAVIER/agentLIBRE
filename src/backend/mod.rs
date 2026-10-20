//! Inference backends behind a minimal interface (D10, D29).

pub mod llama;

use crate::action::Value;
use crate::error::Error;
use crate::function::Sampling;

/// The text that opens every message of the runtime to the model
/// (AGL-009); English always, outside the i18n catalog.
pub const RUNTIME_PREFIX: &str = "[agentLIBRE runtime]";

/// A message of the conversation the chat template renders.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    /// A message of the runtime to the model (AGL-009), rendered as the
    /// user role behind [`RUNTIME_PREFIX`]. It is not the human's: it
    /// does not move the thinking boundary of [`kept_thinking`].
    Runtime(String),
    /// One step of the model: its thinking, its text (an answer, or what
    /// it wrote before a call, D114) and its call.
    Assistant {
        thinking: Option<String>,
        text: Option<String>,
        call: Option<ToolCall>,
    },
    /// The result of the call `id`.
    Tool {
        id: String,
        content: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Vec<(String, Value)>,
}

/// How the template describes a tool to the model (D14, D95).
#[derive(Debug, PartialEq, Eq)]
pub struct ToolDoc {
    pub name: &'static str,
    pub description: &'static str,
    /// Name, whether an integer (else a string), description, required.
    pub params: &'static [(&'static str, bool, Option<&'static str>, bool)],
}

/// The built-in tools as the model sees them (D95).
pub const TOOL_DOCS: [ToolDoc; 3] = [
    ToolDoc {
        name: "bash",
        description: "Run a shell command with `bash -c` in the workspace and return its \
                      output (the end is kept when it is long).",
        params: &[
            ("command", false, None, true),
            ("timeout", true, Some("Seconds; default 60."), false),
        ],
    },
    ToolDoc {
        name: "edit",
        description: "Replace the only occurrence of `old` in the file at `path` with \
                      `new`. Fails when `old` is missing or appears more than once.",
        params: &[
            ("path", false, None, true),
            ("old", false, None, true),
            ("new", false, None, true),
        ],
    },
    ToolDoc {
        name: "forge",
        description: "The workspace forge: the store of the recorded artifacts of this \
                      workspace. Each action takes only its own fields: list (none); show \
                      (name); new (title); edit (name, old, new; drafts only); append \
                      (name, section, text; in-progress specs only); remove (name; drafts \
                      only).",
        params: &[
            ("action", false, Some("list, show, new, edit, append or remove."), true),
            (
                "name",
                false,
                Some(
                    "The artifact's name as `list` shows it, like agl-001-my-task; not its \
                     title. For show, edit, append and remove.",
                ),
                false,
            ),
            (
                "new",
                false,
                Some("For edit: the replacement text; empty deletes `old`."),
                false,
            ),
            (
                "old",
                false,
                Some("For edit: the exact text to replace in the body; it must appear once."),
                false,
            ),
            (
                "section",
                false,
                Some(
                    "For append: the heading to append under, without the `## `, like \
                     Progress. Not for edit.",
                ),
                false,
            ),
            ("text", false, Some("For append: the text of the new bullet."), false),
            (
                "title",
                false,
                Some("For new: the title of the new spec; its name is made from it."),
                false,
            ),
        ],
    },
];

/// What to render: the conversation, then the state instruction as a
/// last user message for this generation only (D14, D19).
pub struct Prompt<'a> {
    pub messages: &'a [Message],
    pub instruction: Option<&'a str>,
    pub tools: &'a [&'a ToolDoc],
    pub preserve_thinking: bool,
}

/// One generation.
pub struct Request<'a> {
    pub prompt: &'a str,
    pub grammar: Option<&'a str>,
    /// Strings that end the generation; the text excludes them.
    pub stop: &'a [&'a str],
    pub max_tokens: u32,
    pub sampling: &'a Sampling,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Generated {
    pub text: String,
    pub stop: Stop,
}

/// Why a generation ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The model or the grammar finished.
    End,
    /// One of the stop strings appeared.
    Word(String),
    /// `max_tokens` was reached (D18).
    Limit,
}

/// An inference backend.
pub trait Backend {
    /// Renders `prompt` with the model's chat template.
    fn render(&self, prompt: &Prompt) -> Result<String, Error>;
    /// Generates text, passing each piece to `on_text` as it arrives.
    fn generate(
        &self,
        request: &Request,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error>;
    /// The number of tokens of `text`.
    fn count_tokens(&self, text: &str) -> Result<usize, Error>;
    /// The context size of the loaded model, in tokens.
    fn context_size(&self) -> Result<usize, Error>;
}

/// Whether the thinking of each message goes to the template. Without
/// `preserve`, only the thinking after the last user message does (D98);
/// a runtime message is not a user message (AGL-009).
pub fn kept_thinking(messages: &[Message], preserve: bool) -> Vec<bool> {
    let current = messages
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .map_or(0, |i| i + 1);
    (0..messages.len())
        .map(|i| preserve || i >= current)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(thinking: &str) -> Message {
        Message::Assistant {
            thinking: Some(thinking.into()),
            text: Some("a".into()),
            call: None,
        }
    }

    #[test]
    fn thinking_of_earlier_turns_is_dropped_without_preserve() {
        let messages = [
            Message::System("s".into()),
            Message::User("1".into()),
            step("t1"),
            Message::User("2".into()),
            step("t2"),
            Message::Tool {
                id: "c".into(),
                content: "r".into(),
            },
            step("t3"),
        ];
        assert_eq!(kept_thinking(&messages, true), vec![true; 7]);
        assert_eq!(
            kept_thinking(&messages, false),
            vec![false, false, false, false, true, true, true]
        );
    }

    #[test]
    fn a_runtime_message_does_not_move_the_thinking_boundary() {
        let messages = [
            Message::System("s".into()),
            Message::User("1".into()),
            step("t1"),
            Message::Runtime("notice".into()),
            step("t2"),
            Message::Runtime("summary".into()),
            step("t3"),
        ];
        assert_eq!(
            kept_thinking(&messages, false),
            vec![false, false, true, true, true, true, true]
        );
    }
}
