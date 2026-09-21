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

`RuntimeHandle::activate` verifies `FUNCTION.lock`, resolves or acquires its
exact entities and Model artifacts, realizes a host load plan, ensures a ready
model service, and returns an immutable `AgentRunSnapshot`. Activation failure
creates no Conversation or Run.

Package-resolution commands and inference services use `agl-execution-api`;
`agl-execd` owns the corresponding child-process lifecycle.

The composition root supplies typed `ExtensionBindings`. A locked declarative
Extension must match a trusted binding's exact version, full content digest,
definition, and Tool definition digests. Declarations never name executable
implementation code.

There is no process-global current model, public Package manager, dynamic
Extension loader, or compatibility reader for selected-away alpha formats.
