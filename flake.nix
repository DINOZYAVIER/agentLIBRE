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
      '';

      # Only what the build reads: a change of the docs does not rebuild.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
        ];
      };

      agl = pkgs.rustPlatform.buildRustPackage {
        pname = cargo.package.name;
        version = cargo.package.version;
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.makeWrapper ];
        # The tests make git repositories of their own, and the agent's
        # bash runs tmux in one of them.
        nativeCheckInputs = [
          pkgs.git
          pkgs.tmux
        ];
        # The build's terminal would page `git log` through a `less` the
        # sandbox does not have.
        preCheck = ''
          export AGENTLIBRE_TOOLS=${tools}/bin
          export HOME=$TMPDIR
          export GIT_PAGER=cat
        '';
        # The wrapper names the tools; it leaves PATH alone (AGL-039, D15).
        postInstall = ''
          for bin in agl agentLIBRE; do
            wrapProgram $out/bin/$bin --set AGENTLIBRE_TOOLS ${tools}/bin
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
