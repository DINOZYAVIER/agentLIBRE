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
scripts/agl-nix-profile.sh build
scripts/agl-nix-profile.sh run -- --version
scripts/agl-nix-profile.sh install
scripts/agl-nix-profile.sh history
scripts/agl-nix-profile.sh rollback
scripts/agl-nix-vulkan.sh --build
scripts/agl-nix-vulkan.sh --diagnose
scripts/agl-nix-vulkan.sh -- cargo test --workspace
```

`agl-nix-profile.sh install` builds and smoke-checks the committed branch
before adding AGL to the user profile. On later runs it upgrades that profile
entry, creating one new profile generation; a failed build leaves the active
generation alone. The script ignores uncommitted files and refuses tracked
changes. Use `rollback [GENERATION]` to return to a previous profile version;
`run -- --version` tests the branch without changing the profile.

The package installs `agl` and `agl-matrix-bridge` into its Nix output. The
inference executable is at `libexec/agentlibre/llama-server` in that same
output, so the default engine lookup works for a Nix profile installation.
The user-systemd manager remains provided by the host; installing the package
does not enable or start user services.
