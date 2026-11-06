# agl on NixOS (AGL-056, D2): `programs.agl.enable`. On NixOS-WSL the package is
# the one whose server runs on Windows, and `wslinfo` and `wslpath`, which agl
# calls, are put on the PATH: only WSL's /init is there, under other names.
{ packages }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.agl;
  wsl = config.wsl.enable or false;
  wslTool = name: pkgs.writeShellScriptBin name ''exec -a ${name} /init "$@"'';
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
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages =
      [
        cfg.package
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
