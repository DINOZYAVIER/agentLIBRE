# agentLIBRE

agentLIBRE is a local-first agentic system.

## Why agent?

It utilizes LLM-powered agents, so that's why.

## Why LIBRE?

It's free (libre) and open-source.

## What it does

`agl` runs a model on your own machine and works in your project through tools: it runs commands, edits and reads files, searches, keeps a memory of the project and task specs. A message can carry images, video and audio, and Ctrl-R dictates one by voice. What the agent is and which model it runs are a **Function**, a small Nix file you can replace.

## Install

agl runs on NixOS. On Windows, that is NixOS-WSL. It is built from source on the first install, as there is no binary cache yet.

### NixOS

In your flake:

```nix
inputs.agentlibre = {
  url = "github:DINOZYAVIER/agentLIBRE";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Then add `inputs.agentlibre.nixosModules.default` to the modules of your system, with:

```nix
programs.agl.enable = true;
```

The module turns on `hardware.graphics`, as the server runs on Vulkan. AMD is checked; NVIDIA and Intel are not yet.

### Windows

agl runs in NixOS-WSL, and starts the official Windows `llama-server.exe`, which sees the GPU directly. It needs Windows 11 22H2 or later and WSL 2.4.4 or later. This path is not checked on a real Windows yet.

1. In `%UserProfile%\.wslconfig`:

   ```ini
   [wsl2]
   networkingMode=mirrored
   ```

2. In PowerShell, with [`nixos.wsl`](https://github.com/nix-community/NixOS-WSL/releases/latest) downloaded:

   ```powershell
   wsl --install --from-file nixos.wsl
   ```

3. In NixOS:

   ```sh
   sudo nixos-rebuild switch --flake github:DINOZYAVIER/agentLIBRE#wsl
   ```

### A server on another machine

On NixOS, `programs.agl.package = inputs.agentlibre.packages.x86_64-linux.outside;` installs agl with no server of its own. `agl server command --function <name>` prints the line that starts the server over there and the links to its files, and `~/.config/agentLIBRE/settings.nix` names it:

```nix
{ agl, ... }: { server.llama = { url = "http://HOST:PORT"; external = true; }; }
```

agl then neither starts nor stops that server, and refuses if it serves another model. Reach it over a network you trust: llama-server has no password by default.

## Run

```sh
agl
```

agl offers to download the files of the Function it needs. `agl function list` shows the Functions, and `agl function use <name>` picks one:

| Function | Model | Context | Memory | Download |
| --- | --- | --- | --- | --- |
| `agentLIBRE-cpu-low` | Gemma 4 E2B | 32K | 4.1 GB RAM | 3.7 GB |
| `agentLIBRE-cpu-medium` | Gemma 4 E4B | 32K | 6.0 GB RAM | 5.3 GB |
| `agentLIBRE-cpu-high` | Gemma 4 E4B | 64K | 6.3 GB RAM | 5.3 GB |
| `agentLIBRE-cpu-very-high` | Gemma 4 E4B | 128K | 6.8 GB RAM | 5.3 GB |
| `agentLIBRE-gpu-low` (default) | Gemma 4 E4B | 128K | 5.5 GB VRAM | 5.3 GB |
| `agentLIBRE-gpu-medium` | Gemma 4 12B | 128K | 8.9 GB VRAM | 7.1 GB |
| `agentLIBRE-gpu-high` | Qwen3.8 27B | 128K | 20.1 GB VRAM | 15.2 GB |
| `agentLIBRE-gpu-very-high` | Qwen3.8 27B | 256K | 24.4 GB VRAM | 15.2 GB |

The memory was measured on one machine with an RX 7900 XTX, on top of what the desktop takes. The Functions live in [agentLIBRE-functions](https://github.com/DINOZYAVIER/agentLIBRE-functions).

## License

LGPL-3.0-or-later. See [LICENSE.md](LICENSE.md).
