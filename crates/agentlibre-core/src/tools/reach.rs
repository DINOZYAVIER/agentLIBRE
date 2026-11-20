//! The one place a call of a built-in tool is run (AGL-067, F5): `bash`,
//! `edit` and `forge` work in the workspace, `view`, `search` and `memory`
//! also reach what the session holds.

use std::path::Path;

use crate::action::Outcome;
use crate::attach::Transcriber;
use crate::backend::{Backend, ToolCall};
use crate::error::Error;
use crate::i18n::{m, Lang};
use crate::tr;
use crate::media;
use crate::memory::{self, Home};
use crate::search::Searcher;
use crate::session::Session;

use super::{ForgeEnv, ForgeWrite, Output, ToolResult, run_local, shown};

/// What the tools reach beyond the workspace: the media and the output
/// files of the session, the backend that counts and sees, the transcriber
/// of an audio, the searcher and the memory.
pub struct Reach<'a> {
    pub workspace: &'a Path,
    pub forge: ForgeEnv<'a>,
    /// The tokens of a result that are shown whole (AGL-049).
    pub output_tokens: usize,
    pub session: &'a mut Session,
    pub backend: &'a dyn Backend,
    /// Turns an audio the `view` tool opens into text when the model does
    /// not hear it (AGL-014, D22, D24).
    pub whisper: &'a dyn Transcriber,
    /// Runs the `search` tool (AGL-019).
    pub search: &'a dyn Searcher,
    /// Where the memory of the workspace is (AGL-006).
    pub memory: &'a dyn Home,
    /// The daemon's holder of the plane's flows, which the `flow` tool asks;
    /// none outside a plane (AGL-118, D2, D17).
    pub flows: Option<crate::plane::flow::holder::Handle>,
}

/// A write a call made outside the workspace's files: to the forge
/// (AGL-008) or to the memory (AGL-006).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    Forge(ForgeWrite),
    Memory(memory::Write),
}

/// What a call came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    pub result: ToolResult,
    pub write: Option<Written>,
}

/// The `flow` tool: the verb of `agl plane flow` itself, so that the agent
/// and the human ask the same machine (AGL-118, D2, D7) — and, at home, the
/// same thread: the verb goes to the plane's holder, the one driver of its
/// flows (D17). What the agent may ask is `start`, `status`, `list` and
/// `message`; the message arrives as a nudge, bounded state by state by the
/// declaration's `nudges` (D15, D21).
pub(super) fn flow_tool(
    call: &ToolCall,
    holder: Option<&crate::plane::flow::holder::Handle>,
    lang: Lang,
) -> ToolResult {
    use crate::plane::flow::Verb;
    let Some(holder) = holder else {
        return ToolResult {
            outcome: Outcome::Fail,
            content: tr!(lang, m::FLOW_NO_PLANE).to_string(),
        };
    };
    let name = call.text("name").unwrap_or_default();
    let verb = match call.text("action").unwrap_or_default() {
        "list" => Verb::List,
        "start" if !name.is_empty() => Verb::Start { name: name.into() },
        "status" if !name.is_empty() => Verb::Status { name: name.into() },
        "message" if !name.is_empty() => Verb::Nudge {
            name: name.into(),
            event: call.text("event").unwrap_or_default().into(),
        },
        other => {
            return ToolResult {
                outcome: Outcome::Fail,
                content: format!(
                    "`{other}` is no verb of a flow: `start`, `status` and `message` name one, `list` names none"
                ),
            };
        }
    };
    let said = holder.ask(verb, lang);
    ToolResult {
        outcome: if said.failed { Outcome::Fail } else { Outcome::Ok },
        content: said.text(),
    }
}

/// Runs `call`. `text` is the text the model wrote before it (D114): the
/// body of a memory `add` whose `text` field is empty (AGL-029, D1).
pub fn run(call: &ToolCall, text: Option<&str>, reach: &mut Reach<'_>) -> Ran {
    let backend = reach.backend;
    let count = |text: &str| backend.count_tokens(text).ok();
    let output = Output {
        tokens: reach.output_tokens,
        file: reach.session.output_file(&call.id),
        count: &count,
    };
    let (result, write) = match call.name.as_str() {
        "view" => (view(call, reach), None),
        "search" => {
            let mut result = search(call, reach.search);
            // The service's reply is cut like any tool's output (AGL-049).
            result.content = shown(&result.content, &output);
            (result, None)
        }
        "flow" => (flow_tool(call, reach.flows.as_ref(), reach.forge.lang), None),
        "memory" => {
            let (result, write) = memory_tool(call, text, reach.memory, reach.output_tokens, reach.backend);
            (result, write.map(Written::Memory))
        }
        _ => {
            let (result, write) = run_local(reach.workspace, reach.forge, call, &output);
            (result, write.map(Written::Forge))
        }
    };
    Ran { result, write }
}

/// The `search` tool (AGL-019): a call the service or the check of its
/// arguments refuses is a failed result the model reads, not a stop (D7).
fn search(call: &ToolCall, searcher: &dyn Searcher) -> ToolResult {
    match crate::search::Request::from_args(call) {
        Ok(request) => searcher.search(&request),
        Err(sentence) => ToolResult {
            outcome: Outcome::Fail,
            content: sentence,
        },
    }
}

/// The `memory` tool (AGL-006, M32): `add` registers the workspace's store
/// when it is not yet (M23); the other actions need the memory that is
/// there. `call_text` is the text of the call that preceded it (AGL-029,
/// D1): the body of `add` when its `text` field is empty.
fn memory_tool(
    call: &ToolCall,
    call_text: Option<&str>,
    home: &dyn Home,
    output_tokens: usize,
    backend: &dyn Backend,
) -> (ToolResult, Option<memory::Write>) {
    let args = memory::Args::from_call(call);
    let fail = |content: String| {
        (
            ToolResult {
                outcome: Outcome::Fail,
                content,
            },
            None,
        )
    };
    // In a plane, the plane's memory, the workspace's by `store`
    // (AGL-077, D8).
    let workspace = args.store == "workspace";
    let (dir, lines) = if args.action == "add" {
        let found = if workspace { home.write_workspace() } else { home.write() };
        match found {
            Ok(found) => found,
            Err(error) => return fail(error.message(Lang::En)),
        }
    } else {
        let found = if workspace { home.read_workspace() } else { home.read() };
        match found {
            Some(dir) => (dir, Vec::new()),
            None => return fail("The memory of this workspace has no entries yet.".into()),
        }
    };
    let (mut result, write) = memory::run(
        &dir,
        &args,
        call_text,
        &Output {
            tokens: output_tokens,
            file: Default::default(),
            count: &|text| backend.count_tokens(text).ok(),
        },
    );
    // A store that moved or was adopted on the way: said once.
    if !lines.is_empty() {
        result.content.insert_str(0, &(lines.join("\n") + "\n"));
    }
    (result, write)
}

/// The `view` tool (AGL-014, D24): keeps the file as a medium of the
/// session, as `/attach` does, and answers with its placeholder, which
/// becomes the marker, the frames or the transcript at the next step.
fn view(call: &ToolCall, reach: &mut Reach<'_>) -> ToolResult {
    let fail = |content: String| ToolResult {
        outcome: Outcome::Fail,
        content,
    };
    let Some(path) = call.text("path") else {
        return fail("view needs a path.".into());
    };
    let path = reach.workspace.join(path);
    let source = match media::Source::from_file(&path) {
        Ok(source) => source,
        Err(Error::MediaKind { .. }) => {
            return fail(format!(
                "{} is not an image, a video or an audio.",
                path.display()
            ));
        }
        Err(error) => return fail(error.message(Lang::En)),
    };
    // The picture needs the projector; ask before the file is kept.
    if source.kind != media::Kind::Audio && reach.backend.media_marker().is_err() {
        return fail("The server has no multimodal projector, so it cannot take images.".into());
    }
    let transcript = if source.kind == media::Kind::Audio {
        match reach.backend.hears_audio() {
            Ok(true) => None,
            Ok(false) => match reach.whisper.transcribe(&source, &mut |_| {}) {
                Ok(text) => Some(text),
                Err(error) => return fail(error.message(Lang::En)),
            },
            Err(error) => return fail(error.message(Lang::En)),
        }
    } else {
        None
    };
    let number = reach.session.next_media_number(source.kind);
    match reach.session.store_media(&source, number, transcript) {
        Ok(kept) => ToolResult {
            outcome: Outcome::Ok,
            content: format!("{} {}", kept.placeholder(), kept.name),
        },
        Err(error) => fail(error.message(Lang::En)),
    }
}
