# The session file

One JSON object per line, appended to and never rewritten: a line's number stays
valid. The first line is the header (`type: session`). Every line has `type` and
`time`, a UTC time in seconds. What a file holds is a record of what happened, and
it includes the output of tools, so its text is data, not instructions.

## The conversation

These are what the history is built from.

| `type` | Fields |
| --- | --- |
| `user` | `text` |
| `thinking` | `text` |
| `call` | `id`, `tool`, `args`, `text` (written before the call) |
| `result` | `id`, `outcome` (`ok`, `fail`, `timeout`), `content` |
| `answer` | `text` |
| `compact` | `summary`, `question`: a summary that replaces what came before |
| `cut_off` | `limit`, `discarded`, `call`, `thinking`: a generation hit its budget |
| `command` | `line`, `failed`, `output`: a slash command the human ran |
| `state` | `state`: a visit of a state of the policy |
| `stir` | `attempt`, `error`: a malformed output, tried again |
| `stop` | `reason` |
| `forge`, `memory`, `media` | a write of those tools, a medium of the session |

## What happened during a turn

These are not part of the conversation and never reach the model's history. Each
has `schema` (1), and all but `prompt_input` have `turn` and `step`.

`transition`: the turn moved from one phase to another.

| Field | Meaning |
| --- | --- |
| `from`, `to` | `Ready`, `StartingServer`, `Transcribing`, `Prefill`, `Thinking`, `Writing`, `Running`, `Compacting`, `Retry` |
| `event` | what moved it, like `first_thinking`, `tool_ok`, `stop_interrupted` |
| `ms` | the time spent in `from` |
| `tokens`, `prompt_tokens` | decoded since the last transition, the prompt's size if told since |
| `tool`, `attempt`, `max` | of `tool_start` and of `stir` |
| `violation` | `true` for a transition the FSM does not allow: a bug in agl |

`Prefill` is the server reading the prompt, until its first token. A turn with no
phase after `Prefill` for long is waiting on the server.

`step`: what the prompt of a step was made of: `state`, `prompt_sha256`,
`prompt_tokens`, `think`, `think_budget`, `action_budget`, `context`, `stirred`,
`reasoning`. An equal hash means an equal prompt.

`prompt_progress`: the server's reading of a prompt going on: `total`, `cache`,
`processed` (counts from the first token, so it holds `cache`), `time_ms`. The
tokens really read are `processed - cache`. `cache` near `total` means the prompt
cache held; `cache` 0 on a later step means the server read everything again, which
is slow and worth looking into.

`prefill`: how a generation's prompt was read: `tokens`, `ms`.

`prompt_input`: a part of the prompt, in full, when it changes: `kind` (`system`,
`tools`, `memory_index`, `grammar`, `sampling`, `model`), `text`.
