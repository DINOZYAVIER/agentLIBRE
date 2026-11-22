# The Functions: every `functions/<name>.nix`, by its name, each applied to the
# `agl` argument. Adding a file is enough.
# These are the tests' built-in Functions, in place of agentLIBRE-functions
# (AGL-053, D8; AGL-121, D26): `fixture-high` has the full agentLIBRE policy
# ("high" as in gpu-high, not that Function), `fixture-minimal` the laconic
# one, `fixture-gemma` the Gemma format. `coder` is no file here: the tests
# write `CODER` (function/tests.rs), a policy of several states, under it.
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
