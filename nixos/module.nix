# agl on NixOS (AGL-056, D2): `programs.agl.enable`. On NixOS-WSL the package is
# the one whose server runs on Windows, and `wslinfo` and `wslpath`, which agl
# calls, are put on the PATH: only WSL's /init is there, under other names.
# `programs.agl.strata` builds the Strata variants asked for and names them to
# agl (AGL-089, D20); none is built otherwise.
{ packages }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.agl;
  wsl = config.wsl.enable or false;
  wslTool = name: pkgs.writeShellScriptBin name ''exec -a ${name} /init "$@"'';
  # The Strata packages by variant, in one folder agl reads by
  # AGENTLIBRE_STRATA: <folder>/<variant> (AGL-089, D20).
  strataPackages = {
    upstream = packages.strata;
    rdna3 = packages.strata-rdna3;
  };
  strata = pkgs.runCommand "agentLIBRE-strata" { } (
    "mkdir -p $out\n"
    + lib.concatMapStrings (variant: "ln -s ${strataPackages.${variant}} $out/${variant}\n") cfg.strata
  );
  package =
    if cfg.strata == [ ] then
      cfg.package
    else
      pkgs.symlinkJoin {
        name = "${cfg.package.name}-strata";
        paths = [ cfg.package ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postBuild = ''
          for bin in agl agentLIBRE; do
            rm $out/bin/$bin
            makeWrapper ${cfg.package}/bin/$bin $out/bin/$bin --set AGENTLIBRE_STRATA ${strata}
          done
        '';
      };
in
{
  options.programs.agl = {
    enable = lib.mkEnableOption "agl, the agentic system agentLIBRE";
    package = lib.mkOption {
      type = lib.types.package;
      default = if wsl then packages.wsl else packages.default;
      defaultText = lib.literalExpression "the `wsl` package on NixOS-WSL, else `default`";
      description = "The package of agl.";
    };
    strata = lib.mkOption {
      type = lib.types.listOf (lib.types.enum [ "upstream" "rdna3" ]);
      default = [ ];
      example = [ "rdna3" ];
      description = ''
        The Strata engines the Strata Functions run on an AMD RDNA3 card (gfx1100):
        `upstream` (Niko1221/Strata), `rdna3` (nicejunjie/Strata-rdna3, tuned for the
        RX 7900 XTX). Each is built on this machine for its CPU, with nixpkgs' ROCm.
      '';
    };
    # The package as it runs: `package`, wrapped with `AGENTLIBRE_STRATA`
    # when `strata` names a variant. Exported (internal) so a home-module
    # daemon runs exactly what the system installs (AGL-120, D4), not the
    # bare `package` — which would load with no engine to find.
    wrappedPackage = lib.mkOption {
      type = lib.types.package;
      internal = true;
      default = package;
      description = "The wrapped agl package the system installs.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages =
      [
        package
        pkgs.git
      ]
      ++ lib.optionals wsl [
        (wslTool "wslinfo")
        (wslTool "wslpath")
      ];
    # The Vulkan driver of the llama-server: on NixOS-WSL it runs on Windows.
    hardware.graphics.enable = lib.mkIf (!wsl) (lib.mkDefault true);
  };
}
