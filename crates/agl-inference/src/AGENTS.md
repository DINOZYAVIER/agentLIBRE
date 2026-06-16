# AGENTS.md

This component owns backend traits, evidence writes, and llama.cpp runtime execution.
Preserve evidence writes for every attempted inference path: request, response or failure, stderr, and events.
