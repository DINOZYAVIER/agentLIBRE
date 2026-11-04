# Qwen3.8-27B in unsloth's UD-IQ4_XS (14.3 GB), the default model (AGL-039,
# D33, D43, D44): multi-token prediction and n-gram drafts of what the context
# already holds (AGL-050), the projector on the CPU, 128K of context with a q8
# KV cache, the whole model on the GPU. The weights are
# found by their file name in the weights folders (D23, D24); `source` says
# where agl gets them, at a pinned version (D5, D27, D29; AGL-050, D8).
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats."qwen3.8";
  weights = "Qwen3.8-27B-UD-IQ4_XS.gguf";
  mmproj = "mmproj-Qwen3.8-27B-F16.gguf";
  mmproj_offload = false;
  spec_type = "draft-mtp,ngram-mod";
  context = 131072;
  args = [ "-ngl" "999" "-fa" "on" "-ctk" "q8_0" "-ctv" "q8_0" ];
  sampling = {
    temperature = 1.0;
    top_p = 0.95;
    top_k = 20;
  };
  source = {
    weights = {
      url = "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-UD-IQ4_XS.gguf";
      sha256 = "40fac4050e940397dbf13087afd50f4734a11805bf9d65ef8ddd7483470e6199";
      size = 14252845984;
    };
    # Saved under the local name above (D29).
    mmproj = {
      url = "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/4ca720788d1e01f1bff70c033e0d0028fd02e502/mmproj-F16.gguf";
      sha256 = "cbb841a9ee0636b2ec172f5bb8df2ea8dfeb01e90fe7c6126581d662a0b4e43e";
      size = 927607488;
    };
  };
}
