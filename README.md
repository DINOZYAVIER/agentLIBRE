# agentLIBRE

agentLIBRE is a local-first agentic system. That's where I put everything needed for running agentic AI on your computer.

## Why agent?

It's agentic, which means you enable working with your computer agentically. Today, this agency is held by Large Language Models, which are also referred to as Artificial Intelligence.

## Why LIBRE?

It's free (libre) and open-source. There are no reasons for making agentLIBRE proprietary software. I think we should have as much FOSS for everything as possible.

## What it does

By running `agl` in CLI, you will open a REPL-chat with your agent. If it's your first time running the command, it will help you download model files to run agentLIBRE.

In the REPL-chat you can tell your agent to read and edit your files, as well as use the terminal. These basic operations are enough for performing almost anything.

There are also additional tools. `agl memory` helps you with providing your agent with cross-session knowledge. `agl forge` is used for managing artifacts for workspaces.

If you don't want your agent to do something, make it impossible for it to do. You need to have your security enforced and/or consider containerization.

## How it does

### agentFUNCTION

What model, what settings and what agent is defined by a Function, a small Nix file you can replace. There are pre-installed Functions that you can use to create the Functions you need.

A Function specifies the system prompt, states, and limits of steps and tokens. It also declares the kind of inference engine used for running your model, the weights of this model, the context size, the arguments for running the server and the sampling parameters, as well as where you can download these weights.

agl has its store in XDG. It creates it when you run `agl` in a folder for the first time. That's where it stores `settings.nix` for this project, memory and forge. If you don't specify a Function by running `agl --function <name>`, it will choose the Function from `settings.nix`. If there is no Function in `settings.nix`, it will be taken from your global `settings.nix`.

You can change the Function in your workspace-specific or global `settings.nix` by running `agl function use <name>` (adding `--user` changes the global `settings.nix`). If none of those declare a Function, it will run the hardcoded `agentLIBRE-cpu-low`.

### Inference

agl uses HTTP to connect to an inference engine, whether it's llama.cpp or Strata. If nothing answers at the address, agl starts a new server itself, with the arguments of the Function. It also closes the server that agl started by itself when you close the REPL-chat.

In the case of llama.cpp, agl builds a grammar from the state the agent is in and sends it with the request. It helps ensure that the model does what the state allows. Tool calls are more likely to be valid this way.

Strata has no grammar. agl checks the structure of the reply after it is generated and asks the model again, up to a specified number of times (`max_stirs` in a Function).

### Finite state machines

agentLIBRE utilizes finite state machines in different components. They can provide more determinism as a remedy for one of the issues with various applications of LLMs.

There is an agent FSM that steers the agent. Each state specifies what the agent may do at the moment: what tools are available, can it answer, can it think, and how many steps it may spend there. The grammar (only for llama.cpp) is built from these allowed actions. For now built-in Functions feature only one state but this FSM will have more applications in the future.

There is also a turn FSM that observes what happens during a turn: the server starting, the prompt being read, thinking, writing, a tool running, compacting, a retry. It is used to provide both agent and user with observability of agl working via logging these states into the session file. agl is capable of accessing its logs and working with them.

## Install

agl runs on Linux with Nix and on Windows in NixOS-WSL. It is built from source on the first install, as there is no binary cache yet.

### NixOS

Add the input to your flake and the module to your system:

```nix
inputs.agentlibre = {
  url = "github:DINOZYAVIER/agentLIBRE";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

```nix
programs.agl.enable = true;
```

### Linux

You need [Nix](https://nixos.org/download/) with flakes, and systemd.

```sh
git clone https://github.com/DINOZYAVIER/agentLIBRE
cd agentLIBRE
nix build
./result/bin/agl
```

### macOS

To be tested.

### Windows

Run [`install-agl.cmd`](windows/install-agl.cmd). It installs WSL, NixOS-WSL and agl, and may ask for administrator rights and a restart. If something fails, run it again.

It needs Windows 11 22H2 or later, virtualization on in the BIOS and a GPU driver. Then:

```powershell
wsl -d NixOS
agl
```

### External server

`agl` can use a llama-server running on another machine. Set `programs.agl.package = inputs.agentlibre.packages.x86_64-linux.outside;` and name the server in `~/.config/agentLIBRE/settings.nix`:

```nix
{ agl, ... }: { server.llama = { url = "http://HOST:PORT"; external = true; }; }
```

`agl server command --function <name>` prints the line that starts the server there. Use a network you trust: llama-server has no password by default.

## License

LGPL-3.0-or-later. See [LICENSE.md](LICENSE.md).
