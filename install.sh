#!/bin/sh
# Installs agl: Nix when it is missing, then the package of this machine: in
# WSL the one whose server runs on Windows, elsewhere the one with its own
# server, or with --outside the one whose server is elsewhere.
#   sh install.sh [--outside]
# AGL_FLAKE names another source of the package, as a checkout.
set -eu

flake="${AGL_FLAKE:-github:DINOZYAVIER/agentLIBRE}"
nix_flags="--extra-experimental-features nix-command --extra-experimental-features flakes"

if [ "${1:-}" = "--outside" ]; then
  variant=outside
elif [ -n "${WSL_DISTRO_NAME:-}" ] || grep -qi microsoft /proc/version 2>/dev/null; then
  variant=wsl
else
  variant=default
fi

load_nix() {
  for file in /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh "$HOME/.nix-profile/etc/profile.d/nix.sh"; do
    if [ -e "$file" ]; then
      # shellcheck disable=SC1090
      . "$file"
      return
    fi
  done
}

command -v nix >/dev/null 2>&1 || load_nix
if ! command -v nix >/dev/null 2>&1; then
  echo "Installing Nix; it asks for sudo."
  if [ -d /run/systemd/system ]; then mode=--daemon; else mode=--no-daemon; fi
  curl --proto '=https' --tlsv1.2 -sSfL https://nixos.org/nix/install | sh -s -- "$mode" --yes
  load_nix
fi

# A variant installed before goes first: the profile names an entry by its
# attribute, so it is told by its store path.
# shellcheck disable=SC2086
nix $nix_flags profile list 2>/dev/null \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | awk '/^Name:/ { name = $2 } /^Store paths:/ && /agentLIBRE/ { print name }' \
  | while read -r name; do
      nix $nix_flags profile remove "$name" >/dev/null 2>&1 || true
    done
# `add` is the newer name of `install`.
# shellcheck disable=SC2086
nix $nix_flags profile add "$flake#$variant" 2>/dev/null || nix $nix_flags profile install "$flake#$variant"

# Where agl starts its server, unless the human has settings already. A server
# outside has its own address, and the human writes it.
settings="${XDG_CONFIG_HOME:-$HOME/.config}/agentLIBRE/settings.nix"
if [ "$variant" != outside ] && [ ! -e "$settings" ]; then
  mkdir -p "$(dirname "$settings")"
  cat > "$settings" <<'NIX'
{ agl, ... }:
{
  server.llama.url = "http://127.0.0.1:18080";
}
NIX
fi

echo
agl --version 2>/dev/null || "$HOME/.nix-profile/bin/agl" --version
case "$variant" in
  wsl) echo "The server runs on Windows, which WSL reaches in its mirrored network: set networkingMode=mirrored under [wsl2] in %UserProfile%\\.wslconfig, then run wsl --shutdown." ;;
  outside) echo "Start the server elsewhere, with the line of: agl server command --function <name>, and set server.llama.url and server.llama.external = true in ~/.config/agentLIBRE/settings.nix." ;;
esac
echo "Open a new terminal if agl is not found, then run: agl"
