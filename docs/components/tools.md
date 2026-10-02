# Tools and Extensions

`agl-core` owns execution-free `ToolDefinition`, `ExtensionDefinition`,
admission provenance, Effect grants, receipts, and FSM-facing request/result
data. A Tool declaration contains only its ID, model-facing description,
bounded JSON input schema, required Effect IDs, and delivery class.

`agl-runtime::extension` parses declarative `EXTENSION.toml` files.
`agl-tool-api` provides the native binding SDK and exposes object-safe
`ToolHandler`, `ToolBinding`, `ExtensionBindings`, and `ToolContext` without a
generic host object or downcasting. The daemon composition root supplies exact
bindings to `AgentService::start`; `agl-daemon` validates their definition
digests and invokes only Tools present in the immutable Run snapshot.

`agl-tools` supplies the `agentlibre.files` Extension (bounded UTF-8
`agentlibre.files:file.read` and digest-preconditioned atomic
`agentlibre.files:file.patch`) and the `agentlibre.forge` Extension
(`agentlibre.forge:artifact.read` for declared document and memory
artifacts). `agentlibre.files:file.read` requires `cursor`
(initially `1`) and `limit_lines`; the schema does not supply hidden defaults.
Directory discovery and text search use `command.exec`, which checks the
admitted executable and contains its working directory within the workspace.
Its Standard execution mode does not install filesystem isolation for command
arguments. Planner Runs request `ReadOnlyWorkspace` instead: execd supplies
a read-only reflink view and a writable scratch directory using the Linux/Nix
profile described in [Execution](execution.md). The executable Effect
allowlist applies in both modes; planner commands have no separate command
whitelist. Filesystem and Forge reads paginate within the result budget;
the driver measures every complete Tool result after serialization. For a
committed effect, stock execution/search bindings preserve the receipt and
can replace oversized content with a bounded notice. The declarations are
`extensions/agentlibre-files/EXTENSION.toml` and
`extensions/agentlibre-forge/EXTENSION.toml`; each compiled binding embeds
those exact bytes and content digest. Registering handlers does not admit them
to a Run: the Function must explicitly select the Extension and Tool IDs, and
patch requires the exact workspace-write Effect grant.

`extensions/agentlibre-execution` declares `command.exec` and
`terminal.session`. `agl-tools::execution` supplies their stock native bindings; daemon retains
Tool and Effect admission. The handlers delegate every process and PTY operation
to `agl-execd` through `agl-execution-api`.

`extensions/agentlibre-searxng` declares the retryable
`agentlibre.searxng:search` Tool and its external-query Effect. Its trusted
binding lives in `agl-tools::searxng`: it sends only the fixed AYEQUE agent
search request, uses private-root mTLS without redirects or ambient proxies,
and independently validates the bounded normalized response. The model cannot
select an endpoint, engine, raw SearXNG parameter, credential, or unadmitted
source class. Search returns result metadata and snippets; it does not fetch
result pages.

`agl-tools` has no dependency on runtime or inference. The daemon parses its
embedded declarations with `agl-runtime::extension`, then passes version,
package digest, and `ExtensionDefinition` to each binding factory. Factories
check the embedded package digest and supported Tool IDs. Filesystem bindings
also receive an artifact resolver and a workspace mutation-lock callback.
The daemon owns artifact resolution and the shared lock registry used by both
patch handlers and memory commits.

For planner commands the daemon also supplies `ToolContext.output_directories`;
execution bindings forward it to execd with the read-only workspace root.
This list comes from the workspace manifest and is not inferred by handlers.
