# agentLIBRE

agentLIBRE -- free and open-source agentic system.

A simple, concise coding agent for local models. It uses finite-state
machines to make local models more deterministic, without heavy
guard rails: the tools it calls keep their own protections.

The command is `agl` (also installed as `agentLIBRE`). Work in
progress: `agl -p "..."` runs one turn; `agl` alone opens the interactive
session (a line editor, a dock with the state and the queue, streamed
thinking and text, tool cards). agl starts the project's server itself when
a turn finds none, or `agl server start` runs it as a systemd user unit:
llama-server, whose actions are constrained by a grammar, or the Strata
server for large MoE models, where the state machine checks each action and
asks again. A Function is the profile of a run — the system prompt, the
states and their tools, the limits, and, if it says so, the server kind and
the model; `agl function list` shows them and `agl function use <name>`
picks the one the runs load (`/function <name>` in a session starts a new
one with it). The settings are `agentLIBRE.toml`, the user's and the
project's: where each kind of server listens (`[server.llama]`,
`[server.strata]`), the model of a Function that names none, whisper and the
search service. Images, video and audio go with a message: Ctrl-V or
`/attach <path>` in a session, `--attach <path>` with `-p`; an audio the
model does not hear goes through whisper.cpp (`[whisper]`), the images
through `mmproj` of `[server.llama]`.

agl needs Nix. It runs its helper programs (`bash`, `git`, `curl`,
`ffmpeg`, `ffprobe`, `wl-paste`) from the directory its package names in
`AGENTLIBRE_TOOLS`; the commands of the agent see your own `PATH`. Build
with `nix build` (`result/bin/agl`), run with `nix run . -- -p "..."`,
develop in `nix develop` (`cargo test`, `cargo clippy --all-targets`), and
check with `nix flake check`. In a NixOS or home-manager flake, add the input
`agl-lite` (with `inputs.nixpkgs.follows = "nixpkgs"`) and its
`packages.x86_64-linux.default` to the packages.
