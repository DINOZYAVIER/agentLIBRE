use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

use super::*;
use crate::backend::{Generated, StructuredCall};
use crate::function::tests::{CODER, function, function_on};
use crate::session::Event;

/// A backend that replays scripted generations.
struct Script {
    outputs: RefCell<VecDeque<Generated>>,
    /// The texts that are generations, for `count_tokens` (AGL-012).
    generated: Vec<String>,
    prompts: RefCell<Vec<String>>,
    /// Token counts to give in turn, then `tokens`.
    counts: RefCell<VecDeque<usize>>,
    tokens: usize,
    /// Whether each rendered prompt carried any thinking.
    thinking: RefCell<Vec<bool>>,
    /// The system message of each rendered prompt.
    systems: RefCell<Vec<String>>,
    /// The reasoning level of each rendered prompt (AGL-043 F16).
    reasonings: RefCell<Vec<Option<ReasoningLevel>>>,
    /// The server's media marker, if it has a projector (AGL-014).
    marker: Option<String>,
    /// Whether the model hears an audio itself (AGL-014, D19).
    hears: bool,
    /// The user texts of each rendered prompt, joined.
    users: RefCell<Vec<String>>,
    /// The media each generation was sent with.
    uploads: RefCell<Vec<Vec<String>>>,
    /// The texts of the runtime messages of each rendered prompt, in order.
    runtimes: RefCell<Vec<Vec<String>>>,
    /// The runtime messages of each structured generation (AGL-030, D9).
    structured: RefCell<Vec<Vec<String>>>,
}

impl Script {
    fn new(outputs: &[(&str, Stop)]) -> Script {
        Script {
            outputs: RefCell::new(
                outputs
                    .iter()
                    .map(|(text, stop)| {
                        Generated::plain(
                            text.to_string(),
                            match stop {
                                Stop::End => Stop::End,
                                Stop::Limit => Stop::Limit,
                                Stop::Word(w) => Stop::Word(w.clone()),
                            },
                        )
                    })
                    .collect(),
            ),
            generated: outputs.iter().map(|(text, _)| text.to_string()).collect(),
            prompts: RefCell::new(Vec::new()),
            counts: RefCell::new(VecDeque::new()),
            tokens: 100,
            thinking: RefCell::new(Vec::new()),
            systems: RefCell::new(Vec::new()),
            reasonings: RefCell::new(Vec::new()),
            marker: Some("<__media_TEST__>".into()),
            hears: false,
            users: RefCell::new(Vec::new()),
            uploads: RefCell::new(Vec::new()),
            runtimes: RefCell::new(Vec::new()),
            structured: RefCell::new(Vec::new()),
        }
    }

    /// A script of structured generations (AGL-030, D9).
    fn structured(outputs: Vec<Generated>) -> Script {
        let script = Script::new(&[]);
        *script.outputs.borrow_mut() = outputs.into_iter().collect();
        script
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
        self.reasonings.borrow_mut().push(prompt.reasoning);
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
        let users: Vec<&str> = prompt
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(text) | Message::Tool { content: text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        self.users.borrow_mut().push(users.join("\n"));
        self.runtimes.borrow_mut().push(
            prompt
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::Runtime(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        );
        let instruction = prompt.instruction.unwrap_or("-");
        Ok(format!(
            "[{} messages; {instruction}]<think>\n",
            prompt.messages.len()
        ))
    }

    fn generate(
        &self,
        request: &Request,
        on_thinking: &mut dyn FnMut(&str),
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        self.prompts.borrow_mut().push(request.prompt.to_owned());
        if let Some(prompt) = request.structured {
            self.structured.borrow_mut().push(
                prompt
                    .messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::Runtime(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
            );
        }
        self.uploads.borrow_mut().push(request.media.to_vec());
        let generated = self
            .outputs
            .borrow_mut()
            .pop_front()
            .expect("a scripted output");
        if let Some(thinking) = &generated.thinking {
            for piece in thinking.split_inclusive(' ') {
                on_thinking(piece);
            }
        }
        for piece in generated.text.split_inclusive(' ') {
            on_text(piece);
        }
        Ok(generated)
    }

    fn count_tokens(&self, text: &str) -> Result<usize, Error> {
        if self.generated.iter().any(|generated| generated == text) {
            // A generation counts as its characters (AGL-012).
            return Ok(text.chars().count());
        }
        Ok(self.counts.borrow_mut().pop_front().unwrap_or(self.tokens))
    }

    fn context_size(&self) -> Result<usize, Error> {
        Ok(16384)
    }

    fn media_marker(&self) -> Result<String, Error> {
        self.marker.clone().ok_or(Error::NoMultimodal)
    }

    fn hears_audio(&self) -> Result<bool, Error> {
        Ok(self.hears)
    }

    fn health(&self) -> Option<u16> {
        None
    }

    fn url(&self) -> &str {
        "http://127.0.0.1:0"
    }
}

#[derive(Default)]
/// The lines of the report, the prompt sizes it was told (D152) and the
/// tokens each generation decoded (AGL-012).
struct Log(Vec<String>, Vec<usize>, Vec<usize>);

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
    fn memory(&mut self, name: &str, kind: crate::memory::Kind, text: Option<&str>) {
        self.0.push(format!("memory {}{name} {}", kind.sign(), text.unwrap_or("-")));
    }
    fn cut_off(&mut self, _cut: &CutOff) {
        self.0.push("[cut off]".into());
    }
    fn stirred(&mut self, attempt: u32, max: u32, error: &str) {
        self.0.push(format!("[stir {attempt}/{max}: {error}]"));
    }
    fn context_used(&mut self, tokens: usize) {
        self.1.push(tokens);
    }
    fn decoded(&mut self, tokens: usize) {
        self.2.push(tokens);
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
    run_level(function, script, dir, message, None)
}

fn run_level(
    function: &Function,
    script: &Script,
    dir: &Dir,
    message: &str,
    reasoning: Option<ReasoningLevel>,
) -> (Result<TurnEnd, Error>, Vec<String>, Session) {
    run_stirred(function, script, dir, message, reasoning, false)
}

/// A turn in the stirred mode (AGL-030): the action phase is unconstrained
/// and a parse failure is a corrective retry.
fn run_stirred(
    function: &Function,
    script: &Script,
    dir: &Dir,
    message: &str,
    reasoning: Option<ReasoningLevel>,
    stirred: bool,
) -> (Result<TurnEnd, Error>, Vec<String>, Session) {
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let end = agent_stirred(function, script, &mut session, dir, reasoning, stirred)
        .turn(message, &mut log);
    (end, log.0, session)
}

fn agent<'a>(
    function: &'a Function,
    script: &'a Script,
    session: &'a mut Session,
    dir: &'a Dir,
    reasoning: Option<ReasoningLevel>,
) -> Agent<'a> {
    agent_stirred(function, script, session, dir, reasoning, false)
}

fn agent_stirred<'a>(
    function: &'a Function,
    script: &'a Script,
    session: &'a mut Session,
    dir: &'a Dir,
    reasoning: Option<ReasoningLevel>,
    stirred: bool,
) -> Agent<'a> {
    agent_searching(function, script, session, dir, reasoning, &crate::search::NoSearch, stirred)
}

fn agent_searching<'a>(
    function: &'a Function,
    script: &'a Script,
    session: &'a mut Session,
    dir: &'a Dir,
    reasoning: Option<ReasoningLevel>,
    search: &'a dyn crate::search::Searcher,
    stirred: bool,
) -> Agent<'a> {
    Agent {
        function,
        function_name: "f",
        backend: script,
        stirred,
        session,
        workspace: &dir.0,
        context: 16384,
        forge: ForgeEnv {
            lang: crate::i18n::Lang::En,
            auto_rehome: true,
            data: &dir.0,
        },
        reasoning,
        image_tokens: 1000,
        whisper: &NoWhisper,
        search,
        memory: &NoMemory,
    }
}

/// A workspace with no memory store.
struct NoMemory;

impl crate::memory::Home for NoMemory {
    fn read(&self) -> Option<PathBuf> {
        None
    }
    fn write(&self) -> Result<(PathBuf, Vec<String>), Error> {
        Err(Error::NotLocalServer { url: "no store".into() })
    }
}

/// A memory directory of the test: read when it exists, made on a write.
struct DirMemory(PathBuf);

impl crate::memory::Home for DirMemory {
    fn read(&self) -> Option<PathBuf> {
        self.0.is_dir().then(|| self.0.clone())
    }
    fn write(&self) -> Result<(PathBuf, Vec<String>), Error> {
        fs::create_dir_all(&self.0).unwrap();
        Ok((self.0.clone(), vec!["the store was made".into()]))
    }
}

/// A searcher that records what it is asked and answers with `reply`.
struct FakeSearch {
    asked: std::cell::RefCell<Vec<crate::search::Request>>,
    reply: ToolResult,
}

impl FakeSearch {
    fn new(outcome: Outcome, content: &str) -> FakeSearch {
        FakeSearch {
            asked: Default::default(),
            reply: ToolResult {
                outcome,
                content: content.into(),
            },
        }
    }
}

impl crate::search::Searcher for FakeSearch {
    fn search(&self, request: &crate::search::Request) -> ToolResult {
        self.asked.borrow_mut().push(crate::search::Request {
            query: request.query.clone(),
            source: request.source.clone(),
            language: request.language.clone(),
            limit: request.limit,
        });
        ToolResult {
            outcome: self.reply.outcome,
            content: self.reply.content.clone(),
        }
    }
}

fn search_call(args: &[(&str, &str)]) -> String {
    let params: String = args
        .iter()
        .map(|(name, value)| format!("<parameter={name}>\n{value}\n</parameter>\n"))
        .collect();
    format!("<tool_call>\n<function=search>\n{params}</function>\n</tool_call>")
}

/// The results of the events, with their outcomes.
fn results(session: &Session) -> Vec<(Outcome, String)> {
    session
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Result { outcome, content, .. } => Some((*outcome, content.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn search_runs_in_the_agent_and_its_text_is_the_result() {
    let dir = Dir::new("search");
    let script = Script::new(&[
        ("Look it up.", THOUGHT),
        (&search_call(&[("language", "uk"), ("limit", "3"), ("query", "Київ"), ("source", "wikipedia")]), Stop::End),
        ("Done.", THOUGHT),
        ("It is a city.", Stop::End),
    ]);
    let search = FakeSearch::new(Outcome::Ok, "1. Київ\n   https://x.example/");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let end = agent_searching(&minimal, &script, &mut session, &dir, None, &search, false)
        .turn("what is Kyiv", &mut Log::default());
    assert!(matches!(end, Ok(TurnEnd::Answer(_))), "{end:?}");
    assert_eq!(
        *search.asked.borrow(),
        vec![crate::search::Request {
            query: "Київ".into(),
            source: "wikipedia".into(),
            language: "uk".into(),
            limit: 3,
        }]
    );
    assert_eq!(
        results(&session),
        vec![(Outcome::Ok, "1. Київ\n   https://x.example/".to_owned())]
    );
}

#[test]
fn a_failed_search_is_a_result_the_model_reads_not_a_stop() {
    let dir = Dir::new("searchfail");
    let script = Script::new(&[
        ("Try.", THOUGHT),
        (&search_call(&[("query", "rust"), ("source", "web")]), Stop::End),
        ("Hm.", THOUGHT),
        // Longer than 512 characters: the check of the arguments refuses it.
        (&search_call(&[("query", &"a".repeat(513)), ("source", "web")]), Stop::End),
        ("Hm.", THOUGHT),
        ("I could not search.", Stop::End),
    ]);
    let search = FakeSearch::new(Outcome::Fail, "The search service is rate limited; try again later.");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let end = agent_searching(&minimal, &script, &mut session, &dir, None, &search, false)
        .turn("search", &mut Log::default());
    assert!(matches!(end, Ok(TurnEnd::Answer(_))), "{end:?}");
    let results = results(&session);
    assert_eq!(results[0], (Outcome::Fail, "The search service is rate limited; try again later.".to_owned()));
    assert_eq!(results[1].0, Outcome::Fail);
    assert!(results[1].1.contains("512"), "{}", results[1].1);
    // The service was asked once: the second call never reached it.
    assert_eq!(search.asked.borrow().len(), 1);
}

#[test]
fn without_search_the_tool_is_not_in_the_list_nor_the_grammar() {
    let mut minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let names = |function: &Function| -> Vec<&'static str> {
        function_tools(function).iter().map(|doc| doc.name).collect()
    };
    assert!(names(&minimal).contains(&"search"));
    minimal.without_tool("search");
    assert!(!names(&minimal).contains(&"search"));
    assert!(names(&minimal).contains(&"bash"));
    let state = &minimal.states[minimal.initial];
    let (grammar, root) = action::grammar(&minimal.format, &state.actions);
    let text = grammar.render(&root);
    assert!(!text.contains("search"), "{text}");
    assert!(text.contains("bash"));
}

/// Whisper that is not there.
struct NoWhisper;

impl crate::attach::Transcriber for NoWhisper {
    fn transcribe(&self, _: &media::Source, _: &mut dyn FnMut(&str)) -> Result<String, Error> {
        Err(Error::EngineNotFound { engine: "whisper-server".into() })
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
            Event::Compact { .. } => "compact",
            Event::CutOff(_) => "cut_off",
            Event::Media(_) => "media",
            Event::Memory { .. } => "memory",
            Event::Stir { .. } => "stir",
        })
        .collect()
}

#[test]
fn a_call_then_an_answer() {
    let dir = Dir::new("answer");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("I will list files", THOUGHT),
        (&bash_call("echo hello"), Stop::End),
        ("Done.", THOUGHT),
        ("It printed hello.", Stop::End),
    ]);
    let (end, log, session) = run(&minimal, &script, &dir, "Say hello");
    assert_eq!(end.unwrap(), TurnEnd::Answer("It printed hello.".into()));
    assert_eq!(
        log,
        [
            "[inference]",
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
fn the_off_level_silences_the_thinking() {
    let function = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    // A level keeps the thinking: two generations.
    let dir = Dir::new("think-level");
    let script = Script::new(&[("I will answer", THOUGHT), ("Answer.", Stop::End)]);
    let (end, log, session) = run_level(&function, &script, &dir, "hi", Some(ReasoningLevel::Low));
    assert_eq!(end.unwrap(), TurnEnd::Answer("Answer.".into()));
    assert!(log.iter().any(|line| line.starts_with("thinking: ")));
    assert_eq!(kinds(&session), ["user", "state", "thinking", "answer"]);
    // `Off` silences it: the one action generation, no thinking.
    let dir = Dir::new("off");
    let script = Script::new(&[("Answer.", Stop::End)]);
    let (end, log, session) = run_level(&function, &script, &dir, "hi", Some(ReasoningLevel::Off));
    assert_eq!(end.unwrap(), TurnEnd::Answer("Answer.".into()));
    assert!(!log.iter().any(|line| line.starts_with("thinking: ")));
    assert_eq!(kinds(&session), ["user", "state", "answer"]);
}

#[test]
fn a_state_without_thinking_is_rendered_at_the_off_level() {
    // The llama mode renders a state that does not think with `Off`,
    // whatever the run's level, as the stirred mode does (AGL-043 F16).
    let quiet = function(
        r#"{ system = "s"; initial = "a";
             state.a = { tools.bash = { }; think = false; answer = true; next.otherwise = "a"; }; }"#,
    )
    .unwrap();
    let dir = Dir::new("quiet-state");
    let script = Script::new(&[("Answer.", Stop::End)]);
    let (end, _, _) = run_level(&quiet, &script, &dir, "hi", Some(ReasoningLevel::Low));
    assert_eq!(end.unwrap(), TurnEnd::Answer("Answer.".into()));
    assert_eq!(*script.reasonings.borrow(), [Some(ReasoningLevel::Off)]);
    // A thinking state keeps the run's level.
    let thinking = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let dir = Dir::new("thinking-state");
    let script = Script::new(&[("I will answer", THOUGHT), ("Answer.", Stop::End)]);
    run_level(&thinking, &script, &dir, "hi", Some(ReasoningLevel::Low)).0.unwrap();
    assert_eq!(script.reasonings.borrow()[0], Some(ReasoningLevel::Low));
}

#[test]
fn every_generation_reports_its_decoded_tokens() {
    let dir = Dir::new("decoded");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let outputs: [(&str, Stop); 4] = [
        ("I will list files", THOUGHT),
        (&bash_call("echo hello"), Stop::End),
        ("Done.", THOUGHT),
        ("It printed hello.", Stop::End),
    ];
    let script = Script::new(&outputs);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    agent(&minimal, &script, &mut session, &dir, None)
        .turn("Say hello", &mut log)
        .unwrap();
    // A generation counts as its characters, in the order decoded.
    assert_eq!(
        log.2,
        outputs
            .iter()
            .map(|(text, _)| text.chars().count())
            .collect::<Vec<_>>()
    );
}

#[test]
fn states_and_instructions() {
    let dir = Dir::new("states");
    // Three steps: the edit, the failed test and the discarded action.
    let coder = function(&CODER.replace("max_steps_total = 50;", "max_steps_total = 3;")).unwrap();
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
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (
            "<tool_call>\n<function=bash>\n<parameter=command>\ncat <<EOF",
            Stop::Limit,
        ),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ]);
    let (end, log, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        kinds(&session),
        ["user", "state", "thinking", "cut_off", "thinking", "answer"]
    );
    // The discarded tokens (the test counts a generation's characters) and
    // where they were heading (AGL-043 F8).
    let discarded = "<tool_call>\n<function=bash>\n<parameter=command>\ncat <<EOF".chars().count();
    assert_eq!(
        session.events[3],
        Event::CutOff(CutOff {
            // An eighth of the test's 16384 context (AGL-043 F14).
            limit: 2048,
            thinking: None,
            discarded: discarded as u32,
            call: true,
        })
    );
    assert!(log.contains(&"[cut off]".to_string()));
    // The discarded text is not kept; the model reads the notice instead.
    let history = session.history();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history[1],
        Message::Runtime(
            "Your last action hit max_action_tokens (2048) and was discarded; nothing was \
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
        r#"{ system = "s"; max_steps_total = 1; initial = "a";
             state.a = { tools.bash = { }; think = false; answer = true; next.otherwise = "a"; }; }"#,
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
        [
            Event::CutOff(CutOff {
                limit: 2048,
                thinking: None,
                discarded: "<tool_call>\n<function=bash>\n<parameter=command>\nyes"
                    .chars()
                    .count() as u32,
                call: true,
            }),
            Event::Stop(StopReason::StepLimit)
        ]
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

/// A structured generation (AGL-030, D9): the thinking and the text the
/// server split, the calls it structured and the exact usage.
fn structured_step(
    thinking: Option<&str>,
    text: &str,
    calls: &[(&str, &str)],
    stop: Stop,
) -> Generated {
    Generated {
        text: text.into(),
        stop,
        thinking: thinking.map(str::to_owned),
        tool_calls: calls
            .iter()
            .map(|(name, arguments)| StructuredCall {
                name: (*name).into(),
                arguments: (*arguments).into(),
            })
            .collect(),
        prompt_tokens: Some(312),
        completion_tokens: Some(20),
        prefill: None,
    }
}

/// A one-state Function with `policy` and `model` attributes added; the
/// stirs are the model's (AGL-039, D2).
fn stirred_function(policy: &str, model: &str) -> Function {
    function_on(
        &format!(
            r#"{{ system = "s"; initial = "a"; {policy}
                 state.a = {{ tools.bash = {{ }}; think = false; answer = true; next.otherwise = "a"; }}; }}"#
        ),
        &format!(r#"{{ format = agl.formats."qwen3.8"; weights = "m.gguf"; {model} }}"#),
    )
    .unwrap()
}

/// The stirred mode runs a step as one structured generation (AGL-030,
/// D9, D10): a call of the state's tool runs, an answer ends the turn,
/// the thinking the server split is recorded and streamed, and the exact
/// usage is what the report is told (D11).
#[test]
fn stirred_runs_a_structured_call_and_answers() {
    let dir = Dir::new("stir-call");
    let thinking = function(
        r#"{ system = "s"; initial = "a"; state.a = { tools.bash = { }; answer = true; next.otherwise = "a"; }; }"#,
    )
    .unwrap();
    let script = Script::structured(vec![
        structured_step(
            Some("Run it."),
            "Looking.",
            &[("bash", "{\"command\":\"echo hi\"}")],
            Stop::End,
        ),
        structured_step(None, "done", &[], Stop::End),
    ]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let end = agent_stirred(&thinking, &script, &mut session, &dir, None, true)
        .turn("go", &mut log)
        .unwrap();
    assert_eq!(end, TurnEnd::Answer("done".into()));
    assert!(log.0.contains(&"thinking: Run it.".to_string()), "{:?}", log.0);
    assert!(log.0.contains(&"> bash".to_string()));
    assert!(log.0.contains(&"  ok".to_string()));
    // The estimate before each generation (the context check), then the
    // exact count of the usage.
    assert_eq!(log.1, vec![100, 312, 100, 312]);
    assert_eq!(log.2, vec![20, 20]);
    assert!(session
        .events
        .iter()
        .any(|event| matches!(event, Event::Thinking(text) if text == "Run it.")));
    assert!(session.events.iter().any(|event| matches!(
        event,
        Event::Call { text: Some(text), call } if text == "Looking." && call.name == "bash"
    )));
    // Both generations were structured, the step's own prompt only.
    assert_eq!(script.structured.borrow().len(), 2);
}

/// A malformed structured output in the stirred mode: the step is stirred
/// a bounded number of times — an unknown tool, a missing parameter, a
/// bad value — and the turn completes when it lands (AGL-030, D4, D10).
#[test]
fn stirred_retries_a_malformed_call() {
    let dir = Dir::new("stir-retry");
    let stirred = stirred_function("", "");
    let script = Script::structured(vec![
        structured_step(None, "", &[("nope", "{}")], Stop::End),
        structured_step(None, "", &[("bash", "{}")], Stop::End),
        structured_step(None, "done", &[], Stop::End),
    ]);
    let (end, lines, session) = run_stirred(&stirred, &script, &dir, "go", None, true);
    assert_eq!(end.unwrap(), TurnEnd::Answer("done".into()));
    // Each stir is reported with its number of the most (AGL-043 F8).
    assert!(lines.contains(&"[stir 1/2: `nope` is not a tool of this state]".to_string()), "{lines:?}");
    assert!(lines.contains(&"[stir 2/2: `bash` needs the parameter `command`]".to_string()));
    let errors: Vec<&str> = session
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Stir { error, .. } => Some(error.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        [
            "`nope` is not a tool of this state",
            "`bash` needs the parameter `command`"
        ]
    );
    // The nudge rides in as a runtime message of the retry, and it names
    // the state and what was wrong; the first attempt has none.
    let runtimes = script.structured.borrow();
    assert!(runtimes[0].is_empty());
    assert_eq!(runtimes[1].len(), 1);
    assert!(runtimes[1][0].contains("`a` state"));
    assert!(runtimes[1][0].contains("`nope` is not a tool of this state"));
    assert_eq!(runtimes[2].len(), 2);
}

/// A model that never lands in the stirred mode: after `max_stirs` the
/// turn stops with `StirLimit`, no more stirs (AGL-030, D4, D5).
#[test]
fn stirred_stops_at_the_limit() {
    let dir = Dir::new("stir-limit");
    let stirred = stirred_function("", "max_stirs = 2;");
    let bad = || structured_step(None, "", &[("nope", "{}")], Stop::End);
    let script = Script::structured(vec![bad(), bad(), bad()]);
    let (end, _, session) = run_stirred(&stirred, &script, &dir, "go", None, true);
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::StirLimit));
    let stirs = session
        .events
        .iter()
        .filter(|event| matches!(event, Event::Stir { .. }))
        .count();
    assert_eq!(stirs, 2);
    assert_eq!(
        session.events.last(),
        Some(&Event::Stop(StopReason::StirLimit))
    );
}

/// A step that hits its budget in the stirred mode — also in the middle of
/// a call's arguments — is a cut-off, not a stir (AGL-030, D10, D154).
#[test]
fn stirred_cut_off_is_not_a_stir() {
    let dir = Dir::new("stir-cut");
    let stirred = stirred_function("max_steps_total = 1;", "");
    let script = Script::structured(vec![structured_step(
        None,
        "",
        &[("bash", "{\"comm")],
        Stop::Limit,
    )]);
    let (end, lines, session) = run_stirred(&stirred, &script, &dir, "go", None, true);
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::StepLimit));
    assert!(lines.contains(&"[cut off]".to_string()), "{lines:?}");
    assert!(!session
        .events
        .iter()
        .any(|event| matches!(event, Event::Stir { .. })));
    // A state without thinking: the budget is the action's, and the
    // discarded output was heading for a call (AGL-043 F8).
    assert!(session.events.iter().any(|event| matches!(
        event,
        Event::CutOff(CutOff { limit: 2048, thinking: None, call: true, .. })
    )), "{:?}", session.events);

    // A thinking state: the budget is the thinking's and the action's
    // together, and the cut-off says what the thinking took of it.
    let thinking = function_on(
        r#"{ system = "s"; initial = "a"; max_steps_total = 1;
             state.a = { tools.bash = { }; answer = true; next.otherwise = "a"; }; }"#,
        r#"{ format = agl.formats."qwen3.8"; weights = "m.gguf"; }"#,
    )
    .unwrap();
    let dir = Dir::new("stir-cut-thinking");
    let script = Script::structured(vec![structured_step(
        Some("a long thought"),
        "",
        &[("bash", "{\"comm")],
        Stop::Limit,
    )]);
    let (_, _, session) = run_stirred(&thinking, &script, &dir, "go", None, true);
    let limit = thinking.action_tokens(16_384) + thinking.max_think_tokens;
    assert!(session.events.iter().any(|event| matches!(
        event,
        Event::CutOff(CutOff { limit: l, thinking: Some(_), call: true, .. }) if *l == limit
    )), "{:?}", session.events);
}

/// The structured mapping (AGL-030, D10): an answer, a call with its
/// arguments in the tool's order, the types and the allowed values.
#[test]
fn structured_output_maps_to_an_action() {
    let actions = crate::action::Actions {
        tools: crate::action::builtin_tools(),
        answer: true,
    };
    let call = |name: &str, arguments: &str| StructuredCall {
        name: name.into(),
        arguments: arguments.into(),
    };
    let map = |content: &str, calls: &[StructuredCall]| {
        crate::action::from_structured(&actions, content, calls)
    };
    assert_eq!(
        map("  hi \n", &[]).unwrap(),
        Action::Answer("hi".into())
    );
    assert_eq!(
        map("Now.", &[call("bash", "{\"timeout\": 5, \"command\": \"ls\"}")]).unwrap(),
        Action::Call {
            text: Some("Now.".into()),
            tool: "bash".into(),
            args: vec![
                ("command".into(), Value::Text("ls".into())),
                ("timeout".into(), Value::Int(5)),
            ],
        }
    );
    // An integer given as a string parses; the range is checked.
    assert!(matches!(
        map("", &[call("bash", "{\"command\":\"ls\",\"timeout\":\"7\"}")]).unwrap(),
        Action::Call { args, .. } if args[1].1 == Value::Int(7)
    ));
    for (calls, content) in [
        (vec![call("bash", "{\"command\":\"ls\",\"timeout\":9999}")], ""),
        (vec![call("bash", "{\"command\":\"ls\",\"extra\":1}")], ""),
        (vec![call("bash", "{\"command\":")], ""),
        (vec![call("bash", "[1]")], ""),
        (vec![call("bash", "{\"command\":\"ls\",\"timeout\":\"soon\"}")], ""),
        (vec![call("nope", "{}")], ""),
        (vec![], ""),
    ] {
        assert!(map(content, &calls).is_err(), "{calls:?}");
    }
    // A state that takes no answer refuses a text without a call.
    let only_tools = crate::action::Actions {
        answer: false,
        ..actions.clone()
    };
    assert!(crate::action::from_structured(&only_tools, "hi", &[]).is_err());
}

#[test]
fn thinking_at_the_limit_is_closed() {
    let dir = Dir::new("limit");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[("a long thought ", Stop::Limit), ("ok", Stop::End)]);
    let (end, _, _) = run(&minimal, &script, &dir, "hi");
    assert_eq!(end.unwrap(), TurnEnd::Answer("ok".into()));
    assert_eq!(
        script.prompts.borrow()[1],
        "[2 messages; -]<think>\na long thought\n</think>\n\n"
    );
}

#[test]
fn only_function_tools_are_listed() {
    let answer_only =
        function(r#"{ system = "s"; initial = "a"; state.a.answer = true; }"#)
            .unwrap();
    assert!(function_tools(&answer_only).is_empty());
    let coder = function(CODER).unwrap();
    let names: Vec<&str> = function_tools(&coder).iter().map(|d| d.name).collect();
    // The implicit forge (AGL-011) is a tool of the tool states.
    assert_eq!(names, ["bash", "edit", "forge"]);
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
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t", THOUGHT),
        (&format!("I will run it.\n{}", bash_call("true")), Stop::End),
        ("t", THOUGHT),
        ("Done.", Stop::End),
    ]);
    let (end, log, _) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        log,
        [
            "[inference]",
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
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS]);
    let (end, log, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        log,
        [
            "[inference]",
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
    // The summary keeps the question that started the turn (F31).
    assert_eq!(
        session.events[5],
        Event::Compact {
            summary: "The user said go; true ran.".into(),
            question: Some("go".into()),
        }
    );
    let prompts = script.prompts.borrow();
    // The summary is asked for after the history, without thinking.
    assert!(prompts[2].starts_with("[4 messages; Summarize the conversation so far"));
    assert!(prompts[2].ends_with("<think>\n\n</think>\n\n"));
    // The renders: two steps, the summary, the count of the history after
    // it (AGL-043 F7), a step.
    assert_eq!(*script.thinking.borrow(), [false, true, false, false, false]);
    // The turn goes on from the summary and the kept question (F31).
    assert!(prompts[3].starts_with("[3 messages; -]"));
    // The summary is a runtime message, not the human's (AGL-009), and the
    // kept question follows it (F31).
    assert_eq!(
        session.history()[0],
        Message::Runtime("The user said go; true ran.".into())
    );
    assert_eq!(session.history()[1], Message::User("go".into()));
}

#[test]
fn the_system_message_is_the_functions_system_and_nothing_more() {
    let dir = Dir::new("system");
    let agentlibre =
        Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS]);
    let (end, _, _) = run(&agentlibre, &script, &dir, "go");
    assert!(end.is_ok());
    // A step, a step, the compaction, the count after it (AGL-043 F7), a
    // step: all carry the Function's `system`, the same text each time
    // (AGL-017, D4, D8): the persona and the runtime part of `agentLIBRE`,
    // no paragraph about the forge.
    let systems = script.systems.borrow();
    assert_eq!(systems.len(), 5);
    // The action's limit is told at the context, an eighth of the test's
    // 16384 (AGL-043 F14).
    let told = agentlibre.system_at(16_384);
    assert!(systems.iter().all(|system| *system == told));
    assert!(told.contains("Messages that start with [agentLIBRE runtime]"));
    assert!(told.contains("at most 2048 tokens"));
    assert!(!told.contains("forge"));
}

#[test]
fn the_agentlibre_function_sends_the_persona_and_the_runtime_part() {
    let dir = Dir::new("agentLIBRE");
    let agentlibre =
        Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high").unwrap();
    let script = Script::new(&[("t1", THOUGHT), ("Hello.", Stop::End)]);
    let (end, _, _) = run(&agentlibre, &script, &dir, "hi");
    assert!(end.is_ok());
    let systems = script.systems.borrow();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0], agentlibre.system_at(16_384));
    assert!(systems[0].starts_with("You are DinoBot"));
    assert!(systems[0].contains("Messages that start with [agentLIBRE runtime]"));
}

#[test]
fn the_minimal_function_sends_the_one_line_system() {
    let dir = Dir::new("minimal");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[("t1", THOUGHT), ("Hello.", Stop::End)]);
    let (end, _, _) = run(&minimal, &script, &dir, "hi");
    assert!(end.is_ok());
    let systems = script.systems.borrow();
    assert_eq!(systems.len(), 1);
    assert_eq!(
        systems[0],
        "You are an AI agent running on agentLIBRE (agl), an agentic system."
    );
}

#[test]
fn the_prompt_size_of_every_step_is_reported() {
    let dir = Dir::new("used");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
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
    agent(&minimal, &script, &mut session, &dir, None)
        .turn("go", &mut log)
        .unwrap();
    // The summary prompt (the third count) is not a step; the history
    // after the summary is reported at once (the fourth, AGL-043 F7), then
    // the next step's prompt (the default count).
    assert_eq!(log.1, [100, OVERFLOW, 200, 100]);
}

#[test]
fn a_cut_summary_is_asked_again_shorter_then_kept_with_a_note() {
    // AGL-043 F6: the summary reached its budget (4096 at the test's
    // 16384 context): one more request, shorter; that one fits.
    let dir = Dir::new("shorter");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("a long summary", Stop::Limit),
        ("Shorter.", Stop::End),
        ("t", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[OVERFLOW, FITS, FITS]);
    let (end, _, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert!(session.events.iter().any(|event| matches!(
        event,
        Event::Compact { summary, .. } if summary == "Shorter."
    )));
    let prompts = script.prompts.borrow();
    assert!(!prompts[0].contains("Write it shorter"));
    assert!(prompts[1].contains("Write it shorter: the summary must fit in 4096 tokens."));
    drop(prompts);

    // Cut again: it is kept, and the model is told it is incomplete.
    let script = Script::new(&[
        ("a long summary", Stop::Limit),
        ("Still long", Stop::Limit),
        ("t", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[OVERFLOW, FITS, FITS]);
    let (end, _, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        session.history()[0],
        Message::Runtime(
            "Still long\n\n(This summary was cut at its limit of 4096 tokens and is incomplete.)"
                .into()
        )
    );
}

#[test]
fn a_context_that_compaction_cannot_free_stops_the_turn() {
    let dir = Dir::new("no-room");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    // The compaction prompt itself does not fit.
    let script = Script::new(&[]).with_counts(&[OVERFLOW, 15_000]);
    let (end, log, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
    assert_eq!(log, ["[inference]", "compacting"]);
    assert_eq!(kinds(&session), ["user", "state", "stop"]);

    // An empty summary is dropped, also when the shorter retry of a cut
    // one is empty (AGL-043 F6).
    for summaries in [&[(" \n", Stop::End)][..], &[("a long summary", Stop::Limit), (" ", Stop::End)]] {
        let script = Script::new(summaries).with_counts(&[OVERFLOW, FITS, FITS]);
        let (end, _, session) = run(&minimal, &script, &dir, "go");
        assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
        assert_eq!(kinds(&session), ["user", "state", "stop"]);
    }

    // Still too long after a summary: nothing is left to compact.
    // The counts: the step, the summary request, the history after the
    // summary (AGL-043 F7), the next step.
    let script =
        Script::new(&[("s", Stop::End)]).with_counts(&[OVERFLOW, FITS, OVERFLOW, OVERFLOW]);
    let (end, _, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Stopped(StopReason::ContextFull));
    assert_eq!(kinds(&session), ["user", "state", "compact", "stop"]);
}

#[test]
fn a_summary_request_over_the_context_is_cut_to_fit() {
    // AGL-038, D5: the summary request of 15000 tokens does not fit with
    // its 4096; the long tool result is cut to its head and tail, and the
    // second count fits.
    let dir = Dir::new("cut-result");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("head -c 5000 /dev/zero | tr '\\0' x"), Stop::End),
        (" A long output. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, 15_000, FITS]);
    let (end, log, session) = run(&minimal, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    assert!(log.contains(&"[compacted]".to_string()), "{log:?}");
    assert_eq!(
        session.events[5],
        Event::Compact {
            summary: "A long output.".into(),
            question: Some("go".into()),
        }
    );
    let users = script.users.borrow();
    // The first summary request has the whole result, the second the cut.
    assert!(users[2].contains(&"x".repeat(5000)));
    assert!(users[3].contains("characters cut for the summary]"));
    assert!(!users[3].contains(&"x".repeat(2000)));
}

#[test]
fn a_summary_request_without_results_to_cut_drops_the_oldest_turn() {
    // AGL-038, D5: no tool result to cut — the first turn goes, the latest
    // is kept.
    let dir = Dir::new("cut-turn");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t", THOUGHT),
        ("Hi.", Stop::End),
        (" Greeted, then go. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, 15_000, FITS]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    agent(&minimal, &script, &mut session, &dir, None)
        .turn("hello", &mut log)
        .unwrap();
    let end = agent(&minimal, &script, &mut session, &dir, None).turn("go", &mut log);
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done.".into()));
    let users = script.users.borrow();
    assert_eq!(users[2], "hello\ngo");
    assert_eq!(users[3], "go");
    // The latest turn alone that does not fit is still impossible (the
    // test `a_context_that_compaction_cannot_free_stops_the_turn`).
}

#[test]
fn the_early_compaction_keeps_room_for_the_summary() {
    // AGL-038, D6, D9: at a 65536 context the reserve is 32768 (the
    // action's eighth 8192 + 4096 + 16384 + the summary's 4096, AGL-043
    // F6, F14); a history of 40000 compacts early with the setting on, and
    // runs the step with it off.
    let dir = Dir::new("early");
    let mut early = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    assert!(!early.compact_early);
    assert_eq!(super::early_reserve(&early, 65_536), 32_768);
    early.compact_early = true;
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" Early. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, 40_000, FITS, 200]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let mut on = agent(&early, &script, &mut session, &dir, None);
    on.context = 65_536;
    assert_eq!(on.turn("go", &mut log).unwrap(), TurnEnd::Answer("Done.".into()));
    assert_eq!(
        kinds(&session),
        ["user", "state", "thinking", "call", "result", "compact", "thinking", "answer"]
    );
    let late = Function { compact_early: false, ..early };
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, 40_000]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut off = agent(&late, &script, &mut session, &dir, None);
    off.context = 65_536;
    assert_eq!(off.turn("go", &mut log).unwrap(), TurnEnd::Answer("Done.".into()));
    assert!(!kinds(&session).contains(&"compact"));
}

#[test]
fn compaction_on_request() {
    let dir = Dir::new("on-request");
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[("t", THOUGHT), ("Hi.", Stop::End), ("Greeted.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    assert_eq!(
        agent(&minimal, &script, &mut session, &dir, None)
            .compact(&mut log)
            .unwrap(),
        Compaction::Nothing
    );
    agent(&minimal, &script, &mut session, &dir, None)
        .turn("hello", &mut log)
        .unwrap();
    let reported = log.1.len();
    assert_eq!(
        agent(&minimal, &script, &mut session, &dir, None)
            .compact(&mut log)
            .unwrap(),
        Compaction::Done
    );
    // The compaction on request reports the context it leaves, with no
    // step after it (AGL-043 F7).
    assert_eq!(log.1.len(), reported + 1);
    assert_eq!(
        session.events.last(),
        Some(&Event::Compact {
            summary: "Greeted.".into(),
            question: Some("hello".into()),
        })
    );
    assert_eq!(
        agent(&minimal, &script, &mut session, &dir, None)
            .compact(&mut log)
            .unwrap(),
        Compaction::Nothing
    );
}

fn source(kind: media::Kind, bytes: &[u8], ext: &str) -> media::Source {
    media::Source {
        kind,
        bytes: bytes.to_vec(),
        ext: ext.into(),
        name: format!("f.{ext}"),
    }
}

/// Runs one turn of `message` in `session` against `script`.
fn turn_in(
    session: &mut Session,
    script: &Script,
    dir: &Dir,
    message: &str,
) -> Result<TurnEnd, Error> {
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    agent(&minimal, script, session, dir, None).turn(message, &mut Log::default())
}

#[test]
fn an_image_becomes_the_marker_and_goes_with_the_request() {
    let dir = Dir::new("image");
    let script = Script::new(&[("I see it.", THOUGHT), ("A red square.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Image, b"IMG", "png"), 1, None)
        .unwrap();
    let end = turn_in(&mut session, &script, &dir, "look at [Image#1] and [Image#9]");
    assert!(matches!(end, Ok(TurnEnd::Answer(_))));
    // The known placeholder is the marker, the unknown one stays text, and
    // the session still has the placeholder (D6).
    assert_eq!(
        script.users.borrow()[0],
        "look at <__media_TEST__> and [Image#9]"
    );
    assert!(matches!(&session.events[1], Event::User(text) if text == "look at [Image#1] and [Image#9]"));
    // Every generation of the step carries the bytes, base64.
    let uploads = script.uploads.borrow();
    assert!(uploads.iter().all(|u| *u == [media::base64(b"IMG")]));
    assert_eq!(uploads.len(), 2);
}

#[test]
fn a_picture_is_counted_by_its_bound() {
    let dir = Dir::new("bound");
    let script = Script::new(&[("Hm.", THOUGHT), ("Fine.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Image, b"IMG", "png"), 1, None)
        .unwrap();
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let mut log = Log::default();
    agent(&minimal, &script, &mut session, &dir, None)
        .turn("see [Image#1]", &mut log)
        .unwrap();
    // 100 tokens of text, 1000 for the image (`image_tokens`, D10).
    assert_eq!(log.1[0], 1100);
    // Without media the count is the text's.
    let script = Script::new(&[("Hm.", THOUGHT), ("Fine.", Stop::End)]);
    let mut other = Session::create(&dir.0.join("sessions2"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    agent(&minimal, &script, &mut other, &dir, None)
        .turn("no media", &mut log)
        .unwrap();
    assert_eq!(log.1[0], 100);
}

#[test]
fn a_bound_that_does_not_fit_stops_the_turn() {
    let dir = Dir::new("toolarge");
    let script = Script::new(&[("Hm.", THOUGHT)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    for number in 1..=20 {
        session
            .store_media(&source(media::Kind::Image, &[number as u8], "png"), number, None)
            .unwrap();
    }
    let all: Vec<String> = (1..=20).map(|n| format!("[Image#{n}]")).collect();
    let end = turn_in(&mut session, &script, &dir, &all.join(" ")).unwrap();
    assert!(matches!(end, TurnEnd::Stopped(StopReason::ContextFull)));
}

#[test]
fn a_video_is_its_frames_each_after_its_time() {
    let dir = Dir::new("video");
    let script = Script::new(&[("Hm.", THOUGHT), ("A clip.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let media_dir = session.media_dir();
    let first = media::put(&media_dir, b"F0", "jpg").unwrap();
    let second = media::put(&media_dir, b"F1", "jpg").unwrap();
    let clip = media::put(&media_dir, b"CLIP", "mp4").unwrap();
    session
        .append(Event::Media(media::Media {
            kind: media::Kind::Video,
            number: 1,
            sha256: clip,
            ext: "mp4".into(),
            name: "c.mp4".into(),
            transcript: None,
            frames: vec![
                media::Frame { at: 0.0, sha256: first },
                media::Frame { at: 61.0, sha256: second },
            ],
        }))
        .unwrap();
    turn_in(&mut session, &script, &dir, "what is in [Video#1]?").unwrap();
    assert_eq!(
        script.users.borrow()[0],
        "what is in [Video#1: frames 0:00 <__media_TEST__> 1:01 <__media_TEST__>]?"
    );
    assert_eq!(
        script.uploads.borrow()[0],
        [media::base64(b"F0"), media::base64(b"F1")]
    );
}

#[test]
fn an_audio_is_its_transcript_or_the_audio_itself_when_the_model_hears() {
    // With a transcript: text, nothing uploaded (D22).
    let dir = Dir::new("audio");
    let script = Script::new(&[("Hm.", THOUGHT), ("Ok.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Audio, b"WAV", "wav"), 1, Some("hello there".into()))
        .unwrap();
    turn_in(&mut session, &script, &dir, "[Audio#1]").unwrap();
    assert_eq!(script.users.borrow()[0], "[Audio#1: \"hello there\"]");
    assert!(script.uploads.borrow()[0].is_empty());
    // Without one, and the model hears: the marker and the audio (D19).
    let mut script = Script::new(&[("Hm.", THOUGHT), ("Ok.", Stop::End)]);
    script.hears = true;
    let mut session = Session::create(&dir.0.join("s2"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Audio, b"WAV", "wav"), 1, None)
        .unwrap();
    turn_in(&mut session, &script, &dir, "[Audio#1]").unwrap();
    assert_eq!(script.users.borrow()[0], "<__media_TEST__>");
    assert_eq!(script.uploads.borrow()[0], [media::base64(b"WAV")]);
    // Without a transcript and a model that does not hear: said so.
    let script = Script::new(&[("Hm.", THOUGHT), ("Ok.", Stop::End)]);
    let mut session = Session::create(&dir.0.join("s3"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Audio, b"WAV", "wav"), 1, None)
        .unwrap();
    turn_in(&mut session, &script, &dir, "[Audio#1]").unwrap();
    assert_eq!(script.users.borrow()[0], "[Audio#1: not transcribed]");
}

#[test]
fn an_image_without_a_projector_is_an_error_that_says_so() {
    let dir = Dir::new("noproj");
    let mut script = Script::new(&[]);
    script.marker = None;
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    session
        .store_media(&source(media::Kind::Image, b"IMG", "png"), 1, None)
        .unwrap();
    let end = turn_in(&mut session, &script, &dir, "[Image#1]");
    assert!(matches!(end, Err(Error::NoMultimodal)));
    // A text without media never asks for the marker.
    let mut script = Script::new(&[("Hm.", THOUGHT), ("Ok.", Stop::End)]);
    script.marker = None;
    let mut session = Session::create(&dir.0.join("s2"), "f", &dir.0).unwrap();
    assert!(turn_in(&mut session, &script, &dir, "plain").is_ok());
}

#[test]
fn a_changed_copy_of_a_medium_stops_the_turn_with_its_name() {
    let dir = Dir::new("changed");
    let script = Script::new(&[]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let kept = session
        .store_media(&source(media::Kind::Image, b"IMG", "png"), 1, None)
        .unwrap();
    fs::write(kept.path(&session.media_dir()), b"other").unwrap();
    let end = turn_in(&mut session, &script, &dir, "[Image#1]");
    assert!(matches!(end, Err(Error::MediaChanged { .. })));
}

fn view_call(path: &str) -> String {
    format!(
        "<tool_call>\n<function=view>\n<parameter=path>\n{path}\n</parameter>\n</function>\n</tool_call>"
    )
}

#[test]
fn view_keeps_the_file_and_the_next_step_sees_it() {
    let dir = Dir::new("view");
    fs::write(dir.0.join("shot.png"), b"SHOT").unwrap();
    let script = Script::new(&[
        ("Let me look.", THOUGHT),
        (&view_call("shot.png"), Stop::End),
        ("It is a screenshot.", THOUGHT),
        ("A screenshot.", Stop::End),
    ]);
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let (end, log, session) = run(&minimal, &script, &dir, "what is in shot.png?");
    assert!(matches!(end, Ok(TurnEnd::Answer(_))), "{log:?}");
    // The call, the medium, then the result, in the log of the session.
    assert_eq!(
        kinds(&session),
        ["user", "state", "thinking", "call", "media", "result", "thinking", "answer"]
    );
    let Event::Result { content, outcome, .. } = &session.events[5] else {
        panic!("not a result");
    };
    assert_eq!(*outcome, Outcome::Ok);
    assert_eq!(content, "[Image#1] shot.png");
    // The first step had no media; the second has the result as the marker
    // with the picture's bytes.
    let uploads = script.uploads.borrow();
    assert!(uploads[0].is_empty() && uploads[1].is_empty());
    assert_eq!(*uploads.last().unwrap(), [media::base64(b"SHOT")]);
    assert!(script.users.borrow().last().unwrap().contains("<__media_TEST__> shot.png"));
}

#[test]
fn view_of_something_else_is_a_failed_call_the_model_reads() {
    let dir = Dir::new("viewbad");
    fs::write(dir.0.join("notes.txt"), b"x").unwrap();
    let script = Script::new(&[
        ("Look.", THOUGHT),
        (&view_call("notes.txt"), Stop::End),
        ("Hm.", THOUGHT),
        (&view_call("missing.png"), Stop::End),
        ("Hm.", THOUGHT),
        ("Cannot.", Stop::End),
    ]);
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let (end, _, session) = run(&minimal, &script, &dir, "see");
    assert!(matches!(end, Ok(TurnEnd::Answer(_))));
    let results: Vec<(Outcome, &str)> = session
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Result { outcome, content, .. } => Some((*outcome, content.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(results[0].0, Outcome::Fail);
    assert!(results[0].1.contains("not an image, a video or an audio"));
    assert_eq!(results[1].0, Outcome::Fail);
    assert!(results[1].1.contains("cannot read"));
    assert!(session.media().is_empty());
}

#[test]
fn view_of_an_image_without_a_projector_fails_before_the_file_is_kept() {
    let dir = Dir::new("viewnoproj");
    fs::write(dir.0.join("shot.png"), b"SHOT").unwrap();
    let mut script = Script::new(&[
        ("Look.", THOUGHT),
        (&view_call("shot.png"), Stop::End),
        ("Hm.", THOUGHT),
        ("No.", Stop::End),
    ]);
    script.marker = None;
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let (_, _, session) = run(&minimal, &script, &dir, "see");
    assert!(session.media().is_empty());
    assert!(matches!(
        session.events.iter().find(|e| matches!(e, Event::Result { .. })),
        Some(Event::Result { outcome: Outcome::Fail, content, .. }) if content.contains("no multimodal projector")
    ));
}

#[test]
fn view_of_an_audio_is_transcribed_when_the_model_does_not_hear() {
    struct Words;
    impl crate::attach::Transcriber for Words {
        fn transcribe(&self, _: &media::Source, _: &mut dyn FnMut(&str)) -> Result<String, Error> {
            Ok("good morning".into())
        }
    }
    let dir = Dir::new("viewaudio");
    fs::write(dir.0.join("v.wav"), b"WAV").unwrap();
    let script = Script::new(&[
        ("Listen.", THOUGHT),
        (&view_call("v.wav"), Stop::End),
        ("Ok.", THOUGHT),
        ("It says good morning.", Stop::End),
    ]);
    let minimal = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut agent = agent(&minimal, &script, &mut session, &dir, None);
    agent.whisper = &Words;
    let end = agent.turn("what is said in v.wav?", &mut Log::default());
    assert!(matches!(end, Ok(TurnEnd::Answer(_))));
    assert_eq!(session.media()[0].transcript.as_deref(), Some("good morning"));
    assert!(script.users.borrow().last().unwrap().contains("[Audio#1: \"good morning\"] v.wav"));
}

fn memory_call(fields: &[(&str, &str)]) -> String {
    let params: String = fields
        .iter()
        .map(|(name, value)| format!("<parameter={name}>\n{value}\n</parameter>\n"))
        .collect();
    format!("<tool_call>\n<function=memory>\n{params}</function>\n</tool_call>")
}

fn turn_with_memory(
    memory: &dyn crate::memory::Home,
    script: &Script,
    dir: &Dir,
    message: &str,
) -> (Result<TurnEnd, Error>, Log, Session) {
    let default = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let mut agent = agent(&default, script, &mut session, dir, None);
    agent.memory = memory;
    let end = agent.turn(message, &mut log);
    (end, log, session)
}

#[test]
fn the_memory_index_is_a_runtime_message_each_step_and_not_in_the_history() {
    let dir = Dir::new("memindex");
    let memory = dir.0.join("memory");
    fs::create_dir_all(&memory).unwrap();
    fs::write(
        memory.join("MEMORY.md"),
        "- [Build](build.md) — cargo build\n- [Server](server.md) — 127.0.0.1:18080\n",
    )
    .unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ]);
    let (end, _, session) = turn_with_memory(&DirMemory(memory), &script, &dir, "go");
    assert!(end.is_ok());
    // Every step carries the index as a runtime message, once.
    let runtimes = script.runtimes.borrow();
    assert_eq!(runtimes.len(), 2);
    for step in runtimes.iter() {
        assert_eq!(
            *step,
            ["workspace memory\n- [Build](build.md) — cargo build\n- [Server](server.md) — 127.0.0.1:18080"]
        );
    }
    // It is not stored: the session has no event for it and the history
    // holds no such message (M7, M27).
    assert!(session.history().iter().all(|m| !matches!(m, Message::Runtime(_))));
    // Without a memory store, or with an empty index, there is no message.
    let script = Script::new(&[("t1", THOUGHT), ("Done.", Stop::End)]);
    let (_, _, _) = turn_with_memory(&NoMemory, &script, &dir, "go");
    assert!(script.runtimes.borrow()[0].is_empty());
    let empty = dir.0.join("empty-memory");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("MEMORY.md"), "\n").unwrap();
    let script = Script::new(&[("t1", THOUGHT), ("Done.", Stop::End)]);
    let (_, _, _) = turn_with_memory(&DirMemory(empty), &script, &dir, "go");
    assert!(script.runtimes.borrow()[0].is_empty());
}

#[test]
fn the_memory_tool_writes_logs_an_event_and_shows_a_marker() {
    let dir = Dir::new("memtool");
    let memory = dir.0.join("memory");
    let script = Script::new(&[
        ("I will note it.", THOUGHT),
        (
            &memory_call(&[
                ("action", "add"),
                ("hook", "how to test"),
                ("text", "Run cargo test."),
                ("title", "Build and test"),
            ]),
            Stop::End,
        ),
        ("Noted.", THOUGHT),
        (
            &memory_call(&[("action", "show"), ("name", "build-and-test")]),
            Stop::End,
        ),
        ("Read.", THOUGHT),
        ("Remembered.", Stop::End),
    ]);
    let (end, log, session) = turn_with_memory(&DirMemory(memory.clone()), &script, &dir, "note the tests");
    assert!(matches!(end, Ok(TurnEnd::Answer(_))), "{:?}", log.0);
    assert_eq!(
        fs::read_to_string(memory.join("build-and-test.md")).unwrap(),
        "# Build and test\n\nRun cargo test.\n"
    );
    assert_eq!(
        fs::read_to_string(memory.join("MEMORY.md")).unwrap(),
        "- [Build and test](build-and-test.md) — how to test\n"
    );
    // The event comes after the call's result; the marker and the text
    // are reported once; a show writes nothing.
    let kinds = kinds(&session);
    assert_eq!(kinds.iter().filter(|k| **k == "memory").count(), 1);
    let call = kinds.iter().position(|k| *k == "call").unwrap();
    assert_eq!(&kinds[call..call + 3], ["call", "result", "memory"]);
    assert!(log.0.contains(&"memory +build-and-test Run cargo test.".to_string()));
    assert_eq!(log.0.iter().filter(|l| l.starts_with("memory ")).count(), 1);
    // The result tells that the store was made (the lines of the home).
    let Event::Result { content, .. } = &session.events[call + 1] else {
        panic!("not a result");
    };
    assert!(content.starts_with("the store was made\nRemembered `build-and-test`."));
    // The next step sees the entry in the index.
    assert_eq!(
        script.runtimes.borrow()[1],
        ["workspace memory\n- [Build and test](build-and-test.md) — how to test"]
    );
}

#[test]
fn a_memory_call_without_a_store_fails_for_everything_but_add() {
    let dir = Dir::new("memnone");
    let script = Script::new(&[
        ("t", THOUGHT),
        (&memory_call(&[("action", "show"), ("name", "x")]), Stop::End),
        ("t", THOUGHT),
        (
            &memory_call(&[
                ("action", "add"),
                ("hook", "h"),
                ("text", "x"),
                ("title", "T"),
            ]),
            Stop::End,
        ),
        ("t", THOUGHT),
        ("Cannot.", Stop::End),
    ]);
    let (end, log, session) = turn_with_memory(&NoMemory, &script, &dir, "go");
    assert!(end.is_ok(), "{:?}", log.0);
    let results: Vec<(Outcome, &str)> = session
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Result { outcome, content, .. } => Some((*outcome, content.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(results[0].0, Outcome::Fail);
    assert!(results[0].1.contains("has no entries yet"));
    // `add` registers the store; here there is none to register: a failure.
    assert_eq!(results[1].0, Outcome::Fail);
    assert!(!kinds(&session).contains(&"memory"));
}

#[test]
fn a_compaction_summary_is_kept_in_the_memory_directory_and_not_in_the_index() {
    let dir = Dir::new("memsummary");
    let memory = dir.0.join("memory");
    let default = Function::load(Path::new("/none"), Path::new("/none"), "agentLIBRE-gpu-high-minimal").unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        (" The user said go; true ran. ", Stop::End),
        ("t2", THOUGHT),
        ("Done.", Stop::End),
    ])
    .with_counts(&[100, OVERFLOW, FITS]);
    let mut session = Session::create(&dir.0.join("sessions"), "f", &dir.0).unwrap();
    let mut log = Log::default();
    let mut agent = agent(&default, &script, &mut session, &dir, None);
    let home = DirMemory(memory.clone());
    agent.memory = &home;
    assert!(agent.turn("go", &mut log).is_ok());
    let id = session.id.clone();
    let summaries: Vec<_> = fs::read_dir(memory.join("summaries")).unwrap().flatten().collect();
    assert_eq!(summaries.len(), 1);
    let name = summaries[0].file_name().to_string_lossy().into_owned();
    assert!(name.ends_with(&format!("-{id}.md")), "{name}");
    assert_eq!(name.len(), "2026-10-21-".len() + id.len() + 3);
    assert_eq!(
        fs::read_to_string(summaries[0].path()).unwrap(),
        "The user said go; true ran.\n"
    );
    // Not in the index (M16): the prompt does not carry it.
    assert!(!memory.join("MEMORY.md").exists());
}

/// The lines of a session file of the given type.
fn lines_of(session: &Session, kind: &str) -> Vec<serde_json::Value> {
    let dir = session.media_dir();
    let path = dir
        .parent()
        .unwrap()
        .join(format!("{}.jsonl", session.id));
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["type"] == kind)
        .collect()
}

#[test]
fn the_session_file_holds_the_turn_and_the_history_does_not() {
    // AGL-046, D1, D9, D14, D15.
    let dir = Dir::new("observed");
    let function = Function::load(
        Path::new("/none"),
        Path::new("/none"),
        "agentLIBRE-gpu-high-minimal",
    )
    .unwrap();
    let script = Script::new(&[
        ("t1", THOUGHT),
        (&bash_call("true"), Stop::End),
        ("t2", THOUGHT),
        ("Done", Stop::End),
    ]);
    let (end, _, session) = run(&function, &script, &dir, "go");
    assert_eq!(end.unwrap(), TurnEnd::Answer("Done".into()));
    let transitions = lines_of(&session, "transition");
    assert!(!transitions.is_empty());
    assert!(transitions.iter().all(|line| line["turn"] == 1));
    assert!(transitions.iter().all(|line| line["schema"] == 1));
    assert_eq!(transitions[0]["from"], "Ready");
    assert_eq!(transitions[0]["to"], "Prefill");
    assert_eq!(transitions.last().unwrap()["to"], "Ready");
    // Each line starts where the last one ended.
    for pair in transitions.windows(2) {
        assert_eq!(pair[0]["to"], pair[1]["from"], "{pair:?}");
    }
    let steps = lines_of(&session, "step");
    assert!(!steps.is_empty());
    assert_eq!(steps[0]["step"], 1);
    assert_eq!(steps[0]["state"], "inference");
    assert_eq!(steps[0]["prompt_sha256"].as_str().unwrap().len(), 64);
    // Each part of the prompt is written when it changes: here, once.
    let inputs = lines_of(&session, "prompt_input");
    for kind in ["system", "tools", "sampling", "model"] {
        let count = inputs.iter().filter(|line| line["kind"] == kind).count();
        assert_eq!(count, 1, "{kind}");
    }
    // None of it is an event, and none of it is read back as one.
    let history = session.history();
    let reopened = Session::open(session.media_dir().parent().unwrap(), &session.id).unwrap();
    assert_eq!(reopened.events, session.events);
    assert_eq!(format!("{:?}", reopened.history()), format!("{history:?}"));
}

#[test]
fn a_part_of_the_prompt_is_written_again_only_when_it_changes() {
    // AGL-046, D14: a reopened session knows what it wrote.
    let dir = Dir::new("inputs");
    let mut session = Session::create(&dir.0, "f", Path::new("/w")).unwrap();
    session.record_input("system", "one").unwrap();
    session.record_input("system", "one").unwrap();
    session.record_input("memory_index", "").unwrap();
    assert_eq!(lines_of(&session, "prompt_input").len(), 1);
    let mut reopened = Session::open(&dir.0, &session.id).unwrap();
    reopened.record_input("system", "one").unwrap();
    assert_eq!(lines_of(&reopened, "prompt_input").len(), 1);
    reopened.record_input("system", "two").unwrap();
    let inputs = lines_of(&reopened, "prompt_input");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1]["text"], "two");
}
