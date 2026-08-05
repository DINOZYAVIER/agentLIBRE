# AGL-151 Terminal Boundary Definition

## Current Packages (Primary Owner: agentLIBRE)

| Package | Category |
| :--- | :--- |
| agl-actions | Execution |
| agl-app | Application |
| agl-artifact | Storage |
| agl-assets | Storage |
| agl-chat | Communication |
| agl-cli | Interface |
| agl-client | Communication |
| agl-config | Configuration |
| agl-content | Storage |
| agl-core-tools | Tooling |
| agl-cron | Scheduling |
| agl-daemon | Runtime |
| agl-events | Communication |
| agl-extension | Extensibility |
| agl-function | Execution |
| agl-hooks | Extensibility |
| agl-host-tools | Tooling |
| agl-ids | Identity |
| agl-inference | AI/ML |
| agl-inference-worker | AI/ML |
| agl-kernel | Runtime |
| agl-llama-cpp-sys | AI/ML |
| agl-loop | Runtime |
| agl-matrix-bridge | Communication |
| agl-memory | Storage |
| agl-model | AI/ML |
| agl-notes | Storage |
| agl-oven | Tooling |
| agl-process | Runtime (agent-side typed terminal client adapter) |
| agl-protocol | Communication |
| agl-repo | Storage |
| agl-runtime | Runtime |
| agl-session | Runtime |
| agl-skill | Extensibility |
| agl-store | Storage |
| agl-supervisor | Runtime |
| agl-turn | Runtime |

## Extracted Packages (Owner: agl-terminal)

### Selected Targets

**Independent engine and contract packages:**
- `agl-exec`
- `agl-pty`
- `agl-terminal`
- `agl-terminal-protocol`
- `agl-terminal-client`

These packages are built and released from the independent `agl-terminal`
repository. agentLIBRE consumes the selected contract crates at one exact Git
revision.

**Independent binary packages:**
- `agl-process-launcher`
- `agl-terminald`

**Interactive product package:**
- `agl-terminal-ui`

### Classified-but-not-moved agentLIBRE Domains
The following are core agentLIBRE domains and are NOT candidates for terminal extraction:
- inference
- Matrix/connectors
- agl-repo
- memory
- notes
- cron
- Functions
- Skills
- Tools

## Boundary Constraints

### Dependency Direction
- **Allowed:** `agl-terminal-ui` $\rightarrow$ Bounded Agent Client SDK.
- **Forbidden:** `agl-exec`, `agl-pty`, `agl-terminal`, `agl-terminal-protocol`, `agl-terminal-client`, `agl-process-launcher`, `agl-terminald` $\rightarrow$ Agent SDK.

### Security & Identity
- **Terminal-owned identities:** Execution, terminal session, request, stream,
  writer lease, and service-generation IDs are canonical to `agl-terminal`.
- **Agent-owned identities:** Agent run, session, step, and attempt IDs remain
  canonical to `agentLIBRE`; terminal packages neither parse nor generate them.
- **Caller Identification:** Opaque caller namespace, owner, and role.
- **Authentication:** Immutable authority fingerprint.
- **Authorization:** `agl-terminald` enforces the exact admitted grant. It does
  not import agent policy or derive authority from caller strings.
- **Adapter ownership:** `agentLIBRE` persists the bounded mapping from its
  owner identity to the opaque terminal caller/authority seam.
- **Strictness:** No compatibility aliases, no fallback mechanisms, and no duplicate ownership.

## Summary of State
- **Current Facts:** terminal execution, PTY, protocol, client, service,
  launcher, and interactive UI sources are owned by the independent terminal
  repository. `agl-process` is the narrow agent-side endpoint adapter.
- **Selected Target:** all eight selected packages are terminal-owned;
  `agl-terminal-ui` is the sole bounded Agent SDK consumer.
