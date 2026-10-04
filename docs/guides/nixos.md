# Nix and NixOS

The repository Flake pins nixpkgs and the private llama.cpp revision. Its
default development shell provides the Rust, GNU, systemd/tmux, CMake and
Vulkan tools used by the build scripts. The `agl` package contains both AGL
binaries and the matching Vulkan-enabled `llama-server` runtime.

Useful commands:

```sh
nix develop
nix build .#agl
nix run .#agl -- --version
nix profile install .#agl
scripts/agl-nix-vulkan.sh --build
scripts/agl-nix-vulkan.sh --diagnose
scripts/agl-nix-vulkan.sh -- cargo test --workspace
```

The package installs `agl` and `agl-matrix-bridge` into its Nix output. The
inference executable is at `libexec/agentlibre/llama-server` in that same
output, so the default engine lookup works for a Nix profile installation.
The user-systemd manager remains provided by the host; installing the package
does not enable or start user services.
