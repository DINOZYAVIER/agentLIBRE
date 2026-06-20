use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use agl_events::AgentEvent;
use agl_turn::TurnTransitionRecord;
use anyhow::{Context, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeEventSink {
    events_jsonl: PathBuf,
}

impl RuntimeEventSink {
    pub(crate) fn new(events_jsonl: impl Into<PathBuf>) -> Self {
        Self {
            events_jsonl: events_jsonl.into(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.events_jsonl
    }

    pub(crate) fn emit_transition(
        &self,
        record: &TurnTransitionRecord,
        event: &AgentEvent,
    ) -> Result<()> {
        self.write_line(
            event
                .to_safe_runtime_jsonl_line(
                    "turn",
                    record.transition.as_str(),
                    record.sequence,
                    record.from.as_str(),
                    record.to.as_str(),
                )
                .context("failed to serialize safe runtime event")?,
        )
    }

    fn write_line(&self, line: String) -> Result<()> {
        if let Some(parent) = self.events_jsonl.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create runtime event directory {}",
                    parent.display()
                )
            })?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_jsonl)
            .with_context(|| {
                format!(
                    "failed to open runtime event stream {}",
                    self.events_jsonl.display()
                )
            })?;
        file.write_all(line.as_bytes()).with_context(|| {
            format!(
                "failed to write runtime event {}",
                self.events_jsonl.display()
            )
        })?;
        file.write_all(b"\n").with_context(|| {
            format!(
                "failed to write runtime event {}",
                self.events_jsonl.display()
            )
        })?;
        file.flush().with_context(|| {
            format!(
                "failed to flush runtime event {}",
                self.events_jsonl.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use agl_events::AgentEvent;

    use super::*;

    fn temp_event_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("agl-cli-event-sink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn writes_safe_runtime_events_with_fsm_metadata() {
        let path = temp_event_path("safe-runtime-jsonl");
        let sink = RuntimeEventSink::new(&path);
        let mut machine = agl_turn::TurnMachine::new("turn-1");
        let record = machine
            .apply(agl_turn::TurnTransition::Start {
                user_input: "secret prompt".to_string(),
            })
            .unwrap();

        sink.emit_transition(
            &record,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".to_string(),
                user_input: "secret prompt".to_string(),
            },
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();

        assert!(content.contains(r#""fsm":"turn""#), "{content}");
        assert!(content.contains(r#""transition":"start""#), "{content}");
        assert!(content.contains(r#""sequence":1"#), "{content}");
        assert!(
            content.contains(r#""from_phase":"initialized""#),
            "{content}"
        );
        assert!(content.contains(r#""to_phase":"started""#), "{content}");
        assert!(content.contains(r#""kind":"turn.started""#), "{content}");
        assert!(!content.contains("secret prompt"), "{content}");

        std::fs::remove_file(path).unwrap();
    }
}
