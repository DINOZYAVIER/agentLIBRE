# AGENTS.md

`agl-actions` parses model output into final answers, tool calls, or malformed tool-call records with conservative repair metadata.
Keep the public surface narrow; `parse_model_action` is the main external entrypoint.
