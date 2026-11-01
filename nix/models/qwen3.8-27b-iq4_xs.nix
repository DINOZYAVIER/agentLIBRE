# Qwen3.8-27B in unsloth's UD-IQ4_XS (14.3 GB), the default model (AGL-039,
# D33, D43, D44): multi-token prediction, the projector on the CPU, 128K of
# context with a q8 KV cache, the whole model on the GPU. The weights are
# found by their file name in the weights folders (D23, D24); `source` says
# where they can be got (D5, D27, D29), agl does not fetch them.
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats."qwen3.8";
  weights = "Qwen3.8-27B-UD-IQ4_XS.gguf";
  mmproj = "mmproj-Qwen3.8-27B-F16.gguf";
  mmproj_offload = false;
  spec_type = "draft-mtp";
  context = 131072;
  args = [ "-ngl" "999" "-fa" "on" "-ctk" "q8_0" "-ctv" "q8_0" ];
  sampling = {
    temperature = 1.0;
    top_p = 0.95;
    top_k = 20;
  };
  source = {
    weights = "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/main/Qwen3.8-27B-UD-IQ4_XS.gguf";
    # Saved under the local name above (D29).
    mmproj = "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/main/mmproj-F16.gguf";
  };
}
