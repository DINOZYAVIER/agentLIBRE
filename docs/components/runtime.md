# Composition

`agl-runtime` is the Function activation and shared-model composition boundary.
`agl-daemon` constructs the path once per process:

```text
StoreHandle::open_at
  -> load WorkerHealth and ResourceQuarantine
  -> RuntimeService::start
       -> InferenceService::start
  -> AgentService::start(runtime-backed inference route)
```

`RuntimeHandle::activate` verifies workspace artifact declarations against
Forge's catalog and lock, resolves the Function and its exact entity references,
acquires Model artifacts, realizes a host load plan, ensures a ready model
service, and returns an immutable `AgentRunSnapshot`. Activation failure creates
no Conversation or Run.

Inference services use a daemon-supplied ProcessHost; systemd owns their child-process
lifecycle. Git package resolution and materialization belong to
`ayeque-forge-core`, called through the runtime and CLI artifact adapters.
Daemon Forge Git commands use the protected ExecutionHost with data FDs and
service Git cgroup limits. Standalone CLI sync uses foreground Git. Sync stages
and verifies copies, then publishes copies, receipts and the portable root lock
through the backend recovery journal.

A Model can declare an optional `[mmproj]` artifact with URL, SHA-256 and size.
It is acquired and verified with the model, included in its Run snapshot and
service identity, and passed to the private engine through a closed file
descriptor. Inference verifies image source digests and sends image data URI
blocks; context capacity is measured with the same multimodal request.

The composition root supplies typed `ExtensionBindings`. A locked declarative
Extension must match a trusted binding's exact version, full content digest,
definition, and Tool definition digests. Declarations never name executable
implementation code.

There is no process-global current model, public Package manager, dynamic
Extension loader, or compatibility reader for selected-away alpha formats.
