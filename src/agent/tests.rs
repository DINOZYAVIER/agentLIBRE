use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

use super::*;
use crate::backend::Generated;
use crate::function::tests::{CODER, function};
use crate::session::Event;

/// A backend that replays scripted generations.
struct Script {
    outputs: RefCell<VecDeque<Generated>>,
    prompts: RefCell<Vec<String>>,
    /// Token counts to give in turn, then `tokens`.
    counts: RefCell<VecDeque<usize>>,
    tokens: usize,
    /// Whether each rendered prompt carried any thinking.
    thinking: RefCell<Vec<bool>>,
    /// The system message of each rendered prompt.
    systems: RefCell<Vec<String>>,
}

impl Script {
    fn new(outputs: &[(&str, Stop)]) -> Script {
        Script {
            outputs: RefCell::new(
                outputs
                    .iter()
                    .map(|(text, stop)| Generated {
                        text: text.to_string(),
                        stop: match stop {
                            Stop::End => Stop::End,
                            Stop::Limit => Stop::Limit,
                            Stop::Word(w) => Stop::Word(w.clone()),
                        },
                    })
                    .collect(),
            ),
            prompts: RefCell::new(Vec::new()),
            counts: RefCell::new(VecDeque::new()),
            tokens: 100,
            thinking: RefCell::new(Vec::new()),
            systems: RefCell::new(Vec::new()),
        }
    }

    fn with_counts(mut self, counts: &[usize]) -> Script {
        self.counts = RefCell::new(counts.iter().copied().collect());
        self
    }
}

impl Backend for Script {
    fn render(&self, prompt: &Prompt) -> Result<String, Error> {
        if let Some(Message::System(text)) = prompt.messages.first() {
            self.systems.borrow_mut().push(text.clone());
        }
        self.thinking
            .borrow_mut()
            .push(prompt.messages.iter().any(|m| {
                matches!(
                    m,
                    Message::Assistant {
                        thinking: Some(_),
                        ..
                    }
                )
            }));
        let instruction = prompt.instruction.unwrap_or("-");
        Ok(format!(
            "[{} messages; {instruction}]<think>\n",
            prompt.messages.len()
        ))
    }

    fn generate(
        &self,
        request: &Request,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        self.prompts.borrow_mut().push(request.prompt.to_owned());
        let generated = self
            .outputs
            .borrow_mut()
            .pop_front()
            .expect("a scripted output");
        for piece in generated.text.split_inclusive(' ') {
            on_text(piece);
        }
        Ok(generated)
    }

    fn count_tokens(&self, _: &str) -> Result<usize, Error> {
        Ok(self.counts.borrow_mut().pop_front().unwrap_or(self.tokens))
    }

    fn context_size(&self) -> Result<usize, Error> {
        Ok(16384)
    }
}

#[derive(Default)]
/// The lines of the report, and the prompt sizes it was told (D152).
struct Log(Vec<String>, Vec<usize>);

impl Report for Log {
    fn state(&mut self, state: &str) {
        self.0.push(format!("[{state}]"));
    }
    fn thinking(&mut self, piece: &str) {
        match self.0.last_mut() {
            Some(last) if last.starts_with("thinking: ") => last.push_str(piece),
            _ => self.0.push(format!("thinking: {piece}")),
        }
    }
    fn thinking_done(&mut self) {}
    fn action_text(&mut self, piece: &str) {
        match self.0.last_mut() {
            Some(last) if last.starts_with("stream: ") => last.push_str(piece),
            _ => self.0.push(format!("stream: {piece}")),
        }
    }
    fn text(&mut self, text: &str) {
        self.0.push(format!("text: {text}"));
    }
    fn answer(&mut self, text: &str) {
        self.0.push(format!("answer: {text}"));
    }
    fn call(&mut self, call: &ToolCall) {
        self.0.push(format!("> {}", call.name));
    }
    fn result(&mut self, _: &ToolCall, result: &ToolResult) {
        self.0.push(format!("  {}", result.outcome.name()));
    }
    fn compacting(&mut self) {
        self.0.push("compacting".into());
    }
    fn compacted(&mut self) {
        self.0.push("[compacted]".into());
    }
    fn cut_off(&mut self) {
        self.0.push("[cut off]".into());
    }
    fn context_used(&mut self, tokens: usize) {
        self.1.push(tokens);
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("agentLIBRE-agent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bash_call(command: &str) -> String {
    format!(
        "<tool_call>\n<function=bash>\n<parameter=command>\n{command}\n</parameter>\n</function>\n</tool_call>"
    )
}

const THOUGHT: Stop = Stop::End;

fn run(
    function: &Function,
    script: &Script,
    dir: &Dir,
    message: &str,
) -> (Result<TurnEnd, Error>, Vec<String>, Session) {
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let end = agent(function, script, &mut session, dir).turn(message, &mut log);
    (end, log.0, session)
}

fn agent<'a>(
    function: &'a Function,
    script: &'a Script,
    session: &'a mut Session,
    dir: &'a Dir,
) -> Agent<'a> {
    Agent {
        function,
        function_name: "f",
        backend: script,
        session,
        workspace: &dir.0,
        context: 16384,
        forge: ForgeEnv {
            lang: crate::i18n::Lang::En,
            auto_rehome: true,
        },
    }
}

fn kinds(session: &Session) -> Vec<&'static str> {
    session
        .events
        .iter()
        .map(|e| match e {
            Event::User(_) => "user",
            Event::State(_) => "state",
            Event::Thinking(_) => "thinking",
            Event::Call { .. } => "call",
            Event::Result { .. } => "result",
            Event::Forge { .. } => "forge",
            Event::Answer(_) => "answer",
            Event::Stop(_) => "stop",
            Event::Compact(_) => "compact",
            Event::CutOff(_) => "cut_off",
        })
        .collect()
}

#[test]
fn a_call_then_an_answer() {
    let dir = Dir::new("answer");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("I will list files", THOUGHT),
        (&bash_call("echo hello"), Stop::End),
        ("Done.", THOUGHT),
        ("It printed hello.", Stop::End),
    ]);
    let (end, log, session) = run(&default, &script, &dir, "Say hello");
    assert_eq!(end.unwrap(), TurnEnd::Answer("It printed hello.".into()));
    assert_eq!(
        log,
        [
            "[work]",
            "thinking: I will list files",
            "> bash",
            "  ok",
            "thinking: Done.",
            "stream: It printed hello.",
            "answer: It printed hello."
        ]
    );
    assert_eq!(
        kinds(&session),
        [
            "user", "state", "thinking", "call", "result", "thinking", "answer"
        ]
    );
    assert!(matches!(&session.events[4], Event::Result { content, .. } if content == "hello\n"));
    let prompts = script.prompts.borrow();
    // The thinking is closed before the action, and the second step sees
    // the first one in its history.
    assert_eq!(
        prompts[1],
        "[2 messages; -]<think>\nI will list files\n</think>\n\n"
    );
    assert!(prompts[2].starts_with("[4 messages; -]"));
}

#[test]
fn states_and_instructions() {
    let dir = Dir::new("states");
    // Three steps: the edit, the failed test and the discarded action.
    let coder = function(&CODER.replace("max_steps_total = 50", "max_steps_total = 3")).unwrap();
    fs::write(dir.0.join("lib.rs"), "fn a() {}\n").unwrap();
    let edit = "<tool_call>\n<function=edit>\n<parameter=path>\nlib.rs\n</parameter>\n\
                <parameter=old>\nfn a\n</parameter>\n<parameter=new>\npub fn a\n</parameter>\n\
                </function>\n</tool_call>";
    let script = Script::new(&[
        ("t", THOUGHT),
        (edit, Stop::End),
        // verify does not think.
        (&bash_call("cargo test"), Stop::End),
        ("t", THOUGHT),
        // explore may not answer, so the model goes on until cut off.
        ("<tool_call>\n<function=bash>", Stop::Limit),
    ]);
    let (end, log, _) = run(&coder, &script, &dir, "Make a public");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::StepLimit));
    assert_eq!(
        log,
        [
            "[explore]",
            "thinking: t",
            "> edit",
            "  ok",
            "[verify]",
            "> bash",
            "  fail",
            "[explore]",
            "thinking: t",
            "[cut off]"
        ]
    );
    assert_eq!(
        fs::read_to_string(dir.0.join("lib.rs")).unwrap(),
        "pub fn a() {}\n"
    );
    let prompts = script.prompts.borrow();
    assert!(prompts[2].contains("Only verification is available now"));
    assert!(prompts[2].ends_with("<think>\n\n</think>\n\n"));
}

#[test]
fn a_cut_off_action_is_discarded_and_the_model_is_told() {
    let dir = Dir::new("cut-off");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (
            "<tool_call>\n<function=bash>\n<parameter=command>\ncat <<EOF",
            Stop::Limit,
        ),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ]);
    let (end, log, session) = run(&default, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        kinds(&session),
        ["user", "state", "thinking", "cut_off", "thinking", "answer"]
    );
    assert_eq!(session.events[3], Event::CutOff(8192));
    assert!(log.contains(&"[cut off]".to_string()));
    // The discarded text is not kept; the model reads the notice instead.
    let history = session.history();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history[1],
        Message::Runtime(
            "Your last action hit max_action_tokens (8192) and was discarded; nothing was \
             executed or shown. Split the work into smaller actions, for example write a \
             large file in parts."
                .into()
        )
    );
    // The turn went on with the notice in the second step's prompt.
    assert!(script.prompts.borrow()[2].starts_with("[3 messages"));
}

#[test]
fn stops() {
    let dir = Dir::new("stops");
    let one_step = function(
        "format = \"qwen3.8\"\nsystem = \"s\"\nmax_steps_total = 1\ninitial = \"a\"\n\
         [state.a]\ntools.bash = {}\nthink = false\nanswer = true\nnext = { else = \"a\" }",
    )
    .unwrap();
    let script = Script::new(&[(&bash_call("true"), Stop::End)]);
    let (end, _, session) = run(&one_step, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::StepLimit));
    assert_eq!(
        session.events.last(),
        Some(&Event::Stop(StopReason::StepLimit))
    );

    let script = Script::new(&[(
        "<tool_call>\n<function=bash>\n<parameter=command>\nyes",
        Stop::Limit,
    )]);
    // A discarded action is a step (D154).
    let (end, _, session) = run(&one_step, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::StepLimit));
    assert_eq!(
        session.events[session.events.len() - 2..],
        [Event::CutOff(8192), Event::Stop(StopReason::StepLimit)]
    );

    let mut full = Script::new(&[]);
    full.tokens = 16384;
    let (end, _, _) = run(&one_step, &full, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));

    let script = Script::new(&[("<tool_call>\n<function=nope>", Stop::End)]);
    let (end, _, session) = run(&one_step, &script, &dir, "go");
    assert!(matches!(end, Err(Error::OutsideGrammar(_))));
    assert_eq!(
        session.events.last(),
        Some(&Event::Stop(StopReason::ServerError))
    );
}

#[test]
fn thinking_at_the_limit_is_closed() {
    let dir = Dir::new("limit");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[("a long thought ", Stop::Limit), ("ok", Stop::End)]);
    let (end, _, _) = run(&default, &script, &dir, "hi");
    assert_eq!(end.unwrap(), TurnEnd::Answer("ok".into()));
    assert_eq!(
        script.prompts.borrow()[1],
        "[2 messages; -]<think>\na long thought\n</think>\n\n"
    );
}

#[test]
fn only_function_tools_are_listed() {
    let answer_only =
        function("format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"a\"\n[state.a]\nanswer = true")
            .unwrap();
    assert!(function_tools(&answer_only).is_empty());
    let coder = function(CODER).unwrap();
    let names: Vec<&str> = function_tools(&coder).iter().map(|d| d.name).collect();
    assert_eq!(names, ["bash", "edit"]);
}

#[test]
fn text_stream_cuts_out_the_call() {
    const BEGIN: &str = "<tool_call>\n<function=";
    let run = |pieces: &[&str]| {
        let mut stream = TextStream::new(BEGIN);
        let mut shown = String::new();
        for piece in pieces {
            shown.extend(stream.push(piece));
        }
        shown.extend(stream.finish());
        shown
    };
    assert_eq!(
        run(&["Let me ", "look.\n\n", "<tool_call>\n<function=bash>"]),
        "Let me look.\n\n"
    );
    // The start of a call split over pieces is never shown.
    assert_eq!(run(&["a<", "tool", "_call>", "\n<function=x>"]), "a");
    // A tail that only looks like the start of a call is shown at the end.
    assert_eq!(run(&["x < y <", "tool"]), "x < y <tool");
    assert_eq!(run(&["no call"]), "no call");
    assert_eq!(run(&["<tool_call>\n<function=bash>"]), "");
    assert_eq!(run(&["é", "<tool_call>\n<fu", "nction=bash>"]), "é");
}

#[test]
fn text_before_a_call_streams_and_is_reported_whole() {
    let dir = Dir::new("stream");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("t", THOUGHT),
        (&format!("I will run it.\n{}", bash_call("true")), Stop::End),
        ("t", THOUGHT),
        ("Done.", Stop::End),
    ]);
    let (end, log, _) = run(&default, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        log,
        [
            "[work]",
            "thinking: t",
            "stream: I will run it.\n",
            "text: I will run it.",
            "> bash",
            "  ok",
            "thinking: t",
            "stream: Done.",
            "answer: Done."
        ]
    );
}

/// Token counts of a step that overflows the context (with 4096 for
/// thinking and 2048 for the action) and of a compaction prompt that fits
/// with its 2048 tokens.
const OVERFLOW: usize = 12_000;
const FITS: usize = 5_000;

#[test]
fn a_full_context_is_compacted_and_the_turn_goes_on() {
    let dir = Dir::new("compact");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS]);
    let (end, log, session) = run(&default, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        log,
        [
            "[work]",
            "thinking: t1",
            "> bash",
            "  ok",
            "compacting",
            "[compacted]",
            "thinking: t2",
            "stream: Done.",
            "answer: Done."
        ]
    );
    assert_eq!(
        kinds(&session),
        [
            "user", "state", "thinking", "call", "result", "compact", "thinking", "answer"
        ]
    );
    assert_eq!(
        session.events[5],
        Event::Compact("The user said go; true ran.".into())
    );
    let prompts = script.prompts.borrow();
    // The summary is asked for after the history, without thinking.
    assert!(prompts[2].starts_with("[4 messages; Summarize the conversation so far"));
    assert!(prompts[2].ends_with("<think>\n\n</think>\n\n"));
    assert_eq!(*script.thinking.borrow(), [false, true, false, false]);
    // The turn goes on from the summary alone.
    assert!(prompts[3].starts_with("[2 messages; -]"));
    // The summary is a runtime message, not the human's (AGL-009).
    assert_eq!(
        session.history()[0],
        Message::Runtime("The user said go; true ran.".into())
    );
}

#[test]
fn the_system_message_explains_the_runtime_prefix_and_the_action_size() {
    let dir = Dir::new("system");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS]);
    let (end, _, _) = run(&default, &script, &dir, "go");
    assert!(end.is_ok());
    // A step, a step, the compaction, a step: all carry the Function's
    // system text and the two sentences, the same text each time.
    let systems = script.systems.borrow();
    assert_eq!(systems.len(), 4);
    let expected = format!(
        "{}\n\nMessages that start with [agentLIBRE runtime] come from the agentLIBRE runtime, \
         not from the human. One action holds at most {} tokens; write a large file in parts.",
        default.system, default.max_action_tokens
    );
    assert!(systems.iter().all(|system| *system == expected));
    assert!(expected.contains("at most 8192 tokens"));
}

#[test]
fn the_prompt_size_of_every_step_is_reported() {
    let dir = Dir::new("used");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS, 200]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    agent(&default, &script, &mut session, &dir)
        .turn("go", &mut log)
        .unwrap();
    // The summary prompt (the third count) is not a step.
    assert_eq!(log.1, [100, OVERFLOW, 200]);
}

#[test]
fn a_context_that_compaction_cannot_free_stops_the_turn() {
    let dir = Dir::new("no-room");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    // The compaction prompt itself does not fit.
    let script = Script::new(&[]).with_counts(&[OVERFLOW, 15_000]);
    let (end, log, session) = run(&default, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
    assert_eq!(log, ["[work]", "compacting"]);
    assert_eq!(kinds(&session), ["user", "state", "stop"]);

    // A summary cut off by max_compact_tokens or an empty one is dropped.
    for summary in [("a long summary", Stop::Limit), (" \n", Stop::End)] {
        let script = Script::new(&[summary]).with_counts(&[OVERFLOW, FITS]);
        let (end, _, session) = run(&default, &script, &dir, "go");
        assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
        assert_eq!(kinds(&session), ["user", "state", "stop"]);
    }

    // Still too long after a summary: nothing is left to compact.
    let script = Script::new(&[("s", Stop::End)]).with_counts(&[OVERFLOW, FITS, OVERFLOW]);
    let (end, _, session) = run(&default, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
    assert_eq!(kinds(&session), ["user", "state", "compact", "stop"]);
}

#[test]
fn compaction_on_request() {
    let dir = Dir::new("on-request");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "default").unwrap();
    let script = Script::new(&[("t", THOUGHT), ("Hi.", Stop::End), ("Greeted.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    assert_eq!(
        agent(&default, &script, &mut session, &dir)
            .compact(&mut log)
            .unwrap(),
        Compaction::Nothing
    );
    agent(&default, &script, &mut session, &dir)
        .turn("hello", &mut log)
        .unwrap();
    assert_eq!(
        agent(&default, &script, &mut session, &dir)
            .compact(&mut log)
            .unwrap(),
        Compaction::Done
    );
    assert_eq!(
        session.events.last(),
        Some(&Event::Compact("Greeted.".into()))
    );
    assert_eq!(
        agent(&default, &script, &mut session, &dir)
            .compact(&mut log)
            .unwrap(),
        Compaction::Nothing
    );
}
