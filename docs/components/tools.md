# Tools and Extensions

`agl-core` owns execution-free `ToolDefinition`, `ExtensionDefinition`,
admission provenance, Effect grants, receipts, and FSM-facing request/result
data. A Tool declaration contains only its ID, model-facing description,
bounded JSON input schema, required Effect IDs, and delivery class.

`agl-runtime::extension` parses declarative `EXTENSION.toml` files and provides the
small native binding SDK. It exposes object-safe
`ToolHandler`, `ToolBinding`, `ExtensionBindings`, and `ToolContext` without a
generic host object or downcasting. The daemon composition root supplies exact
bindings to `AgentService::start`; `agl-daemon` validates their definition
digests and invokes only Tools present in the immutable Run snapshot.

The private `agl-daemon::tools` modules supply two filesystem handlers: bounded
UTF-8 read and digest-preconditioned atomic patch. `fs_read` requires `cursor`
(initially `1`) and `limit_lines`; the schema does not supply hidden defaults.
Directory discovery and text search use the confined `command.exec` provider,
so the Function's command permission and workspace boundary apply uniformly.
Every Tool result is measured after serialization; oversized results fail with
`result_too_large` instead of being sliced. Their declaration is
`extensions/agentlibre-builtins/EXTENSION.toml`; the compiled binding embeds
those exact bytes and content digest. Registering handlers does not admit them
to a Run: the Function must explicitly select the Extension and Tool IDs, and
patch requires the exact workspace-write Effect grant.

`extensions/agentlibre-execution` declares `command.exec` and
`terminal.session`. `agl-daemon` supplies their stock native bindings after
normal Tool and Effect admission, and delegates every process and PTY operation
to `agl-execd` through `agl-execution-api`.

`extensions/agentlibre-searxng` declares the retryable
`agentlibre.searxng:search` Tool and its external-query Effect. Its trusted
binding lives in `agl-daemon::tools`: it sends only the fixed AYEQUE agent
search request, uses private-root mTLS without redirects or ambient proxies,
and independently validates the bounded normalized response. The model cannot
select an endpoint, engine, raw SearXNG parameter, credential, or unadmitted
source class. Search returns result metadata and snippets; it does not fetch
result pages.
