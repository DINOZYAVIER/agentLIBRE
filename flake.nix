{
  description = "agentLIBRE runtime and local Vulkan development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    llamaCpp = {
      url = "github:DINOZYAVIER/llama.cpp/cab1892bc9847f2ba17efdc5f74cd955d8995748";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, llamaCpp }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          lib = pkgs.lib;
          rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;

          llamaServer = pkgs.stdenv.mkDerivation {
            pname = "agentlibre-llama-server";
            version = "cab1892";
            src = llamaCpp;

            nativeBuildInputs = with pkgs; [
              autoPatchelfHook
              cmake
              glslang
              ninja
              pkg-config
              python3
              shaderc
              spirv-headers
              spirv-tools
            ];

            buildInputs = with pkgs; [
              libdrm
              libglvnd
              libx11
              libXau
              libXdmcp
              libxcb
              libxkbcommon
              libxshmfence
              vulkan-headers
              vulkan-loader
              wayland
              zlib
              zstd
            ];

            cmakeFlags = [
              "-DGGML_BACKEND_DL=ON"
              "-DGGML_CPU_ALL_VARIANTS=ON"
              "-DGGML_NATIVE=OFF"
              "-DGGML_VULKAN=ON"
              "-DLLAMA_BUILD_TESTS=OFF"
              "-DLLAMA_BUILD_EXAMPLES=OFF"
              "-DLLAMA_BUILD_TOOLS=ON"
              "-DLLAMA_BUILD_SERVER=ON"
              "-DLLAMA_BUILD_UI=OFF"
              "-DLLAMA_USE_PREBUILT_UI=OFF"
              "-DLLAMA_BUILD_APP=OFF"
              "-DMTMD_VIDEO=OFF"
              "-DCMAKE_BUILD_RPATH_USE_ORIGIN=ON"
              "-DCMAKE_BUILD_WITH_INSTALL_RPATH=ON"
              "-DCMAKE_INSTALL_RPATH=$ORIGIN"
              "-DVulkan_INCLUDE_DIR=${pkgs.vulkan-headers}/include"
              "-DVulkan_LIBRARY=${pkgs.vulkan-loader}/lib/libvulkan.so"
              "-DVulkan_GLSLC_EXECUTABLE=${pkgs.shaderc.bin}/bin/glslc"
              "-DVulkan_GLSLANG_VALIDATOR_EXECUTABLE=${pkgs.glslang}/bin/glslangValidator"
              "-DCMAKE_PREFIX_PATH=${pkgs.spirv-headers}:${pkgs.spirv-tools}"
              "-DCMAKE_CXX_FLAGS=-I${pkgs.spirv-headers}/include"
            ];

            buildPhase = ''
              runHook preBuild
              cmake --build . --target llama llama-common mtmd llama-server --parallel "$NIX_BUILD_CORES"
              runHook postBuild
            '';

            installPhase = ''
              runHook preInstall
              install -d "$out/libexec/agentlibre"
              install -m 0755 bin/llama-server "$out/libexec/agentlibre/llama-server"
              shopt -s nullglob
              libraries=(bin/lib*.so*)
              shopt -u nullglob
              if (( ''${#libraries[@]} == 0 )); then
                echo "llama.cpp build produced no shared libraries" >&2
                exit 1
              fi
              cp -a "''${libraries[@]}" "$out/libexec/agentlibre/"
              runHook postInstall
            '';

            dontStrip = true;
          };

          runtimeTools = with pkgs; [
            bubblewrap
            coreutils
            findutils
            git
            gnugrep
            gnused
            tmux
          ];

          agentlibre = pkgs.rustPlatform.buildRustPackage {
            pname = "agentlibre";
            inherit version;
            src = self;
            cargoLock = {
              lockFile = "${self}/Cargo.lock";
              outputHashes."ayeque-forge-core-0.1.0-alpha.1" = "sha256-j40pYjHjgBAWBbfz7ydlNmXWf4vPIAmXX1XDeoDKuhE=";
            };

            nativeBuildInputs = with pkgs; [ cmake makeWrapper pkg-config perl ];
            cargoBuildFlags = [ "--workspace" ];
            doCheck = false;

            installPhase = ''
              runHook preInstall
              install -d "$out/bin" "$out/libexec/agentlibre"
              install -m 0755 "target/${rustTarget}/release/agl" "$out/bin/agl.real"
              install -m 0755 "target/${rustTarget}/release/agl-matrix-bridge" "$out/bin/agl-matrix-bridge.real"
              makeWrapper "$out/bin/agl.real" "$out/bin/agl" \
                --prefix PATH : "${lib.makeBinPath runtimeTools}"
              makeWrapper "$out/bin/agl-matrix-bridge.real" "$out/bin/agl-matrix-bridge" \
                --prefix PATH : "${lib.makeBinPath runtimeTools}"
              cp -a "${llamaServer}/libexec/agentlibre/." "$out/libexec/agentlibre/"
              runHook postInstall
            '';

            meta = {
              description = "Local-first agent runtime with systemd/tmux execution and private inference";
              homepage = "https://github.com/agentlibre/agentLIBRE";
              license = [ lib.licenses.lgpl3Plus lib.licenses.mit ];
              mainProgram = "agl";
              platforms = systems;
            };
          };
        in
        {
          default = agentlibre;
          agl = agentlibre;
          llama-server = llamaServer;
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = import ./nix/agl-local-vulkan.nix { inherit pkgs; };
        });
    };
}
