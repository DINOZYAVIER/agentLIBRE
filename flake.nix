{
  description = "agentLIBRE (agl): a free and open-source agentic system";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  # The engine that comes with the package (AGL-052).
  inputs.llama-cpp.url = "github:ggml-org/llama.cpp/v0.9.0";
  # The Functions that come with the package (AGL-053).
  inputs.functions = {
    url = "github:DINOZYAVIER/agentLIBRE-functions";
    flake = false;
  };

  outputs =
    { self, nixpkgs, llama-cpp, functions }:
    let
      # Linux on x86-64, also in WSL (AGL-052, D1, D2).
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;
      cargo = lib.importTOML ./Cargo.toml;

      # The helper programs agl runs by their absolute paths (AGL-040;
      # AGL-039, D15). systemd-run and systemctl stay the system's (AGL-039,
      # D16); the engines are below (AGL-052).
      tools = pkgs.runCommand "agentLIBRE-tools" { } ''
        mkdir -p $out/bin
        ln -s ${pkgs.bash}/bin/bash $out/bin/bash
        ln -s ${pkgs.git}/bin/git $out/bin/git
        ln -s ${pkgs.curl}/bin/curl $out/bin/curl
        ln -s ${pkgs.coreutils}/bin/sha256sum $out/bin/sha256sum
        ln -s ${pkgs.ffmpeg}/bin/ffmpeg $out/bin/ffmpeg
        ln -s ${pkgs.ffmpeg}/bin/ffprobe $out/bin/ffprobe
        ln -s ${pkgs.wl-clipboard}/bin/wl-paste $out/bin/wl-paste
        ln -s ${pkgs.step-cli}/bin/step $out/bin/step
      '';

      # Only what the build reads: a change of the docs does not rebuild.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./nix
        ];
      };

      # llama-server of the pinned version. Outside NixOS it finds no Vulkan
      # driver of the system, and none of them works from the store (AGL-052,
      # measured on Ubuntu): it takes Mesa from the store, on AMD and Intel.
      linuxEngine = pkgs.writeShellScriptBin "llama-server" ''
        if [ -z "''${VK_DRIVER_FILES:-}" ] && [ ! -d /run/opengl-driver ]; then
          export VK_DRIVER_FILES=${pkgs.mesa}/share/vulkan/icd.d/radeon_icd.x86_64.json:${pkgs.mesa}/share/vulkan/icd.d/intel_icd.x86_64.json
        fi
        exec ${llama-cpp.packages.${system}.vulkan}/bin/llama-server "$@"
      '';

      # The official Windows release of the same commit as v0.9.0, for WSL,
      # which runs it from the store (AGL-052, D9).
      windowsEngine = pkgs.runCommand "llama-server-windows-b12110"
        {
          src = pkgs.fetchzip {
            url = "https://github.com/ggml-org/llama.cpp/releases/download/b12110/llama-b12110-bin-win-vulkan-x64.zip";
            hash = "sha256-TjgWYdERy0toaWcoebLfCiWucRb2d4Pp/mHBx+TMl4I=";
            stripRoot = false;
          };
        }
        ''
          mkdir -p $out/bin
          cp -r $src/. $out/bin/
          chmod -R u+w,a+rx $out/bin
        '';

      # agl with the engine of a machine: the one build, wrapped (AGL-052).
      withEngine =
        name: engine:
        pkgs.runCommand name
          {
            nativeBuildInputs = [ pkgs.makeWrapper ];
            meta = agl.meta;
          }
          ''
            mkdir -p $out/bin
            for bin in agl agentLIBRE; do
              makeWrapper ${agl}/bin/$bin $out/bin/$bin \
                --set AGENTLIBRE_ENGINES ${engine}/bin
            done
            ln -s ${agl}/share $out/share
          '';

      agl = pkgs.rustPlatform.buildRustPackage {
        pname = cargo.package.name;
        version = cargo.package.version;
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.makeWrapper ];
        # The tests make git repositories of their own, the agent's bash
        # runs tmux in one of them, and they evaluate Functions and settings
        # with the evaluator of the system (AGL-039, D38).
        nativeCheckInputs = [
          pkgs.git
          pkgs.tmux
          pkgs.nix
        ];
        # The build's terminal would page `git log` through a `less` the
        # sandbox does not have; the evaluations need no store.
        preCheck = ''
          export AGENTLIBRE_TOOLS=${tools}/bin
          export AGENTLIBRE_NIX=$PWD/nix
          export HOME=$TMPDIR
          export GIT_PAGER=cat
          export NIX_REMOTE=dummy://
        '';
        # The built-ins go with the package (AGL-039, D7); the wrapper names
        # them and the tools, and leaves PATH alone (AGL-039, D15).
        postInstall = ''
          mkdir -p $out/share/agentLIBRE
          cp -r nix $out/share/agentLIBRE/nix
          for bin in agl agentLIBRE; do
            wrapProgram $out/bin/$bin \
              --set AGENTLIBRE_TOOLS ${tools}/bin \
              --set AGENTLIBRE_NIX $out/share/agentLIBRE/nix \
              --set AGENTLIBRE_FUNCTIONS ${functions} \
              --set AGENTLIBRE_REV ${self.shortRev or self.dirtyShortRev or "unknown"}
          done
        '';
        meta = {
          description = cargo.package.description;
          license = lib.licenses.lgpl3Plus;
          mainProgram = "agl";
          platforms = [ system ];
        };
      };
    in
    {
      # The `agl` argument of the Functions and the settings: the built-in
      # policies, formats and models, and the Functions of the repository
      # agentLIBRE-functions (AGL-039, D7, D35; AGL-053).
      lib = import ./nix { inherit functions; };

      packages.${system} = {
        # Linux with its own server.
        default = withEngine "agentLIBRE" linuxEngine;
        # WSL, where the server runs on Windows (AGL-052, D10).
        wsl = withEngine "agentLIBRE-wsl" windowsEngine;
        # A machine whose server is outside it (AGL-052, D4).
        outside = agl;
        inherit tools;
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          pkgs.cargo
          pkgs.rustc
          pkgs.clippy
          pkgs.rustfmt
          pkgs.git
        ];
        AGENTLIBRE_TOOLS = "${tools}/bin";
        AGENTLIBRE_FUNCTIONS = "${functions}";
        # The checkout's built-ins, so a change of them is seen without a
        # build (AGL-039, D7).
        shellHook = ''
          export AGENTLIBRE_NIX="$(git rev-parse --show-toplevel)/nix"
        '';
      };

      checks.${system} = {
        # The build and `cargo test`.
        package = agl;
        # The pinned Functions all load, on the package's own `agl`, and one is
        # the default (AGL-053, D8). A line of `function list` with no kind in it
        # is the error of a Function that does not load.
        functions = pkgs.runCommand "${cargo.package.name}-functions"
          { nativeBuildInputs = [ pkgs.git pkgs.nix ]; }
          ''
            export HOME=$TMPDIR NIX_REMOTE=dummy:// GIT_PAGER=cat
            mkdir project && cd project && git init -q
            ${agl}/bin/agl function list > list
            cat list
            want=$(ls ${functions}/functions | wc -l)
            test "$want" -gt 0
            test "$(grep -c ' llama ' list)" = "$want"
            test "$(wc -l < list)" = "$want"
            grep -q '^\* .* llama ' list
            touch $out
          '';
        clippy = agl.overrideAttrs (old: {
          pname = "${cargo.package.name}-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = ''
            runHook preBuild
            cargo clippy --offline --all-targets -- -D warnings
            runHook postBuild
          '';
          doCheck = false;
          installPhase = "touch $out";
        });
      };
    };
}
