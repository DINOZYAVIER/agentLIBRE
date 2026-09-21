# CLI

`products/agl-cli` builds the deliberately small `agl` surface:

```bash
agl [--function function:agentlibre.coder@^1.0] [--entity-root /absolute/path/to/agents/coder]
agl resume [conversation-id-or-name] [--last] [--all]
agl chat [FUNCTION]
agl conversation rename conversation-id-or-name new-name
agl serve
agl config check [--function FUNCTION]
agl config apply [--function FUNCTION]
agl doctor
agl [--entity-root /path/to/agents/coder] function lock /path/to/functions/coder
agl [--entity-root /path/to/agents/coder] run /path/to/functions/coder "prompt"
agl view run_...
agl cancel run_...
```

Root `agl` and `agl chat` are the line-oriented coding-agent REPL, not a TUI.
`agl` is retained as an alias for `agl chat`. `agl chat FUNCTION` or
`agl chat --function FUNCTION` selects an explicit Function; without it, the
configured default is used. Both use the current directory as the Conversation
workspace and read generated state after `agl config apply`. The human source is one strict
`$XDG_CONFIG_HOME/agentLIBRE/agentLIBRE.toml` file:

```toml
[chat]
default_function = "function:agentlibre.coder@^1.0"

[repl]
decorations_default = "default"

[packages]
directories = ["/absolute/path/to/local/source"]

[inference.memory]
keep_free_ram = "12 GiB"
```

`AGL_HOME` replaces the XDG roots for isolated runs. `--function` overrides the
chat default for a new Conversation and accepts either a typed package
requirement or an existing Function directory. Repeated `--entity-root`
arguments replace generated package roots for that invocation. `agl run` also
uses the current directory as workspace and has no workspace option; it creates
one Conversation turn and exits.

During a turn, Agent text, operation state, Tool activity, and terminal Run
state are rendered from the daemon subscription as they arrive. One `Ctrl-C`
requests cancellation of the admitted Run and waits
for a terminal result. At the idle input prompt, one `Ctrl-C` closes the CLI;
the durable Conversation remains resumable. EOF also closes input.
The exact `/quit` and `/exit` inputs close the REPL from an idle prompt as
aliases; surrounding whitespace is ignored.

`agl config check` is read-only. `agl config apply` resolves the requested
Function graphs, writes source-adjacent locks and atomically activates generated
state; unfinished Runs make it return busy. `agl doctor` reports active state
and unfinished Runs without credential bytes. `agl serve` reads generated state
and owns one same-UID Unix socket below the state root. Obsolete `agl.toml` and
`daemon.json` files are not read.
Events and message queries remain internal protocol operations; they are not
public CLI commands.
## Interactive REPL presentation

The TTY REPL uses reedline with a slash command menu. Typing `/` at the start
of a line opens completion; Tab accepts a suggestion and Escape closes it.
`/tool N` displays the result of operation N from the most recent Run, while
`/tool N input` displays its request. Full output is available from the
built-in viewer and is safely escaped before it reaches the terminal.

`decorations=default` is the default. It renders CommonMark (including tables
and fenced code), shows formatted Run, operation, and Tool cards, and streams
model output. Automatic Tool input and result previews stop at the first Function
limit reached: `presentation.tool_output.lines` or
`presentation.tool_output.chars`. `/tool-output lines N` and
`/tool-output chars N` override one limit for the current REPL process;
`/tool-output reset` restores the Function values. `/tool N input|result`
opens the complete value. `off` keeps the plain transcript while explicit
`/tool` inspection remains available. `/decorations reset` restores the value
configured in `agentLIBRE.toml`.

Tool cards use a horizontal frame when `[presentation.tool].frame = true`.
`/tool-frame on|off|reset` overrides this independently for the current REPL;
`reset` restores the Function snapshot value.
The lower frame edge includes `Worked for`, measured from the first
`OperationStarted` to the terminal `OperationCompleted`, including retries.

`decorations=full` adds the verbose Run summary, including the Run id and
elapsed time. In the default mode the terminal line is only `RUN Completed`.

Decorated model text is enclosed by a horizontal rule before and after the
answer. The lower edge embeds `Worked for`, using seconds, minutes, or hours as
appropriate.

Model generation cards are compact by default: `MODEL GENERATION #N  Succeeded`
shows only the generation number and state. `/model-generation details on`
enables delivery, attempt, request, and result details for the current REPL;
`details off` returns to the compact card and `details reset` restores the
Function value from `[presentation.model_generation]`.

Colors and text attributes come from the Function's `[presentation.colors]`
table. Roles cover rules, Run/status/operation/Tool labels, JSON scalar types,
and Markdown constructs. Values may be `none`, `bold`, `dim`, `italic`, or
`underline` (combined with `#RRGGBB` or `oklch(L C H)`). The CLI converts OKLCH
to sRGB and emits truecolor ANSI only on TTY stdout when
`COLORTERM=truecolor|24bit`, with `NO_COLOR` unset and `TERM` not `dumb`. If those conditions are not met, the
same rendered text is emitted without any color/style escapes. A fallback to
limited-color terminals is intentionally deferred.

On a TTY, the REPL keeps a full-width input dock below the transcript while a
Run is active. The separator reports `Ready`, `Generating`, `Running <Tool ID>`,
or `Retrying`, with `· queued N` when messages are waiting. Enter queues normal
messages FIFO during a Run; slash commands remain immediate process-local
controls. An empty dock shows `Type a message or / for commands`; a trailing
single backslash keeps the existing continuation behavior.
