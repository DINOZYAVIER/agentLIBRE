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
asks again.

A Function is the profile of a run, written in Nix: a function of
`{ agl, ... }` that returns its `policy` (the system prompt, the states and
their tools, the limits) and its `model` (the server kind, the format, the
weights and the server's parameters). The built-in ones are
`agentLIBRE-gpu-high` (the default) and `agentLIBRE-gpu-high-minimal`; yours
go in `~/.config/agentLIBRE/functions/<name>.nix` or the project's
`functions/`, and take parts from `agl` (`agl.policies.agentLIBRE`,
`agl.formats."qwen3.8"`, `agl.models."qwen3.8-27b-iq4_xs"`):

```nix
{ agl, ... }: {
  policy = agl.policies.agentLIBRE;
  model = agl.models."qwen3.8-27b-iq4_xs" // { context = 65536; };
}
```

`agl function list` shows them and `agl function use <name>` picks the one
the runs load (`/function <name>` in a session starts a new one with it).
The weights are looked for by their file name in
`~/.local/share/agentLIBRE/models/`, then in the folders of `models` in
your settings; a file found nowhere is asked for in a terminal, and its
folder is kept. The settings are `settings.nix`, yours
(`~/.config/agentLIBRE/`) and the project's: where each kind of server
listens (`server.llama.url`, `server.strata.url`), whisper, the search
service, the interface. agl writes these files itself when you choose a
Function or name a folder. Images, video and audio go with a message:
Ctrl-V or `/attach <path>` in a session, `--attach <path>` with `-p`; an
audio the model does not hear goes through whisper.cpp (`whisper`), the
images through the model's `mmproj`.

agl needs Nix. It runs its helper programs (`bash`, `git`, `curl`,
`ffmpeg`, `ffprobe`, `wl-paste`) from the directory its package names in
`AGENTLIBRE_TOOLS`; the commands of the agent see your own `PATH`. Build
with `nix build` (`result/bin/agl`), run with `nix run . -- -p "..."`,
develop in `nix develop` (`cargo test`, `cargo clippy --all-targets`), and
check with `nix flake check`. In a NixOS or home-manager flake, add the input
`agl-lite` (with `inputs.nixpkgs.follows = "nixpkgs"`) and its
`packages.x86_64-linux.default` to the packages.
