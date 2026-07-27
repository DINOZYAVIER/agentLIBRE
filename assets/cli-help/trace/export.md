# `agl trace export`

Read canonical runtime event JSONL and immutable runtime identity JSON, validate
the event sequence, and atomically write one deterministic semantic trace.

Provider-formatted transcripts and synthetic training rows are rejected because
they are not canonical runtime events.
