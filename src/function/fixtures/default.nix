# The Functions: every `functions/<name>.nix`, by its name, each applied to the
# `agl` argument. Adding a file is enough.
{ agl }:
let
  files = builtins.readDir ./functions;
  named = file: builtins.match "(.+)\\.nix" file;
  regular = builtins.filter (file: files.${file} == "regular" && named file != null) (builtins.attrNames files);
in
builtins.listToAttrs (
  map (file: {
    name = builtins.head (named file);
    value = import (./functions + "/${file}") { inherit agl; };
  }) regular
)
