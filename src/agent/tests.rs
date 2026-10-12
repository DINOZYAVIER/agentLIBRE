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
    tokens: usize,
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
            tokens: 100,
        }
    }
}

impl Backend for Script {
    fn render(&self, prompt: &Prompt) -> Result<String, Error> {
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
        Ok(self.tokens)
    }

    fn context_size(&self) -> Result<usize, Error> {
        Ok(16384)
    }
}

#[derive(Default)]
struct Log(Vec<String>);

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
    fn text(&mut self, text: &str) {
        self.0.push(format!("text: {text}"));
    }
    fn call(&mut self, call: &ToolCall) {
        self.0.push(format!("> {}", call.name));
    }
    fn result(&mut self, _: &ToolCall, result: &ToolResult) {
        self.0.push(format!("  {}", result.outcome.name()));
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir =
            std::env::temp_dir().join(format!("agl-lite-agent-{name}-{}", std::process::id()));
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
    let end = Agent {
        function,
        function_name: "f",
        backend: script,
        session: &mut session,
        workspace: &dir.0,
        context: 16384,
    }
    .turn(message, &mut log);
    (end, log.0, session)
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
            "thinking: Done."
        ]
    );
    let kinds: Vec<&str> = session
        .events
        .iter()
        .map(|e| match e {
            Event::User(_) => "user",
            Event::State(_) => "state",
            Event::Thinking(_) => "thinking",
            Event::Call { .. } => "call",
            Event::Result { .. } => "result",
            Event::Answer(_) => "answer",
            Event::Stop(_) => "stop",
        })
        .collect();
    assert_eq!(
        kinds,
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
    let coder = function(CODER).unwrap();
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
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::CutOff));
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
            "thinking: t"
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
    let (end, _, session) = run(&one_step, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::CutOff));
    assert_eq!(
        session.events.last(),
        Some(&Event::Stop(StopReason::CutOff))
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
