Delete only removed cache entries with exact agentLIBRE provenance.

The whole Hugging Face cache is never swept. Active bindings, setup state,
downloads, jobs, and model-manager leases remain protected.
