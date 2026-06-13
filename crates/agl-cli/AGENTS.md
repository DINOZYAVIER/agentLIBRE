# AGENTS.md

This crate owns the user-facing `agentLIBRE` CLI entrypoint.
Keep it thin: parse command-line input, load config, and delegate preparation
and inference to the owning crates.
