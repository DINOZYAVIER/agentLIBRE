# AGENTS.md

This component owns backend-neutral rendered model request structures and
dialect-specific tool-call transcript formatting.
When adding a dialect, tool-call format, or rendered artifact, add round-trip or
golden-style tests for the exact output. Final backend chat-template application
belongs in `agl-inference`.
