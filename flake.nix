{
  description = "agentLIBRE (agl): a free and open-source agentic system";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The human's machine only (AGL-040, D5).
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;
      cargo = lib.importTOML ./Cargo.toml;

      # The helper programs agl runs by their absolute paths (AGL-040;
      # AGL-039, D15). The engines, systemd-run and systemctl stay the
      # system's (AGL-039, D16).
      tools = pkgs.runCommand "agentLIBRE-tools" { } ''
        mkdir -p $out/bin
        ln -s ${pkgs.bash}/bin/bash $out/bin/bash
        ln -s ${pkgs.git}/bin/git $out/bin/git
        ln -s ${pkgs.curl}/bin/curl $out/bin/curl
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
              --set AGENTLIBRE_NIX $out/share/agentLIBRE/nix
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
      # policies, formats, models and Functions (AGL-039, D7, D35).
      lib = import ./nix;

      packages.${system} = {
        default = agl;
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
        # The checkout's built-ins, so a change of them is seen without a
        # build (AGL-039, D7).
        shellHook = ''
          export AGENTLIBRE_NIX="$(git rev-parse --show-toplevel)/nix"
        '';
      };

      checks.${system} = {
        # The build and `cargo test`.
        package = agl;
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
