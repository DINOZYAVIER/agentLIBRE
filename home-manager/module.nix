# agl's daemon as a home-manager user service (AGL-120, D4): the one
# resident driver of the planes' flows. It reads its socket under
# XDG_RUNTIME_DIR and holds the sessions' planes, so it is a user unit —
# it belongs to the home, not to the system. Where it is installed, `agl`
# asks systemd for that one daemon instead of forking its own, so a unit
# and a terminal process no longer race for the socket.
#
# `package` defaults to the system module's wrapped package (the one it
# installs, with `AGENTLIBRE_STRATA` when Strata is asked for); fall back
# to nixpkgs' agl. Set it outright to run a daemon of another build.
{
  config,
  lib,
  pkgs,
  osConfig ? { },
  ...
}:
let
  cfg = config.services.agl-daemon;
in
{
  options.services.agl-daemon = {
    enable = lib.mkEnableOption "agl daemon, the resident driver of the planes' flows";
    package = lib.mkOption {
      type = lib.types.package;
      default = osConfig.programs.agl.wrappedPackage or pkgs.agentLIBRE;
      defaultText = lib.literalExpression "osConfig.programs.agl.wrappedPackage or pkgs.agentLIBRE";
      description = "The agl package whose daemon runs.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.agl-daemon = {
      Unit = {
        Description = "agl daemon, the planes of agl's sessions and their flows";
        Documentation = [ "man:agl" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/agl daemon";
        Restart = "on-failure";
        RestartSec = "2s";
      };
      Install.WantedBy = [ "default.target" ];
    };
  };
}
