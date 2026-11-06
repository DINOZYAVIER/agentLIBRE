# A whole NixOS-WSL system with agl (AGL-056, D4), for one command on a fresh one:
#   sudo nixos-rebuild switch --flake github:DINOZYAVIER/agentLIBRE#wsl
{ pkgs, ... }:
{
  wsl.enable = true;
  # WSL generates wsl-mnt-guard.service with `ExecStart=/bin/true`. With no such
  # file the unit fails, and a switch that is done returns 4 (AGL-057).
  wsl.extraBin = [ { src = "${pkgs.coreutils}/bin/true"; } ];
  programs.agl.enable = true;
  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  # The release of the `nixos.wsl` this is for (2605.7.2).
  system.stateVersion = "26.05";
}
