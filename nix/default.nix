# The `agl` argument of every Function and settings file (AGL-039, D7,
# D35): the built-in policies, formats, models and Functions, and the engine
# arguments the models share. A fixpoint, so the built-in Functions and models
# take their parts from it.
let
  agl = {
    policies = {
      agentLIBRE = import ./policies/agentLIBRE.nix;
      minimal = import ./policies/minimal.nix;
    };
    formats = {
      "qwen3.8" = import ./formats/qwen3.8.nix;
      gemma4 = import ./formats/gemma4.nix;
    };
    # A model with a separate drafter (AGL-050): whole on the GPU, or on the
    # CPU with no GPU touched. The KV cache is in q8 (D3), and that needs
    # FlashAttention.
    # The second slot is for the dictation: it goes on while a turn runs
    # (AGL-051), and the two share the one context.
    args = {
      gpu = [ "-ngl" "999" "-ngld" "999" "-fa" "on" "-ctk" "q8_0" "-ctv" "q8_0" "-np" "2" "-kvu" ];
      cpu = [ "--device" "none" "-ngl" "0" "--device-draft" "none" "-ngld" "0" "-fa" "on" "-ctk" "q8_0" "-ctv" "q8_0" "-np" "2" "-kvu" ];
    };
    models = {
      "qwen3.8-27b-iq4_xs" = import ./models/qwen3.8-27b-iq4_xs.nix { inherit agl; };
      "gemma-4-e2b-qat" = import ./models/gemma-4-e2b-qat.nix { inherit agl; };
      "gemma-4-e4b-qat" = import ./models/gemma-4-e4b-qat.nix { inherit agl; };
      "gemma-4-12b-qat" = import ./models/gemma-4-12b-qat.nix { inherit agl; };
    };
    functions = {
      agentLIBRE-cpu-low = import ./functions/agentLIBRE-cpu-low.nix { inherit agl; };
      agentLIBRE-cpu-medium = import ./functions/agentLIBRE-cpu-medium.nix { inherit agl; };
      agentLIBRE-cpu-high = import ./functions/agentLIBRE-cpu-high.nix { inherit agl; };
      agentLIBRE-cpu-very-high = import ./functions/agentLIBRE-cpu-very-high.nix { inherit agl; };
      agentLIBRE-gpu-low = import ./functions/agentLIBRE-gpu-low.nix { inherit agl; };
      agentLIBRE-gpu-medium = import ./functions/agentLIBRE-gpu-medium.nix { inherit agl; };
      agentLIBRE-gpu-high = import ./functions/agentLIBRE-gpu-high.nix { inherit agl; };
      agentLIBRE-gpu-high-minimal = import ./functions/agentLIBRE-gpu-high-minimal.nix { inherit agl; };
      agentLIBRE-gpu-very-high = import ./functions/agentLIBRE-gpu-very-high.nix { inherit agl; };
    };
  };
in
agl
