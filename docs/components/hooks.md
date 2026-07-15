# Hooks

Hooks are validation and guard checks that enforce runtime boundaries such as
repository-safe paths, required verification notes, and runtime identity
claims.

## What Exists

Core guard hooks are registered by the builtin `core-guards` provider and run
through the turn FSM:

- `json.validate`
- `repo_path.validate`
- `task_spec.validate`
- `secret_scan.validate`
- `diff_scope.validate`
- `verification.validate`
- `commit_message.validate`
- `skill_manifest.validate`
- `review_pack.validate`

Selected skills declare required hooks. The chat/run runtime groups selected
skill hooks by event and passes those batches into each turn. Required hook
failures currently fail the turn closed. `HookStatus::Repair` exists in the
types and emits `hook.repair_prepared`, but generic hook repair is not
implemented yet.

## Goals

- Make LLM claims about agentLIBRE runtime state checkable.
- Stop accepting hallucinated function, skill, profile, or subagent ids as
  operator-visible facts.
- Repair simple identity mistakes with a bounded model retry before failing the
  turn.
- Keep the UI/profile manager grounded in structured evidence instead of model
  prose.
- Preserve fail-closed behavior when repair is unavailable or exhausted.

## Non-Goals

- Do not make hooks mutate runtime state.
- Do not let hooks silently rewrite final answers without model-visible
  evidence.
- Do not require every normal chat answer to repeat function or skill ids.
- Do not parse ids from rendered prompt text. Hooks should receive structured
  expected identity in the payload.

## Runtime Identity Hooks

Add two builtin guard hooks:

- `runtime.identity.validate`
- `runtime.identity.require`

Both hooks run on `artifact.write`, because that event has the final answer
text. They compare model claims against the resolved runtime identity:

```json
{
  "turn_id": "run-001",
  "artifact_kind": "answer",
  "content": "function=repo-analyst; skill=repo-status",
  "content_bytes": 42,
  "runtime_identity": {
    "function": {
      "id": "repo-analyst",
      "source": "explicit",
      "path": "/tmp/functions/repo-analyst/FUNCTION.md"
    },
    "model_profile": "local",
    "skills": ["repo-status"],
    "subagents": ["reviewer"],
    "workspace_root": "/repo"
  },
  "runtime_identity_validation": {
    "required": false,
    "fields": ["function", "skills", "subagents"],
    "repair_attempts": 1
  }
}
```

`runtime.identity.validate` is claim-sensitive:

- Pass if the answer does not claim runtime ids.
- Pass if all claimed ids match the structured identity.
- Fail or request repair if the answer claims ids that do not match.

`runtime.identity.require` is strict:

- Requires every field listed by `runtime_identity_validation.fields`.
- Fails or requests repair if a required field is absent.
- Fails or requests repair if any claimed field is incorrect.

Use `runtime.identity.validate` for normal function-backed chat. Use
`runtime.identity.require` for smoke tests, `agl function doctor`, profile
manager diagnostics, and explicit user questions such as "what function,
skills, and subagents are loaded?".

## Identity Claims

The validator should recognize conservative, structured claim forms first:

```text
function=repo-analyst
skill=repo-status
skills=repo-status,coding-style
subagent=reviewer
subagents=reviewer,tester
model_profile=local
```

It may also recognize simple prose claims such as:

- `function id: repo-analyst`
- `skill id: repo-status`
- `subagent id: reviewer`
- `model profile: local`

Do not attempt broad semantic extraction in the MVP. If the answer says "the
repository skill" without an id, validation should treat that as no explicit id
claim.

List comparisons should be order-insensitive and exact. Unknown extra ids are
mismatches. Missing fields are mismatches only under `runtime.identity.require`
or `runtime_identity_validation.required=true`.

## Message Codes

Identity hook failures should use stable codes:

- `runtime_identity_missing`: a required field was not claimed.
- `runtime_identity_mismatch`: a claimed id differs from runtime evidence.
- `runtime_identity_unknown_field`: the answer claims an unsupported identity
  field.
- `runtime_identity_unavailable`: runtime did not provide required identity
  evidence.

Each message should include a concise `fix` string suitable for repair:

```text
Use function=repo-analyst; skills=repo-status; subagents=reviewer.
```

Do not include secrets or hidden prompt text in hook messages. Paths may be
included in evidence files, but repair prompts should prefer ids over paths.

## Function Validation

agentFUNCTIONs may declare identity validation policy:

```yaml
validation:
  runtime_identity:
    required: false
    fields:
      - function
      - skills
      - subagents
    repair_attempts: 1
```

When the `validation.runtime_identity` block is absent, no identity hook is
added. `required = false` selects claim-sensitive validation;
`required = true` selects strict validation. `repair_attempts` bounds model
regeneration after a repairable failure.

## Repair Loop

Generic hook repair should be a bounded retry of model generation, not a hidden
text rewrite.

When a required hook batch returns `Repair`, or when a required identity hook
fails with a repairable code, the runner should:

1. Emit `hook.batch_finished` with outcome `repair` or `fail`.
2. Emit `hook.repair_prepared` with the event, message codes, and attempt.
3. Append a system repair message to the in-memory turn messages.
4. Request the model again with the same user input, context, tools, and
   visible runtime capabilities.
5. Re-parse the repaired model response and re-run the same hook batches.
6. Accept the answer only if all required hooks pass.
7. Fail closed after `repair_attempts`.

The repair message should be narrow and factual:

```text
The previous answer claimed incorrect agentLIBRE runtime identity.
Expected: function=repo-analyst; skills=repo-status; subagents=reviewer.
Rewrite the answer. Do not invent ids. Keep all other user-requested content.
```

Repair messages must not be persisted as user or assistant transcript messages.
They may be recorded in `agent-events.jsonl` and request artifacts so operators
can audit why the second request happened.

Configure `repair_attempts` in `validation.runtime_identity`. A value of zero
disables regeneration after identity validation fails.

## FSM And Events

The existing FSM already has `PrepareRepair`. Extend the repair path instead of
adding a separate runner:

- Add a turn-level repair attempt counter.
- Let repair return to `model.request` rather than immediately rejecting the
  hook batch.
- Preserve the original failed hook summary in events.
- Add event fields for `repairable`, `attempt`, `max_attempts`, and
  `repair_message_codes`.

New or extended events:

- `hook.repair_prepared`: repair prompt constructed.
- `hook.repair_requested`: model retry requested.
- `hook.repair_succeeded`: repaired answer passed required hooks.
- `hook.repair_failed`: repair exhausted or repaired answer still failed.

If repair is disabled or the hook is not repairable, existing fail-closed
behavior remains:

```text
required hook batch `artifact.write` failed
```

## Evidence

Every function-backed run should already write:

- `function-resolution.json`
- `function-context.md`
- `subagent-registry.json`
- `skill-context.json`

Identity hooks should add:

- `runtime-identity.json`: exact identity object used by hooks.
- `identity-validation.json`: effective identity validation after defaults and CLI
  overrides.

`agent-events.jsonl` should show each repair attempt and final hook outcome.
`events.jsonl` should keep model request and response artifact paths for every
attempt.

The profile manager should read identity from these structured files, not from
assistant prose.

## Implementation Plan

1. Add `runtime.identity.validate` and `runtime.identity.require` constants,
   declarations, validators, and tests in `agl-tools`.
2. Extend `artifact_write_payload` in `agl-loop` or the chat host so it can
   include `runtime_identity` and `runtime_identity_validation`.
3. Extend `InferenceSession` to build runtime identity from resolved function,
   selected skills, subagents, model profile, workspace root, and tool mode.
4. Parse `validation.runtime_identity` and write the effective validation into
   evidence.
5. Add selected identity hook batches when a function is active or doctor mode
   requires identity.
6. Implement generic hook repair in `agl-loop` with bounded retries and event
   evidence.
7. Add CLI/doctor smoke tests for mismatch, missing required id, successful
   repair, and exhausted repair.
8. Add local multi-turn smoke covering `/reload`: change `FUNCTION.md` or
   `SYSTEM.md`, run `/reload`, then require the new identity marker to be
   validated.

## Acceptance Criteria

- If a function-backed answer says `function=wrong`, the turn is repaired or
  blocked before the answer is accepted.
- If a function-backed answer does not mention ids, normal chat passes under
  `validate_claims`.
- `agl function doctor` requires function, selected skills, and subagents to be
  named correctly.
- Repair attempts are visible in events and request artifacts.
- The profile manager can display runtime identity from JSON evidence without
  asking the LLM.
- Existing guard hooks keep their fail-closed behavior when repair is not
  configured.
