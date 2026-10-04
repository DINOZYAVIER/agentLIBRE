# Functions

A Function is a portable declarative environment for an Agent. Its
`FUNCTION.toml` selects one Agent, one GGUF Model, optional Skills and
Extensions, exact admitted Tools and authority, inference settings, service
settings, and finite Run limits. Physical inference fields are optional: an
omitted field is selected by the host, while a supplied field is a hard
requirement.

Reasoning is also typed. An enabled reasoning section declares a token budget,
an explicit `preserve` choice, and an optional `default = "low|medium|xhigh"`. The
Model must declare that effort and preservation capability, and the selected
backend must confirm support during activation. Backend-specific free-form
arguments are not accepted.

`--reasoning low|medium|xhigh` overrides the default for new Runs in that CLI
invocation. The REPL accepts `/reasoning` to inspect the default and effective
level, `/reasoning low|medium|xhigh` to change subsequent turns, and
`/reasoning default` to clear the process-local override. These commands are
not conversation messages. Admission validates Model capabilities and records
the effective setting in the Run snapshot; replay never updates it.

Public source uses fixed visible roots and filenames:

```text
functions/<ui-name>/FUNCTION.toml
agents/<ui-name>/AGENT.md
skills/<ui-name>/SKILL.md
models/<ui-name>/MODEL.toml
extensions/<ui-name>/EXTENSION.toml
```

The directory name is the local UI name and may differ from the manifest
`id`. Manifests expose their entity schema directly; there is no public
Package wrapper.

The configured catalog's `agentLIBRE.toml` declares the Function and its Agent,
Model, Skill and Extension artifacts, including Git source, full commit SHA
and path within that source. `agl forge sync` reconciles these declarations
with Forge's generated catalog, lock and materialized directories.

Activation resolves the Function directory and every referenced entity through
this verified Forge catalog. It checks each dependency's kind, schema, manifest
ID and exact version, and computes its content digest for the Run snapshot.
The calling workspace's artifact declaration is checked separately. An explicit
Function directory must be a materialized Function from the AGL catalog.

Forge owns Git package materialization and its generated lock. The runtime
acquires Model bytes and constructs the inference service from the verified
entity contents.

Compatible Functions using the same GGUF and realized load plan share one
resident model service. Generation settings remain per Run. A Function's
`inference.service.idle_timeout` defaults to 15 minutes and is only a soft
keep-warm deadline.

Run limits include time, cumulative model tokens/calls, Tool calls, and a Tool
result byte ceiling. `limits.tool_result_bytes` defaults to and cannot exceed
the runtime hard maximum of 65,536 serialized bytes.

Every field is set directly in the Function's `[limits]` table. After changing
the Function source, commit it in its source repository and update the
Function artifact's pinned revision through `agl forge update` in the AGL
workspace. The new limits apply to new Conversations; an existing Conversation
retains its immutable Function snapshot.

`model_input_tokens` is cumulative accounting, not the model's per-call
context window: the complete retained prompt is counted again for every model
call. Omit this optional field for no aggregate input cap; no numeric sentinel
is needed. Other call/output limits remain active. The independent
`inference.load.context_tokens` check still bounds every individual request.

REPL Tool previews are Function-owned and become part of the immutable
Conversation snapshot. Their standard values are declared as:

```toml
[presentation.tool_output]
lines = 10
chars = 500
```

Each Tool card can also have its own horizontal frame:

```toml
[presentation.tool]
frame = true
```

The frame setting is independent from the Tool input/result preview limits.

REPL presentation colors are also Function-owned. Define one style for each
semantic role in the shared renderer:

```toml
[presentation.colors]
rule = "dim"
run = "bold oklch(0.72 0.18 330)"
run_id = "#FFFFFF"
status_success = "bold #7BD88F"
status_failure = "bold #FF0000"
status_pending = "bold #FFFF00"
operation = "bold #00FFFF"
tool = "bold #00FFFF"
ordinal = "dim"
field = "bold #8AA2D8"
muted = "dim"
json_key = "#00FFFF"
json_string = "#7BD88F"
json_number = "#FFFF00"
json_boolean = "#FF00FF"
json_null = "dim"
markdown_heading = "bold #00FFFF"
markdown_code = "dim"
markdown_inline_code = "#FFFF00"
markdown_strong = "bold"
markdown_emphasis = "italic"
markdown_link = "#8AA2D8"
markdown_quote = "dim"
markdown_bullet = "#00FFFF"
markdown_rule = "dim"
input_rule = "oklch(0.439 0 0)"
input_background = "oklch(0.269 0 0)"
input_prompt = "bold oklch(0.718 0.202 349.761)"
input_hint = "dim oklch(0.823 0.12 346.018)"
input_text = "oklch(0.936 0.032 17.717)"
input_activity = "oklch(0.823 0.12 346.018)"
input_selected = "bold oklch(0.518 0.253 323.949)"
```

Each value is `none`, optional `bold`/`dim`/`italic`/`underline` attributes,
and either `#RRGGBB` or `oklch(L C H)`. The palette is captured in the
immutable Conversation snapshot. The CLI emits truecolor ANSI only when
stdout is a TTY, `COLORTERM` is `truecolor` or `24bit`, and color has not been disabled by
`NO_COLOR` or `TERM=dumb`; otherwise it emits no color or style escapes.
`input_background` is applied as the background of the editable input and
its hint; `input_text` controls entered text.
Alacritty supports truecolor when it advertises that capability. A 256/16-color
fallback is intentionally deferred to a future decision.

The interactive CLI may override either value for its current process with
`/tool-output`; it does not mutate the Function or an existing Conversation.

Model generation cards are compact by default. A Function can opt into the
expanded delivery, attempt, request, and result fields with:

```toml
[presentation.model_generation]
details = true
```

The REPL can override this presentation for the current process with
`/model-generation details on|off|reset`; the Function and existing Conversation
snapshot are unchanged.

The dedicated `agentlibre.search-qwen38-27b-reasoning` Function is the first
search consumer. It admits `agentlibre.searxng:search` and grants
`agentlibre.searxng:query` for logical service `ayeque-search` with the sorted
source set `web`, `wikipedia`. Existing chat Functions do not admit search.

Function and Family resolution uses `${data}/source/` by default. Set an
absolute `[forge] catalog_root` in the user TOML to select another catalog.
A missing catalog produces an error; resolution never searches the build tree.
The catalog path is stored in generated active configuration and passed to the
runtime separately from the conversation workspace.

## Autonomous schedules

A Function can declare timer and workspace file triggers:

```toml
[schedule]
interval_seconds = 60
prompt = "Inspect changes and report the result"
max_run_seconds = 30
events = ["file:src/main.rs"]
```

Both numbers must be positive. Events name relative workspace files. The
Function's permissions apply to each autonomous Run. Declaring a schedule
leaves it disabled until `agl schedule enable FUNCTION_ID --workspace PATH`.
Enable displays its permissions. `agl schedule show [--workspace PATH]` lists
scheduled Functions, enabled state, triggers, last Run and next timer.

Ticks follow a fixed grid from enable; the first occurs after one interval.
An active Run of the same Function in that workspace skips the trigger and
logs the skip. Different workspaces can run concurrently. A manual start
gets Busy while that workspace's autonomous Run is active.

A schedule has a separate persistent Conversation. Previous Run messages
become its history; the configured prompt is added to Run instructions,
without a User message. File changes during an active Run of this Function
are discarded, including external changes. The baseline is refreshed after
completion; events from that interval are not replayed. Restart skips elapsed ticks
and offline file changes, then resumes the original grid with a fresh baseline.

`agl schedule disable FUNCTION_ID --workspace PATH` prevents future triggers
and cancels the active autonomous Run and its executions. Re-enabling the
same Function preserves history and starts a new grid. Repeated enable of an
already enabled unchanged Function preserves both history and grid. Enable
with a changed snapshot creates a new schedule Conversation; the previous
Conversation remains available. Source updates alone do not change an enabled
schedule's snapshot or permissions.
