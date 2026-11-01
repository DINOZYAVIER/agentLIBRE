# The `agl` argument of every Function and settings file (AGL-039, D7,
# D35): the built-in policies, formats, models and Functions. A fixpoint,
# so the built-in Functions and models take their parts from it.
let
  agl = {
    policies = {
      agentLIBRE = import ./policies/agentLIBRE.nix;
      minimal = import ./policies/minimal.nix;
    };
    formats = {
      "qwen3.8" = import ./formats/qwen3.8.nix;
    };
    models = {
      "qwen3.8-27b-iq4_xs" = import ./models/qwen3.8-27b-iq4_xs.nix { inherit agl; };
    };
    functions = {
      agentLIBRE-gpu-high = import ./functions/agentLIBRE-gpu-high.nix { inherit agl; };
      agentLIBRE-gpu-high-minimal = import ./functions/agentLIBRE-gpu-high-minimal.nix { inherit agl; };
    };
  };
in
agl
