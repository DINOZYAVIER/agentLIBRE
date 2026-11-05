# agentLIBRE

agentLIBRE is a local-first agentic system.

## Why agent?

It utilizes LLM-powered agents, so that's why.

## Why LIBRE?

It's free (libre) and open-source.

## Install

On Linux and in WSL, one command installs Nix when it is missing, then agl:

```sh
curl -fsSL https://raw.githubusercontent.com/DINOZYAVIER/agentLIBRE/master/install.sh | sh
```

Then run `agl`. `agl function list` shows the Functions, by the memory they take, and `agl function use <name>` picks one; agl offers to download the files it needs.

- **Linux:** the package has its own `llama-server`, with Vulkan drivers for AMD and Intel cards. NVIDIA is not checked yet.
- **Windows:** in WSL, agl starts the official Windows `llama-server.exe`, which sees the GPU directly. WSL reaches it in the mirrored network: set `networkingMode=mirrored` under `[wsl2]` in `%UserProfile%\.wslconfig`, then run `wsl --shutdown`. This path is not checked on a real Windows yet.
- **A server outside the machine,** as with a Linux virtual machine on another system: `sh install.sh --outside` installs agl alone. `agl server command --function <name>` prints the line that starts the server there and the links to its files, and `~/.config/agentLIBRE/settings.nix` names it:

  ```nix
  { agl, ... }: { server.llama = { url = "http://HOST:PORT"; external = true; }; }
  ```

  agl then neither starts nor stops that server, and refuses if it serves another model. Reach it over a network you trust: llama-server has no password by default.

## License

LGPL-3.0-or-later. See [LICENSE.md](LICENSE.md).
