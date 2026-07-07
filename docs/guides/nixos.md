# NixOS

On Nix/NixOS machines, use `scripts/agl-nix-vulkan.sh` to enter a development environment with Rust, CMake, Vulkan, SPIR-V, and llama.cpp runtime variables.

Useful commands:

```sh
scripts/agl-nix-vulkan.sh --build
scripts/agl-nix-vulkan.sh --diagnose
scripts/agl-nix-vulkan.sh -- ./target/debug/agl config paths
```
