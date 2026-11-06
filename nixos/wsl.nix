# A whole NixOS-WSL system with agl (AGL-056, D4), for one command on a fresh one:
#   sudo nixos-rebuild switch --flake github:DINOZYAVIER/agentLIBRE#wsl
{
  wsl.enable = true;
  programs.agl.enable = true;
  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  # The release of the `nixos.wsl` this is for (2605.7.2).
  system.stateVersion = "26.05";
}
