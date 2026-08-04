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
| agl-process | Runtime (temporary consumer facade; removed in Step 04) |
| agl-protocol | Communication |
| agl-repo | Storage |
| agl-runtime | Runtime |
| agl-session | Runtime |
| agl-skill | Extensibility |
| agl-store | Storage |
| agl-supervisor | Runtime |
| agl-turn | Runtime |

## Future Packages (Owner: agl-terminal)

### Selected Targets

**In-tree split packages (Step 02):**
- `agl-exec`
- `agl-pty`
- `agl-terminal`
- `agl-terminal-protocol`
- `agl-terminal-client`

These packages are still owned by the selected future `agl-terminal`
repository. Their temporary workspace sources exist only to build the
extraction seam before the Step 03 immutable Git cutover.

**In-tree split binary packages (Step 02):**
- `agl-process-launcher`
- `agl-terminald`

**Selected binary package introduced in Step 05:**
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
- **Current Facts:** 37 packages remain owned by `agentLIBRE`; `agl-process` is
  now only the temporary Step 04 consumer facade.
- **Selected Target:** 8 packages for `agl-terminal` ownership. Seven are
  present as in-tree split packages; `agl-terminal-ui` is introduced in Step
  05.
