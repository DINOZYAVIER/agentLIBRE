use std::time::{Duration, Instant};

use agl_actions::ParsedModelOutput;
use agl_content::MAX_TEXT_PART_BYTES;
use agl_ids::AttemptId;

use agl_inference::{InferenceOutputEvent, InferenceOutputSink, OutputDelivery};

use super::decode::trim_generated_continuation;
use super::prompt::{
    generated_assistant_prefix_is_pending, isolated_tool_call_with_repair,
    strip_generated_assistant_prefix,
};

const MAX_DELTA_BYTES: usize = 4 * 1024;
const MAX_COALESCE_LATENCY: Duration = Duration::from_millis(20);
const TOOL_CALL_OPENINGS: [&str; 2] = ["<tool_call>", "<|tool_call>"];
const STOP_MARKERS: [&str; 4] = ["\nUser:", "\nAssistant:", "\nTool:", "<|im_end|>"];

pub(super) struct IncrementalResponseClassifier<'a> {
    attempt_id: AttemptId,
    sink: &'a dyn InferenceOutputSink,
    utf8: IncrementalUtf8,
    decoded_content: String,
    content: String,
    classified_len: usize,
    pending_delta: String,
    next_sequence: u64,
    last_flush: Instant,
    answer_committed: bool,
    action: bool,
    continuation: bool,
    delivery_suspended: bool,
    content_byte_limit_reached: bool,
    additional_stops: Vec<String>,
    repair_malformed_tool_calls: bool,
}

pub(super) struct ClassifiedResponse {
    pub(super) decoded_content: String,
    pub(super) content: String,
    pub(super) content_byte_limit_reached: bool,
}

impl<'a> IncrementalResponseClassifier<'a> {
    #[cfg(test)]
    pub(super) fn new(attempt_id: AttemptId, sink: &'a dyn InferenceOutputSink) -> Self {
        Self::new_with_stops(attempt_id, sink, Vec::new())
    }

    #[cfg(test)]
    pub(super) fn new_with_stops(
        attempt_id: AttemptId,
        sink: &'a dyn InferenceOutputSink,
        additional_stops: Vec<String>,
    ) -> Self {
        Self::new_with_policy(attempt_id, sink, additional_stops, true)
    }

    pub(super) fn new_with_policy(
        attempt_id: AttemptId,
        sink: &'a dyn InferenceOutputSink,
        additional_stops: Vec<String>,
        repair_malformed_tool_calls: bool,
    ) -> Self {
        Self {
            attempt_id,
            sink,
            utf8: IncrementalUtf8::default(),
            decoded_content: String::new(),
            content: String::new(),
            classified_len: 0,
            pending_delta: String::new(),
            next_sequence: 1,
            last_flush: Instant::now(),
            answer_committed: false,
            action: false,
            continuation: false,
            delivery_suspended: false,
            content_byte_limit_reached: false,
            additional_stops,
            repair_malformed_tool_calls,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> bool {
        let decoded = self.utf8.push(bytes);
        if !decoded.is_empty() {
            if self
                .decoded_content
                .len()
                .checked_add(decoded.len())
                .is_none_or(|length| length > MAX_TEXT_PART_BYTES)
            {
                self.content_byte_limit_reached = true;
                return true;
            }
            self.decoded_content.push_str(&decoded);
            self.refresh(false);
        }
        self.action
    }

    pub(super) fn stopped_on_continuation(&self) -> bool {
        self.continuation
    }

    pub(super) fn content_byte_limit_reached(&self) -> bool {
        self.content_byte_limit_reached
    }

    pub(super) fn finish(mut self) -> ClassifiedResponse {
        let decoded = self.utf8.finish();
        if !decoded.is_empty() {
            if self
                .decoded_content
                .len()
                .checked_add(decoded.len())
                .is_some_and(|length| length <= MAX_TEXT_PART_BYTES)
            {
                self.decoded_content.push_str(&decoded);
            } else {
                self.content_byte_limit_reached = true;
            }
        }
        self.refresh(true);
        if !self.action {
            self.flush_all();
        }
        ClassifiedResponse {
            decoded_content: self.decoded_content,
            content: self.content,
            content_byte_limit_reached: self.content_byte_limit_reached,
        }
    }

    fn refresh(&mut self, terminal: bool) {
        self.content.clone_from(&self.decoded_content);
        strip_generated_assistant_prefix(&mut self.content);
        self.continuation |= trim_additional_stops(&mut self.content, &self.additional_stops);
        self.continuation |= trim_generated_continuation(&mut self.content);

        if !self.answer_committed {
            if !terminal && generated_assistant_prefix_is_pending(&self.decoded_content) {
                return;
            }
            if isolated_tool_call_with_repair(&self.content, self.repair_malformed_tool_calls)
                .is_some()
            {
                self.action = true;
                self.pending_delta.clear();
                return;
            }
            if tool_call_candidate(&self.content) {
                if terminal {
                    if !matches!(
                        agl_actions::parse_model_output(&self.content),
                        ParsedModelOutput::Answer(_)
                    ) {
                        self.action = true;
                        return;
                    }
                    self.answer_committed = true;
                } else {
                    return;
                }
            } else {
                self.answer_committed = true;
            }
        }

        let stable_end = if terminal {
            self.content.len()
        } else {
            stable_text_end_with_stops(&self.content, &self.additional_stops)
        };
        if stable_end > self.classified_len {
            self.pending_delta
                .push_str(&self.content[self.classified_len..stable_end]);
            self.classified_len = stable_end;
        }
        self.flush_ready(terminal);
    }

    fn flush_ready(&mut self, terminal: bool) {
        while !self.pending_delta.is_empty() {
            let newline_end = self.pending_delta.find('\n').map(|index| index + 1);
            let elapsed = self.last_flush.elapsed() >= MAX_COALESCE_LATENCY;
            let requested = if let Some(newline_end) = newline_end {
                newline_end.min(MAX_DELTA_BYTES)
            } else if self.pending_delta.len() >= MAX_DELTA_BYTES {
                MAX_DELTA_BYTES
            } else if terminal || elapsed {
                self.pending_delta.len().min(MAX_DELTA_BYTES)
            } else {
                break;
            };
            let end = char_boundary_at_or_before(&self.pending_delta, requested);
            if end == 0 {
                break;
            }
            let text = self.pending_delta[..end].to_string();
            self.pending_delta.drain(..end);
            self.emit(text);
            if self.delivery_suspended {
                self.pending_delta.clear();
                break;
            }
        }
    }

    fn flush_all(&mut self) {
        self.flush_ready(true);
    }

    fn emit(&mut self, text: String) {
        if self.delivery_suspended || text.is_empty() {
            return;
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let delivery = self.sink.try_emit(InferenceOutputEvent::TextDelta {
            attempt_id: self.attempt_id.clone(),
            sequence,
            text,
        });
        self.last_flush = Instant::now();
        if matches!(delivery, OutputDelivery::Lagged | OutputDelivery::Closed) {
            self.delivery_suspended = true;
        }
    }
}

fn tool_call_candidate(content: &str) -> bool {
    let content = content.trim_start();
    TOOL_CALL_OPENINGS
        .iter()
        .any(|opening| opening.starts_with(content) || content.starts_with(opening))
}

fn stable_text_end_with_stops(content: &str, additional_stops: &[String]) -> usize {
    let mut end = content.len();
    for marker in STOP_MARKERS
        .iter()
        .copied()
        .chain(additional_stops.iter().map(String::as_str))
    {
        for prefix_len in 1..marker.len() {
            if content.ends_with(&marker[..prefix_len]) {
                end = end.min(content.len() - prefix_len);
            }
        }
    }
    end
}

fn trim_additional_stops(content: &mut String, additional_stops: &[String]) -> bool {
    let marker_offset = additional_stops
        .iter()
        .filter(|marker| !marker.is_empty())
        .filter_map(|marker| content.find(marker))
        .min();
    if let Some(offset) = marker_offset {
        content.truncate(offset);
        true
    } else {
        false
    }
}

fn char_boundary_at_or_before(value: &str, requested: usize) -> usize {
    let mut boundary = requested.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

#[derive(Default)]
struct IncrementalUtf8 {
    pending: Vec<u8>,
}

impl IncrementalUtf8 {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    output.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    if valid_end > consumed {
                        output.push_str(
                            std::str::from_utf8(&self.pending[consumed..valid_end])
                                .expect("UTF-8 validator identified a valid prefix"),
                        );
                    }
                    consumed = valid_end;
                    let Some(error_len) = error.error_len() else {
                        break;
                    };
                    output.push('\u{fffd}');
                    consumed = consumed.saturating_add(error_len);
                }
            }
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        output
    }

    fn finish(&mut self) -> String {
        let mut output = self.push(&[]);
        if !self.pending.is_empty() {
            output.push_str(&String::from_utf8_lossy(&self.pending));
            self.pending.clear();
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct RecordingSink {
        events: Mutex<Vec<InferenceOutputEvent>>,
        delivery: OutputDelivery,
    }

    impl RecordingSink {
        fn delivered() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                delivery: OutputDelivery::Delivered,
            }
        }

        fn with_delivery(delivery: OutputDelivery) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                delivery,
            }
        }

        fn deltas(&self) -> Vec<(u64, String)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    InferenceOutputEvent::TextDelta { sequence, text, .. } => {
                        Some((*sequence, text.clone()))
                    }
                    InferenceOutputEvent::Stage(_) => None,
                })
                .collect()
        }
    }

    impl InferenceOutputSink for RecordingSink {
        fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
            self.events.lock().unwrap().push(event);
            self.delivery
        }
    }

    fn attempt_id() -> AttemptId {
        AttemptId::parse("attempt_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b35").unwrap()
    }

    #[test]
    fn utf8_split_across_pieces_is_emitted_once_and_intact() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);
        let bytes = "A€界\n".as_bytes();

        assert!(!classifier.push(&bytes[..2]));
        assert!(!classifier.push(&bytes[2..5]));
        assert!(!classifier.push(&bytes[5..]));
        let response = classifier.finish();

        assert_eq!(response.content, "A€界\n");
        assert_eq!(sink.deltas(), vec![(1, "A€界\n".to_string())]);
    }

    #[test]
    fn isolated_tool_actions_never_publish_text() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);

        assert!(!classifier.push(b"<tool_"));
        assert!(!classifier.push(b"call>{\"name\":\"fs.read\","));
        assert!(classifier.push(b"\"arguments\":{}}</tool_call>"));
        let response = classifier.finish();

        assert!(response.content.starts_with("<tool_call>"));
        assert!(sink.deltas().is_empty());
    }

    #[test]
    fn common_plan_stop_is_removed_exactly() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new_with_stops(
            attempt_id(),
            &sink,
            vec!["<exact_stop>".to_string()],
        );

        assert!(!classifier.push(b"answer<exact_"));
        assert!(!classifier.push(b"stop>ignored"));
        let response = classifier.finish();

        assert_eq!(response.content, "answer");
        assert!(classifier_stop_was_not_published(
            &sink.deltas(),
            "<exact_stop>"
        ));
    }

    fn classifier_stop_was_not_published(deltas: &[(u64, String)], marker: &str) -> bool {
        !deltas.iter().any(|(_, delta)| delta.contains(marker))
    }

    #[test]
    fn chunks_are_bounded_unicode_safe_and_strictly_sequenced() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);
        let content = format!("{}\nlast", "界".repeat(1_500));

        assert!(!classifier.push(content.as_bytes()));
        let response = classifier.finish();
        let deltas = sink.deltas();

        assert_eq!(response.content, content);
        assert_eq!(
            deltas
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>(),
            content
        );
        assert!(deltas.iter().all(|(_, text)| text.len() <= MAX_DELTA_BYTES));
        assert_eq!(
            deltas
                .iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            (1..=u64::try_from(deltas.len()).unwrap()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lagged_or_closed_sink_suspends_delivery_without_affecting_final_content() {
        for delivery in [OutputDelivery::Lagged, OutputDelivery::Closed] {
            let sink = RecordingSink::with_delivery(delivery);
            let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);

            assert!(!classifier.push(b"first\n"));
            assert!(!classifier.push(b"second\n"));
            let response = classifier.finish();

            assert_eq!(response.content, "first\nsecond\n");
            assert_eq!(sink.deltas(), vec![(1, "first\n".to_string())]);
        }
    }

    #[test]
    fn content_byte_limit_stops_before_accepting_the_crossing_piece() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);
        let accepted = vec![b'a'; MAX_TEXT_PART_BYTES];

        assert!(!classifier.push(&accepted));
        assert!(classifier.push(b"b"));
        assert!(classifier.content_byte_limit_reached());
        let response = classifier.finish();

        assert_eq!(response.content.len(), MAX_TEXT_PART_BYTES);
        assert!(response.content_byte_limit_reached);
        assert!(response.content.bytes().all(|byte| byte == b'a'));
        assert_eq!(
            sink.deltas()
                .into_iter()
                .map(|(_, text)| text.len())
                .sum::<usize>(),
            MAX_TEXT_PART_BYTES
        );
    }

    #[test]
    fn utf8_finish_reports_byte_limit_when_buffered_replacement_would_cross_it() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);
        let accepted = vec![b'a'; MAX_TEXT_PART_BYTES];

        assert!(!classifier.push(&accepted));
        assert!(!classifier.push(&[0xe2]));
        assert!(!classifier.content_byte_limit_reached());
        let response = classifier.finish();

        assert!(response.content_byte_limit_reached);
        assert_eq!(response.content.len(), MAX_TEXT_PART_BYTES);
        assert!(response.content.bytes().all(|byte| byte == b'a'));
    }

    #[test]
    fn assistant_prefix_and_unfinished_stop_marker_are_not_exposed() {
        let sink = RecordingSink::delivered();
        let mut classifier = IncrementalResponseClassifier::new(attempt_id(), &sink);

        assert!(!classifier.push(b"Assist"));
        assert!(!classifier.push(b"ant: hello\nUs"));
        assert!(!classifier.push(b"er: next"));
        let response = classifier.finish();

        assert_eq!(response.content, "hello");
        assert_eq!(sink.deltas(), vec![(1, "hello".to_string())]);
    }
}
