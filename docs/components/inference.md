# Inference

`agl-runtime::inference` runs exact digest-checked GGUF artifacts through
private `llama-server` services owned by `agl-execd`. The daemon config supplies the engine executable and
may supply private host profiles keyed by GGUF digest. The engine bundle digest
is calculated and checked by agentLIBRE; Model format and acquisition come from
the locked Model declaration.

Download and explicit import verify the complete artifact SHA-256. Later
activations open the already verified managed-cache file without rehashing it;
they require the declared size and GGUF header plus current-user ownership, one
hard link, owner-only access, and stable file identity. The retained descriptor
is cloned into the execd-owned inference launch, so activation does not reopen
the model through an untrusted path.

`agl-runtime` owns a process-local registry keyed by the exact realized load
plan. Compatible Functions reuse one resident service. Each service has
bounded slots and queue capacity; llama.cpp continuous batching is enabled by
default, while prompts, KV state, cancellation, usage and Tool state remain
isolated per operation. Incompatible load plans use separate services when
capacity permits.

The child is constrained by descriptor, path, address-space and network
checks. Generation supports streaming deltas, cancellation and deadlines.
Before generation, the backend applies the exact chat template and tokenizes
the complete prompt. The prompt plus reserved generation budget must fit the
realized context or the operation fails with `context_exhausted`; context and
reasoning budgets are never silently reduced.
Idle services normally unload after the Function keep-warm deadlines expire
and may be evicted earlier under memory pressure. Active or queued operations
hold leases. Worker crashes/device loss produce cooldown health, while
allocation overage produces resource quarantine; both are persisted by the
daemon Store.

When execd reports that an engine ended from a signal, worker health stores the
exact signal as `unattributed_signal`. A signal alone is never labeled as OOM;
device loss and allocation failures require their own observed backend or
allocation evidence.

The current native request and `agl-core::Content` value are text-only.
