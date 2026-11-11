# Qwen3.8-27B as the ThinkingCap finetune of BottleCap AI (the weights cut the
# reasoning tokens about 37% on average while holding 85.8% of the base
# accuracy), in its IQ4_XS (15.5 GB): the attention projections of the
# full-attention layers and the linear-attention output projections stay at
# 6-8 bit, which makes it 1.2 GB heavier than unsloth's UD-IQ4_XS. As the
# default model: multi-token prediction of the file's own MTP head and n-gram
# drafts (AGL-050), 128K of context with a q8 KV cache, the whole model on the
# GPU (D3). The vision projector is the finetune's own. The weights are
# found by their file name in the weights folders (D23, D24); `source` says
# where agl gets them, at a pinned version (D5, D27, D29).
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats."qwen3.8";
  weights = "ThinkingCap-Qwen3.8-27B-IQ4_XS.gguf";
  mmproj = "mmproj-ThinkingCap-Qwen3.8-27B-f16.gguf";
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
      url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-27B-GGUF/resolve/dc3aa8563d5d48d1d83637eb67147e1b52215cf7/ThinkingCap-Qwen3.8-27B-IQ4_XS.gguf";
      sha256 = "c69ef3bbd56fe528bc9683186d14047243b00f41d3707016718775841d877cb2";
      size = 15475951712;
    };
    # Saved under the local name above (D29).
    mmproj = {
      url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-27B-GGUF/resolve/dc3aa8563d5d48d1d83637eb67147e1b52215cf7/mmproj-ThinkingCap-Qwen3.8-27B-f16.gguf";
      sha256 = "98fa9aad59b42449786a16bbce96bcd92204d03cac0aee0cdccca711c2adefd1";
      size = 931146272;
    };
  };
}
